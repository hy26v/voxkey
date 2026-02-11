# ABOUTME: Mock XDG Desktop Portal for testing voxkey without a live compositor.
# ABOUTME: Runs on a private dbus-daemon and implements GlobalShortcuts, RemoteDesktop, and Registry.

import asyncio
import subprocess
import threading
import time
import uuid

from dbus_next import Message, MessageType, Variant
from dbus_next.aio import MessageBus
from dbus_next.constants import PropertyAccess
from dbus_next.service import ServiceInterface, dbus_property, method, signal


PORTAL_BUS_NAME = "org.freedesktop.portal.Desktop"
PORTAL_OBJECT_PATH = "/org/freedesktop/portal/desktop"

GLOBAL_SHORTCUTS_IFACE = "org.freedesktop.portal.GlobalShortcuts"
REMOTE_DESKTOP_IFACE = "org.freedesktop.portal.RemoteDesktop"
REGISTRY_IFACE = "org.freedesktop.host.portal.Registry"
REQUEST_IFACE = "org.freedesktop.portal.Request"
SESSION_IFACE = "org.freedesktop.portal.Session"


# ---------------------------------------------------------------------------
# Service interfaces (property exposure + introspection)
# ---------------------------------------------------------------------------

class GlobalShortcutsService(ServiceInterface):
    """Exposes GlobalShortcuts properties and method signatures for introspection.

    Method bodies are never called — the raw message handler intercepts all
    method calls before dbus_next dispatches to the ServiceInterface.
    """

    def __init__(self):
        super().__init__(GLOBAL_SHORTCUTS_IFACE)

    @dbus_property(access=PropertyAccess.READ)
    def version(self) -> "u":
        return 1

    @method()
    def CreateSession(self, options: "a{sv}") -> "o":
        return "/"

    @method()
    def BindShortcuts(self, session_handle: "o", shortcuts: "a(sa{sv})",
                      parent_window: "s", options: "a{sv}") -> "o":
        return "/"

    @method()
    def ListShortcuts(self, session_handle: "o",
                      options: "a{sv}") -> "o":
        return "/"

    @signal()
    def Activated(self) -> "osta{sv}":
        return ["/", "", 0, {}]

    @signal()
    def Deactivated(self) -> "osta{sv}":
        return ["/", "", 0, {}]


class RemoteDesktopService(ServiceInterface):
    """Exposes RemoteDesktop properties and method signatures for introspection.

    Method bodies are never called — the raw message handler intercepts first.
    """

    def __init__(self):
        super().__init__(REMOTE_DESKTOP_IFACE)

    @dbus_property(access=PropertyAccess.READ)
    def version(self) -> "u":
        return 2

    @dbus_property(access=PropertyAccess.READ)
    def AvailableDeviceTypes(self) -> "u":
        return 7  # keyboard(1) | pointer(2) | touchscreen(4)

    @method()
    def CreateSession(self, options: "a{sv}") -> "o":
        return "/"

    @method()
    def SelectDevices(self, session_handle: "o", options: "a{sv}") -> "o":
        return "/"

    @method()
    def Start(self, session_handle: "o", parent_window: "s",
              options: "a{sv}") -> "o":
        return "/"

    @method()
    def NotifyKeyboardKeysym(self, session_handle: "o", options: "a{sv}",
                             keysym: "i", state: "u"):
        pass


class RegistryService(ServiceInterface):
    """Stub interface so introspection includes the Registry interface."""

    def __init__(self):
        super().__init__(REGISTRY_IFACE)

    @method()
    def Register(self, app_id: "s", options: "a{sv}"):
        pass


# ---------------------------------------------------------------------------
# Portal state and controller
# ---------------------------------------------------------------------------

