# ABOUTME: Integration tests for the full dictation cycle from shortcut to text output.
# ABOUTME: Verifies hold-shortcut, record, transcribe, and inject flow via mock portal.

import os
import select
import time

import pytest

from helpers.mock_portal import keysyms_to_text


SHORTCUT_HOLD_SECONDS = 3
TRANSCRIPTION_WAIT_SECONDS = 10
INJECTION_WAIT_SECONDS = 5


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


def _assert_daemon_ready(daemon_process):
    """Assert the daemon reached Idle."""
    assert daemon_process.reached_idle, (
        "Daemon did not reach Idle — mock portal setup incomplete"
    )


class TestDictationCycle:
    """Full end-to-end dictation cycle tests.

    Each test triggers the shortcut via mock portal signals,
    streams audio through the virtual mic, and checks keysym log output.
    """

    def test_single_dictation_cycle(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir
    ):
        """Hold shortcut, speak, release, and verify text is injected."""
        _assert_daemon_ready(daemon_process)
        wav_path = os.path.join(fixtures_dir, "hello_world.wav")

        # Hold the shortcut to start recording
        portal_control.emit_activated()

        # Stream pre-recorded audio while shortcut is held
        virtual_mic.stream_file(wav_path)
        time.sleep(SHORTCUT_HOLD_SECONDS)

        # Release shortcut to stop recording and trigger transcription
        portal_control.emit_deactivated()

        # Wait for transcription and injection to complete
        time.sleep(TRANSCRIPTION_WAIT_SECONDS + INJECTION_WAIT_SECONDS)

        captured = keysyms_to_text(portal_control.get_keysym_log())

        assert len(captured.strip()) > 0, "No text was injected after dictation cycle"
        assert "hello" in captured.lower(), (
            f"Expected 'hello' in captured text, got: {captured!r}"
        )

    @pytest.mark.xfail(reason="whisper-cpp transcription accuracy varies by model/hardware")
    def test_dictation_produces_correct_transcript(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir
    ):
        """The injected text should match the known audio content."""
        _assert_daemon_ready(daemon_process)
        wav_path = os.path.join(fixtures_dir, "the_quick_brown_fox.wav")

        portal_control.emit_activated()
        virtual_mic.stream_file(wav_path)
        time.sleep(SHORTCUT_HOLD_SECONDS)
        portal_control.emit_deactivated()

        time.sleep(TRANSCRIPTION_WAIT_SECONDS + INJECTION_WAIT_SECONDS)

        captured = keysyms_to_text(portal_control.get_keysym_log())

        assert "quick" in captured.lower(), (
            f"Expected 'quick' in captured text, got: {captured!r}"
        )
        assert "fox" in captured.lower(), (
            f"Expected 'fox' in captured text, got: {captured!r}"
        )

    def test_shortcut_press_starts_recording(
        self, daemon_process, portal_control
    ):
        """Pressing the shortcut should transition daemon to recording state."""
        _assert_daemon_ready(daemon_process)
        portal_control.emit_activated()
        time.sleep(1)

        lines = _collect_stderr(daemon_process, timeout=1.0)
        state_lines = [l for l in lines if "STATE:" in l]

        assert daemon_process.poll() is None, "Daemon crashed on shortcut press"

        portal_control.emit_deactivated()

        assert state_lines, "No state change after shortcut press"

    def test_shortcut_release_stops_recording(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir
    ):
        """Releasing the shortcut should stop recording and begin transcription."""
        _assert_daemon_ready(daemon_process)
        wav_path = os.path.join(fixtures_dir, "hello_world.wav")

        portal_control.emit_activated()
        virtual_mic.stream_file(wav_path)
        time.sleep(SHORTCUT_HOLD_SECONDS)

        portal_control.emit_deactivated()
        time.sleep(1)

        # Daemon should still be running after release
        assert daemon_process.poll() is None, (
            "Daemon crashed after shortcut release"
        )

    def test_two_consecutive_dictation_cycles(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir
    ):
        """Two back-to-back dictation cycles should both produce text."""
        _assert_daemon_ready(daemon_process)
        wav_path = os.path.join(fixtures_dir, "hello_world.wav")

        # First cycle
        portal_control.emit_activated()
        virtual_mic.stream_file(wav_path)
        time.sleep(SHORTCUT_HOLD_SECONDS)
        portal_control.emit_deactivated()
        time.sleep(TRANSCRIPTION_WAIT_SECONDS + INJECTION_WAIT_SECONDS)

        # Second cycle
        portal_control.emit_activated()
        virtual_mic.stream_file(wav_path)
        time.sleep(SHORTCUT_HOLD_SECONDS)
        portal_control.emit_deactivated()
        time.sleep(TRANSCRIPTION_WAIT_SECONDS + INJECTION_WAIT_SECONDS)

        captured = keysyms_to_text(portal_control.get_keysym_log())

        # Both cycles should have injected text
        occurrences = captured.lower().count("hello")
        assert occurrences >= 2, (
            f"Expected 'hello' at least twice, found {occurrences} times "
            f"in: {captured!r}"
        )

    def test_rapid_repeated_dictation_cycles(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir
    ):
        """Multiple rapid dictation cycles should not interleave or drop text."""
        _assert_daemon_ready(daemon_process)
        wav_path = os.path.join(fixtures_dir, "hello_world.wav")

        # First cycle
        portal_control.emit_activated()
        virtual_mic.stream_file(wav_path)
        time.sleep(2)
        portal_control.emit_deactivated()
        time.sleep(TRANSCRIPTION_WAIT_SECONDS)

        for _ in range(2):
            portal_control.emit_activated()
            virtual_mic.stream_file(wav_path)
            time.sleep(2)
            portal_control.emit_deactivated()
            time.sleep(TRANSCRIPTION_WAIT_SECONDS)

        # Give final injection time to finish
        time.sleep(INJECTION_WAIT_SECONDS)

        captured = keysyms_to_text(portal_control.get_keysym_log())

        occurrences = captured.lower().count("hello")
        assert occurrences >= 3, (
            f"Expected 'hello' at least 3 times, found {occurrences} times "
            f"in: {captured!r}"
        )

    def test_daemon_stays_alive_through_full_cycle(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir
    ):
        """The daemon process should remain running throughout a full cycle."""
        _assert_daemon_ready(daemon_process)
        wav_path = os.path.join(fixtures_dir, "hello_world.wav")

        assert daemon_process.poll() is None, "Daemon not running at start"

        portal_control.emit_activated()
        virtual_mic.stream_file(wav_path)
        time.sleep(SHORTCUT_HOLD_SECONDS)
        assert daemon_process.poll() is None, "Daemon died during shortcut hold"

        portal_control.emit_deactivated()
        time.sleep(TRANSCRIPTION_WAIT_SECONDS + INJECTION_WAIT_SECONDS)
        assert daemon_process.poll() is None, "Daemon died after shortcut release"

    @pytest.mark.xfail(reason="whisper-cpp hallucinates short words on silence")
    def test_empty_audio_produces_no_injection(
        self, daemon_process, portal_control, fixtures_dir
    ):
        """Holding the shortcut with silence should produce no injected text."""
        _assert_daemon_ready(daemon_process)

        # Hold and release without streaming audio
        portal_control.emit_activated()
        time.sleep(1)
        portal_control.emit_deactivated()

        time.sleep(TRANSCRIPTION_WAIT_SECONDS)

        captured = keysyms_to_text(portal_control.get_keysym_log())

        assert captured.strip() == "", (
            f"Expected no text from empty audio, got: {captured!r}"
        )
