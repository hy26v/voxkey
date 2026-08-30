# ABOUTME: Holding the dictation shortcut must not start a second recording.
# ABOUTME: GNOME repeats Activated while the key is held, across state changes.

import asyncio
import json
import sys

import pytest

from helpers.dbus_portal import safe_introspect


DAEMON_BUS_NAME = "io.github.hy26v.Voxkey.Daemon"
DAEMON_OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
DAEMON_INTERFACE = "io.github.hy26v.Voxkey.Daemon1"

# GNOME's shortcut repeat interval while a key stays held.
REPEAT_INTERVAL = 0.03


async def _wait_until(get_value, predicate, timeout=10):
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        value = await get_value()
        if predicate(value):
            return value
        await asyncio.sleep(0.05)
    return await get_value()


async def _daemon_with_silent_backend(dbus_session):
    """A daemon whose transcriber returns nothing, so a dictation completes
    without making text-injection behavior part of this repeat test."""
    introspection = await safe_introspect(
        dbus_session, DAEMON_BUS_NAME, DAEMON_OBJECT_PATH,
    )
    daemon = dbus_session.get_proxy_object(
        DAEMON_BUS_NAME, DAEMON_OBJECT_PATH, introspection,
    ).get_interface(DAEMON_INTERFACE)

    transcriber = json.loads(await daemon.get_transcriber_config())
    transcriber["provider"] = "whisper-cpp"
    transcriber["whisper_cpp"] = {
        "command": sys.executable,
        "args": ["-c", "pass", "{audio_file}"],
    }
    await daemon.call_set_transcriber_config(json.dumps(transcriber))
    await asyncio.sleep(0.5)

    async def session_ready():
        return await daemon.get_portal_connected(), await daemon.get_state()

    assert await _wait_until(
        session_ready, lambda value: value == (True, "Idle"),
    ) == (True, "Idle")
    await asyncio.sleep(0.3)
    return daemon


@pytest.mark.asyncio
async def test_holding_the_shortcut_past_a_dictation_starts_nothing_new(
    daemon_process, dbus_session, portal_control, virtual_mic,
):
    """Keeping the shortcut held must not begin a second dictation.

    The shortcut is repeated for as long as the key is down. Voxkey ignored
    those repeats only while it was recording, so a dictation that finished
    while the key was still held saw the very next repeat as a fresh press
    and started recording again behind the user's back.
    """
    assert daemon_process.reached_idle, "Daemon did not reach Idle"
    daemon = await _daemon_with_silent_backend(dbus_session)

    # Press and hold: one press plus repeats.
    portal_control.emit_activated()
    assert await _wait_until(
        daemon.get_state, lambda value: value == "Recording",
    ) == "Recording"
    for _ in range(5):
        portal_control.emit_activated()
        await asyncio.sleep(REPEAT_INTERVAL)

    # Release, then press and hold again to stop the dictation.
    portal_control.emit_deactivated()
    await asyncio.sleep(0.05)
    portal_control.emit_activated()

    # Keep the key held while the dictation finishes and returns to Idle.
    for _ in range(60):
        portal_control.emit_activated()
        await asyncio.sleep(REPEAT_INTERVAL)
    portal_control.emit_deactivated()

    state = await _wait_until(
        daemon.get_state,
        lambda value: value == "Idle",
    )
    assert state == "Idle", (
        f"holding the shortcut started another recording; daemon is {state!r}"
    )


@pytest.mark.asyncio
async def test_a_separate_press_still_starts_a_new_dictation(
    daemon_process, dbus_session, portal_control, virtual_mic,
):
    """The repeat filter must not swallow a genuine second press."""
    assert daemon_process.reached_idle, "Daemon did not reach Idle"
    daemon = await _daemon_with_silent_backend(dbus_session)

    portal_control.emit_activated()
    assert await _wait_until(
        daemon.get_state, lambda value: value == "Recording",
    ) == "Recording"

    portal_control.emit_deactivated()
    await asyncio.sleep(0.4)
    portal_control.emit_activated()
    portal_control.emit_deactivated()
    assert await _wait_until(
        daemon.get_state, lambda value: value == "Idle",
    ) == "Idle", "the second press did not stop the dictation"

    # A deliberate new press, well after the previous one.
    await asyncio.sleep(0.4)
    portal_control.emit_activated()
    assert await _wait_until(
        daemon.get_state, lambda value: value == "Recording",
    ) == "Recording", "a deliberate new press was mistaken for a key repeat"

    portal_control.emit_deactivated()
    await asyncio.sleep(0.4)
    portal_control.emit_activated()
    portal_control.emit_deactivated()


@pytest.mark.asyncio
async def test_back_to_back_release_and_repress_keep_portal_order(
    daemon_process, dbus_session, portal_control, virtual_mic,
):
    """A queued release must always be handled before the following press."""
    assert daemon_process.reached_idle, "Daemon did not reach Idle"
    daemon = await _daemon_with_silent_backend(dbus_session)

    portal_control.emit_activated()
    assert await _wait_until(
        daemon.get_state, lambda value: value == "Recording",
    ) == "Recording"

    # Leave each press held, then queue its release immediately before the
    # next press. Separate Activated and Deactivated subscriptions used to let
    # select! reverse this boundary when both streams were ready together.
    expected = "Idle"
    for _ in range(20):
        portal_control.emit_deactivated()
        portal_control.emit_activated()
        assert await _wait_until(
            daemon.get_state, lambda value: value == expected,
        ) == expected
        expected = "Recording" if expected == "Idle" else "Idle"

    portal_control.emit_deactivated()
    portal_control.emit_activated()
    portal_control.emit_deactivated()
    assert await _wait_until(
        daemon.get_state, lambda value: value == "Idle",
    ) == "Idle"