class PortalController:
    """Thread-safe API for tests to interact with the mock portal.

    Provides methods to emit GlobalShortcuts signals and inspect keysym
    events sent by the daemon.
    """

    def __init__(self, bus, loop):
        self._bus = bus
        self._loop = loop
        self._keysym_log = []
        self._lock = threading.Lock()
        self._sessions = {}  # session_path -> session state
        self._bound_shortcuts = {}  # session_path -> list of shortcut defs

    def emit_activated(self, shortcut_id="dictate_hold"):
        """Emit a GlobalShortcuts Activated signal for the given shortcut."""
        async def _emit():
            for session_path in list(self._sessions.keys()):
                timestamp = int(time.time() * 1000)
                msg = Message.new_signal(
                    PORTAL_OBJECT_PATH,
                    GLOBAL_SHORTCUTS_IFACE,
                    "Activated",
                )
                msg.signature = "osta{sv}"
                msg.body = [session_path, shortcut_id, timestamp, {}]
                await self._bus.send(msg)

        asyncio.run_coroutine_threadsafe(_emit(), self._loop).result(timeout=5)

    def emit_deactivated(self, shortcut_id="dictate_hold"):
        """Emit a GlobalShortcuts Deactivated signal for the given shortcut."""
        async def _emit():
            for session_path in list(self._sessions.keys()):
                timestamp = int(time.time() * 1000)
                msg = Message.new_signal(
                    PORTAL_OBJECT_PATH,
                    GLOBAL_SHORTCUTS_IFACE,
                    "Deactivated",
                )
                msg.signature = "osta{sv}"
                msg.body = [session_path, shortcut_id, timestamp, {}]
                await self._bus.send(msg)

        asyncio.run_coroutine_threadsafe(_emit(), self._loop).result(timeout=5)

    def log_keysym(self, keysym, state):
        """Record a keysym event from NotifyKeyboardKeysym."""
        with self._lock:
            self._keysym_log.append((keysym, state))

    def get_keysym_log(self):
        """Return a copy of all logged keysym events."""
        with self._lock:
            return list(self._keysym_log)

    def clear_keysym_log(self):
        """Clear the keysym log."""
        with self._lock:
            self._keysym_log.clear()

    def register_session(self, session_path, session_type):
        """Track a created session."""
        self._sessions[session_path] = {"type": session_type, "started": False}

    def remove_session(self, session_path):
        """Remove a tracked session."""
        self._sessions.pop(session_path, None)
        self._bound_shortcuts.pop(session_path, None)

    def set_bound_shortcuts(self, session_path, shortcuts):
        """Record bound shortcuts for a session."""
        self._bound_shortcuts[session_path] = shortcuts

    def get_bound_shortcuts(self, session_path):
        """Get bound shortcuts for a session, or None."""
        return self._bound_shortcuts.get(session_path)


def keysyms_to_text(log):
    """Convert a keysym log (list of (keysym, state) tuples) to a string.

    Only considers "pressed" events (state=1) and maps keysyms to chars.
    Press-release pairs each produce one character.
    """
    XKB_KEY_RETURN = 0xff0d
    XKB_KEY_TAB = 0xff09

    chars = []
    for keysym, state in log:
        if state != 1:  # Only pressed events produce characters
            continue
        if keysym == XKB_KEY_RETURN:
            chars.append("\n")
        elif keysym == XKB_KEY_TAB:
            chars.append("\t")
        elif 0x01000000 <= keysym <= 0x0110ffff:
            # Unicode keysym range: keysym = 0x01000000 + codepoint
            chars.append(chr(keysym - 0x01000000))
        elif 0x20 <= keysym <= 0x7e:
            # Latin-1 printable ASCII maps directly
            chars.append(chr(keysym))
        else:
            # Other keysyms we can't easily map; skip
            pass
    return "".join(chars)


# ---------------------------------------------------------------------------
# Raw message handler
# ---------------------------------------------------------------------------

def _make_request_path(sender, token):
    """Build the portal request object path from sender and token."""
    sender_escaped = sender.replace(".", "_").replace(":", "")
    return f"/org/freedesktop/portal/desktop/request/{sender_escaped}/{token}"


