# ABOUTME: Tests the RemoteDesktop portal session for keyboard device access.
# ABOUTME: Validates session creation, device selection, session start, and keystroke injection.

import asyncio
import uuid

import pytest
import pytest_asyncio
from dbus_next import Variant

from helpers.dbus_portal import (
    REMOTE_DESKTOP_IFACE,
    PORTAL_BUS_NAME,
    KEYBOARD_DEVICE_BIT,
    has_keyboard_support,
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


async def _create_rd_session(bus, portal_proxy):
    """Create a RemoteDesktop session and return the session handle."""
    rd = portal_proxy.get_interface(REMOTE_DESKTOP_IFACE)

    token = _handle_token()
    session_token = _handle_token()
    request_path = _make_request_path(bus, token)

    response_task = asyncio.ensure_future(
        _await_response(bus, PORTAL_BUS_NAME, request_path)
    )
    await asyncio.sleep(0.05)

    await rd.call_create_session(
        {
            "handle_token": Variant("s", token),
            "session_handle_token": Variant("s", session_token),
        }
    )

    response_code, results = await response_task
    assert response_code == 0, f"CreateSession failed with response {response_code}"

    return results.get("session_handle", Variant("s", "")).value


async def _select_keyboard(bus, portal_proxy, session_handle):
    """Call SelectDevices requesting keyboard access."""
    rd = portal_proxy.get_interface(REMOTE_DESKTOP_IFACE)

    token = _handle_token()
    request_path = _make_request_path(bus, token)

    response_task = asyncio.ensure_future(
        _await_response(bus, PORTAL_BUS_NAME, request_path)
    )
    await asyncio.sleep(0.05)

    await rd.call_select_devices(
        session_handle,
        {
            "handle_token": Variant("s", token),
            "types": Variant("u", KEYBOARD_DEVICE_BIT),
            "persist_mode": Variant("u", 0),
        },
    )

    response_code, results = await response_task
    assert response_code == 0, f"SelectDevices failed with response {response_code}"
    return results


async def _start_session(bus, portal_proxy, session_handle):
    """Call Start on a RemoteDesktop session and return the results."""
    rd = portal_proxy.get_interface(REMOTE_DESKTOP_IFACE)

    token = _handle_token()
    request_path = _make_request_path(bus, token)

    response_task = asyncio.ensure_future(
        _await_response(bus, PORTAL_BUS_NAME, request_path)
    )
    await asyncio.sleep(0.05)

    await rd.call_start(
        session_handle,
        "",
        {"handle_token": Variant("s", token)},
    )

    response_code, results = await response_task
    assert response_code == 0, f"Start failed with response {response_code}"
    return results


async def _close_session(bus, session_handle):
    """Close a portal session."""
    await close_portal_session(bus, session_handle)


@pytest.mark.asyncio
async def test_create_remote_desktop_session(dbus_session, portal_proxy):
    """Creating a RemoteDesktop session should succeed and return a valid handle."""
    session_handle = await _create_rd_session(dbus_session, portal_proxy)
    assert session_handle, "Session handle must not be empty"
    assert session_handle.startswith("/org/freedesktop/portal/desktop/session/")

    await _close_session(dbus_session, session_handle)


@pytest.mark.asyncio
async def test_select_keyboard_device(dbus_session, portal_proxy):
    """SelectDevices with keyboard type should succeed."""
    session_handle = await _create_rd_session(dbus_session, portal_proxy)

    results = await _select_keyboard(dbus_session, portal_proxy, session_handle)
    # SelectDevices response is typically empty on success (code 0 is enough)
    assert results is not None

    await _close_session(dbus_session, session_handle)


@pytest.mark.asyncio
async def test_start_session_grants_keyboard(dbus_session, portal_proxy):
    """Starting a session after selecting keyboard should include keyboard in devices."""
    session_handle = await _create_rd_session(dbus_session, portal_proxy)
    await _select_keyboard(dbus_session, portal_proxy, session_handle)

    results = await _start_session(dbus_session, portal_proxy, session_handle)

    devices = results.get("devices", Variant("u", 0)).value
    assert has_keyboard_support(devices), (
        f"Keyboard bit not set in Start result devices: {devices}"
    )

    await _close_session(dbus_session, session_handle)


@pytest.mark.asyncio
async def test_notify_keyboard_keysym(dbus_session, portal_proxy):
    """NotifyKeyboardKeysym should send a keystroke without error."""
    session_handle = await _create_rd_session(dbus_session, portal_proxy)
    await _select_keyboard(dbus_session, portal_proxy, session_handle)
    await _start_session(dbus_session, portal_proxy, session_handle)

    rd = portal_proxy.get_interface(REMOTE_DESKTOP_IFACE)

    # Send 'a' keysym (0x61) -- press then release
    XKB_KEY_a = 0x61
    try:
        await rd.call_notify_keyboard_keysym(
            session_handle,
            {},
            XKB_KEY_a,
            1,  # pressed
        )
        await rd.call_notify_keyboard_keysym(
            session_handle,
            {},
            XKB_KEY_a,
            0,  # released
        )
    finally:
        await _close_session(dbus_session, session_handle)


@pytest.mark.asyncio
async def test_session_cleanup(dbus_session, portal_proxy):
    """Closing a RemoteDesktop session should succeed without error."""
    session_handle = await _create_rd_session(dbus_session, portal_proxy)
    assert session_handle

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
