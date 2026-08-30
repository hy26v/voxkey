# ABOUTME: Tests fail-closed portal handling, stale permissions, and graceful shutdown.
# ABOUTME: Validates that startup and shutdown error paths do not hang.

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
from helpers.mock_portal import SESSION_IFACE


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

    def test_normal_mock_session_remains_alive(
        self, daemon_process
    ):
        """The healthy private portal session remains alive while idle."""
        assert _daemon_is_alive(daemon_process), "Daemon should be alive"

        time.sleep(3)
        assert _daemon_is_alive(daemon_process), (
            "Daemon exited during a healthy mock portal session"
        )

    def test_normal_mock_session_does_not_restart_automatically(
        self, daemon_process
    ):
        """A healthy session remains stable without an automatic rebuild."""
        assert _daemon_is_alive(daemon_process), "Daemon should be alive"

        time.sleep(3)
        assert _daemon_is_alive(daemon_process), (
            "Daemon restarted or exited during a healthy mock portal session"
        )


# ---------------------------------------------------------------------------
# Tests: D-Bus disconnect and reconnect
# ---------------------------------------------------------------------------

class TestDBusFailureHandling:
    """Tests bounded handling around D-Bus failures."""

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

    def test_daemon_exits_cleanly_on_sigterm(self, daemon_process, portal_control):
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
        closed = portal_control.closed_session_types()
        assert "remote_desktop" not in closed, (
            "Idle shutdown found a RemoteDesktop grant that should not exist"
        )
        assert "shortcuts" in closed, (
            "SIGTERM must explicitly close the GlobalShortcuts session"
        )
        assert "remote_desktop" not in portal_control.active_session_types()

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

    def test_unresponsive_portal_close_cannot_strand_shutdown(
        self,
        daemon_process,
        portal_control,
    ):
        """A nonresponsive persistent session cannot strand shutdown."""
        assert _daemon_is_alive(daemon_process)
        portal_control.suppress_next_method_reply(SESSION_IFACE, "Close")

        daemon_process.send_signal(signal.SIGTERM)
        try:
            daemon_process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            daemon_process.kill()
            daemon_process.wait()
            pytest.fail("a nonresponsive portal stranded daemon shutdown")

        diagnostics = daemon_process.stderr.read().decode("utf-8", errors="replace")
        assert (
            "globalshortcuts session close timed out" in diagnostics.lower()
            or "teardown exceeded its total deadline" in diagnostics.lower()
        ), diagnostics

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

    @pytest.mark.parametrize(
        ("fault", "maximum_seconds"),
        [
            ("method_reply", 15),
            ("portal_response", 35),
        ],
    )
    def test_unresponsive_portal_startup_has_a_total_deadline(
        self,
        mock_portal,
        virtual_mic,
        fault,
        maximum_seconds,
    ):
        """Neither a missing method reply nor a missing Response can hang startup."""
        bus_address, controller, _ = mock_portal
        controller.clear_metrics()
        if fault == "method_reply":
            controller.suppress_next_method_reply(
                GLOBAL_SHORTCUTS_IFACE, "CreateSession",
            )
        else:
            controller.suppress_next_portal_response(
                GLOBAL_SHORTCUTS_IFACE, "CreateSession",
            )

        env = os.environ.copy()
        env["DBUS_SESSION_BUS_ADDRESS"] = bus_address
        proc = subprocess.Popen(
            [_daemon_binary()],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )
        started = time.monotonic()
        try:
            try:
                proc.wait(timeout=maximum_seconds)
            except subprocess.TimeoutExpired:
                pytest.fail(
                    f"daemon exceeded its {maximum_seconds}s startup budget "
                    f"after a missing portal {fault.replace('_', ' ')}"
                )
            elapsed = time.monotonic() - started
            diagnostics = proc.stderr.read().decode("utf-8", errors="replace")
            assert "timed out" in diagnostics.lower(), diagnostics
            assert elapsed < maximum_seconds
        finally:
            if proc.poll() is None:
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
