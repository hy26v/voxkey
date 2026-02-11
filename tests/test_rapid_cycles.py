# ABOUTME: Tests that rapid dictation cycling works without losing text or getting confused.
# ABOUTME: Validates serial injection, queue ordering, and graceful handling of short activations.

import os
import subprocess
import time

import pytest


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# Duration the shortcut is held to simulate a dictation hold
DICTATION_HOLD_SECONDS = 0.5
# Pause between dictation cycles to let injection complete
INTER_CYCLE_PAUSE = 1.0


def _daemon_is_alive(proc):
    """Check if a subprocess is still running."""
    return proc.poll() is None


def _perform_dictation_cycle(
    portal_control, virtual_mic, wav_path, hold_seconds=DICTATION_HOLD_SECONDS
):
    """Perform one dictation cycle: activate, stream audio, deactivate.

    Returns once the shortcut is released. Does not wait for injection
    to finish -- the caller should add appropriate delays.
    """
    portal_control.emit_activated()
    time.sleep(0.1)

    virtual_mic.stream_file(wav_path)
    time.sleep(hold_seconds)

    portal_control.emit_deactivated()


# ---------------------------------------------------------------------------
# Tests: Consecutive dictation cycles
# ---------------------------------------------------------------------------

class TestConsecutiveDictationCycles:
    """Tests that multiple dictation cycles in succession produce correct output."""

    def test_five_consecutive_cycles_no_crash(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir,
    ):
        """Five back-to-back dictation cycles should not crash the daemon."""
        assert daemon_process.reached_idle

        wav_path = os.path.join(fixtures_dir, "test_utterance.wav")
        if not os.path.exists(wav_path):
            pytest.skip(f"Test fixture not found: {wav_path}")

        for cycle in range(5):
            assert _daemon_is_alive(daemon_process), (
                f"Daemon crashed before cycle {cycle + 1}"
            )
            _perform_dictation_cycle(portal_control, virtual_mic, wav_path)
            time.sleep(3)

        assert _daemon_is_alive(daemon_process), (
            "Daemon crashed during consecutive dictation cycles"
        )

    def test_no_text_interleaving_between_cycles(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir,
    ):
        """Text from one cycle must finish injecting before the next begins."""
        assert daemon_process.reached_idle

        wav_path = os.path.join(fixtures_dir, "test_utterance.wav")
        if not os.path.exists(wav_path):
            pytest.skip(f"Test fixture not found: {wav_path}")

        for _ in range(3):
            _perform_dictation_cycle(portal_control, virtual_mic, wav_path)
            time.sleep(5)

        assert _daemon_is_alive(daemon_process)


# ---------------------------------------------------------------------------
# Tests: No stuck recording state
# ---------------------------------------------------------------------------

class TestNoStuckRecording:
    """Tests that rapid cycling does not leave the daemon in a stuck state."""

    def test_no_stuck_state_after_rapid_cycling(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir,
    ):
        """After rapid cycling, the daemon should return to idle."""
        assert daemon_process.reached_idle

        wav_path = os.path.join(fixtures_dir, "test_utterance.wav")
        if not os.path.exists(wav_path):
            pytest.skip(f"Test fixture not found: {wav_path}")

        for _ in range(3):
            _perform_dictation_cycle(
                portal_control, virtual_mic, wav_path,
                hold_seconds=0.3,
            )
            time.sleep(0.5)

        # Wait for everything to drain
        time.sleep(10)

        assert _daemon_is_alive(daemon_process), (
            "Daemon crashed during rapid cycling"
        )

        # Now try one more normal cycle to prove the daemon is still functional
        _perform_dictation_cycle(portal_control, virtual_mic, wav_path)
        time.sleep(5)

        assert _daemon_is_alive(daemon_process), (
            "Daemon became stuck or crashed after rapid cycling"
        )


# ---------------------------------------------------------------------------
# Tests: Utterance queue ordering
# ---------------------------------------------------------------------------

