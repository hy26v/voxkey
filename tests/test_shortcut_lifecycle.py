# ABOUTME: Tests the GlobalShortcuts portal session and shortcut binding lifecycle.
# ABOUTME: Validates create session, bind shortcuts, activated/deactivated signals, and cleanup.

import asyncio
import uuid

import pytest
import pytest_asyncio
from dbus_next import Variant

from helpers.dbus_portal import (
    GLOBAL_SHORTCUTS_IFACE,
    PORTAL_BUS_NAME,
    has_interface,
    await_portal_response,
    close_portal_session,
    safe_introspect,
)


def _handle_token():
    """Generate a unique handle_token for portal requests."""
    return "voxkey_test_" + uuid.uuid4().hex[:8]


async def _await_response(bus, sender_name, request_path, timeout=10):
    """Wait for the portal Response signal on a request path."""
    return await await_portal_response(bus, request_path, timeout=timeout)


def _make_request_path(bus, token):
    """Build the expected portal request object path."""
    sender = bus.unique_name.replace(".", "_").replace(":", "")
    return f"/org/freedesktop/portal/desktop/request/{sender}/{token}"


async def _create_shortcuts_session(bus, portal_proxy):
    """Create a GlobalShortcuts session and return the session handle."""
    gs = portal_proxy.get_interface(GLOBAL_SHORTCUTS_IFACE)

    token = _handle_token()
    session_token = _handle_token()

    request_path = _make_request_path(bus, token)

    # Subscribe to the response before making the call
    response_task = asyncio.ensure_future(
        _await_response(bus, PORTAL_BUS_NAME, request_path)
    )
    # Small yield so the subscription is active
    await asyncio.sleep(0.05)

    await gs.call_create_session(
        {
            "handle_token": Variant("s", token),
            "session_handle_token": Variant("s", session_token),
        }
    )

    response_code, results = await response_task
    assert response_code == 0, f"CreateSession failed with response {response_code}"

    return results.get("session_handle", Variant("s", "")).value


async def _close_session(bus, session_handle):
    """Close a portal session."""
    await close_portal_session(bus, session_handle)


@pytest.mark.asyncio
async def test_create_global_shortcuts_session(dbus_session, portal_proxy):
    """Creating a GlobalShortcuts session should succeed and return a session handle."""
    session_handle = await _create_shortcuts_session(dbus_session, portal_proxy)
    assert session_handle, "Session handle must not be empty"
    assert session_handle.startswith("/org/freedesktop/portal/desktop/session/")

    await _close_session(dbus_session, session_handle)


async def _bind_dictate_shortcut(bus, portal_proxy, session_handle):
    """Bind the dictate_hold shortcut. Returns (response_code, results)."""
    gs = portal_proxy.get_interface(GLOBAL_SHORTCUTS_IFACE)

    token = _handle_token()
    request_path = _make_request_path(bus, token)

    shortcuts = [
        ["dictate_hold", {
            "description": Variant("s", "Hold to dictate"),
            "preferred-trigger": Variant("s", "<Super>space"),
        }],
    ]

    response_task = asyncio.ensure_future(
        _await_response(bus, PORTAL_BUS_NAME, request_path)
    )
    await asyncio.sleep(0.05)

    await gs.call_bind_shortcuts(
        session_handle,
        shortcuts,
        "",
        {"handle_token": Variant("s", token)},
    )

    return await response_task


@pytest.mark.asyncio
async def test_bind_shortcut(dbus_session, portal_proxy):
    """Binding a shortcut to a session should succeed."""
    session_handle = await _create_shortcuts_session(dbus_session, portal_proxy)

    response_code, results = await _bind_dictate_shortcut(
        dbus_session, portal_proxy, session_handle,
    )

    assert response_code == 0, (
        f"BindShortcuts failed with response {response_code}"
    )

    # Verify the shortcut appears in the results
    bound_shortcuts = results.get("shortcuts", Variant("a(sa{sv})", [])).value
    shortcut_ids = [s[0] for s in bound_shortcuts]
    assert "dictate_hold" in shortcut_ids, (
        f"'dictate_hold' not in bound shortcuts: {shortcut_ids}"
    )

    await _close_session(dbus_session, session_handle)


