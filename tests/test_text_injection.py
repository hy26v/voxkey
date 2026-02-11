# ABOUTME: Integration tests for text injection via RemoteDesktop portal keysym events.
# ABOUTME: Verifies characters are injected accurately using the mock portal keysym log.

import os
import time

import pytest

from helpers.mock_portal import keysyms_to_text


INJECTION_WAIT_SECONDS = 5
TRANSCRIPTION_WAIT_SECONDS = 10


class TestTextInjection:
    """Tests for text injection accuracy using the RemoteDesktop portal.

    These verify that transcribed text is typed correctly by checking
    the keysym press/release events logged by the mock portal.
    """

    def test_simple_ascii_injection(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir
    ):
        """Simple ASCII text like 'Hello, world!' should be typed accurately."""
        assert daemon_process.reached_idle
        wav_path = os.path.join(fixtures_dir, "hello_world.wav")

        portal_control.emit_activated()
        virtual_mic.stream_file(wav_path)
        time.sleep(3)
        portal_control.emit_deactivated()

        time.sleep(TRANSCRIPTION_WAIT_SECONDS + INJECTION_WAIT_SECONDS)

        captured = keysyms_to_text(portal_control.get_keysym_log())

        assert len(captured.strip()) > 0, "No text was injected"
        lower = captured.lower()
        assert "hello" in lower, f"Expected 'hello' in: {captured!r}"

    @pytest.mark.xfail(reason="whisper-cpp transcription accuracy varies by model/hardware")
    def test_no_characters_lost_during_injection(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir
    ):
        """All characters from a known phrase should appear without drops."""
        assert daemon_process.reached_idle
        wav_path = os.path.join(fixtures_dir, "the_quick_brown_fox.wav")

        portal_control.emit_activated()
        virtual_mic.stream_file(wav_path)
        time.sleep(3)
        portal_control.emit_deactivated()

        time.sleep(TRANSCRIPTION_WAIT_SECONDS + INJECTION_WAIT_SECONDS)

        captured = keysyms_to_text(portal_control.get_keysym_log())

        lower = captured.lower()
        for word in ["the", "quick", "brown", "fox"]:
            assert word in lower, (
                f"Missing word '{word}' in captured text: {captured!r}"
            )

    @pytest.mark.xfail(reason="whisper-cpp transcription accuracy varies by model/hardware")
    def test_serial_injection_no_interleaving(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir
    ):
        """Two sequential dictations should produce non-interleaved text."""
        assert daemon_process.reached_idle
        wav_a = os.path.join(fixtures_dir, "hello_world.wav")
        wav_b = os.path.join(fixtures_dir, "the_quick_brown_fox.wav")

        # First dictation
        portal_control.emit_activated()
        virtual_mic.stream_file(wav_a)
        time.sleep(3)
        portal_control.emit_deactivated()
        time.sleep(TRANSCRIPTION_WAIT_SECONDS + INJECTION_WAIT_SECONDS)

        # Second dictation
        portal_control.emit_activated()
        virtual_mic.stream_file(wav_b)
        time.sleep(3)
        portal_control.emit_deactivated()
        time.sleep(TRANSCRIPTION_WAIT_SECONDS + INJECTION_WAIT_SECONDS)

        captured = keysyms_to_text(portal_control.get_keysym_log())

        lower = captured.lower()
        hello_pos = lower.find("hello")
        fox_pos = lower.find("fox")
        assert hello_pos != -1, f"Missing 'hello' in: {captured!r}"
        assert fox_pos != -1, f"Missing 'fox' in: {captured!r}"
        assert hello_pos < fox_pos, (
            f"Text was interleaved: 'hello' at {hello_pos}, 'fox' at {fox_pos} "
            f"in: {captured!r}"
        )

    def test_punctuation_injection(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir
    ):
        """Punctuation characters (comma, period, exclamation) should be injected."""
        assert daemon_process.reached_idle
        wav_path = os.path.join(fixtures_dir, "punctuation.wav")

        portal_control.emit_activated()
        virtual_mic.stream_file(wav_path)
        time.sleep(3)
        portal_control.emit_deactivated()

        time.sleep(TRANSCRIPTION_WAIT_SECONDS + INJECTION_WAIT_SECONDS)

        captured = keysyms_to_text(portal_control.get_keysym_log())

        assert len(captured.strip()) > 0, "No text was injected for punctuation audio"
        has_punctuation = any(c in captured for c in ".,!?;:'\"")
        assert has_punctuation, (
            f"Expected punctuation in captured text, got: {captured!r}"
        )

    def test_spaces_preserved_between_words(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir
    ):
        """Spaces between words should be preserved in the injected text."""
        assert daemon_process.reached_idle
        wav_path = os.path.join(fixtures_dir, "hello_world.wav")

        portal_control.emit_activated()
        virtual_mic.stream_file(wav_path)
        time.sleep(3)
        portal_control.emit_deactivated()

        time.sleep(TRANSCRIPTION_WAIT_SECONDS + INJECTION_WAIT_SECONDS)

        captured = keysyms_to_text(portal_control.get_keysym_log())

        assert " " in captured.strip(), (
            f"Expected spaces between words, got: {captured!r}"
        )

    @pytest.mark.xfail(reason="whisper-cpp transcription accuracy varies by model/hardware")
    def test_long_text_injection(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir
    ):
        """Longer text should be injected fully without truncation."""
        assert daemon_process.reached_idle
        wav_path = os.path.join(fixtures_dir, "long_passage.wav")

        portal_control.emit_activated()
        virtual_mic.stream_file(wav_path)
        time.sleep(5)
        portal_control.emit_deactivated()

        time.sleep(TRANSCRIPTION_WAIT_SECONDS + INJECTION_WAIT_SECONDS)

        captured = keysyms_to_text(portal_control.get_keysym_log())

        assert len(captured.strip()) > 50, (
            f"Expected long text (>50 chars), got {len(captured.strip())} chars: "
            f"{captured!r}"
        )
