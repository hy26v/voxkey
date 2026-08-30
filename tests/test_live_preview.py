# ABOUTME: Verifies batch transcription publishes replaceable, dictionary-corrected previews.
# ABOUTME: Uses a deterministic size-aware backend while audio keeps recording.

import asyncio
import json
import os
import sys

import pytest
from dbus_next.errors import DBusError

from helpers.dbus_portal import safe_introspect


DAEMON_BUS_NAME = "io.github.hy26v.Voxkey.Daemon"
DAEMON_OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
DAEMON_INTERFACE = "io.github.hy26v.Voxkey.Daemon1"


async def _wait_until(get_value, predicate, timeout=10):
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        value = await get_value()
        if predicate(value):
            return value
        await asyncio.sleep(0.1)
    return await get_value()


@pytest.mark.asyncio
async def test_batch_preview_is_replaced_with_dictionary_corrected_text(
    daemon_process,
    dbus_session,
    portal_control,
    virtual_mic,
    fixtures_dir,
):
    assert daemon_process.reached_idle, "Daemon did not reach Idle"

    introspection = await safe_introspect(
        dbus_session,
        DAEMON_BUS_NAME,
        DAEMON_OBJECT_PATH,
    )
    proxy = dbus_session.get_proxy_object(
        DAEMON_BUS_NAME,
        DAEMON_OBJECT_PATH,
        introspection,
    )
    daemon = proxy.get_interface(DAEMON_INTERFACE)

    transcriber = json.loads(await daemon.get_transcriber_config())
    transcriber["provider"] = "whisper-cpp"
    transcriber["whisper_cpp"] = {
        "command": sys.executable,
        "args": [
            "-c",
            "import sys,wave; reader=wave.open(sys.argv[1]); "
            "speech_frames=max(0,reader.getnframes()-reader.getframerate()); "
            "print(('draft' if speech_frames < 3*reader.getframerate() "
            "else 'corrected') + ' vox key preview')",
            "{audio_file}",
        ],
    }
    dictionary = {
        "replacements": [
            {
                "original": "vox key",
                "replacement": "Voxkey",
                "enabled": True,
            }
        ],
        "vocabulary": [],
    }

    await daemon.call_set_dictionary_config(json.dumps(dictionary))
    await daemon.call_set_transcriber_config(json.dumps(transcriber))

    # Both configuration writes request a daemon session rebuild. Require a
    # stable connected/idle observation so activation targets the rebuilt one.
    await asyncio.sleep(0.5)

    async def session_ready():
        return await daemon.get_portal_connected(), await daemon.get_state()

    ready = await _wait_until(session_ready, lambda value: value == (True, "Idle"))
    assert ready == (True, "Idle")
    await asyncio.sleep(0.3)

    portal_control.emit_activated()
    state = await _wait_until(daemon.get_state, lambda value: value == "Recording")
    assert state == "Recording"

    try:
        virtual_mic.stream_file(os.path.join(fixtures_dir, "long_passage.wav"))
        first_preview = await _wait_until(
            daemon.get_live_transcript,
            lambda value: value == "draft Voxkey preview",
            timeout=12,
        )
        assert first_preview == "draft Voxkey preview"

        corrected_preview = await _wait_until(
            daemon.get_live_transcript,
            lambda value: value == "corrected Voxkey preview",
            timeout=12,
        )
        assert corrected_preview == "corrected Voxkey preview"
        assert await daemon.get_state() == "Recording"
    finally:
        virtual_mic.stop_playback()


