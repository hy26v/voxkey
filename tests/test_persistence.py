# ABOUTME: Tests that voxkey persists the RemoteDesktop restore token correctly.
# ABOUTME: Validates file permissions, token rotation, and corrupt/missing token handling.

import json
import os
import select
import signal
import stat
import subprocess
import time
from pathlib import Path

import pytest


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _voxkey_config_dir():
    """Return the expected voxkey config directory.

    Follows XDG Base Directory spec: $XDG_CONFIG_HOME/voxkey or ~/.config/voxkey.
    """
    xdg_config = os.environ.get("XDG_CONFIG_HOME", os.path.expanduser("~/.config"))
    return os.path.join(xdg_config, "voxkey")


def _token_file_path():
    """Return the expected path to the restore token file."""
    return os.path.join(_voxkey_config_dir(), "restore_token")


def _read_token():
    """Read the current restore token from disk, or None if absent."""
    path = _token_file_path()
    if not os.path.exists(path):
        return None
    with open(path, "r") as f:
        return f.read().strip()


def _write_token(content):
    """Write arbitrary content to the token file for testing."""
    path = _token_file_path()
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(content)


def _remove_token():
    """Remove the token file if it exists."""
    path = _token_file_path()
    if os.path.exists(path):
        os.unlink(path)


def _daemon_binary():
    """Path to the voxkey daemon binary."""
    return os.environ.get("VOXKEY_BIN", "voxkey")


def _start_daemon(bus_address, timeout=15):
    """Start the voxkey daemon against the given bus address.

    Sets proc.reached_idle and proc.startup_lines on the returned process.
    """
    env = os.environ.copy()
    env["DBUS_SESSION_BUS_ADDRESS"] = bus_address

    proc = subprocess.Popen(
        [_daemon_binary()],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )

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

    proc.reached_idle = reached_idle
    proc.startup_lines = lines
    return proc


def _stop_daemon(proc):
    """Stop the daemon gracefully."""
    if proc.poll() is not None:
        return
    proc.send_signal(signal.SIGTERM)
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()


def _collect_stderr(proc, timeout=0.5):
    """Drain available stderr lines from the daemon process."""
    lines = []
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        ready = select.select([proc.stderr], [], [], 0.1)[0]
        if ready:
            line = proc.stderr.readline()
            if line:
                lines.append(line.decode("utf-8", errors="replace").strip())
            else:
                break
    return lines


def _daemon_state(bus_address):
    result = subprocess.run(
        [
            "gdbus", "call", "--address", bus_address,
            "--dest", "io.github.hy26v.Voxkey.Daemon",
            "--object-path", "/io/github/hy26v/Voxkey/Daemon",
            "--method", "org.freedesktop.DBus.Properties.Get",
            "io.github.hy26v.Voxkey.Daemon1", "State",
        ],
        capture_output=True,
        text=True,
        timeout=3,
    )
    return result.stdout if result.returncode == 0 else ""


def _insert_last_transcript(bus_address, portal_control, previous_token=None):
    """Insert seeded history and wait until its short-lived grant is gone."""
    result = subprocess.run(
        [
            "gdbus", "call", "--address", bus_address,
            "--dest", "io.github.hy26v.Voxkey.Daemon",
            "--object-path", "/io/github/hy26v/Voxkey/Daemon",
            "--method", "io.github.hy26v.Voxkey.Daemon1.InsertLastTranscript",
        ],
        capture_output=True,
        text=True,
        timeout=5,
    )
    assert result.returncode == 0, result.stderr

    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        token = _read_token()
        if (
            token
            and token != previous_token
            and "Idle" in _daemon_state(bus_address)
            and "remote_desktop" not in portal_control.active_session_types()
        ):
            return token
        time.sleep(0.05)
    pytest.fail(
        "Text insertion did not rotate its token and release RemoteDesktop; "
        f"state={_daemon_state(bus_address)!r}, "
        f"active={portal_control.active_session_types()!r}, token={_read_token()!r}"
    )


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture(autouse=True)
def _clean_token_file():
    """Save and restore the token file around each test."""
    path = _token_file_path()
    original = None
    if os.path.exists(path):
        with open(path, "r") as f:
            original = f.read()

    yield

    # Restore original state
    if original is not None:
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write(original)
    elif os.path.exists(path):
        os.unlink(path)


