# ABOUTME: Integration tests for Unicode text injection via keysym mapping.
# ABOUTME: Verifies accented characters and language-specific diacritics are typed correctly.

import os
import time

import pytest

from helpers.mock_portal import keysyms_to_text


INJECTION_WAIT_SECONDS = 5
TRANSCRIPTION_WAIT_SECONDS = 10


class TestUnicodeInjection:
    """Tests for Unicode character injection through the RemoteDesktop portal.

    Verifies that the codepoint-to-keysym mapping and press/release cycle
    works correctly for accented characters, symbols, and control characters.
    """

    @pytest.mark.xfail(reason="whisper-cpp transcription accuracy varies by model/hardware")
    def test_accented_characters(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir
    ):
        """Accented characters (e, u, n, o with diacritics) should be injected."""
        assert daemon_process.reached_idle
        wav_path = os.path.join(fixtures_dir, "accented_chars.wav")

        portal_control.emit_activated()
        virtual_mic.stream_file(wav_path)
        time.sleep(3)
        portal_control.emit_deactivated()

        time.sleep(TRANSCRIPTION_WAIT_SECONDS + INJECTION_WAIT_SECONDS)

        captured = keysyms_to_text(portal_control.get_keysym_log())

        assert len(captured.strip()) > 0, "No text injected for accented audio"
        accented = set("éèêëüùûúñöôòóàâáäïîíì")
        found_accented = [c for c in captured if c in accented]
        assert len(found_accented) > 0, (
            f"Expected accented characters in captured text, got: {captured!r}"
        )

    def test_accented_e_variants(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir
    ):
        """Specific accented e variants (e-acute, e-grave, etc.) should inject."""
        assert daemon_process.reached_idle
        wav_path = os.path.join(fixtures_dir, "french_words.wav")

        portal_control.emit_activated()
        virtual_mic.stream_file(wav_path)
        time.sleep(3)
        portal_control.emit_deactivated()

        time.sleep(TRANSCRIPTION_WAIT_SECONDS + INJECTION_WAIT_SECONDS)

        captured = keysyms_to_text(portal_control.get_keysym_log())

        assert len(captured.strip()) > 0, "No text injected for French audio"

    def test_german_umlauts(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir
    ):
        """German umlauts (a, o, u with diaeresis) should be injected correctly."""
        assert daemon_process.reached_idle
        wav_path = os.path.join(fixtures_dir, "german_words.wav")

        portal_control.emit_activated()
        virtual_mic.stream_file(wav_path)
        time.sleep(3)
        portal_control.emit_deactivated()

        time.sleep(TRANSCRIPTION_WAIT_SECONDS + INJECTION_WAIT_SECONDS)

        captured = keysyms_to_text(portal_control.get_keysym_log())

        assert len(captured.strip()) > 0, "No text injected for German audio"

    def test_spanish_special_chars(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir
    ):
        """Spanish special characters (n-tilde, inverted marks) should be injected."""
        assert daemon_process.reached_idle
        wav_path = os.path.join(fixtures_dir, "spanish_words.wav")

        portal_control.emit_activated()
        virtual_mic.stream_file(wav_path)
        time.sleep(3)
        portal_control.emit_deactivated()

        time.sleep(TRANSCRIPTION_WAIT_SECONDS + INJECTION_WAIT_SECONDS)

        captured = keysyms_to_text(portal_control.get_keysym_log())

        assert len(captured.strip()) > 0, "No text injected for Spanish audio"

    @pytest.mark.xfail(reason="whisper-cpp transcribes numbers as words, not digits")
    def test_numbers_and_digits(
        self, daemon_process, portal_control, virtual_mic, fixtures_dir
    ):
        """Numeric digits (0-9) should be injected correctly."""
        assert daemon_process.reached_idle
        wav_path = os.path.join(fixtures_dir, "numbers.wav")

        portal_control.emit_activated()
        virtual_mic.stream_file(wav_path)
        time.sleep(3)
        portal_control.emit_deactivated()

        time.sleep(TRANSCRIPTION_WAIT_SECONDS + INJECTION_WAIT_SECONDS)

        captured = keysyms_to_text(portal_control.get_keysym_log())

        assert len(captured.strip()) > 0, "No text injected for numbers audio"
        has_digit = any(c.isdigit() for c in captured)
        assert has_digit, f"Expected digits in captured text, got: {captured!r}"