@pytest.mark.asyncio
async def test_shortcut_activated_signal(dbus_session, portal_proxy, portal_control):
    """Emitting an Activated signal should be received by the signal listener."""
    session_handle = await _create_shortcuts_session(dbus_session, portal_proxy)
    gs = portal_proxy.get_interface(GLOBAL_SHORTCUTS_IFACE)

    response_code, _ = await _bind_dictate_shortcut(
        dbus_session, portal_proxy, session_handle,
    )
    assert response_code == 0, f"BindShortcuts failed (response={response_code})"

    # Listen for Activated signal
    loop = asyncio.get_event_loop()
    activated = loop.create_future()

    def _on_activated(sess_handle, shortcut_id, timestamp, options):
        if shortcut_id == "dictate_hold" and not activated.done():
            activated.set_result((shortcut_id, timestamp))

    gs.on_activated(_on_activated)
    # Allow time for the match rule to reach the dbus-daemon
    await asyncio.sleep(0.3)

    # Emit Activated via the mock portal controller
    portal_control.emit_activated("dictate_hold")

    try:
        shortcut_id, timestamp = await asyncio.wait_for(activated, timeout=5)
        assert shortcut_id == "dictate_hold"
    finally:
        await _close_session(dbus_session, session_handle)


@pytest.mark.asyncio
async def test_shortcut_deactivated_signal(dbus_session, portal_proxy, portal_control):
    """Emitting a Deactivated signal should be received by the signal listener."""
    session_handle = await _create_shortcuts_session(dbus_session, portal_proxy)
    gs = portal_proxy.get_interface(GLOBAL_SHORTCUTS_IFACE)

    response_code, _ = await _bind_dictate_shortcut(
        dbus_session, portal_proxy, session_handle,
    )
    assert response_code == 0, f"BindShortcuts failed (response={response_code})"

    # Listen for Deactivated signal
    loop = asyncio.get_event_loop()
    deactivated = loop.create_future()

    def _on_deactivated(sess_handle, shortcut_id, timestamp, options):
        if shortcut_id == "dictate_hold" and not deactivated.done():
            deactivated.set_result((shortcut_id, timestamp))

    gs.on_deactivated(_on_deactivated)
    # Allow time for the match rule to reach the dbus-daemon
    await asyncio.sleep(0.3)

    # Emit Deactivated via the mock portal controller
    portal_control.emit_deactivated("dictate_hold")

    try:
        shortcut_id, timestamp = await asyncio.wait_for(deactivated, timeout=5)
        assert shortcut_id == "dictate_hold"
    finally:
        await _close_session(dbus_session, session_handle)


@pytest.mark.asyncio
async def test_session_cleanup(dbus_session, portal_proxy):
    """Closing a session should succeed without error."""
    session_handle = await _create_shortcuts_session(dbus_session, portal_proxy)
    assert session_handle

    # Closing should not raise
    await _close_session(dbus_session, session_handle)

    # After closing, the session object should either be gone (introspection
    # raises) or no longer expose the Session interface.
    session_gone = False
    try:
        node = await safe_introspect(dbus_session, PORTAL_BUS_NAME, session_handle)
        iface_names = [i.name for i in node.interfaces]
        session_gone = "org.freedesktop.portal.Session" not in iface_names
    except Exception:
        session_gone = True
    assert session_gone, "Session interface still available after Close()"


@pytest.mark.asyncio
async def test_duplicate_bind_fails_gracefully(dbus_session, portal_proxy):
    """A second BindShortcuts on the same session should fail or be rejected."""
    session_handle = await _create_shortcuts_session(dbus_session, portal_proxy)

    # First bind
    code1, _ = await _bind_dictate_shortcut(
        dbus_session, portal_proxy, session_handle,
    )
    assert code1 == 0, f"First BindShortcuts failed (response={code1})"

    # Second bind on the same session -- per the spec, BindShortcuts can only
    # be attempted once per session. The portal should return a non-zero
    # response or raise an error.
    second_bind_rejected = False
    try:
        code2, _ = await _bind_dictate_shortcut(
            dbus_session, portal_proxy, session_handle,
        )
        second_bind_rejected = code2 != 0
    except Exception:
        # D-Bus error also counts as graceful rejection
        second_bind_rejected = True

    assert second_bind_rejected, (
        "Second BindShortcuts on the same session should be rejected"
    )

    await _close_session(dbus_session, session_handle)
