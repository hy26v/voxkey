# ABOUTME: Verifies that public daemon D-Bus events accompany completed transcriptions.
# ABOUTME: Uses a deterministic local transcriber so signal delivery is observable offline.

import asyncio
import json
import os
import sys

import pytest

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


async def _daemon_interface(dbus_session):
    introspection = await safe_introspect(
        dbus_session,
        DAEMON_BUS_NAME,
        DAEMON_OBJECT_PATH,
    )
    return dbus_session.get_proxy_object(
        DAEMON_BUS_NAME,
        DAEMON_OBJECT_PATH,
        introspection,
    ).get_interface(DAEMON_INTERFACE)


@pytest.mark.asyncio
async def test_set_audio_notifies_both_changed_properties(
    daemon_process,
    dbus_session,
):
    assert daemon_process.reached_idle, (
        f"Daemon did not reach Idle: {daemon_process.startup_lines}"
    )
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
    properties = proxy.get_interface("org.freedesktop.DBus.Properties")

    sample_rate_changed = asyncio.get_running_loop().create_future()
    channels_changed = asyncio.get_running_loop().create_future()

    def on_properties_changed(interface_name, changed, _invalidated):
        if interface_name != DAEMON_INTERFACE:
            return
        if "SampleRate" in changed and not sample_rate_changed.done():
            sample_rate_changed.set_result(changed["SampleRate"].value)
        if "Channels" in changed and not channels_changed.done():
            channels_changed.set_result(changed["Channels"].value)

    properties.on_properties_changed(on_properties_changed)

    await daemon.call_set_audio(22_050, 2)

    assert await asyncio.wait_for(sample_rate_changed, timeout=1) == 22_050
    assert await asyncio.wait_for(channels_changed, timeout=1) == 2


@pytest.mark.asyncio
async def test_desktop_shortcut_change_updates_the_public_binding_description(
    daemon_process,
    dbus_session,
    portal_control,
):
    assert daemon_process.reached_idle, (
        f"Daemon did not reach Idle: {daemon_process.startup_lines}"
    )
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
    properties = proxy.get_interface("org.freedesktop.DBus.Properties")
    changed_description = asyncio.get_running_loop().create_future()

    def on_properties_changed(interface_name, changed, _invalidated):
        if (
            interface_name == DAEMON_INTERFACE
            and "ShortcutDescription" in changed
            and not changed_description.done()
        ):
            changed_description.set_result(
                changed["ShortcutDescription"].value,
            )

    properties.on_properties_changed(on_properties_changed)

    assert await daemon.get_shortcut_description() == "<Super><Alt>d"
    portal_control.emit_shortcuts_changed("F13")

    assert await asyncio.wait_for(changed_description, timeout=1) == "F13"
    assert await daemon.get_shortcut_description() == "F13"


@pytest.mark.asyncio
async def test_completed_dictation_emits_transcription_complete(
    daemon_process,
    dbus_session,
    portal_control,
    virtual_mic,
    fixtures_dir,
):
    assert daemon_process.reached_idle, (
        f"Daemon did not reach Idle: {daemon_process.startup_lines}"
    )

    daemon = await _daemon_interface(dbus_session)

    expected = "signal delivery works"
    received = asyncio.get_running_loop().create_future()

    def on_transcription_complete(text):
        if not received.done():
            received.set_result(text)

    daemon.on_transcription_complete(on_transcription_complete)

    transcriber = json.loads(await daemon.get_transcriber_config())
    transcriber["provider"] = "whisper-cpp"
    transcriber["whisper_cpp"] = {
        "command": sys.executable,
        "args": ["-c", f"print({expected!r})", "{audio_file}"],
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
        virtual_mic.stream_file(os.path.join(fixtures_dir, "hello.wav"))
        await asyncio.sleep(0.5)
        portal_control.emit_deactivated()
        await asyncio.sleep(0.1)
        portal_control.emit_activated()
        portal_control.emit_deactivated()

        assert await asyncio.wait_for(received, timeout=10) == expected
    finally:
        virtual_mic.stop_playback()


@pytest.mark.asyncio
async def test_failed_dictation_emits_error_occurred(
    daemon_process,
    dbus_session,
    portal_control,
    virtual_mic,
    fixtures_dir,
):
    assert daemon_process.reached_idle, (
        f"Daemon did not reach Idle: {daemon_process.startup_lines}"
    )
    daemon = await _daemon_interface(dbus_session)

    received = asyncio.get_running_loop().create_future()

    def on_error_occurred(message):
        if not received.done():
            received.set_result(message)

    daemon.on_error_occurred(on_error_occurred)

    transcriber = json.loads(await daemon.get_transcriber_config())
    transcriber["provider"] = "whisper-cpp"
    transcriber["whisper_cpp"] = {
        "command": sys.executable,
        "args": [
            "-c",
            "import sys; sys.stderr.write('deterministic failure'); sys.exit(7)",
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
        virtual_mic.stream_file(os.path.join(fixtures_dir, "hello.wav"))
        await asyncio.sleep(0.5)
        portal_control.emit_deactivated()
        await asyncio.sleep(0.1)
        portal_control.emit_activated()
        portal_control.emit_deactivated()

        error = await asyncio.wait_for(received, timeout=10)
    finally:
        virtual_mic.stop_playback()

    assert "deterministic failure" in error
    assert await daemon.get_last_error() == error
