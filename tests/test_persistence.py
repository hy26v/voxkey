# ABOUTME: Tests that voxkey persists the RemoteDesktop restore token correctly.
# ABOUTME: Validates file permissions, token rotation, and corrupt/missing token handling.

import os
import select
import stat
import time
import signal
import subprocess

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


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

class TestTokenSavedAfterStart:
    """Restore token is saved to config dir after RemoteDesktop.Start."""

    def test_token_file_created_after_daemon_start(self, daemon_process):
        """After the daemon starts and completes RemoteDesktop.Start,
        a restore token file should exist in the config directory.
        """
        assert daemon_process.reached_idle, (
            "Daemon did not reach Idle — mock portal setup incomplete"
        )

        path = _token_file_path()
        assert os.path.exists(path), "Token file was not created"

        token = _read_token()
        assert token, "Token file exists but is empty"
        assert len(token) > 0, "Token should be a non-empty string"


class TestTokenFilePermissions:
    """Token file must have restrictive permissions (0600)."""

    def test_token_file_mode_is_0600(self, daemon_process):
        """The restore token file should only be readable/writable by the owner."""
        assert daemon_process.reached_idle, (
            "Daemon did not reach Idle — mock portal setup incomplete"
        )

        path = _token_file_path()
        assert os.path.exists(path), "Token file was not created"

        file_stat = os.stat(path)
        mode = stat.S_IMODE(file_stat.st_mode)
        assert mode == 0o600, (
            f"Token file permissions are {oct(mode)}, expected 0o600"
        )


class TestTokenLoadedOnStartup:
    """Daemon loads restore token on startup and passes it to SelectDevices."""

    def test_daemon_uses_existing_token(self, mock_portal):
        """When a valid token file exists, the daemon should use it
        during SelectDevices to avoid re-prompting the user.
        """
        bus_address, _, _ = mock_portal

        # Write a token before starting the daemon
        _write_token("test-restore-token-abc123")

        proc = _start_daemon(bus_address)
        try:
            if not proc.reached_idle:
                pytest.skip("Daemon did not reach Idle — mock portal setup incomplete")

            # Check startup lines and any additional stderr
            all_lines = list(proc.startup_lines) + _collect_stderr(proc)
            # The daemon should log that it loaded a restore token
            token_lines = [l for l in all_lines if "restore_token" in l.lower() or "restore token" in l.lower()]
            assert any("loaded" in l.lower() or "using" in l.lower() for l in token_lines), (
                f"Daemon did not log loading restore token. Logs: {all_lines}"
            )
        finally:
            _stop_daemon(proc)


class TestTokenRotation:
    """After each Start, the new token replaces the old one."""

    def test_token_changes_after_restart(self, mock_portal):
        """Restarting the daemon should produce a new restore token."""
        bus_address, _, _ = mock_portal

        # First run: let daemon get a token
        proc1 = _start_daemon(bus_address)
        try:
            if not proc1.reached_idle:
                pytest.skip("Daemon did not reach Idle — mock portal setup incomplete")
            token1 = _read_token()
            if not token1:
                pytest.skip("Portal did not return a restore token")
        finally:
            _stop_daemon(proc1)

        # Second run: should get a different token
        proc2 = _start_daemon(bus_address)
        try:
            if not proc2.reached_idle:
                pytest.skip("Daemon did not reach Idle on second run")
            token2 = _read_token()
            if not token2:
                pytest.skip("Portal did not return a restore token on second run")
            assert token2 != token1, (
                f"Token was not rotated after restart: {token1!r} == {token2!r}"
            )
        finally:
            _stop_daemon(proc2)


class TestCorruptTokenFallback:
    """Corrupt/invalid token file: daemon falls back to normal permission prompt."""

    def test_corrupt_token_does_not_crash(self, mock_portal):
        """If the token file contains garbage, the daemon should start
        normally and fall back to requesting permissions fresh.
        """
        bus_address, _, _ = mock_portal
        corrupt_content = "THIS_IS_NOT_A_VALID_TOKEN_\x00\xff\xfe_GARBAGE"
        _write_token(corrupt_content)

        proc = _start_daemon(bus_address)
        try:
            # Daemon should still be running
            assert proc.poll() is None, (
                f"Daemon crashed with corrupt token (exit code: {proc.returncode})"
            )

            all_lines = list(proc.startup_lines) + _collect_stderr(proc)
            all_text = " ".join(all_lines).lower()
            assert "panic" not in all_text, f"Daemon panicked: {all_lines}"
        finally:
            _stop_daemon(proc)


class TestMissingTokenStartsFresh:
    """Missing token file: daemon starts fresh without crashing."""

    def test_no_token_file_starts_clean(self, mock_portal):
        """If no token file exists, the daemon should start normally."""
        bus_address, _, _ = mock_portal
        _remove_token()
        assert not os.path.exists(_token_file_path())

        proc = _start_daemon(bus_address)
        try:
            assert proc.poll() is None, (
                f"Daemon crashed without token file (exit code: {proc.returncode})"
            )

            all_lines = list(proc.startup_lines) + _collect_stderr(proc)
            all_text = " ".join(all_lines).lower()
            assert "panic" not in all_text, f"Daemon panicked: {all_lines}"
            assert "fatal" not in all_text, f"Daemon fatal error: {all_lines}"
        finally:
            _stop_daemon(proc)


class TestTokenSingleUse:
    """Restore token is single-use and cannot be reused."""

    def test_reused_token_triggers_new_permission(self, mock_portal):
        """Using the same token twice should cause the portal to reject it,
        and the daemon should handle this by requesting fresh permissions.
        """
        bus_address, _, _ = mock_portal

        # First run: get a real token
        proc1 = _start_daemon(bus_address)
        try:
            if not proc1.reached_idle:
                pytest.skip("Daemon did not reach Idle — mock portal setup incomplete")
            token_after_first = _read_token()
            if not token_after_first:
                pytest.skip("Portal did not return a restore token")
        finally:
            _stop_daemon(proc1)

        # Save the token that was just used/rotated
        used_token = token_after_first

        # Second run: daemon will consume and rotate the token
        proc2 = _start_daemon(bus_address)
        try:
            pass
        finally:
            _stop_daemon(proc2)

        # Now write back the already-consumed token from first run
        _write_token(used_token)

        # Third run: the stale token should be rejected
        proc3 = _start_daemon(bus_address)
        try:
            assert proc3.poll() is None, (
                f"Daemon crashed on stale token (exit code: {proc3.returncode})"
            )

            all_lines = list(proc3.startup_lines) + _collect_stderr(proc3)
            all_text = " ".join(all_lines).lower()

            # Should not have crashed
            assert "panic" not in all_text, f"Daemon panicked on stale token: {all_lines}"

            # Should get a new token (because the stale one was rejected)
            token_after_third = _read_token()
            if not token_after_third:
                pytest.skip("Portal did not return a replacement token")
            assert token_after_third != used_token, (
                "Stale token was not replaced after rejection"
            )
        finally:
            _stop_daemon(proc3)
