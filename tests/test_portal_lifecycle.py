# ABOUTME: Verifies Voxkey only holds privacy-sensitive portal grants while using them.
# ABOUTME: Keeps the global shortcut ready without leaving RemoteDesktop active at idle.

import asyncio
import json
import os
import signal
import sys
import time
from pathlib import Path

import pytest

from helpers.dbus_portal import safe_introspect


DAEMON_BUS_NAME = "io.github.hy26v.Voxkey.Daemon"
DAEMON_OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
DAEMON_INTERFACE = "io.github.hy26v.Voxkey.Daemon1"


@pytest.fixture
def daemon_config(isolated_voxkey_home):
    path = isolated_voxkey_home / "voxkey" / "config.toml"
    path.write_text(
        f'''[transcriber]
provider = "whisper-cpp"

[transcriber.whisper_cpp]
command = {json.dumps(sys.executable)}
args = ["-c", "print('privacy indicator lifecycle')", "{{audio_file}}"]

[injection]
typing_delay_ms = 50

[preview]
mode = "never"
'''
    )
    os.chmod(path, 0o600)

    state_dir = Path(os.environ["XDG_STATE_HOME"]) / "voxkey"
    state_dir.mkdir(parents=True, exist_ok=True)
    (state_dir / "history.json").write_text(
        json.dumps(
            [
                {
                    "id": 1,
                    "recorded_at_unix_ms": 1,
                    "text": "privacy indicator lifecycle",
                    "provider": "Whisper.cpp",
                    "outcome": "completed",
                }
            ]
        )
    )
    yield


async def _wait_until(get_value, predicate, timeout=10):
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        value = await get_value()
        if predicate(value):
            return value
        await asyncio.sleep(0.05)
    return await get_value()


async def _daemon(dbus_session):
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


def test_idle_daemon_releases_remote_desktop_but_keeps_shortcuts(
    daemon_process,
    portal_control,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines

    active = portal_control.active_session_types()
    assert "shortcuts" in active, active
    assert "remote_desktop" not in active, (
        "an idle daemon is still holding the RemoteDesktop grant and keeping "
        "GNOME's screen-sharing privacy indicator active"
    )


@pytest.mark.asyncio
async def test_remote_desktop_exists_only_while_text_is_being_inserted(
    daemon_process,
    dbus_session,
    portal_control,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    daemon = await _daemon(dbus_session)
    portal_control.clear_metrics()

    await daemon.call_insert_last_transcript()

    async def state_and_grants():
        return await daemon.get_state(), portal_control.active_session_types()

    inserting = await _wait_until(
        state_and_grants,
        lambda value: value[0] == "Injecting" and "remote_desktop" in value[1],
    )
    assert inserting[0] == "Injecting"

    idle = await _wait_until(
        state_and_grants,
        lambda value: value[0] == "Idle",
    )
    assert idle[0] == "Idle"
    assert "shortcuts" in idle[1], idle
    assert "remote_desktop" not in idle[1], (
        "RemoteDesktop remained active after the last key was inserted"
    )
    assert "remote_desktop" in portal_control.closed_session_types()


@pytest.mark.asyncio
async def test_shutdown_releases_an_active_remote_desktop_grant(
    daemon_process,
    dbus_session,
    portal_control,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    daemon = await _daemon(dbus_session)
    portal_control.clear_metrics()

    await daemon.call_insert_last_transcript()

    async def active_grant():
        return await daemon.get_state(), portal_control.active_session_types()

    active = await _wait_until(
        active_grant,
        lambda value: value[0] == "Injecting" and "remote_desktop" in value[1],
    )
    assert active[0] == "Injecting"

    started = time.monotonic()
    daemon_process.send_signal(signal.SIGTERM)
    exit_code = await asyncio.wait_for(
        asyncio.to_thread(daemon_process.wait),
        timeout=5,
    )

    assert exit_code == 0
    assert time.monotonic() - started < 5
    closed = portal_control.closed_session_types()
    assert "remote_desktop" in closed
    assert "shortcuts" in closed
    assert "remote_desktop" not in portal_control.active_session_types()
