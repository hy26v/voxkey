# ABOUTME: Verifies the mock portal drops a session when its owning client disconnects.
# ABOUTME: A stale session left behind by a killed daemon must not receive future signals.

import asyncio

import pytest
from dbus_next import Message, Variant
from dbus_next.aio import MessageBus

from helpers.dbus_portal import (
    GLOBAL_SHORTCUTS_IFACE,
    PORTAL_BUS_NAME,
    PORTAL_OBJECT_PATH,
    await_portal_response,
)


async def _create_shortcuts_session(bus):
    """Call GlobalShortcuts.CreateSession and return the session handle."""
    reply = await bus.call(
        Message(
            destination=PORTAL_BUS_NAME,
            path=PORTAL_OBJECT_PATH,
            interface=GLOBAL_SHORTCUTS_IFACE,
            member="CreateSession",
            signature="a{sv}",
            body=[{}],
        )
    )
    request_path = reply.body[0]
    _response_code, results = await await_portal_response(bus, request_path)
    return results["session_handle"].value


class TestStaleSessionCleanup:
    """A session whose owning bus connection has closed must not linger."""

    @pytest.mark.asyncio
    async def test_disconnected_client_session_is_dropped(
        self, mock_portal, portal_control
    ):
        bus_address, controller, _stop = mock_portal

        bus_a = await MessageBus(bus_address=bus_address).connect()
        bus_b = await MessageBus(bus_address=bus_address).connect()

        try:
            session_a = await _create_shortcuts_session(bus_a)
            session_b = await _create_shortcuts_session(bus_b)

            assert session_a in controller._sessions
            assert session_b in controller._sessions

            bus_a.disconnect()

            # NameOwnerChanged propagation is asynchronous; poll briefly.
            for _ in range(50):
                if session_a not in controller._sessions:
                    break
                await asyncio.sleep(0.1)

            assert session_a not in controller._sessions, (
                "Session for a disconnected client was not removed"
            )
            assert session_b in controller._sessions, (
                "Session for the still-connected client was wrongly removed"
            )
        finally:
            bus_b.disconnect()

    @pytest.mark.asyncio
    async def test_emit_activated_does_not_replay_to_dead_sessions(
        self, mock_portal, portal_control
    ):
        """A signal emitted after one client dies must reach the survivor exactly once."""
        bus_address, controller, _stop = mock_portal

        bus_a = await MessageBus(bus_address=bus_address).connect()
        bus_b = await MessageBus(bus_address=bus_address).connect()

        try:
            await _create_shortcuts_session(bus_a)
            session_b = await _create_shortcuts_session(bus_b)

            bus_a.disconnect()
            for _ in range(50):
                if len(controller._sessions) <= 1:
                    break
                await asyncio.sleep(0.1)

            await bus_b.call(
                Message(
                    destination="org.freedesktop.DBus",
                    path="/org/freedesktop/DBus",
                    interface="org.freedesktop.DBus",
                    member="AddMatch",
                    signature="s",
                    body=[f"type='signal',interface='{GLOBAL_SHORTCUTS_IFACE}',member='Activated'"],
                )
            )

            received = []

            def _on_signal(msg):
                if (
                    msg.interface == GLOBAL_SHORTCUTS_IFACE
                    and msg.member == "Activated"
                    and msg.path == PORTAL_OBJECT_PATH
                ):
                    received.append(msg.body[0])
                return False

            bus_b.add_message_handler(_on_signal)

            portal_control.emit_activated()
            await asyncio.sleep(0.5)

            # The real daemon does not filter Activated by session_path (only
            # by shortcut_id), so any extra delivery for a dead session is
            # indistinguishable from a genuine second press.
            assert len(received) == 1, (
                f"Expected exactly one Activated delivery, got: {received}"
            )
        finally:
            bus_b.disconnect()