@pytest.fixture(autouse=True)
def _seed_insertion_history(isolated_voxkey_home):
    """Give every manually started daemon deterministic text to insert."""
    state_dir = Path(os.environ["XDG_STATE_HOME"]) / "voxkey"
    state_dir.mkdir(parents=True, exist_ok=True)
    (state_dir / "history.json").write_text(json.dumps([
        {
            "id": 1,
            "recorded_at_unix_ms": 1,
            "text": "restore token lifecycle",
            "provider": "Whisper.cpp",
            "outcome": "completed",
        }
    ]))


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

class TestTokenSavedOnDemand:
    """Restore tokens are absent at idle and saved for actual insertion."""

    def test_token_file_created_by_first_insertion(
        self, daemon_process, portal_control,
    ):
        assert daemon_process.reached_idle, (
            "Daemon did not reach Idle — mock portal setup incomplete"
        )

        path = _token_file_path()
        assert not os.path.exists(path), (
            "Idle startup acquired RemoteDesktop instead of waiting for insertion"
        )
        token = _insert_last_transcript(
            daemon_process.bus_address, portal_control,
        )

        assert token, "Token file exists but is empty"
        assert os.path.exists(path), "Insertion did not persist its restore token"


class TestTokenFilePermissions:
    """Token file must have restrictive permissions (0600)."""

    def test_token_file_mode_is_0600(
        self, daemon_process, portal_control,
    ):
        """The restore token file should only be readable/writable by the owner."""
        assert daemon_process.reached_idle, (
            "Daemon did not reach Idle — mock portal setup incomplete"
        )

        path = _token_file_path()
        _insert_last_transcript(daemon_process.bus_address, portal_control)

        file_stat = os.stat(path)
        mode = stat.S_IMODE(file_stat.st_mode)
        assert mode == 0o600, (
            f"Token file permissions are {oct(mode)}, expected 0o600"
        )


class TestConfigFilePermissions:
    """config.toml must be private: it holds the dictionary, and a plaintext
    API key whenever the system keyring was unavailable."""

    @pytest.mark.asyncio
    async def test_config_written_by_the_daemon_is_mode_0600(
        self, daemon_process, dbus_session,
    ):
        """A setting changed over D-Bus must be persisted owner-only."""
        assert daemon_process.reached_idle, (
            "Daemon did not reach Idle — mock portal setup incomplete"
        )

        from helpers.dbus_portal import safe_introspect

        bus_name = "io.github.hy26v.Voxkey.Daemon"
        object_path = "/io/github/hy26v/Voxkey/Daemon"
        introspection = await safe_introspect(dbus_session, bus_name, object_path)
        daemon = dbus_session.get_proxy_object(
            bus_name, object_path, introspection,
        ).get_interface("io.github.hy26v.Voxkey.Daemon1")

        # Start from a deliberately world-readable file so the assertion
        # proves the daemon tightens permissions rather than inheriting them.
        path = os.path.join(_voxkey_config_dir(), "config.toml")
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as handle:
            handle.write("")
        os.chmod(path, 0o644)

        await daemon.call_set_shortcut("<Super><Alt>k")

        mode = stat.S_IMODE(os.stat(path).st_mode)
        assert mode == 0o600, (
            f"config.toml permissions are {oct(mode)}, expected 0o600"
        )

        leftovers = [
            name
            for name in os.listdir(_voxkey_config_dir())
            if ".tmp" in name
        ]
        assert not leftovers, f"save left scratch files behind: {leftovers}"


class TestTokenLoadedOnDemand:
    """An existing restore token is not consumed until text insertion."""

    def test_daemon_uses_existing_token_for_insertion(self, mock_portal):
        """When a valid token file exists, the daemon should use it
        during SelectDevices to avoid re-prompting the user.
        """
        bus_address, portal_control, _ = mock_portal

        existing_token = "test-restore-token-abc123"
        _write_token(existing_token)
        portal_control.clear_metrics()

        proc = _start_daemon(bus_address)
        try:
            assert proc.reached_idle, proc.startup_lines
            assert portal_control.selected_restore_tokens() == [], (
                "Idle startup consumed a RemoteDesktop restore token"
            )
            rotated = _insert_last_transcript(
                bus_address, portal_control, existing_token,
            )
            assert portal_control.selected_restore_tokens() == [existing_token]
            assert rotated != existing_token
        finally:
            _stop_daemon(proc)