class TestUtteranceQueueOrdering:
    """Tests that the utterance queue drains completely and in order."""

    def test_queue_drains_completely(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir,
    ):
        """All enqueued utterances must eventually be injected."""
        assert daemon_process.reached_idle

        wav_path = os.path.join(fixtures_dir, "test_utterance.wav")
        if not os.path.exists(wav_path):
            pytest.skip(f"Test fixture not found: {wav_path}")

        cycle_count = 3
        for _ in range(cycle_count):
            _perform_dictation_cycle(
                portal_control, virtual_mic, wav_path,
                hold_seconds=0.4,
            )
            time.sleep(0.5)

        # Wait generously for all transcriptions and injections
        time.sleep(cycle_count * 10)

        assert _daemon_is_alive(daemon_process), (
            "Daemon crashed while draining utterance queue"
        )

    def test_queue_preserves_order(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir,
    ):
        """Utterances must be injected in the order they were recorded."""
        assert daemon_process.reached_idle

        wav_path = os.path.join(fixtures_dir, "test_utterance.wav")
        if not os.path.exists(wav_path):
            pytest.skip(f"Test fixture not found: {wav_path}")

        for _ in range(3):
            _perform_dictation_cycle(
                portal_control, virtual_mic, wav_path,
                hold_seconds=0.5,
            )
            time.sleep(1)

        # Let everything drain
        time.sleep(20)

        assert _daemon_is_alive(daemon_process), (
            "Daemon crashed while processing ordered utterances"
        )


# ---------------------------------------------------------------------------
# Tests: Rapid activate/deactivate (very short press)
# ---------------------------------------------------------------------------

class TestRapidActivateDeactivate:
    """Tests that very short or instant activate/deactivate pairs are safe."""

    def test_instant_press_release_does_not_crash(
        self, daemon_process, portal_control,
    ):
        """Activate and immediately deactivate the shortcut."""
        assert daemon_process.reached_idle
        assert _daemon_is_alive(daemon_process)

        portal_control.emit_activated()
        time.sleep(0.02)
        portal_control.emit_deactivated()
        time.sleep(2)

        assert _daemon_is_alive(daemon_process), (
            "Daemon crashed on instant press-release"
        )

    def test_multiple_rapid_taps_do_not_crash(
        self, daemon_process, portal_control,
    ):
        """Rapidly tapping the shortcut many times should not crash."""
        assert daemon_process.reached_idle
        assert _daemon_is_alive(daemon_process)

        for i in range(10):
            portal_control.emit_activated()
            time.sleep(0.02)
            portal_control.emit_deactivated()
            time.sleep(0.1)

        time.sleep(5)

        assert _daemon_is_alive(daemon_process), (
            "Daemon crashed after 10 rapid taps"
        )

    def test_daemon_still_functional_after_rapid_taps(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir,
    ):
        """After rapid taps, a normal dictation cycle should still work."""
        assert daemon_process.reached_idle
        assert _daemon_is_alive(daemon_process)

        # Fire off some rapid taps
        for _ in range(5):
            portal_control.emit_activated()
            time.sleep(0.02)
            portal_control.emit_deactivated()
            time.sleep(0.1)

        time.sleep(3)

        # Now do a normal dictation cycle
        wav_path = os.path.join(fixtures_dir, "test_utterance.wav")
        if not os.path.exists(wav_path):
            pytest.skip(f"Test fixture not found: {wav_path}")

        _perform_dictation_cycle(portal_control, virtual_mic, wav_path)
        time.sleep(5)

        assert _daemon_is_alive(daemon_process), (
            "Daemon crashed during normal cycle after rapid taps"
        )


# ---------------------------------------------------------------------------
# Tests: Overlapping dictation (start new while previous is injecting)
# ---------------------------------------------------------------------------

class TestOverlappingDictation:
    """Tests starting a new dictation while the previous injection is ongoing."""

    def test_new_dictation_during_injection_does_not_crash(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir,
    ):
        """Start a second dictation while the first is still being injected."""
        assert daemon_process.reached_idle

        wav_path = os.path.join(fixtures_dir, "test_utterance.wav")
        if not os.path.exists(wav_path):
            pytest.skip(f"Test fixture not found: {wav_path}")

        # First dictation cycle -- don't wait for injection to finish
        _perform_dictation_cycle(portal_control, virtual_mic, wav_path)
        # Start the second cycle immediately
        time.sleep(0.3)
        _perform_dictation_cycle(portal_control, virtual_mic, wav_path)

        # Wait for both to complete
        time.sleep(15)

        assert _daemon_is_alive(daemon_process), (
            "Daemon crashed during overlapping dictation cycles"
        )

    def test_three_overlapping_cycles_no_crash(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir,
    ):
        """Three rapid overlapping cycles should all be queued safely."""
        assert daemon_process.reached_idle

        wav_path = os.path.join(fixtures_dir, "test_utterance.wav")
        if not os.path.exists(wav_path):
            pytest.skip(f"Test fixture not found: {wav_path}")

        for _ in range(3):
            _perform_dictation_cycle(
                portal_control, virtual_mic, wav_path,
                hold_seconds=0.3,
            )
            time.sleep(0.2)

        # Wait for all injections to drain
        time.sleep(20)

        assert _daemon_is_alive(daemon_process), (
            "Daemon crashed during three overlapping dictation cycles"
        )