@pytest.mark.asyncio
async def test_settings_change_cannot_abandon_an_active_recording_preview(
    daemon_process,
    dbus_session,
    portal_control,
    virtual_mic,
    fixtures_dir,
):
    assert daemon_process.reached_idle, "Daemon did not reach Idle"

    introspection = await safe_introspect(
        dbus_session,
        DAEMON_BUS_NAME,
        DAEMON_OBJECT_PATH,
    )
    daemon = dbus_session.get_proxy_object(
        DAEMON_BUS_NAME,
        DAEMON_OBJECT_PATH,
        introspection,
    ).get_interface(DAEMON_INTERFACE)

    transcriber = json.loads(await daemon.get_transcriber_config())
    transcriber["provider"] = "whisper-cpp"
    transcriber["whisper_cpp"] = {
        "command": sys.executable,
        "args": ["-c", "print('abandoned preview')", "{audio_file}"],
    }
    await daemon.call_set_transcriber_config(json.dumps(transcriber))
    await asyncio.sleep(0.5)

    async def session_ready():
        return await daemon.get_portal_connected(), await daemon.get_state()

    assert await _wait_until(
        session_ready, lambda value: value == (True, "Idle"),
    ) == (True, "Idle")
    await asyncio.sleep(0.3)

    portal_control.emit_activated()
    assert await _wait_until(
        daemon.get_state, lambda value: value == "Recording",
    ) == "Recording"

    try:
        virtual_mic.stream_file(os.path.join(fixtures_dir, "long_passage.wav"))
        assert await _wait_until(
            daemon.get_live_transcript,
            lambda value: value == "abandoned preview",
            timeout=12,
        ) == "abandoned preview"

        injection = await daemon.get_injection_config()
        with pytest.raises(DBusError, match="Cannot change settings"):
            await daemon.call_set_injection_config(injection)

        assert await daemon.get_state() == "Recording"
        assert await daemon.get_live_transcript() == "abandoned preview"
        await daemon.call_cancel_dictation()
        assert await _wait_until(
            session_ready, lambda value: value == (True, "Idle"),
        ) == (True, "Idle")
        assert await _wait_until(
            daemon.get_live_transcript,
            lambda value: value == "",
            timeout=3,
        ) == "", "preview from the cancelled recording remained visible"
    finally:
        virtual_mic.stop_playback()


@pytest.mark.asyncio
async def test_final_transcript_reuses_the_live_preview(
    daemon_process,
    dbus_session,
    portal_control,
    virtual_mic,
    fixtures_dir,
):
    """The inserted text must be exactly what the overlay displayed.

    VoiceInk-style guarantee: the final transcript reuses the last
    whole-recording preview decode instead of running a second, independent
    decode that could disagree. We assert the visible property (the final
    transcript equals the preview the user saw) and that the daemon logged
    the reuse. The isolated portal's EIS keyboard accepts the text, so the
    daemon must remain connected and return cleanly to Idle after publishing it.
    """
    assert daemon_process.reached_idle, "Daemon did not reach Idle"

    introspection = await safe_introspect(
        dbus_session,
        DAEMON_BUS_NAME,
        DAEMON_OBJECT_PATH,
    )
    daemon = dbus_session.get_proxy_object(
        DAEMON_BUS_NAME,
        DAEMON_OBJECT_PATH,
        introspection,
    ).get_interface(DAEMON_INTERFACE)

    preview_text = "voxkey stable preview text now"
    transcriber = json.loads(await daemon.get_transcriber_config())
    transcriber["provider"] = "whisper-cpp"
    transcriber["whisper_cpp"] = {
        "command": sys.executable,
        "args": [
            "-c",
            "import os,sys; "
            f"print({preview_text!r} if "
            "os.path.basename(sys.argv[1]).startswith('voxkey_preview_') "
            "else 'unexpected fresh final decode')",
            "{audio_file}",
        ],
    }
    await daemon.call_set_transcriber_config(json.dumps(transcriber))

    await asyncio.sleep(0.5)

    async def session_ready():
        return await daemon.get_portal_connected(), await daemon.get_state()

    assert await _wait_until(
        session_ready, lambda value: value == (True, "Idle"),
    ) == (True, "Idle")
    await asyncio.sleep(0.3)

    portal_control.emit_activated()
    assert await _wait_until(
        daemon.get_state, lambda value: value == "Recording",
    ) == "Recording"

    try:
        virtual_mic.stream_file(os.path.join(fixtures_dir, "long_passage.wav"))
        assert await _wait_until(
            daemon.get_live_transcript,
            lambda value: value == preview_text,
            timeout=12,
        ) == preview_text
        await asyncio.to_thread(virtual_mic.wait_for_playback, 15)
    finally:
        virtual_mic.stop_playback()

    # The signal is emitted when the transcript is published, before the
    # isolated EIS keyboard acknowledges the injection.
    received = asyncio.get_running_loop().create_future()

    def on_transcription_complete(text):
        if not received.done():
            received.set_result(text)

    daemon.on_transcription_complete(on_transcription_complete)

    await daemon.call_stop_dictation()

    final = await asyncio.wait_for(received, timeout=15)
    assert final == preview_text, "overlay text and inserted text disagree"
    assert await _wait_until(
        session_ready, lambda value: value == (True, "Idle"), timeout=10,
    ) == (True, "Idle")
    assert daemon_process.poll() is None, "isolated EIS injection killed the daemon"
