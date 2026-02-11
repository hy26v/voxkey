# ABOUTME: Tests that the voxkey daemon state machine handles transitions correctly.
# ABOUTME: Validates edge cases like duplicate signals, rapid cycling, and error recovery.

import time

import pytest


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _collect_stderr(proc, timeout=0.5):
    """Drain available stderr lines from the daemon process."""
    import select
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


def _wait_for_state(proc, target_state, timeout=30):
    """Poll daemon stderr until target_state appears or timeout.

    Returns (found, collected_lines).
    """
    import select
    lines = []
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            break
        ready = select.select([proc.stderr], [], [], 0.5)[0]
        if ready:
            line = proc.stderr.readline()
            if line:
                decoded = line.decode("utf-8", errors="replace").strip()
                lines.append(decoded)
                if "STATE:" in decoded and target_state in decoded.split("STATE:")[-1]:
                    return True, lines
    return False, lines


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

class TestIdleToRecording:
    """Activated signal transitions the daemon from Idle to Recording."""

    def test_activated_starts_recording(
        self, daemon_process, portal_control
    ):
        """When the daemon is Idle, an Activated signal should start recording."""
        assert daemon_process.reached_idle, (
            "Daemon did not reach Idle — mock portal setup incomplete"
        )

        # Simulate pressing the global shortcut
        portal_control.emit_activated()

        found, lines = _wait_for_state(daemon_process, "Recording", timeout=10)
        assert found, (
            f"Expected Recording state after Activated signal, got: "
            f"{[l for l in lines if 'STATE:' in l]}"
        )

        # Release the shortcut
        portal_control.emit_deactivated()


class TestFullDictationCycle:
    """Recording -> Transcribing -> Injecting -> Idle flow completes."""

    def test_recording_through_idle_cycle(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir
    ):
        """A complete hold-release cycle should pass through all states back to Idle."""
        import os

        assert daemon_process.reached_idle, (
            "Daemon did not reach Idle — mock portal setup incomplete"
        )

        audio_file = os.path.join(fixtures_dir, "hello.wav")

        # Start the virtual mic streaming audio
        virtual_mic.stream_file(audio_file)

        # Activate shortcut to start recording
        portal_control.emit_activated()

        found, lines = _wait_for_state(daemon_process, "Recording", timeout=10)
        assert found, f"Did not reach Recording state: {lines}"

        # Hold for a moment to capture audio, then release
        time.sleep(1.5)
        portal_control.emit_deactivated()

        # Poll until we return to Idle (transcription + injection)
        found_idle, more_lines = _wait_for_state(daemon_process, "Idle", timeout=60)
        all_lines = lines + more_lines

        state_lines = [l for l in all_lines if "STATE:" in l]
        states_seen = []
        for l in state_lines:
            state = l.split("STATE:")[-1].strip()
            if not states_seen or states_seen[-1] != state:
                states_seen.append(state)

        assert "Recording" in states_seen, f"Missing Recording state: {states_seen}"
        assert "Transcribing" in states_seen, f"Missing Transcribing state: {states_seen}"
        assert found_idle, (
            f"Did not return to Idle within timeout. States seen: {states_seen}"
        )


class TestDuplicateActivatedIgnored:
    """Duplicate Activated while already Recording should be ignored."""

    def test_double_activated_no_double_record(
        self, daemon_process, portal_control
    ):
        """A second Activated while Recording should not start a second recording."""
        assert daemon_process.reached_idle, (
            "Daemon did not reach Idle — mock portal setup incomplete"
        )

        # First activation -> Recording
        portal_control.emit_activated()

        found, initial_lines = _wait_for_state(daemon_process, "Recording", timeout=10)
        assert found, "First Activated did not produce Recording state"

        # Simulate another activation while already recording
        portal_control.emit_activated()
        time.sleep(1.0)

        # Check that we didn't get another Recording transition
        extra_lines = _collect_stderr(daemon_process, timeout=1.0)
        recording_count_after = sum(
            1 for l in extra_lines
            if "STATE:" in l and "Recording" in l.split("STATE:")[-1]
        )

        assert recording_count_after == 0, (
            f"Duplicate Activated caused extra Recording transitions: {extra_lines}"
        )

        # Clean up
        portal_control.emit_deactivated()


