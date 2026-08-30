# ABOUTME: Exercises bounded portal-session replacement across GNOME screen lock and unlock.
# ABOUTME: Proves the next shortcut press uses fresh sessions instead of a stale input grant.

import asyncio
import json
import os
from pathlib import Path

import pytest

from helpers.dbus_portal import safe_introspect


DAEMON_BUS_NAME = "io.github.hy26v.Voxkey.Daemon"
DAEMON_OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
DAEMON_INTERFACE = "io.github.hy26v.Voxkey.Daemon1"


@pytest.fixture
def daemon_config(isolated_voxkey_home):
    config_path = isolated_voxkey_home / "voxkey" / "config.toml"
    config_path.write_text('''[injection]
typing_delay_ms = 50

[preview]
mode = "never"
''')
    os.chmod(config_path, 0o600)

    state_dir = Path(os.environ["XDG_STATE_HOME"]) / "voxkey"
    state_dir.mkdir(parents=True, exist_ok=True)
    (state_dir / "history.json").write_text(json.dumps([
        {
            "id": 1,
            "recorded_at_unix_ms": 1,
            "text": "screen lock recovery keeps every temporary key safe",
            "provider": "Whisper.cpp",
            "outcome": "completed",
        }
    ]))
    yield


async def _wait_until(get_value, predicate, timeout=10):
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        value = await get_value()
        if predicate(value):
            return value
        await asyncio.sleep(0.05)
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
async def test_lock_retires_sessions_and_unlock_rebuilds_before_next_press(
    daemon_process,
    dbus_session,
    portal_control,
):
    assert daemon_process.reached_idle, (
        f"Daemon did not reach Idle: {daemon_process.startup_lines}"
    )
    daemon = await _daemon_interface(dbus_session)
    portal_control.clear_metrics()

    async def state_and_connection():
        return await daemon.get_state(), await daemon.get_portal_connected()

    try:
        portal_control.emit_screen_locked(True)
        locked = await _wait_until(
            state_and_connection,
            lambda value: value == ("RecoveringSession", False),
        )
        assert locked == ("RecoveringSession", False)
        assert daemon_process.poll() is None, "Daemon exited instead of waiting for unlock"

        closed = portal_control.closed_session_types()
        assert "remote_desktop" not in closed, (
            "Screen lock found a RemoteDesktop grant while Voxkey was idle"
        )
        assert "shortcuts" in closed

        portal_control.emit_screen_locked(False)
        ready = await _wait_until(
            state_and_connection,
            lambda value: value == ("Idle", True),
        )
        assert ready == ("Idle", True)
        assert daemon_process.poll() is None
        assert portal_control.active_session_types() == ["shortcuts"]

        portal_control.emit_activated()
        assert await _wait_until(
            daemon.get_state,
            lambda value: value == "Recording",
        ) == "Recording"
        assert "remote_desktop" not in portal_control.active_session_types(), (
            "Recording audio acquired text-insertion access prematurely"
        )
    finally:
        portal_control.emit_screen_locked(False)


@pytest.mark.asyncio
async def test_eis_loss_just_before_lock_signal_enters_bounded_recovery(
    daemon_process,
    dbus_session,
    portal_control,
):
    """GNOME can remove EIS before ActiveChanged(true) reaches the daemon."""
    assert daemon_process.reached_idle, (
        f"Daemon did not reach Idle: {daemon_process.startup_lines}"
    )
    daemon = await _daemon_interface(dbus_session)
    portal_control.clear_metrics()

    async def state_and_connection():
        return await daemon.get_state(), await daemon.get_portal_connected()

    try:
        await daemon.call_insert_last_transcript()

        async def injecting_with_remote_desktop():
            return await daemon.get_state(), portal_control.active_session_types()

        active = await _wait_until(
            injecting_with_remote_desktop,
            lambda value: (
                value[0] == "Injecting" and "remote_desktop" in value[1]
            ),
        )
        assert active[0] == "Injecting"

        portal_control.emit_remote_desktop_loss_then_screen_locked()
        locked = await _wait_until(
            state_and_connection,
            lambda value: value == ("RecoveringSession", False),
        )
        assert locked == ("RecoveringSession", False)
        assert daemon_process.poll() is None, (
            "Daemon treated the real lock-order race as a fatal portal failure"
        )

        closed = portal_control.closed_session_types()
        assert "remote_desktop" in closed
        assert "shortcuts" in closed

        portal_control.emit_screen_locked(False)
        ready = await _wait_until(
            state_and_connection,
            lambda value: value == ("Idle", True),
        )
        assert ready == ("Idle", True)
        assert daemon_process.poll() is None
        assert portal_control.active_session_types() == ["shortcuts"]
    finally:
        portal_control.emit_screen_locked(False)
