# ABOUTME: Verifies opt-in system-output muting against the isolated PipeWire server.
# ABOUTME: Proves Voxkey restores only the mute state it changed itself.

import asyncio
import subprocess

import pytest

from helpers.dbus_portal import safe_introspect


BUS = "io.github.hy26v.Voxkey.Daemon"
PATH = "/io/github/hy26v/Voxkey/Daemon"
IFACE = "io.github.hy26v.Voxkey.Daemon1"


@pytest.fixture
def daemon_config(isolated_voxkey_home):
    config = isolated_voxkey_home / "voxkey/config.toml"
    config.write_text("[audio]\nmute_output_while_recording = true\ntail_capture_ms = 0\n")
    yield


def pactl(*arguments):
    return subprocess.run(
        ["pactl", *arguments], capture_output=True, text=True, check=True
    ).stdout.strip()


async def wait_for_state(daemon, expected, timeout=10):
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        state = await daemon.get_state()
        if state == expected:
            return state
        await asyncio.sleep(0.1)
    return await daemon.get_state()


async def daemon_proxy(daemon_process, dbus_session):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    introspection = await safe_introspect(dbus_session, BUS, PATH)
    return dbus_session.get_proxy_object(BUS, PATH, introspection).get_interface(IFACE)


@pytest.mark.asyncio
async def test_voxkey_restores_an_output_it_muted(daemon_process, dbus_session):
    daemon = await daemon_proxy(daemon_process, dbus_session)
    sink = pactl("get-default-sink")
    pactl("set-sink-mute", sink, "0")

    await daemon.call_start_dictation()
    assert await wait_for_state(daemon, "Recording") == "Recording"
    assert pactl("get-sink-mute", sink) == "Mute: yes"

    await daemon.call_cancel_dictation()
    assert await wait_for_state(daemon, "Idle") == "Idle"
    assert pactl("get-sink-mute", sink) == "Mute: no"


@pytest.mark.asyncio
async def test_voxkey_leaves_an_already_muted_output_muted(daemon_process, dbus_session):
    daemon = await daemon_proxy(daemon_process, dbus_session)
    sink = pactl("get-default-sink")
    pactl("set-sink-mute", sink, "1")
    try:
        await daemon.call_start_dictation()
        assert await wait_for_state(daemon, "Recording") == "Recording"
        assert pactl("get-sink-mute", sink) == "Mute: yes"

        await daemon.call_cancel_dictation()
        assert await wait_for_state(daemon, "Idle") == "Idle"
        assert pactl("get-sink-mute", sink) == "Mute: yes"
    finally:
        pactl("set-sink-mute", sink, "0")