def _make_session_path(sender, token):
    """Build the portal session object path from sender and token."""
    sender_escaped = sender.replace(".", "_").replace(":", "")
    return f"/org/freedesktop/portal/desktop/session/{sender_escaped}/{token}"


def _gen_token():
    """Generate a unique token."""
    return uuid.uuid4().hex[:16]


def _create_message_handler(bus, controller, loop):
    """Create a raw message handler that intercepts portal method calls.

    Handles the request/response pattern: returns the request path immediately,
    then schedules a Response signal emission after a short delay.
    """

    def handler(msg):
        if msg.message_type != MessageType.METHOD_CALL:
            return False

        iface = msg.interface
        member = msg.member
        sender = msg.sender

        path = msg.path or ""

        # For the portal object path, let Properties/Introspectable pass
        # through to the ServiceInterface handlers
        if iface in (
            "org.freedesktop.DBus.Properties",
            "org.freedesktop.DBus.Introspectable",
            "org.freedesktop.DBus.Peer",
        ):
            if path == PORTAL_OBJECT_PATH:
                return False
            # For request/session subpaths, reply with minimal introspection
            # to prevent "no interfaces at path" errors
            if iface == "org.freedesktop.DBus.Introspectable" and member == "Introspect":
                xml = '<node />'
                if "/request/" in path:
                    xml = (
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
                elif "/session/" in path:
                    # Only expose Session interface if the session is still alive
                    if path in controller._sessions:
                        xml = (
                            '<node>'
                            '  <interface name="org.freedesktop.portal.Session">'
                            '    <method name="Close"/>'
                            '    <signal name="Closed"/>'
                            '  </interface>'
                            '</node>'
                        )
                reply = Message.new_method_return(msg)
                reply.signature = "s"
                reply.body = [xml]
                bus.send(reply)
                return True
            if iface == "org.freedesktop.DBus.Properties":
                # Return empty properties for request/session paths
                if member == "GetAll":
                    reply = Message.new_method_return(msg)
                    reply.signature = "a{sv}"
                    reply.body = [{}]
                    bus.send(reply)
                    return True
                if member == "Get":
                    from dbus_next import ErrorType
                    reply = Message.new_error(
                        msg,
                        ErrorType.UNKNOWN_PROPERTY.value,
                        f"No properties at {path}",
                    )
                    bus.send(reply)
                    return True
            return False

        # --- Registry.Register ---
        if iface == REGISTRY_IFACE and member == "Register":
            reply = Message.new_method_return(msg)
            bus.send(reply)
            return True

        # --- GlobalShortcuts.CreateSession ---
        if iface == GLOBAL_SHORTCUTS_IFACE and member == "CreateSession":
            options = msg.body[0] if msg.body else {}
            handle_token = options.get("handle_token", Variant("s", _gen_token())).value
            session_token = options.get("session_handle_token", Variant("s", _gen_token())).value

            request_path = _make_request_path(sender, handle_token)
            session_path = _make_session_path(sender, session_token)

            controller.register_session(session_path, "shortcuts")

            # Reply with the request handle
            reply = Message.new_method_return(msg)
            reply.signature = "o"
            reply.body = [request_path]
            bus.send(reply)

            # Schedule Response signal
            _schedule_response(bus, loop, request_path, sender, {
                "session_handle": Variant("s", session_path),
            })
            return True

        # --- GlobalShortcuts.BindShortcuts ---
        if iface == GLOBAL_SHORTCUTS_IFACE and member == "BindShortcuts":
            session_handle = msg.body[0]
            shortcuts_arg = msg.body[1]
            # body[2] = parent_window (s)
            options = msg.body[3] if len(msg.body) > 3 else {}
            handle_token = options.get("handle_token", Variant("s", _gen_token())).value

            request_path = _make_request_path(sender, handle_token)

            # Check for duplicate bind
            if controller.get_bound_shortcuts(session_handle) is not None:
                reply = Message.new_method_return(msg)
                reply.signature = "o"
                reply.body = [request_path]
                bus.send(reply)
                _schedule_response(bus, loop, request_path, sender, {},
                                   response_code=2)
                return True

            # Record the bound shortcuts
            controller.set_bound_shortcuts(session_handle, shortcuts_arg)

            # Build the response shortcuts with fields ashpd expects
            bound = []
            for shortcut in shortcuts_arg:
                shortcut_id = shortcut[0]
                shortcut_opts = shortcut[1] if len(shortcut) > 1 else {}
                response_opts = dict(shortcut_opts)
                # ashpd requires trigger_description in the response
                if "trigger_description" not in response_opts:
                    trigger = response_opts.get(
                        "preferred-trigger", Variant("s", ""),
                    ).value
                    response_opts["trigger_description"] = Variant(
                        "s", trigger or "Super+Space",
                    )
                bound.append([shortcut_id, response_opts])

            reply = Message.new_method_return(msg)
            reply.signature = "o"
            reply.body = [request_path]
            bus.send(reply)

            _schedule_response(bus, loop, request_path, sender, {
                "shortcuts": Variant("a(sa{sv})", bound),
            })
            return True

        # --- RemoteDesktop.CreateSession ---
        if iface == REMOTE_DESKTOP_IFACE and member == "CreateSession":
            options = msg.body[0] if msg.body else {}
            handle_token = options.get("handle_token", Variant("s", _gen_token())).value
            session_token = options.get("session_handle_token", Variant("s", _gen_token())).value

            request_path = _make_request_path(sender, handle_token)
            session_path = _make_session_path(sender, session_token)

            controller.register_session(session_path, "remote_desktop")

            reply = Message.new_method_return(msg)
            reply.signature = "o"
            reply.body = [request_path]
            bus.send(reply)

            _schedule_response(bus, loop, request_path, sender, {
                "session_handle": Variant("s", session_path),
            })
            return True

        # --- RemoteDesktop.SelectDevices ---
        if iface == REMOTE_DESKTOP_IFACE and member == "SelectDevices":
            # body: (session_handle: o, options: a{sv})
            options = msg.body[1] if len(msg.body) > 1 else {}
            handle_token = options.get("handle_token", Variant("s", _gen_token())).value

            request_path = _make_request_path(sender, handle_token)

            reply = Message.new_method_return(msg)
            reply.signature = "o"
            reply.body = [request_path]
            bus.send(reply)

            _schedule_response(bus, loop, request_path, sender, {})
            return True

        # --- RemoteDesktop.Start ---
        if iface == REMOTE_DESKTOP_IFACE and member == "Start":
            # body: (session_handle: o, parent_window: s, options: a{sv})
            options = msg.body[2] if len(msg.body) > 2 else {}
            handle_token = options.get("handle_token", Variant("s", _gen_token())).value

            request_path = _make_request_path(sender, handle_token)

            # Generate a restore token for the daemon
            restore_token = f"mock-restore-token-{_gen_token()}"

            reply = Message.new_method_return(msg)
            reply.signature = "o"
            reply.body = [request_path]
            bus.send(reply)

            _schedule_response(bus, loop, request_path, sender, {
                "devices": Variant("u", 7),  # keyboard | pointer | touchscreen
                "restore_token": Variant("s", restore_token),
            })
            return True

        # --- RemoteDesktop.NotifyKeyboardKeysym ---
        if iface == REMOTE_DESKTOP_IFACE and member == "NotifyKeyboardKeysym":
            # body: (session_handle: o, options: a{sv}, keysym: i, state: u)
            keysym = msg.body[2]
            state = msg.body[3]
            controller.log_keysym(keysym, state)

            reply = Message.new_method_return(msg)
            bus.send(reply)
            return True

        # --- Session.Close ---
        if iface == SESSION_IFACE and member == "Close":
            path = msg.path
            controller.remove_session(path)

            reply = Message.new_method_return(msg)
            bus.send(reply)

            # Emit Closed signal
            closed_msg = Message.new_signal(path, SESSION_IFACE, "Closed")
            bus.send(closed_msg)
            return True

        # --- Request.Close ---
        if iface == REQUEST_IFACE and member == "Close":
            reply = Message.new_method_return(msg)
            bus.send(reply)
            return True

        return False

    return handler


def _schedule_response(bus, loop, request_path, destination, results,
                       response_code=0, delay=0.05):
    """Schedule a portal Response signal after a short delay."""
    async def _send():
        await asyncio.sleep(delay)
        sig = Message.new_signal(request_path, REQUEST_IFACE, "Response")
        sig.signature = "ua{sv}"
        sig.body = [response_code, results]
        bus.send(sig)

    asyncio.run_coroutine_threadsafe(_send(), loop)


# ---------------------------------------------------------------------------
# Public API: start and stop
# ---------------------------------------------------------------------------

def start_mock_portal():
    """Start a private dbus-daemon and mock portal service.

    Returns (bus_address, controller, stop_fn).

    - bus_address: DBUS_SESSION_BUS_ADDRESS for the daemon subprocess
    - controller: PortalController for test interaction
    - stop_fn: callable to shut everything down
    """
    # Start a private dbus-daemon
    dbus_proc = subprocess.Popen(
        ["dbus-daemon", "--session", "--nofork", "--print-address"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )

    # Read the bus address from stdout
    address_line = dbus_proc.stdout.readline().decode("utf-8").strip()
    if not address_line:
        dbus_proc.kill()
        raise RuntimeError("dbus-daemon did not print an address")

    bus_address = address_line

    # Set up asyncio loop in a background thread
    loop = asyncio.new_event_loop()
    controller = [None]  # mutable container for closure
    ready_event = threading.Event()
    error_container = [None]

    def _run_loop():
        asyncio.set_event_loop(loop)
        try:
            loop.run_until_complete(_setup_and_run(
                bus_address, loop, controller, ready_event,
            ))
        except Exception as e:
            error_container[0] = e
            ready_event.set()

    thread = threading.Thread(target=_run_loop, daemon=True)
    thread.start()

    # Wait for the mock portal to be ready
    if not ready_event.wait(timeout=10):
        dbus_proc.kill()
        raise RuntimeError("Mock portal did not become ready within 10 seconds")

    if error_container[0]:
        dbus_proc.kill()
        raise error_container[0]

    ctrl = controller[0]

    def stop():
        loop.call_soon_threadsafe(loop.stop)
        thread.join(timeout=5)
        dbus_proc.terminate()
        try:
            dbus_proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            dbus_proc.kill()
            dbus_proc.wait()

    return bus_address, ctrl, stop


async def _setup_and_run(bus_address, loop, controller_out, ready_event):
    """Connect to the private bus, export interfaces, and run."""
    bus = await MessageBus(bus_address=bus_address).connect()

    ctrl = PortalController(bus, loop)
    controller_out[0] = ctrl

    # Export service interfaces at the portal object path
    gs_service = GlobalShortcutsService()
    rd_service = RemoteDesktopService()
    reg_service = RegistryService()

    bus.export(PORTAL_OBJECT_PATH, gs_service)
    bus.export(PORTAL_OBJECT_PATH, rd_service)
    bus.export(PORTAL_OBJECT_PATH, reg_service)

    # Register the raw message handler (runs before ServiceInterface methods)
    handler = _create_message_handler(bus, ctrl, loop)
    bus.add_message_handler(handler)

    # Request the well-known portal bus name
    await bus.request_name(PORTAL_BUS_NAME)

    ready_event.set()

    # Keep running until the loop is stopped
    try:
        while True:
            await asyncio.sleep(1)
    except asyncio.CancelledError:
        pass
    finally:
        bus.disconnect()
