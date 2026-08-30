# ABOUTME: Pytest configuration and shared fixtures for voxkey integration tests.
# ABOUTME: Provides mock portal, D-Bus connections, virtual mic, and daemon lifecycle.

import asyncio
import os
import select
import shutil
import stat
import subprocess
import signal
import time
from pathlib import Path

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
# Fixtures: isolate mutable Voxkey state from the live desktop session
# ---------------------------------------------------------------------------

@pytest.fixture(scope="session", autouse=True)
def isolated_voxkey_home(tmp_path_factory):
    """Redirect all mutable Voxkey state to a temporary XDG root.

    Prevents the spawned daemon from reading or writing the caller's real
    ~/.config/voxkey/config.toml or portal restore token.
    Downloaded Parakeet models are the one read-only exception: the isolated
    data home gets a symlink to the host's real model directory.
    """
    config_home = tmp_path_factory.mktemp("voxkey-xdg-config")
    state_home = tmp_path_factory.mktemp("voxkey-xdg-state")
    voxkey_config_dir = config_home / "voxkey"
    voxkey_config_dir.mkdir()
    data_home = config_home / "data"
    data_voxkey_dir = data_home / "voxkey"
    data_voxkey_dir.mkdir(parents=True)

    test_config_source = os.environ.get("VOXKEY_TEST_CONFIG")
    if test_config_source is not None:
        source_path = Path(test_config_source)
        if not source_path.is_file():
            raise pytest.UsageError(
                f"VOXKEY_TEST_CONFIG={test_config_source!r} is not a regular file"
            )
        dest_config = voxkey_config_dir / "config.toml"
        shutil.copyfile(source_path, dest_config)
        os.chmod(dest_config, 0o600)

    host_data_home = os.environ.get("XDG_DATA_HOME") or os.path.expanduser("~/.local/share")
    host_models_dir = Path(host_data_home) / "voxkey" / "models"
    if host_models_dir.is_dir():
        (data_voxkey_dir / "models").symlink_to(host_models_dir)

    saved_env = {
        key: os.environ.get(key)
        for key in (
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "VOXKEY_RESTORE_TOKEN_PATH",
        )
    }

    os.environ["XDG_CONFIG_HOME"] = str(config_home)
    os.environ["XDG_DATA_HOME"] = str(data_home)
    os.environ["XDG_STATE_HOME"] = str(state_home)
    os.environ["VOXKEY_RESTORE_TOKEN_PATH"] = str(voxkey_config_dir / "restore_token")
    try:
        yield config_home
    finally:
        for key, value in saved_env.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value


def _snapshot(path):
    if not path.exists():
        return None
    return (path.read_bytes(), stat.S_IMODE(path.stat().st_mode))


def _restore(path, snapshot):
    if snapshot is None:
        path.unlink(missing_ok=True)
        return
    contents, mode = snapshot
    path.write_bytes(contents)
    os.chmod(path, mode)


@pytest.fixture(autouse=True)
def _reset_mutable_voxkey_state(isolated_voxkey_home):
    """Restore isolated config, token, and transcription history after every test.

    Keeps a D-Bus config round trip in one test from leaking into the next.
    Depends on isolated_voxkey_home so its teardown (which happens before
    isolated_voxkey_home's, and after daemon_process's) restores the
    baseline only once the daemon has already been stopped.
    """
    voxkey_config_dir = isolated_voxkey_home / "voxkey"
    config_path = voxkey_config_dir / "config.toml"
    token_path = voxkey_config_dir / "restore_token"
    history_path = Path(os.environ["XDG_STATE_HOME"]) / "voxkey" / "history.json"

    config_snapshot = _snapshot(config_path)
    token_snapshot = _snapshot(token_path)
    history_snapshot = _snapshot(history_path)

    yield

    _restore(config_path, config_snapshot)
    _restore(token_path, token_snapshot)
    _restore(history_path, history_snapshot)


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
    """Provide the PortalController and clear recorded cleanup state."""
    _, controller, _ = mock_portal
    controller.clear_metrics()
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
            remaining = proc.stderr.read()
            if remaining:
                lines.extend(remaining.decode("utf-8", errors="replace").splitlines())
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
def daemon_config(isolated_voxkey_home):
    """Write config.toml before the daemon starts.

    A no-op by default; tests override it to customize the daemon. The
    autouse state reset snapshots the config before this fixture runs, so
    the baseline is restored once the test finishes.
    """
    yield


@pytest.fixture
def daemon_process(mock_portal, virtual_mic, daemon_config):
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
