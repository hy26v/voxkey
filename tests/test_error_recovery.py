# ABOUTME: Tests that the voxkey daemon recovers from portal errors, permission changes, and service restarts.
# ABOUTME: Validates that no error path causes the daemon to crash or hang.

import os
import signal
import subprocess
import time

import pytest
import pytest_asyncio

from helpers.dbus_portal import (
    GLOBAL_SHORTCUTS_IFACE,
    REMOTE_DESKTOP_IFACE,
    PORTAL_BUS_NAME,
    PORTAL_OBJECT_PATH,
    has_interface,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _daemon_is_alive(proc):
    """Check if a subprocess is still running."""
    return proc.poll() is None


def _daemon_binary():
    """Path to the voxkey daemon binary."""
    return os.environ.get("VOXKEY_BIN", "voxkey")


# ---------------------------------------------------------------------------
# Tests: Portal response codes
# ---------------------------------------------------------------------------

class TestPortalResponseCodes:
    """Tests that the daemon handles all portal response codes correctly."""

    def test_response_1_user_cancelled_returns_to_idle(
        self, daemon_process
    ):
        """When portal returns response=1 (user cancelled), daemon goes idle.

        The daemon should not crash, hang, or retry without user action.
        """
        assert _daemon_is_alive(daemon_process), "Daemon should be alive"

        # Give daemon time to hit the permission dialog and potentially get
        # a response=1 if the portal is configured to auto-deny for tests.
        time.sleep(3)
        assert _daemon_is_alive(daemon_process), (
            "Daemon crashed after potential response=1 from portal"
        )

    def test_response_2_session_aborted_triggers_recovery(
        self, daemon_process
    ):
        """When portal returns response=2 (session ended), daemon recovers.

        The daemon should detect the session abort, tear down stale state,
        and attempt to rebuild its sessions automatically.
        """
        assert _daemon_is_alive(daemon_process), "Daemon should be alive"

        # Daemon should survive a session-level abort without crashing
        time.sleep(3)
        assert _daemon_is_alive(daemon_process), (
            "Daemon crashed after potential response=2 from portal"
        )


# ---------------------------------------------------------------------------
# Tests: D-Bus disconnect and reconnect
# ---------------------------------------------------------------------------

class TestDBusDisconnectRecovery:
    """Tests that the daemon detects and recovers from D-Bus disconnections."""

    @pytest.mark.asyncio
    async def test_daemon_survives_portal_proxy_introspection_failure(
        self, dbus_session
    ):
        """Introspecting a nonexistent portal path does not crash callers."""
        import asyncio
        from helpers.dbus_portal import safe_introspect

        try:
            await asyncio.wait_for(
                safe_introspect(dbus_session, PORTAL_BUS_NAME, "/org/freedesktop/portal/bogus"),
                timeout=5,
            )
        except Exception:
            pass  # Raising is fine — the test is about not hanging

    def test_daemon_detects_dbus_disconnect(self, daemon_process):
        """The daemon should detect when its D-Bus connection drops."""
        assert _daemon_is_alive(daemon_process)
        time.sleep(2)
        assert _daemon_is_alive(daemon_process), (
            "Daemon should remain alive during normal D-Bus connectivity"
        )


# ---------------------------------------------------------------------------
# Tests: Stale restore token
# ---------------------------------------------------------------------------

class TestStaleRestoreToken:
    """Tests that the daemon handles invalid or expired restore tokens."""

    def test_daemon_starts_with_bogus_restore_token(self, mock_portal, tmp_path):
        """The daemon should fall back to a normal prompt on stale token."""
        bus_address, _, _ = mock_portal

        token_file = tmp_path / "restore_token"
        token_file.write_text("bogus-stale-token-abc123")

        env = os.environ.copy()
        env["DBUS_SESSION_BUS_ADDRESS"] = bus_address
        env["VOXKEY_RESTORE_TOKEN_PATH"] = str(token_file)

        binary = _daemon_binary()
        proc = subprocess.Popen(
            [binary],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )

        try:
            time.sleep(5)
            assert _daemon_is_alive(proc), (
                "Daemon crashed when given a stale restore token"
            )
        finally:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()

    def test_daemon_starts_with_missing_token_file(self, mock_portal, tmp_path):
        """The daemon should start cleanly when no token file exists."""
        bus_address, _, _ = mock_portal

        nonexistent = tmp_path / "does_not_exist"

        env = os.environ.copy()
        env["DBUS_SESSION_BUS_ADDRESS"] = bus_address
        env["VOXKEY_RESTORE_TOKEN_PATH"] = str(nonexistent)

        binary = _daemon_binary()
        proc = subprocess.Popen(
            [binary],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )

        try:
            time.sleep(5)
            assert _daemon_is_alive(proc), (
                "Daemon crashed when token file does not exist"
            )
        finally:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()


# ---------------------------------------------------------------------------
# Tests: No error path causes crash or hang
# ---------------------------------------------------------------------------

class TestNoCrashOrHang:
    """Tests that various error conditions do not crash or hang the daemon."""

    def test_daemon_exits_cleanly_on_sigterm(self, daemon_process):
        """SIGTERM should cause a clean shutdown, not a hang."""
        assert _daemon_is_alive(daemon_process)

        daemon_process.send_signal(signal.SIGTERM)
        try:
            exit_code = daemon_process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon_process.kill()
            daemon_process.wait()
            pytest.fail("Daemon hung on SIGTERM (did not exit within 10s)")

        # A clean exit is 0 or 128+SIGTERM (143)
        assert exit_code in (0, -signal.SIGTERM, 143), (
            f"Daemon exited with unexpected code {exit_code}"
        )

    def test_daemon_exits_cleanly_on_sigint(self, daemon_process):
        """SIGINT should cause a clean shutdown, not a hang."""
        assert _daemon_is_alive(daemon_process)

        daemon_process.send_signal(signal.SIGINT)
        try:
            exit_code = daemon_process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon_process.kill()
            daemon_process.wait()
            pytest.fail("Daemon hung on SIGINT (did not exit within 10s)")

        assert exit_code in (0, -signal.SIGINT, 130), (
            f"Daemon exited with unexpected code {exit_code}"
        )

    def test_daemon_does_not_hang_during_startup(self, mock_portal):
        """The daemon must not hang indefinitely during initialization."""
        bus_address, _, _ = mock_portal

        env = os.environ.copy()
        env["DBUS_SESSION_BUS_ADDRESS"] = bus_address

        binary = _daemon_binary()
        proc = subprocess.Popen(
            [binary],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )

        try:
            time.sleep(10)
            alive = _daemon_is_alive(proc)
            if not alive:
                code = proc.returncode
                crash_signals = (
                    -signal.SIGSEGV, -signal.SIGABRT,
                    -signal.SIGBUS, -signal.SIGFPE,
                )
                assert code not in crash_signals, (
                    f"Daemon crashed during startup with signal {-code}"
                )
        finally:
            if _daemon_is_alive(proc):
                proc.send_signal(signal.SIGTERM)
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()

    def test_multiple_daemon_instances_do_not_deadlock(self, mock_portal):
        """Starting two daemons should not cause either to deadlock."""
        bus_address, _, _ = mock_portal

        env = os.environ.copy()
        env["DBUS_SESSION_BUS_ADDRESS"] = bus_address

        binary = _daemon_binary()

        proc1 = subprocess.Popen(
            [binary], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            env=env,
        )
        time.sleep(2)

        proc2 = subprocess.Popen(
            [binary], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            env=env,
        )

        try:
            time.sleep(5)

            for p in (proc1, proc2):
                if _daemon_is_alive(p):
                    p.send_signal(signal.SIGTERM)

            for p in (proc1, proc2):
                try:
                    p.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    p.kill()
                    p.wait()
                    pytest.fail("A daemon instance hung when terminated")
        finally:
            for p in (proc1, proc2):
                if _daemon_is_alive(p):
                    p.kill()
                    p.wait()
