# ABOUTME: Tests shortcut-driven recording without invoking keyboard injection.
# ABOUTME: EIS output is covered by the Rust protocol tests, not this portal mock.

import select
import time


def _collect_stderr(proc, timeout=0.5):
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


def _assert_daemon_ready(daemon_process):
    assert daemon_process.reached_idle, (
        "Daemon did not reach Idle — mock portal setup incomplete"
    )


def test_shortcut_press_starts_recording(daemon_process, portal_control):
    """The activation signal reaches Recording without touching EIS."""
    _assert_daemon_ready(daemon_process)
    portal_control.emit_activated()
    time.sleep(1)

    lines = _collect_stderr(daemon_process, timeout=1.0)
    assert daemon_process.poll() is None, "Daemon crashed on shortcut press"
    assert any("STATE: Recording" in line for line in lines), (
        "No Recording state change after shortcut activation"
    )


def test_shortcut_release_is_harmless_in_toggle_mode(daemon_process, portal_control):
    """Deactivated is drained and ignored by the configured toggle workflow."""
    _assert_daemon_ready(daemon_process)
    portal_control.emit_activated()
    time.sleep(0.2)
    portal_control.emit_deactivated()
    time.sleep(0.2)

    assert daemon_process.poll() is None, "Daemon exited on shortcut release"
