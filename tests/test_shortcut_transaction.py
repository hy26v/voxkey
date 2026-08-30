# ABOUTME: Verifies shortcut changes are persisted only after portal acceptance.
# ABOUTME: A rejected binding must leave the live daemon and saved configuration usable.

import asyncio
import os
import tomllib
from pathlib import Path

import pytest

from helpers.dbus_portal import safe_introspect


BUS_NAME = "io.github.hy26v.Voxkey.Daemon"
OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
INTERFACE = "io.github.hy26v.Voxkey.Daemon1"


async def _daemon_proxy(dbus_session):
    return dbus_session.get_proxy_object(
        BUS_NAME,
        OBJECT_PATH,
        await safe_introspect(dbus_session, BUS_NAME, OBJECT_PATH),
    ).get_interface(INTERFACE)


async def _wait_for_effective_shortcut(proxy, expected, timeout=3):
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        if await proxy.get_shortcut_description() == expected:
            return
        await asyncio.sleep(0.05)
    assert await proxy.get_shortcut_description() == expected


@pytest.mark.asyncio
async def test_safe_single_key_shortcut_is_persisted_after_portal_acceptance(
    daemon_process,
    dbus_session,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    proxy = await _daemon_proxy(dbus_session)

    await proxy.call_set_shortcut("F13")

    assert daemon_process.poll() is None, "setting F13 stopped the daemon"
    assert await proxy.get_shortcut_trigger() == "F13"
    await _wait_for_effective_shortcut(proxy, "F13")
    config_path = Path(os.environ["XDG_CONFIG_HOME"]) / "voxkey" / "config.toml"
    with config_path.open("rb") as config_file:
        shortcut = tomllib.load(config_file)["shortcut"]
    assert shortcut["trigger"] == "F13"
    assert shortcut["id"].startswith("dictate_toggle_")


@pytest.mark.asyncio
async def test_rejected_shortcut_is_not_persisted(
    daemon_process,
    dbus_session,
    portal_control,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    proxy = await _daemon_proxy(dbus_session)
    old_trigger = await proxy.get_shortcut_trigger()
    config_path = Path(os.environ["XDG_CONFIG_HOME"]) / "voxkey" / "config.toml"
    before = config_path.read_bytes() if config_path.exists() else None

    portal_control.reject_next_shortcut_bind()
    with pytest.raises(Exception):
        await proxy.call_set_shortcut("<Super><Alt>k")

    assert daemon_process.poll() is None, "a rejected shortcut stopped the daemon"
    assert await proxy.get_shortcut_trigger() == old_trigger
    after = config_path.read_bytes() if config_path.exists() else None
    assert after == before, "the rejected shortcut changed config.toml"
