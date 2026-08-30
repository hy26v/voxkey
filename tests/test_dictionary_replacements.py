# ABOUTME: Verifies dictionary rules only fire on whole words in non-ASCII text.
# ABOUTME: Drives the real daemon over D-Bus and reads the corrected preview back.

import asyncio
import json
import os
import sys

import pytest

from helpers.dbus_portal import safe_introspect


DAEMON_BUS_NAME = "io.github.hy26v.Voxkey.Daemon"
DAEMON_OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
DAEMON_INTERFACE = "io.github.hy26v.Voxkey.Daemon1"

# What the stub backend "transcribes". "se" appears twice: once inside the
# Spanish word "señor", once as a standalone word. Only the standalone one is
# a legitimate target for a "se" rule.
STUB_TRANSCRIPT = "el señor se fue"
EXPECTED_PREVIEW = "el señor SE fue"


async def _wait_until(get_value, predicate, timeout=10):
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        value = await get_value()
        if predicate(value):
            return value
        await asyncio.sleep(0.1)
    return await get_value()


async def _configure(daemon, stub_output, replacements):
    """Point the daemon at a stub backend and a dictionary, then wait for the
    rebuilt session to settle."""
    transcriber = json.loads(await daemon.get_transcriber_config())
    transcriber["provider"] = "whisper-cpp"
    transcriber["whisper_cpp"] = {
        "command": sys.executable,
        "args": [
            "-c",
            "import sys; sys.stdout.reconfigure(encoding='utf-8'); "
            f"print({stub_output!r})",
            "{audio_file}",
        ],
    }

    await daemon.call_set_dictionary_config(
        json.dumps({"replacements": replacements, "vocabulary": []})
    )
    await daemon.call_set_transcriber_config(json.dumps(transcriber))

    # Both configuration writes request a daemon session rebuild. Require a
    # stable connected/idle observation so activation targets the rebuilt one.
    await asyncio.sleep(0.5)

    async def session_ready():
        return await daemon.get_portal_connected(), await daemon.get_state()

    ready = await _wait_until(session_ready, lambda value: value == (True, "Idle"))
    assert ready == (True, "Idle")
    await asyncio.sleep(0.3)


@pytest.mark.asyncio
async def test_a_large_dictionary_is_applied_to_previews(
    daemon_process,
    dbus_session,
    portal_control,
    virtual_mic,
    fixtures_dir,
):
    """Every rule in a realistically large dictionary must still be applied.

    Dictionaries grow, and the corrections run again on each preview refresh
    during a recording, so this covers both that no rule is dropped and that
    a big dictionary still produces a preview promptly.
    """
    assert daemon_process.reached_idle, "Daemon did not reach Idle"

    introspection = await safe_introspect(
        dbus_session, DAEMON_BUS_NAME, DAEMON_OBJECT_PATH,
    )
    daemon = dbus_session.get_proxy_object(
        DAEMON_BUS_NAME, DAEMON_OBJECT_PATH, introspection,
    ).get_interface(DAEMON_INTERFACE)

    replacements = [
        {"original": f"term{index}", "replacement": f"Term{index}", "enabled": True}
        for index in range(60)
    ]
    spoken = "term0 and term37 and term59 done"
    expected = "Term0 and Term37 and Term59 done"

    await _configure(daemon, spoken, replacements)

    portal_control.emit_activated()
    assert await _wait_until(
        daemon.get_state, lambda value: value == "Recording",
    ) == "Recording"

    try:
        virtual_mic.stream_file(os.path.join(fixtures_dir, "long_passage.wav"))
        preview = await _wait_until(
            daemon.get_live_transcript, lambda value: value == expected, timeout=12,
        )
        assert preview == expected, (
            f"a 60-rule dictionary did not correct every term: {preview!r}"
        )
    finally:
        virtual_mic.stop_playback()


