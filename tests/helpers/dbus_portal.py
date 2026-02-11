# ABOUTME: D-Bus helpers for interacting with XDG Desktop Portal interfaces.
# ABOUTME: Provides capability checks, session management, and signal subscriptions.

import asyncio
import re
import xml.etree.ElementTree as ET

from dbus_next.aio import MessageBus
from dbus_next import BusType, Message, MessageType, Variant
from dbus_next.introspection import Node


PORTAL_BUS_NAME = "org.freedesktop.portal.Desktop"
PORTAL_OBJECT_PATH = "/org/freedesktop/portal/desktop"

GLOBAL_SHORTCUTS_IFACE = "org.freedesktop.portal.GlobalShortcuts"
REMOTE_DESKTOP_IFACE = "org.freedesktop.portal.RemoteDesktop"
PROPERTIES_IFACE = "org.freedesktop.DBus.Properties"

KEYBOARD_DEVICE_BIT = 1
POINTER_DEVICE_BIT = 2
TOUCHSCREEN_DEVICE_BIT = 4

_MEMBER_NAME_RE = re.compile(r'^[A-Za-z_][A-Za-z0-9_]*$')


def _strip_invalid_members(element):
    """Remove XML elements whose 'name' attribute is not a valid D-Bus member name."""
    to_remove = []
    for child in element:
        name = child.get("name", "")
        if child.tag in ("property", "method", "signal", "arg") and name and not _MEMBER_NAME_RE.match(name):
            to_remove.append(child)
        else:
            _strip_invalid_members(child)
    for child in to_remove:
        element.remove(child)


async def safe_introspect(bus, bus_name, object_path):
    """Introspect a D-Bus object, stripping members with invalid names.

    dbus_next strictly validates member names (no hyphens allowed), but some
    portal interfaces expose properties like 'power-saver-enabled'. This
    fetches the raw XML and removes those members before parsing.
    """
    reply = await bus.call(
        Message(
            destination=bus_name,
            path=object_path,
            interface="org.freedesktop.DBus.Introspectable",
            member="Introspect",
        )
    )

    if reply.message_type == MessageType.ERROR:
        raise Exception(f"Introspection failed for {bus_name} at {object_path}: {reply.body}")

    xml_str = reply.body[0]
    root = ET.fromstring(xml_str)
    _strip_invalid_members(root)
    cleaned_xml = ET.tostring(root, encoding="unicode")
    return Node.parse(cleaned_xml)


async def get_session_bus():
    """Connect to the session D-Bus."""
    return await MessageBus(bus_type=BusType.SESSION).connect()


async def get_portal_proxy(bus):
    """Get a proxy object for the portal Desktop interface."""
    introspection = await safe_introspect(bus, PORTAL_BUS_NAME, PORTAL_OBJECT_PATH)
    return bus.get_proxy_object(PORTAL_BUS_NAME, PORTAL_OBJECT_PATH, introspection)


async def get_interface_version(proxy, interface_name):
    """Read the 'version' property from a portal interface."""
    props = proxy.get_interface(PROPERTIES_IFACE)
    version_variant = await props.call_get(interface_name, "version")
    return version_variant.value


async def get_available_device_types(proxy):
    """Read AvailableDeviceTypes from RemoteDesktop interface."""
    props = proxy.get_interface(PROPERTIES_IFACE)
    types_variant = await props.call_get(REMOTE_DESKTOP_IFACE, "AvailableDeviceTypes")
    return types_variant.value


async def has_interface(proxy, interface_name):
    """Check if the portal exposes a given interface."""
    try:
        proxy.get_interface(interface_name)
        return True
    except Exception:
        return False


def has_keyboard_support(device_types_bitmask):
    """Check if keyboard bit is set in AvailableDeviceTypes."""
    return bool(device_types_bitmask & KEYBOARD_DEVICE_BIT)


# Pre-built introspection for portal Request and Session interfaces.
# These are well-defined by the XDG portal spec and don't require
# introspecting ephemeral objects that may not exist yet.

_REQUEST_INTROSPECTION = Node.parse(
    '<node>'
    '  <interface name="org.freedesktop.portal.Request">'
    '    <method name="Close"/>'
    '    <signal name="Response">'
    '      <arg type="u" name="response"/>'
    '      <arg type="a{sv}" name="results"/>'
    '    </signal>'
    '  </interface>'
    '</node>'
)

_SESSION_INTROSPECTION = Node.parse(
    '<node>'
    '  <interface name="org.freedesktop.portal.Session">'
    '    <method name="Close"/>'
    '    <signal name="Closed"/>'
    '  </interface>'
    '</node>'
)


async def await_portal_response(bus, request_path, timeout=10):
    """Wait for the portal Response signal on a request path.

    Uses pre-built introspection to avoid racing with request object creation.
    Returns (response_code, results_dict).
    """
    loop = asyncio.get_event_loop()
    future = loop.create_future()

    def _on_response(response, results):
        if not future.done():
            future.set_result((response, results))

    proxy = bus.get_proxy_object(PORTAL_BUS_NAME, request_path, _REQUEST_INTROSPECTION)
    request_iface = proxy.get_interface("org.freedesktop.portal.Request")
    request_iface.on_response(_on_response)

    return await asyncio.wait_for(future, timeout=timeout)


async def close_portal_session(bus, session_handle):
    """Close a portal session using pre-built introspection."""
    proxy = bus.get_proxy_object(PORTAL_BUS_NAME, session_handle, _SESSION_INTROSPECTION)
    session_iface = proxy.get_interface("org.freedesktop.portal.Session")
    await session_iface.call_close()
