# ABOUTME: Exercises the acknowledged dictation controls used by the GNOME Shell extension.
# ABOUTME: Verifies state serialization, cancellation, and real microphone telemetry.

import asyncio
import json
import os
import subprocess
import sys
from pathlib import Path

import pytest
from dbus_next.errors import DBusError

from helpers.dbus_portal import safe_introspect


DAEMON_BUS_NAME = "io.github.hy26v.Voxkey.Daemon"
DAEMON_OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
DAEMON_INTERFACE = "io.github.hy26v.Voxkey.Daemon1"
EXPECTED_DAEMON_PROTOCOL_VERSION = 1


async def _wait_until(get_value, predicate, timeout=10):
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        value = await get_value()
        if predicate(value):
            return value
        await asyncio.sleep(0.05)
    return await get_value()


async def _daemon_proxy(dbus_session):
    introspection = await safe_introspect(
        dbus_session, DAEMON_BUS_NAME, DAEMON_OBJECT_PATH,
    )
    return dbus_session.get_proxy_object(
        DAEMON_BUS_NAME, DAEMON_OBJECT_PATH, introspection,
    ).get_interface(DAEMON_INTERFACE)


async def _configure_backend(daemon, program):
    transcriber = json.loads(await daemon.get_transcriber_config())
    transcriber["provider"] = "whisper-cpp"
    transcriber["whisper_cpp"] = {
        "command": sys.executable,
        "args": ["-c", program, "{audio_file}"],
    }
    await daemon.call_set_transcriber_config(json.dumps(transcriber))

    # The setter acknowledges persistence before the serialized session loop
    # consumes its restart notification. Avoid accepting the old session's
    # still-connected Idle state as evidence that the replacement is ready.
    await asyncio.sleep(0.5)

    async def ready():
        return await daemon.get_portal_connected(), await daemon.get_state()

    assert await _wait_until(
        ready, lambda value: value == (True, "Idle"), timeout=15,
    ) == (True, "Idle")
    # Do not let a request target the retiring portal generation after the
    # first connected/idle observation during session reconstruction.
    await asyncio.sleep(0.3)


