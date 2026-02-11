# ABOUTME: Tests that the system exposes required XDG Desktop Portal interfaces.
# ABOUTME: Validates GlobalShortcuts and RemoteDesktop versions and device support.

import pytest
import pytest_asyncio

from helpers.dbus_portal import (
    GLOBAL_SHORTCUTS_IFACE,
    REMOTE_DESKTOP_IFACE,
    KEYBOARD_DEVICE_BIT,
    get_interface_version,
    get_available_device_types,
    has_interface,
    has_keyboard_support,
)


@pytest.mark.asyncio
async def test_global_shortcuts_interface_exists(portal_proxy):
    """GlobalShortcuts interface must be present on the portal."""
    assert await has_interface(portal_proxy, GLOBAL_SHORTCUTS_IFACE)


@pytest.mark.asyncio
async def test_global_shortcuts_version_at_least_1(portal_proxy):
    """GlobalShortcuts version must be >= 1 for shortcut binding."""
    version = await get_interface_version(portal_proxy, GLOBAL_SHORTCUTS_IFACE)
    assert version >= 1, f"GlobalShortcuts version {version} < 1"


@pytest.mark.asyncio
async def test_remote_desktop_interface_exists(portal_proxy):
    """RemoteDesktop interface must be present on the portal."""
    assert await has_interface(portal_proxy, REMOTE_DESKTOP_IFACE)


@pytest.mark.asyncio
async def test_remote_desktop_version_at_least_2(portal_proxy):
    """RemoteDesktop version must be >= 2 for persistence features."""
    version = await get_interface_version(portal_proxy, REMOTE_DESKTOP_IFACE)
    assert version >= 2, f"RemoteDesktop version {version} < 2"


@pytest.mark.asyncio
async def test_available_device_types_includes_keyboard(portal_proxy):
    """AvailableDeviceTypes bitmask must include keyboard (bit 1)."""
    device_types = await get_available_device_types(portal_proxy)
    assert has_keyboard_support(device_types), (
        f"Keyboard bit not set in AvailableDeviceTypes: {device_types}"
    )


@pytest.mark.asyncio
async def test_graceful_error_for_missing_interface(portal_proxy):
    """Querying a nonexistent interface should return False, not crash."""
    result = await has_interface(portal_proxy, "org.freedesktop.portal.DoesNotExist")
    assert result is False