class TestTokenRotation:
    """Each insertion's portal Start rotates the saved token."""

    def test_token_changes_after_each_insertion(self, mock_portal):
        bus_address, portal_control, _ = mock_portal
        portal_control.clear_metrics()
        proc = _start_daemon(bus_address)
        try:
            assert proc.reached_idle, proc.startup_lines
            token1 = _insert_last_transcript(bus_address, portal_control)
            token2 = _insert_last_transcript(bus_address, portal_control, token1)
            assert token2 != token1, (
                f"Token was not rotated after insertion: {token1!r} == {token2!r}"
            )
            assert portal_control.selected_restore_tokens() == [None, token1]
        finally:
            _stop_daemon(proc)


class TestCorruptTokenFallback:
    """Corrupt/invalid token file: daemon falls back to normal permission prompt."""

    def test_corrupt_token_does_not_crash(self, mock_portal):
        """An unusable token is ignored when insertion actually needs access."""
        bus_address, portal_control, _ = mock_portal
        corrupt_content = "THIS_IS_NOT_A_VALID_TOKEN_\x00\xff\xfe_GARBAGE"
        _write_token(corrupt_content)
        portal_control.clear_metrics()

        proc = _start_daemon(bus_address)
        try:
            assert proc.reached_idle, (
                f"Daemon did not recover from corrupt token: {proc.startup_lines}"
            )
            assert proc.poll() is None, (
                f"Daemon crashed with corrupt token (exit code: {proc.returncode}); "
                f"logs: {proc.startup_lines}"
            )
            assert _read_token() == corrupt_content, (
                "Idle startup touched a token before RemoteDesktop was needed"
            )
            token = _insert_last_transcript(
                bus_address, portal_control, corrupt_content,
            )
            assert token != corrupt_content
            assert portal_control.selected_restore_tokens() == [None]

            all_lines = list(proc.startup_lines) + _collect_stderr(proc)
            all_text = " ".join(all_lines).lower()
            assert "panic" not in all_text, f"Daemon panicked: {all_lines}"
        finally:
            _stop_daemon(proc)


class TestMissingTokenStartsFresh:
    """Missing token file: idle needs no grant and first insertion starts fresh."""

    def test_no_token_file_starts_clean(self, mock_portal):
        bus_address, portal_control, _ = mock_portal
        _remove_token()
        assert not os.path.exists(_token_file_path())
        portal_control.clear_metrics()

        proc = _start_daemon(bus_address)
        try:
            assert proc.reached_idle, proc.startup_lines
            assert proc.poll() is None, (
                f"Daemon crashed without token file (exit code: {proc.returncode})"
            )
            assert portal_control.active_session_types() == ["shortcuts"]
            _insert_last_transcript(bus_address, portal_control)
            assert portal_control.selected_restore_tokens() == [None]

            all_lines = list(proc.startup_lines) + _collect_stderr(proc)
            all_text = " ".join(all_lines).lower()
            assert "panic" not in all_text, f"Daemon panicked: {all_lines}"
            assert "fatal" not in all_text, f"Daemon fatal error: {all_lines}"
        finally:
            _stop_daemon(proc)


class TestRejectedTokenFallback:
    """A portal-rejected token is retried once without persistence."""

    def test_rejected_token_triggers_fresh_permission(self, mock_portal):
        bus_address, portal_control, _ = mock_portal
        stale_token = "mock-stale-restore-token"
        _write_token(stale_token)
        portal_control.clear_metrics()
        portal_control.reject_next_restore_token()

        proc = _start_daemon(bus_address)
        try:
            assert proc.reached_idle, proc.startup_lines
            replacement = _insert_last_transcript(
                bus_address, portal_control, stale_token,
            )
            assert proc.poll() is None
            assert portal_control.selected_restore_tokens() == [stale_token, None]
            assert replacement != stale_token, (
                "Stale token was not replaced after rejection"
            )
        finally:
            _stop_daemon(proc)