class TestDuplicateDeactivatedIgnored:
    """Deactivated while not Recording should be ignored."""

    def test_deactivated_while_idle_is_noop(
        self, daemon_process, portal_control
    ):
        """Releasing the shortcut while Idle should not cause errors or state changes."""
        assert daemon_process.reached_idle, (
            "Daemon did not reach Idle — mock portal setup incomplete"
        )

        # Drain initial startup logs
        _collect_stderr(daemon_process)

        # Send a release without ever pressing (daemon should be Idle)
        portal_control.emit_deactivated()
        time.sleep(0.5)

        lines = _collect_stderr(daemon_process)
        # Should not see any state transitions or errors
        state_changes = [l for l in lines if "STATE:" in l]
        error_lines = [l for l in lines if "ERROR" in l.upper()]

        assert len(state_changes) == 0, (
            f"Unexpected state changes on spurious Deactivated: {state_changes}"
        )
        assert len(error_lines) == 0, (
            f"Errors on spurious Deactivated: {error_lines}"
        )


class TestSerialInjectionQueue:
    """Multiple utterances must be injected serially, never concurrently."""

    def test_no_concurrent_injection(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir
    ):
        """Two dictation cycles should inject text one after the other, never concurrently."""
        import os

        assert daemon_process.reached_idle, (
            "Daemon did not reach Idle — mock portal setup incomplete"
        )

        audio_file = os.path.join(fixtures_dir, "hello.wav")
        all_lines = []

        # First dictation cycle
        virtual_mic.stream_file(audio_file)
        portal_control.emit_activated()

        found, lines = _wait_for_state(daemon_process, "Recording", timeout=10)
        assert found, "First cycle did not reach Recording"
        all_lines.extend(lines)

        time.sleep(1.5)
        portal_control.emit_deactivated()

        # Wait for the first cycle to complete before starting the second
        found_idle, lines = _wait_for_state(daemon_process, "Idle", timeout=60)
        all_lines.extend(lines)
        assert found_idle, "First cycle did not return to Idle"

        # Second dictation cycle
        virtual_mic.stream_file(audio_file)
        portal_control.emit_activated()

        found, lines = _wait_for_state(daemon_process, "Recording", timeout=10)
        assert found, "Second cycle did not reach Recording"
        all_lines.extend(lines)

        time.sleep(1.5)
        portal_control.emit_deactivated()

        found_idle, lines = _wait_for_state(daemon_process, "Idle", timeout=60)
        all_lines.extend(lines)
        assert found_idle, "Second cycle did not return to Idle"

        # Count how many times we entered the Injecting state
        injecting_count = sum(
            1 for l in all_lines
            if "STATE:" in l and l.split("STATE:")[-1].strip() == "Injecting"
        )

        assert injecting_count >= 2, (
            f"Expected at least 2 injection cycles, got {injecting_count}: "
            f"{[l for l in all_lines if 'STATE:' in l]}"
        )


class TestRapidCycling:
    """Rapid Activated/Deactivated cycling should not break state."""

    def test_rapid_toggle_does_not_corrupt_state(
        self, daemon_process, portal_control
    ):
        """Rapidly pressing and releasing the shortcut should not leave the daemon
        in a broken state or crash it.
        """
        assert daemon_process.reached_idle, (
            "Daemon did not reach Idle — mock portal setup incomplete"
        )

        # Rapid press/release cycles
        for _ in range(5):
            portal_control.emit_activated()
            time.sleep(0.15)
            portal_control.emit_deactivated()
            time.sleep(0.15)

        # Poll until daemon settles back to Idle
        found_idle, lines = _wait_for_state(daemon_process, "Idle", timeout=30)

        # Daemon should still be alive
        assert daemon_process.poll() is None, (
            f"Daemon crashed during rapid cycling (exit code: {daemon_process.returncode})"
        )

        # Should end up back in Idle
        state_lines = [l for l in lines if "STATE:" in l]
        if state_lines:
            final_state = state_lines[-1].split("STATE:")[-1].strip()
            assert final_state == "Idle", (
                f"Expected Idle after rapid cycling, got: {final_state}"
            )

        # No unhandled errors
        panic_lines = [l for l in lines if "panic" in l.lower() or "FATAL" in l]
        assert len(panic_lines) == 0, (
            f"Daemon panicked during rapid cycling: {panic_lines}"
        )
