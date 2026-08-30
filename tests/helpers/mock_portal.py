# ABOUTME: Mock XDG Desktop Portal for testing voxkey without a live compositor.
# ABOUTME: Implements the portal interfaces needed by Voxkey and its PipeWire test audio.

import asyncio
import os
import socket
import subprocess
import tempfile
import threading
import time
import uuid

from dbus_next import Message, MessageType, Variant
from dbus_next.aio import MessageBus
from dbus_next.constants import PropertyAccess
from dbus_next.service import ServiceInterface, dbus_property, method, signal


# A private bus config with no service-activation directories. The default
# `--session` config includes <standard_session_servicedirs/>, which makes
# the private dbus-daemon activatable for real system services (e.g.
# org.freedesktop.secrets via gnome-keyring). A call to such a name then
# blocks for up to service_start_timeout (120s) waiting for activation that
# can never succeed in this isolated bus, instead of failing fast with
# ServiceUnknown.
_DBUS_CONFIG = """<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <keep_umask/>
  <listen>unix:tmpdir=/tmp</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"""


PORTAL_BUS_NAME = "org.freedesktop.portal.Desktop"
PORTAL_OBJECT_PATH = "/org/freedesktop/portal/desktop"

GLOBAL_SHORTCUTS_IFACE = "org.freedesktop.portal.GlobalShortcuts"
REMOTE_DESKTOP_IFACE = "org.freedesktop.portal.RemoteDesktop"
REALTIME_IFACE = "org.freedesktop.portal.Realtime"
REGISTRY_IFACE = "org.freedesktop.host.portal.Registry"
REQUEST_IFACE = "org.freedesktop.portal.Request"
SESSION_IFACE = "org.freedesktop.portal.Session"

SCREEN_SAVER_BUS_NAME = "org.gnome.ScreenSaver"
SCREEN_SAVER_OBJECT_PATH = "/org/gnome/ScreenSaver"
SCREEN_SAVER_IFACE = "org.gnome.ScreenSaver"


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

    @signal()
    def ShortcutsChanged(self) -> "oa(sa{sv})":
        return ["/", []]


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
    def ConnectToEIS(self, session_handle: "o", options: "a{sv}") -> "h":
        return 0


class RealtimeService(ServiceInterface):
    """No-op realtime portal used by PipeWire on the isolated test bus."""

    def __init__(self):
        super().__init__(REALTIME_IFACE)

    @dbus_property(access=PropertyAccess.READ)
    def version(self) -> "u":
        return 1

    @dbus_property(access=PropertyAccess.READ)
    def MaxRealtimePriority(self) -> "i":
        return 0

    @dbus_property(access=PropertyAccess.READ)
    def MinNiceLevel(self) -> "i":
        return 0

    @dbus_property(access=PropertyAccess.READ)
    def RTTimeUSecMax(self) -> "x":
        return 0

    @method()
    def MakeThreadRealtimeWithPID(
        self, process: "t", thread: "t", priority: "u",
    ):
        pass

    @method()
    def MakeThreadHighPriorityWithPID(
        self, process: "t", thread: "t", priority: "i",
    ):
        pass

class RegistryService(ServiceInterface):
    """Stub interface so introspection includes the Registry interface."""

    def __init__(self):
        super().__init__(REGISTRY_IFACE)

    @method()
    def Register(self, app_id: "s", options: "a{sv}"):
        pass


class ScreenSaverService(ServiceInterface):
    """Small GNOME screen-shield mock used to exercise lock-safe recovery."""

    def __init__(self):
        super().__init__(SCREEN_SAVER_IFACE)
        self.active = False

    @method()
    def GetActive(self) -> "b":
        return self.active

    @signal()
    def ActiveChanged(self, active: "b") -> "b":
        return active


# ---------------------------------------------------------------------------
# Portal state and controller
# ---------------------------------------------------------------------------