@pytest.mark.asyncio
async def test_a_longer_phrase_wins_over_a_rule_listing_short_alternatives(
    daemon_process,
    dbus_session,
    portal_control,
    virtual_mic,
    fixtures_dir,
):
    """Rule precedence follows the longest phrase a rule can match.

    A rule listing several short alternatives must not take precedence over a
    rule holding one longer phrase just because its list of alternatives is
    longer as text.
    """
    assert daemon_process.reached_idle, "Daemon did not reach Idle"

    introspection = await safe_introspect(
        dbus_session, DAEMON_BUS_NAME, DAEMON_OBJECT_PATH,
    )
    daemon = dbus_session.get_proxy_object(
        DAEMON_BUS_NAME, DAEMON_OBJECT_PATH, introspection,
    ).get_interface(DAEMON_INTERFACE)

    replacements = [
        {"original": "vox, box", "replacement": "Voxkey", "enabled": True},
        {"original": "vox key", "replacement": "Voxkey Pro", "enabled": True},
    ]
    await _configure(daemon, "open vox key now", replacements)

    portal_control.emit_activated()
    assert await _wait_until(
        daemon.get_state, lambda value: value == "Recording",
    ) == "Recording"

    try:
        virtual_mic.stream_file(os.path.join(fixtures_dir, "long_passage.wav"))
        preview = await _wait_until(
            daemon.get_live_transcript, lambda value: value != "", timeout=12,
        )
        assert preview == "open Voxkey Pro now", (
            f"the shorter rule took precedence over the longer phrase: {preview!r}"
        )
    finally:
        virtual_mic.stop_playback()


@pytest.mark.asyncio
async def test_replacement_does_not_fire_inside_an_accented_word(
    daemon_process,
    dbus_session,
    portal_control,
    virtual_mic,
    fixtures_dir,
):
    assert daemon_process.reached_idle, "Daemon did not reach Idle"

    introspection = await safe_introspect(
        dbus_session,
        DAEMON_BUS_NAME,
        DAEMON_OBJECT_PATH,
    )
    proxy = dbus_session.get_proxy_object(
        DAEMON_BUS_NAME,
        DAEMON_OBJECT_PATH,
        introspection,
    )
    daemon = proxy.get_interface(DAEMON_INTERFACE)

    transcriber = json.loads(await daemon.get_transcriber_config())
    transcriber["provider"] = "whisper-cpp"
    transcriber["whisper_cpp"] = {
        "command": sys.executable,
        "args": [
            "-c",
            "import sys; sys.stdout.reconfigure(encoding='utf-8'); "
            f"print({STUB_TRANSCRIPT!r})",
            "{audio_file}",
        ],
    }
    dictionary = {
        "replacements": [
            {"original": "se", "replacement": "SE", "enabled": True},
        ],
        "vocabulary": [],
    }

    await daemon.call_set_dictionary_config(json.dumps(dictionary))
    await daemon.call_set_transcriber_config(json.dumps(transcriber))

    # Both configuration writes request a daemon session rebuild. Require a
    # stable connected/idle observation so activation targets the rebuilt one.
    await asyncio.sleep(0.5)

    async def session_ready():
        return await daemon.get_portal_connected(), await daemon.get_state()

    ready = await _wait_until(session_ready, lambda value: value == (True, "Idle"))
    assert ready == (True, "Idle")
    await asyncio.sleep(0.3)

    portal_control.emit_activated()
    state = await _wait_until(daemon.get_state, lambda value: value == "Recording")
    assert state == "Recording"

    try:
        virtual_mic.stream_file(os.path.join(fixtures_dir, "long_passage.wav"))
        preview = await _wait_until(
            daemon.get_live_transcript,
            lambda value: value != "",
            timeout=12,
        )
        assert preview == EXPECTED_PREVIEW, (
            f"dictionary rule 'se' corrupted a non-ASCII word: {preview!r}"
        )
    finally:
        virtual_mic.stop_playback()
