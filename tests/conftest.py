# ABOUTME: Pytest configuration and shared fixtures for voxkey integration tests.
# ABOUTME: Provides mock portal, D-Bus connections, virtual mic, and daemon lifecycle.

import asyncio
import os
import select
import subprocess
import signal
import time

import pytest
import pytest_asyncio
from dbus_next.aio import MessageBus

from helpers.dbus_portal import (
    get_portal_proxy,
    has_interface,
    safe_introspect,
    GLOBAL_SHORTCUTS_IFACE,
    REMOTE_DESKTOP_IFACE,
    PORTAL_BUS_NAME,
    PORTAL_OBJECT_PATH,
)
from helpers.mock_portal import start_mock_portal
from helpers.virtual_microphone import VirtualMicrophone


# ---------------------------------------------------------------------------
# Environment detection
# ---------------------------------------------------------------------------

def _daemon_binary():
    """Path to the voxkey daemon binary. Override with VOXKEY_BIN env var."""
    return os.environ.get("VOXKEY_BIN", "voxkey")


# ---------------------------------------------------------------------------
# Fixtures: mock portal (session-scoped)
# ---------------------------------------------------------------------------

@pytest.fixture(scope="session")
def mock_portal():
    """Start a private dbus-daemon with a mock portal for the test session.

    Returns (bus_address, controller, stop_fn). The bus_address is used by
    daemon_process and dbus_session to connect to the isolated bus.
    """
    bus_address, controller, stop = start_mock_portal()
    yield bus_address, controller, stop
    stop()


@pytest.fixture
def portal_control(mock_portal):
    """Provide the PortalController and clear keysym log between tests."""
    _, controller, _ = mock_portal
    controller.clear_keysym_log()
    return controller


# ---------------------------------------------------------------------------
# Fixtures: D-Bus (connected to mock portal)
# ---------------------------------------------------------------------------

@pytest_asyncio.fixture
async def dbus_session(mock_portal):
    """Provide a D-Bus connection to the mock portal bus."""
    bus_address, _, _ = mock_portal
    bus = await MessageBus(bus_address=bus_address).connect()
    yield bus
    bus.disconnect()


@pytest_asyncio.fixture
async def portal_proxy(dbus_session):
    """Provide a proxy to the mock XDG Desktop Portal."""
    introspection = await safe_introspect(
        dbus_session, PORTAL_BUS_NAME, PORTAL_OBJECT_PATH,
    )
    return dbus_session.get_proxy_object(
        PORTAL_BUS_NAME, PORTAL_OBJECT_PATH, introspection,
    )


# ---------------------------------------------------------------------------
# Fixtures: virtual devices
# ---------------------------------------------------------------------------

@pytest.fixture(scope="session")
def virtual_mic():
    """Provide a virtual microphone routed as the default audio source.

    Session-scoped so it's created before any function-scoped daemon_process,
    ensuring cpal picks up the virtual source instead of the real microphone.
    """
    mic = VirtualMicrophone()
    yield mic
    mic.close()


# ---------------------------------------------------------------------------
# Fixtures: daemon lifecycle
# ---------------------------------------------------------------------------

def _wait_for_daemon_idle(proc, timeout=15):
    """Poll daemon stderr for 'STATE: Idle', collecting all startup lines.

    Returns (reached_idle, startup_lines).
    """
    lines = []
    deadline = time.monotonic() + timeout
    reached_idle = False

    while time.monotonic() < deadline:
        if proc.poll() is not None:
            break
        ready = select.select([proc.stderr], [], [], 0.5)[0]
        if ready:
            line = proc.stderr.readline()
            if line:
                decoded = line.decode("utf-8", errors="replace").strip()
                lines.append(decoded)
                if "STATE:" in decoded and "Idle" in decoded.split("STATE:")[-1]:
                    reached_idle = True
                    break

    return reached_idle, lines


@pytest.fixture
def daemon_process(mock_portal, virtual_mic):
    """Start and stop the voxkey daemon against the mock portal.

    The daemon runs on the isolated bus so it never touches the real
    desktop session. The virtual_mic dependency ensures audio routing
    is ready before the daemon starts.

    proc.reached_idle and proc.startup_lines are set for tests that need them.
    """
    bus_address, _, _ = mock_portal
    binary = _daemon_binary()

    env = os.environ.copy()
    env["DBUS_SESSION_BUS_ADDRESS"] = bus_address

    proc = subprocess.Popen(
        [binary],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )

    reached_idle, startup_lines = _wait_for_daemon_idle(proc)
    proc.reached_idle = reached_idle
    proc.startup_lines = startup_lines
    proc.bus_address = bus_address

    yield proc

    if proc.poll() is None:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()


@pytest.fixture
def fixtures_dir():
    """Path to the test fixtures directory."""
    return os.path.join(os.path.dirname(__file__), "fixtures")