class PortalController:
    """Thread-safe API for tests to interact with the mock portal.

    Provides methods to emit GlobalShortcuts signals and inspect session
    cleanup performed by the daemon. EIS connections are backed by the small
    Rust compositor helper built exclusively for the isolated test stack.
    """

    def __init__(self, bus, loop, screen_saver):
        self._bus = bus
        self._loop = loop
        self._screen_saver = screen_saver
        self._closed_session_types = []
        self._selected_restore_tokens = []
        self._lock = threading.Lock()
        self._sessions = {}  # session_path -> session state
        self._bound_shortcuts = {}  # session_path -> list of shortcut defs
        # GNOME persists the first accepted binding for an application action
        # ID. Later BindShortcuts calls using that ID keep the saved trigger,
        # even when the app supplies a different preferred_trigger.
        self._saved_shortcuts = {}  # shortcut ID -> effective shortcut opts
        self._next_shortcut_bind_response = None
        self._next_suppressed_method_reply = None
        self._next_suppressed_portal_response = None
        self._reject_next_restore_token = False
        self._eis_processes = []

    def track_eis_process(self, process):
        """Own a compositor-side EIS helper until it disconnects or teardown."""
        with self._lock:
            self._eis_processes.append(process)

    def shutdown_eis_processes(self):
        """Reap every EIS helper, terminating only stale isolated children."""
        with self._lock:
            processes = self._eis_processes
            self._eis_processes = []
        for process in processes:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            if process.stderr is not None:
                process.stderr.close()

    def _event_shortcut_id(self, session_path, requested_id):
        if requested_id is not None:
            return requested_id
        shortcuts = self._bound_shortcuts.get(session_path) or []
        return shortcuts[0][0] if shortcuts else "dictate_hold"

    def emit_screen_locked(self, active):
        """Set GNOME's lock state and emit its authoritative transition."""
        async def _emit():
            self._screen_saver.active = active
            self._screen_saver.ActiveChanged(active)

        asyncio.run_coroutine_threadsafe(_emit(), self._loop).result(timeout=5)

    def emit_remote_desktop_loss_then_screen_locked(self, delay=0.05):
        """Model GNOME removing EIS just before publishing the lock state."""
        self.shutdown_eis_processes()

        async def _emit():
            remote_desktop_paths = [
                path
                for path, info in self._sessions.items()
                if info["type"] == "remote_desktop"
            ]
            for path in remote_desktop_paths:
                message = Message.new_signal(path, SESSION_IFACE, "Closed")
                await self._bus.send(message)

            await asyncio.sleep(delay)
            self._screen_saver.active = True
            self._screen_saver.ActiveChanged(True)

        asyncio.run_coroutine_threadsafe(_emit(), self._loop).result(timeout=5)

    def emit_activated(self, shortcut_id=None):
        """Emit a GlobalShortcuts Activated signal for the given shortcut."""
        async def _emit():
            for session_path in list(self._sessions.keys()):
                event_id = self._event_shortcut_id(session_path, shortcut_id)
                timestamp = int(time.time() * 1000)
                msg = Message.new_signal(
                    PORTAL_OBJECT_PATH,
                    GLOBAL_SHORTCUTS_IFACE,
                    "Activated",
                )
                msg.signature = "osta{sv}"
                msg.body = [session_path, event_id, timestamp, {}]
                await self._bus.send(msg)

        asyncio.run_coroutine_threadsafe(_emit(), self._loop).result(timeout=5)

    def emit_deactivated(self, shortcut_id=None):
        """Emit a GlobalShortcuts Deactivated signal for the given shortcut."""
        async def _emit():
            for session_path in list(self._sessions.keys()):
                event_id = self._event_shortcut_id(session_path, shortcut_id)
                timestamp = int(time.time() * 1000)
                msg = Message.new_signal(
                    PORTAL_OBJECT_PATH,
                    GLOBAL_SHORTCUTS_IFACE,
                    "Deactivated",
                )
                msg.signature = "osta{sv}"
                msg.body = [session_path, event_id, timestamp, {}]
                await self._bus.send(msg)

        asyncio.run_coroutine_threadsafe(_emit(), self._loop).result(timeout=5)

    def emit_shortcuts_changed(self, trigger_description, shortcut_id=None):
        """Publish a desktop-side change to a bound shortcut's display text."""
        async def _emit():
            shortcut_sessions = [
                path
                for path, info in self._sessions.items()
                if info["type"] == "shortcuts"
            ]
            for session_path in shortcut_sessions:
                event_id = self._event_shortcut_id(session_path, shortcut_id)
                msg = Message.new_signal(
                    PORTAL_OBJECT_PATH,
                    GLOBAL_SHORTCUTS_IFACE,
                    "ShortcutsChanged",
                )
                msg.signature = "oa(sa{sv})"
                msg.body = [
                    session_path,
                    [[
                        event_id,
                        {
                            "description": Variant("s", "Toggle dictation"),
                            "trigger_description": Variant(
                                "s", trigger_description,
                            ),
                        },
                    ]],
                ]
                await self._bus.send(msg)

        asyncio.run_coroutine_threadsafe(_emit(), self._loop).result(timeout=5)

    def clear_metrics(self):
        """Clear recorded session-cleanup metrics between tests."""
        with self._lock:
            self._closed_session_types.clear()
            self._selected_restore_tokens.clear()
            self._next_shortcut_bind_response = None
            self._next_suppressed_method_reply = None
            self._next_suppressed_portal_response = None
            self._reject_next_restore_token = False

    def suppress_next_method_reply(self, interface, member):
        """Accept one matching D-Bus method call without ever replying."""
        with self._lock:
            self._next_suppressed_method_reply = (interface, member)

    def take_method_reply_suppression(self, interface, member):
        with self._lock:
            expected = (interface, member)
            if self._next_suppressed_method_reply != expected:
                return False
            self._next_suppressed_method_reply = None
            return True

    def suppress_next_portal_response(self, interface, member):
        """Return a request handle but omit its one matching Response signal."""
        with self._lock:
            self._next_suppressed_portal_response = (interface, member)

    def take_portal_response_suppression(self, interface, member):
        with self._lock:
            expected = (interface, member)
            if self._next_suppressed_portal_response != expected:
                return False
            self._next_suppressed_portal_response = None
            return True

    def reject_next_shortcut_bind(self, response_code=2):
        """Make exactly one subsequent BindShortcuts request fail."""
        with self._lock:
            self._next_shortcut_bind_response = response_code

    def take_shortcut_bind_rejection(self):
        with self._lock:
            response_code = self._next_shortcut_bind_response
            self._next_shortcut_bind_response = None
            return response_code

    def reject_next_restore_token(self):
        """Reject one token-bearing SelectDevices request."""
        with self._lock:
            self._reject_next_restore_token = True

    def record_restore_token_selection(self, options):
        """Record and optionally reject the restore token sent by the daemon."""
        token = options.get("restore_token")
        token = token.value if token is not None else None
        with self._lock:
            self._selected_restore_tokens.append(token)
            rejected = token is not None and self._reject_next_restore_token
            if rejected:
                self._reject_next_restore_token = False
            return rejected

    def closed_session_types(self):
        with self._lock:
            return list(self._closed_session_types)

    def active_session_types(self):
        """Return the portal grants the daemon is holding right now."""
        with self._lock:
            return [info["type"] for info in self._sessions.values()]

    def selected_restore_tokens(self):
        with self._lock:
            return list(self._selected_restore_tokens)

    def register_session(self, session_path, session_type, owner):
        """Track a created session and the unique bus name that owns it."""
        with self._lock:
            self._sessions[session_path] = {
                "type": session_type,
                "started": False,
                "owner": owner,
            }

    def remove_session(self, session_path):
        """Remove a tracked session."""
        with self._lock:
            self._sessions.pop(session_path, None)
            self._bound_shortcuts.pop(session_path, None)

    def close_session(self, session_path):
        """Record an explicit Session.Close separately from owner disconnect."""
        with self._lock:
            info = self._sessions.get(session_path)
            if info is not None:
                self._closed_session_types.append(info["type"])
            self._sessions.pop(session_path, None)
            self._bound_shortcuts.pop(session_path, None)

    def remove_sessions_for_owner(self, owner):
        """Drop every session whose owning bus connection has disconnected.

        Mirrors the real portal: a session is only valid as long as its
        client's unique bus name is alive. Without this, a daemon killed by
        SIGTERM (skipping Session.Close) leaves a stale session behind that
        keeps receiving future Activated/Deactivated broadcasts alongside
        the live one.
        """
        with self._lock:
            stale = [
                path for path, info in self._sessions.items()
                if info.get("owner") == owner
            ]
            for path in stale:
                self._sessions.pop(path, None)
                self._bound_shortcuts.pop(path, None)

    def set_bound_shortcuts(self, session_path, shortcuts):
        """Record bound shortcuts for a session."""
        self._bound_shortcuts[session_path] = shortcuts

    def get_bound_shortcuts(self, session_path):
        """Get bound shortcuts for a session, or None."""
        return self._bound_shortcuts.get(session_path)

    def bind_persisted_shortcuts(self, requested_shortcuts):
        """Apply GNOME's app/action-ID persistence to a bind request."""
        bound = []
        replacement = {}
        for shortcut in requested_shortcuts:
            shortcut_id = shortcut[0]
            requested_opts = shortcut[1] if len(shortcut) > 1 else {}
            saved_opts = self._saved_shortcuts.get(shortcut_id)
            if saved_opts is not None:
                response_opts = dict(saved_opts)
            else:
                response_opts = dict(requested_opts)
                trigger = response_opts.get(
                    "preferred_trigger",
                    response_opts.get(
                        "preferred-trigger", Variant("s", ""),
                    ),
                ).value
                response_opts["trigger_description"] = Variant(
                    "s", trigger or "Super+Alt+D",
                )
            bound.append([shortcut_id, response_opts])
            replacement[shortcut_id] = dict(response_opts)

        # GNOME's confirmation dialog stores the action set offered by the
        # latest successful registration for this application.
        self._saved_shortcuts = replacement
        return bound


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

        if controller.take_method_reply_suppression(iface, member):
            return True

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

            controller.register_session(session_path, "shortcuts", sender)

            # Reply with the request handle
            reply = Message.new_method_return(msg)
            reply.signature = "o"
            reply.body = [request_path]
            bus.send(reply)

            # Schedule Response signal
            if not controller.take_portal_response_suppression(iface, member):
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

            rejected_response = controller.take_shortcut_bind_rejection()
            if rejected_response is not None:
                reply = Message.new_method_return(msg)
                reply.signature = "o"
                reply.body = [request_path]
                bus.send(reply)
                _schedule_response(
                    bus, loop, request_path, sender, {},
                    response_code=rejected_response,
                )
                return True

            # Check for duplicate bind
            if controller.get_bound_shortcuts(session_handle) is not None:
                reply = Message.new_method_return(msg)
                reply.signature = "o"
                reply.body = [request_path]
                bus.send(reply)
                _schedule_response(bus, loop, request_path, sender, {},
                                   response_code=2)
                return True

            # Record the portal's effective bindings, not merely the new
            # preferences the client requested.
            bound = controller.bind_persisted_shortcuts(shortcuts_arg)
            controller.set_bound_shortcuts(session_handle, bound)

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

            controller.register_session(session_path, "remote_desktop", sender)

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
            reject_restore_token = controller.record_restore_token_selection(options)

            reply = Message.new_method_return(msg)
            reply.signature = "o"
            reply.body = [request_path]
            bus.send(reply)

            _schedule_response(
                bus,
                loop,
                request_path,
                sender,
                {},
                response_code=2 if reject_restore_token else 0,
            )
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

        # --- RemoteDesktop.ConnectToEIS ---
        if iface == REMOTE_DESKTOP_IFACE and member == "ConnectToEIS":
            helper = os.environ.get("VOXKEY_TEST_EIS_SERVER")
            if not helper or not os.path.isfile(helper):
                reply = Message.new_error(
                    msg,
                    "org.freedesktop.DBus.Error.Failed",
                    "the isolated EIS test server is not built",
                )
                bus.send(reply)
                return True

            client_socket, server_socket = socket.socketpair()
            process = subprocess.Popen(
                [helper, str(server_socket.fileno())],
                pass_fds=(server_socket.fileno(),),
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )
            server_socket.close()
            controller.track_eis_process(process)

            reply = Message.new_method_return(
                msg,
                signature="h",
                body=[0],
                unix_fds=[client_socket.fileno()],
            )
            bus.send(reply)
            # `bus.send` queues the sendmsg operation. Keep the portal's copy
            # alive briefly, then leave the daemon and helper as sole owners.
            loop.call_later(0.5, client_socket.close)
            return True

        # --- Session.Close ---
        if iface == SESSION_IFACE and member == "Close":
            path = msg.path
            controller.close_session(path)

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
    # Start a private dbus-daemon, isolated from real system service activation
    config_file = tempfile.NamedTemporaryFile(
        mode="w", suffix=".conf", delete=False,
    )
    config_file.write(_DBUS_CONFIG)
    config_file.close()

    dbus_proc = subprocess.Popen(
        ["dbus-daemon", f"--config-file={config_file.name}", "--nofork", "--print-address"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )

    # Read the bus address from stdout
    address_line = dbus_proc.stdout.readline().decode("utf-8").strip()
    if not address_line:
        dbus_proc.kill()
        os.unlink(config_file.name)
        raise RuntimeError("dbus-daemon did not print an address")

    bus_address = address_line

    # Set up asyncio loop in a background thread
    loop = asyncio.new_event_loop()
    controller = [None]  # mutable container for closure
    shutdown_event = [None]
    ready_event = threading.Event()
    error_container = [None]

    def _run_loop():
        asyncio.set_event_loop(loop)
        try:
            loop.run_until_complete(_setup_and_run(
                bus_address, loop, controller, shutdown_event, ready_event,
            ))
        except Exception as e:
            error_container[0] = e
            ready_event.set()

    thread = threading.Thread(target=_run_loop, daemon=True)
    thread.start()

    # Wait for the mock portal to be ready
    if not ready_event.wait(timeout=10):
        dbus_proc.kill()
        os.unlink(config_file.name)
        raise RuntimeError("Mock portal did not become ready within 10 seconds")

    if error_container[0]:
        dbus_proc.kill()
        os.unlink(config_file.name)
        raise error_container[0]

    ctrl = controller[0]

    def stop():
        loop.call_soon_threadsafe(shutdown_event[0].set)
        thread.join(timeout=5)
        ctrl.shutdown_eis_processes()
        loop.close()
        dbus_proc.terminate()
        try:
            dbus_proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            dbus_proc.kill()
            dbus_proc.wait()
        os.unlink(config_file.name)

    return bus_address, ctrl, stop


async def _setup_and_run(
    bus_address, loop, controller_out, shutdown_event_out, ready_event
):
    """Connect to the private bus, export interfaces, and run."""
    bus = await MessageBus(bus_address=bus_address, negotiate_unix_fd=True).connect()

    shutdown_event = asyncio.Event()
    shutdown_event_out[0] = shutdown_event

    # Export service interfaces at the portal object path
    gs_service = GlobalShortcutsService()
    rd_service = RemoteDesktopService()
    realtime_service = RealtimeService()
    reg_service = RegistryService()
    screen_saver = ScreenSaverService()
    ctrl = PortalController(bus, loop, screen_saver)
    controller_out[0] = ctrl

    bus.export(PORTAL_OBJECT_PATH, gs_service)
    bus.export(PORTAL_OBJECT_PATH, rd_service)
    bus.export(PORTAL_OBJECT_PATH, realtime_service)
    bus.export(PORTAL_OBJECT_PATH, reg_service)
    bus.export(SCREEN_SAVER_OBJECT_PATH, screen_saver)

    # Register the raw message handler (runs before ServiceInterface methods)
    handler = _create_message_handler(bus, ctrl, loop)
    bus.add_message_handler(handler)

    # Drop sessions when their owning client disconnects (e.g. SIGTERM,
    # which skips the graceful Session.Close call). Mirrors the real portal,
    # where a session dies with its client's unique bus name.
    await bus.call(
        Message(
            destination="org.freedesktop.DBus",
            path="/org/freedesktop/DBus",
            interface="org.freedesktop.DBus",
            member="AddMatch",
            signature="s",
            body=["type='signal',interface='org.freedesktop.DBus',member='NameOwnerChanged'"],
        )
    )

    def _on_name_owner_changed(msg):
        if msg.interface == "org.freedesktop.DBus" and msg.member == "NameOwnerChanged":
            name, _old_owner, new_owner = msg.body
            if name.startswith(":") and new_owner == "":
                ctrl.remove_sessions_for_owner(name)
        return False

    bus.add_message_handler(_on_name_owner_changed)

    # Request the well-known portal bus name
    await bus.request_name(PORTAL_BUS_NAME)
    await bus.request_name(SCREEN_SAVER_BUS_NAME)

    ready_event.set()

    # Keep running until the fixture requests an orderly shutdown.
    try:
        await shutdown_event.wait()
    finally:
        bus.disconnect()
