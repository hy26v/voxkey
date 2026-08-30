# ABOUTME: Verifies that the daemon follows the settings application's lifetime.
# ABOUTME: Uses a private D-Bus and virtual test microphone; never touches the desktop session.

import asyncio
import time

import pytest
from dbus_next.aio import MessageBus

from helpers.dbus_portal import safe_introspect


DAEMON_BUS_NAME = "io.github.hy26v.Voxkey.Daemon"
DAEMON_OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
DAEMON_INTERFACE = "io.github.hy26v.Voxkey.Daemon1"
SETTINGS_BUS_NAME = "io.github.hy26v.Voxkey"


@pytest.mark.asyncio
async def test_daemon_stops_when_attached_settings_application_disappears(
    daemon_process,
    mock_portal,
):
    """A GUI crash must stop the daemon even though no GUI exit hook can run."""
    assert daemon_process.reached_idle, daemon_process.startup_lines
    bus_address, _, _ = mock_portal
    settings_bus = await MessageBus(bus_address=bus_address).connect()
    await settings_bus.request_name(SETTINGS_BUS_NAME)

    introspection = await safe_introspect(
        settings_bus,
        DAEMON_BUS_NAME,
        DAEMON_OBJECT_PATH,
    )
    daemon = settings_bus.get_proxy_object(
        DAEMON_BUS_NAME,
        DAEMON_OBJECT_PATH,
        introspection,
    ).get_interface(DAEMON_INTERFACE)
    await daemon.call_attach_settings()

    # Hiding a real window leaves this application name owned. Merely staying
    # connected therefore must not stop the service.
    await asyncio.sleep(0.25)
    assert daemon_process.poll() is None

    # A process exit or SIGKILL has the same observable D-Bus result: its
    # connection and well-known application name disappear.
    settings_bus.disconnect()
    deadline = time.monotonic() + 5
    while daemon_process.poll() is None and time.monotonic() < deadline:
        await asyncio.sleep(0.05)

    assert daemon_process.poll() == 0