@pytest.mark.asyncio
async def test_daemon_reports_the_settings_protocol_it_implements(
    daemon_process, dbus_session,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    daemon = await _daemon_proxy(dbus_session)

    assert await daemon.get_protocol_version() == EXPECTED_DAEMON_PROTOCOL_VERSION


@pytest.mark.asyncio
async def test_dbus_start_cancel_and_audio_level(
    daemon_process, dbus_session, virtual_mic, fixtures_dir,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    daemon = await _daemon_proxy(dbus_session)
    await _configure_backend(daemon, "pass")

    await daemon.call_start_dictation()
    assert await daemon.get_state() == "Recording"

    try:
        virtual_mic.stream_file(os.path.join(fixtures_dir, "long_passage.wav"))
        level = await _wait_until(
            daemon.get_audio_level,
            lambda value: 0.0 < value <= 1.0,
            timeout=8,
        )
        assert 0.0 < level <= 1.0

        with pytest.raises(DBusError, match="Cannot start dictation"):
            await daemon.call_start_dictation()

        await daemon.call_cancel_dictation()
        assert await _wait_until(
            daemon.get_state, lambda value: value == "Idle",
        ) == "Idle"
        assert await daemon.get_live_transcript() == ""
        assert await daemon.get_audio_level() == 0.0
    finally:
        virtual_mic.stop_playback()


@pytest.mark.asyncio
async def test_dbus_stop_finishes_the_normal_batch_flow(
    daemon_process, dbus_session, virtual_mic,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    daemon = await _daemon_proxy(dbus_session)
    await _configure_backend(daemon, "pass")

    await daemon.call_start_dictation()
    assert await daemon.get_state() == "Recording"
    await daemon.call_stop_dictation()

    assert await _wait_until(
        daemon.get_state, lambda value: value == "Idle", timeout=15,
    ) == "Idle"
    with pytest.raises(DBusError, match="no active recording"):
        await daemon.call_stop_dictation()


@pytest.mark.asyncio
async def test_cancel_aborts_a_pending_final_transcription(
    daemon_process, dbus_session, virtual_mic,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    daemon = await _daemon_proxy(dbus_session)
    await _configure_backend(
        daemon,
        "import time; time.sleep(20); print('must never be published')",
    )

    await daemon.call_start_dictation()
    await daemon.call_stop_dictation()
    assert await daemon.get_state() == "Transcribing"

    await asyncio.wait_for(daemon.call_cancel_dictation(), timeout=3)
    assert await daemon.get_state() == "Idle"
    await asyncio.sleep(0.5)
    assert await daemon.get_state() == "Idle"
    assert await daemon.get_last_transcript() != "must never be published"


@pytest.mark.asyncio
async def test_missing_selected_microphone_fails_closed_instead_of_using_default(
    daemon_process, dbus_session,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    daemon = await _daemon_proxy(dbus_session)
    config_path = Path(os.environ["XDG_CONFIG_HOME"]) / "voxkey" / "config.toml"
    missing_device = "Voxkey integration device that does not exist"
    config_path.write_text(
        f'''[audio]
input_device = "{missing_device}"
'''
    )
    os.chmod(config_path, 0o600)

    await daemon.call_reload_config()
    await asyncio.sleep(0.5)

    async def ready():
        return await daemon.get_portal_connected(), await daemon.get_state()

    assert await _wait_until(
        ready, lambda value: value == (True, "Idle"), timeout=15,
    ) == (True, "Idle")
    with pytest.raises(DBusError, match="unavailable"):
        await daemon.call_start_dictation()

    assert await daemon.get_state() == "Idle"
    current_error = await daemon.get_last_error()
    assert missing_device in current_error
    assert await daemon.get_audio_level() == 0.0

    with pytest.raises(DBusError, match="newer error"):
        await daemon.call_dismiss_last_error("an older error from the UI")
    assert await daemon.get_last_error() == current_error

    await daemon.call_dismiss_last_error(current_error)
    assert await daemon.get_last_error() == ""


def _pactl(*args):
    return subprocess.run(
        ["pactl", *args],
        check=True,
        capture_output=True,
        text=True,
        timeout=5,
    ).stdout.strip()


@pytest.mark.asyncio
async def test_capture_limit_fails_promptly_and_restores_owned_output(
    daemon_process, dbus_session, virtual_mic, fixtures_dir,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    daemon = await _daemon_proxy(dbus_session)
    config_path = Path(os.environ["XDG_CONFIG_HOME"]) / "voxkey" / "config.toml"
    config_path.write_text(
        """[audio]
tail_capture_ms = 0
max_recording_seconds = 1
mute_output_while_recording = true
"""
    )
    os.chmod(config_path, 0o600)
    sink = _pactl("get-default-sink")
    _pactl("set-sink-mute", sink, "0")

    try:
        await daemon.call_reload_config()
        await asyncio.sleep(0.5)

        async def ready():
            return await daemon.get_portal_connected(), await daemon.get_state()

        assert await _wait_until(
            ready, lambda value: value == (True, "Idle"), timeout=15,
        ) == (True, "Idle")

        await daemon.call_start_dictation()
        assert await daemon.get_state() == "Recording"
        assert _pactl("get-sink-mute", sink) == "Mute: yes"
        virtual_mic.stream_file(os.path.join(fixtures_dir, "long_passage.wav"))

        assert await _wait_until(
            daemon.get_state, lambda value: value == "Idle", timeout=10,
        ) == "Idle"
        assert "limit" in (await daemon.get_last_error()).lower()
        assert await daemon.get_audio_level() == 0.0
        assert _pactl("get-sink-mute", sink) == "Mute: no"
    finally:
        virtual_mic.stop_playback()
        _pactl("set-sink-mute", sink, "0")
