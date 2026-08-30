# ABOUTME: Drives the realtime streaming backend against a mock WebSocket server.
# ABOUTME: Covers delta accumulation, dictionary correction, and holdback of unfinished phrases.

import asyncio
import json
import os
from pathlib import Path

import pytest

from helpers.dbus_portal import safe_introspect
from helpers.mock_realtime_server import MockRealtimeServer


DAEMON_BUS_NAME = "io.github.hy26v.Voxkey.Daemon"
DAEMON_OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
DAEMON_INTERFACE = "io.github.hy26v.Voxkey.Daemon1"


async def _wait_until(get_value, predicate, timeout=10):
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        value = await get_value()
        if predicate(value):
            return value
        await asyncio.sleep(0.1)
    return await get_value()


async def _daemon_interface(dbus_session):
    introspection = await safe_introspect(
        dbus_session, DAEMON_BUS_NAME, DAEMON_OBJECT_PATH,
    )
    return dbus_session.get_proxy_object(
        DAEMON_BUS_NAME, DAEMON_OBJECT_PATH, introspection,
    ).get_interface(DAEMON_INTERFACE)


async def _install_realtime_test_key(daemon):
    """Load a fake key from the isolated config without touching a real keyring."""
    config_path = Path(os.environ["XDG_CONFIG_HOME"]) / "voxkey" / "config.toml"
    config_path.write_text(
        '''[transcriber.mistral_realtime]
api_key = "integration-test-key"
'''
    )
    os.chmod(config_path, 0o600)
    await daemon.call_reload_config()
    await asyncio.sleep(0.5)


@pytest.mark.asyncio
async def test_streaming_holds_back_a_phrase_a_rule_will_rewrite(
    daemon_process, dbus_session, portal_control, virtual_mic, fixtures_dir,
):
    """A rule spanning several words must not be typed one word at a time.

    The daemon may only inject text once no dictionary rule can still grow
    into it. Here "vox key" arrives as two deltas and a rule rewrites the
    pair, so nothing may be injected until the phrase is complete -- otherwise
    the user watches "vox" get typed and the saved transcript says "Voxkey".

    The isolated EIS peer acknowledges key events but has no focused desktop
    surface. What is observable here is that the daemon accumulated and
    corrected the phrase without attempting a doomed early injection.
    """
    assert daemon_process.reached_idle, "Daemon did not reach Idle"

    server = MockRealtimeServer(deltas=["vox", " key"])
    try:
        daemon = await _daemon_interface(dbus_session)
        await _install_realtime_test_key(daemon)

        transcriber = json.loads(await daemon.get_transcriber_config())
        transcriber["provider"] = "mistral-realtime"
        transcriber["mistral_realtime"] = {
            "api_key": "",
            "model": "voxtral-mini-transcribe-realtime-2602",
            "endpoint": server.url,
        }
        dictionary = {
            "replacements": [
                {"original": "vox key", "replacement": "Voxkey", "enabled": True},
            ],
            "vocabulary": [],
        }

        await daemon.call_set_dictionary_config(json.dumps(dictionary))
        await daemon.call_set_transcriber_config(json.dumps(transcriber))
        await asyncio.sleep(0.5)

        async def session_ready():
            return await daemon.get_portal_connected(), await daemon.get_state()

        assert await _wait_until(
            session_ready, lambda value: value == (True, "Idle"),
        ) == (True, "Idle")
        await asyncio.sleep(0.3)

        portal_control.emit_activated()
        assert await _wait_until(
            daemon.get_state, lambda value: value == "Streaming",
        ) == "Streaming", "daemon did not enter the streaming flow"

        try:
            virtual_mic.stream_file(os.path.join(fixtures_dir, "long_passage.wav"))

            transcript = await _wait_until(
                daemon.get_live_transcript,
                lambda value: value != "",
                timeout=15,
            )
            assert transcript == "Voxkey", (
                f"streaming transcript was not corrected by the dictionary: {transcript!r}"
            )
            assert await daemon.get_state() == "Streaming", (
                "the streaming session ended early, which happens when the daemon "
                "injects a word a dictionary rule was about to rewrite"
            )
        finally:
            virtual_mic.stop_playback()

        assert server.received_audio_chunks > 0, "no audio reached the streaming endpoint"
        assert server.session_updates, "daemon never announced its audio format"
        assert server.session_updates[0]["session"]["audio_format"] == {
            "encoding": "pcm_s16le",
            "sample_rate": 16000,
        }
    finally:
        server.close()


@pytest.mark.asyncio
async def test_failed_stream_still_records_the_partial_transcript(
    daemon_process, dbus_session, portal_control, virtual_mic, fixtures_dir,
):
    assert daemon_process.reached_idle, "Daemon did not reach Idle"

    server = MockRealtimeServer(
        deltas=["hello ", "there "],
        fail_after_deltas="upstream connection reset",
    )
    try:
        daemon = await _daemon_interface(dbus_session)
        await _install_realtime_test_key(daemon)

        transcriber = json.loads(await daemon.get_transcriber_config())
        transcriber["provider"] = "mistral-realtime"
        transcriber["mistral_realtime"] = {
            "api_key": "",
            "model": "voxtral-mini-transcribe-realtime-2602",
            "endpoint": server.url,
        }
        dictionary = {
            "replacements": [
                {
                    "original": "hello there friend",
                    "replacement": "greetings",
                    "enabled": True,
                },
            ],
            "vocabulary": [],
        }
        await daemon.call_set_dictionary_config(json.dumps(dictionary))
        await daemon.call_set_transcriber_config(json.dumps(transcriber))
        await asyncio.sleep(0.5)

        async def session_ready():
            return await daemon.get_portal_connected(), await daemon.get_state()

        assert await _wait_until(
            session_ready, lambda value: value == (True, "Idle"),
        ) == (True, "Idle")
        await asyncio.sleep(0.3)

        portal_control.emit_activated()
        assert await _wait_until(
            daemon.get_state, lambda value: value == "Streaming",
        ) == "Streaming"

        try:
            virtual_mic.stream_file(os.path.join(fixtures_dir, "long_passage.wav"))

            async def provider_failure_delivered():
                return server.sent_failure

            assert await _wait_until(
                provider_failure_delivered, bool, timeout=15,
            ), "mock provider never delivered its failure"
            assert server.sent_deltas == 2
            recorded = await _wait_until(
                daemon.get_last_transcript,
                lambda value: "hello there" in value,
                timeout=3,
            )
        finally:
            virtual_mic.stop_playback()

        assert "hello there" in recorded
        history = json.loads(await daemon.get_transcription_history())
        saved = next(entry for entry in history if "hello there" in entry["text"])
        assert saved["outcome"] == "partial_provider_error"
        assert saved["pending_insertion"].strip() == "hello there"
        assert await _wait_until(
            daemon.get_state, lambda value: value == "Idle",
        ) == "Idle"
        assert daemon_process.poll() is None
    finally:
        server.close()


@pytest.mark.asyncio
async def test_broken_websocket_still_records_the_partial_transcript(
    daemon_process, dbus_session, portal_control, virtual_mic, fixtures_dir,
):
    assert daemon_process.reached_idle, "Daemon did not reach Idle"

    server = MockRealtimeServer(
        deltas=["transport ", "survived "],
        disconnect_after_deltas=True,
    )
    try:
        daemon = await _daemon_interface(dbus_session)
        await _install_realtime_test_key(daemon)

        transcriber = json.loads(await daemon.get_transcriber_config())
        transcriber["provider"] = "mistral-realtime"
        transcriber["mistral_realtime"] = {
            "api_key": "",
            "model": "voxtral-mini-transcribe-realtime-2602",
            "endpoint": server.url,
        }
        dictionary = {
            "replacements": [
                {
                    "original": "transport survived intact",
                    "replacement": "kept",
                    "enabled": True,
                },
            ],
            "vocabulary": [],
        }
        await daemon.call_set_dictionary_config(json.dumps(dictionary))
        await daemon.call_set_transcriber_config(json.dumps(transcriber))
        await asyncio.sleep(0.5)

        async def session_ready():
            return await daemon.get_portal_connected(), await daemon.get_state()

        assert await _wait_until(
            session_ready, lambda value: value == (True, "Idle"),
        ) == (True, "Idle")
        await asyncio.sleep(0.3)

        portal_control.emit_activated()
        assert await _wait_until(
            daemon.get_state, lambda value: value == "Streaming",
        ) == "Streaming"

        try:
            virtual_mic.stream_file(os.path.join(fixtures_dir, "long_passage.wav"))

            async def transport_disconnected():
                return server.disconnected

            assert await _wait_until(
                transport_disconnected, bool, timeout=15,
            ), "mock provider never broke the WebSocket transport"
            assert server.sent_deltas == 2
            recorded = await _wait_until(
                daemon.get_last_transcript,
                lambda value: "transport survived" in value,
                timeout=3,
            )
        finally:
            virtual_mic.stop_playback()

        assert "transport survived" in recorded
        history = json.loads(await daemon.get_transcription_history())
        saved = next(entry for entry in history if "transport survived" in entry["text"])
        assert saved["outcome"] == "partial_transport_close"
        assert saved["pending_insertion"].strip() == "transport survived"
        assert await _wait_until(
            daemon.get_state, lambda value: value == "Idle",
        ) == "Idle"
        assert daemon_process.poll() is None
    finally:
        server.close()


@pytest.mark.asyncio
async def test_graceful_close_before_stop_is_reported_as_transport_failure(
    daemon_process, dbus_session, portal_control, virtual_mic, fixtures_dir,
):
    assert daemon_process.reached_idle, "Daemon did not reach Idle"

    server = MockRealtimeServer(
        deltas=[],
        graceful_close_after_deltas=True,
    )
    try:
        daemon = await _daemon_interface(dbus_session)
        await _install_realtime_test_key(daemon)

        transcriber = json.loads(await daemon.get_transcriber_config())
        transcriber["provider"] = "mistral-realtime"
        transcriber["mistral_realtime"] = {
            "api_key": "",
            "model": "voxtral-mini-transcribe-realtime-2602",
            "endpoint": server.url,
        }
        await daemon.call_set_transcriber_config(json.dumps(transcriber))
        await asyncio.sleep(0.5)

        async def session_ready():
            return await daemon.get_portal_connected(), await daemon.get_state()

        assert await _wait_until(
            session_ready, lambda value: value == (True, "Idle"),
        ) == (True, "Idle")

        portal_control.emit_activated()

        try:
            virtual_mic.stream_file(os.path.join(fixtures_dir, "long_passage.wav"))
            assert await _wait_until(
                lambda: asyncio.sleep(0, result=server.disconnected),
                bool,
                timeout=15,
            ), "mock provider never closed the WebSocket"
        finally:
            virtual_mic.stop_playback()

        error = await _wait_until(
            daemon.get_last_error,
            lambda value: "closed" in value.lower() or "ended" in value.lower(),
            timeout=3,
        )
        assert error, "premature graceful close was silently reported as success"
        assert await _wait_until(
            daemon.get_state, lambda value: value == "Idle",
        ) == "Idle"
        assert daemon_process.poll() is None
    finally:
        server.close()


@pytest.mark.asyncio
async def test_graceful_close_during_drain_preserves_partial_status(
    daemon_process, dbus_session, portal_control, virtual_mic, fixtures_dir,
):
    assert daemon_process.reached_idle, "Daemon did not reach Idle"

    server = MockRealtimeServer(
        deltas=["during drain "],
        graceful_close_on_audio_end=True,
    )
    try:
        daemon = await _daemon_interface(dbus_session)
        await _install_realtime_test_key(daemon)
        transcriber = json.loads(await daemon.get_transcriber_config())
        transcriber["provider"] = "mistral-realtime"
        transcriber["mistral_realtime"] = {
            "api_key": "",
            "model": "voxtral-mini-transcribe-realtime-2602",
            "endpoint": server.url,
        }
        dictionary = {
            "replacements": [
                {
                    "original": "during drain later",
                    "replacement": "unused",
                    "enabled": True,
                },
            ],
            "vocabulary": [],
        }
        await daemon.call_set_dictionary_config(json.dumps(dictionary))
        await daemon.call_set_transcriber_config(json.dumps(transcriber))
        await asyncio.sleep(0.5)

        async def session_ready():
            return await daemon.get_portal_connected(), await daemon.get_state()

        assert await _wait_until(
            session_ready, lambda value: value == (True, "Idle"),
        ) == (True, "Idle")

        portal_control.emit_activated()
        portal_control.emit_deactivated()
        assert await _wait_until(
            daemon.get_state, lambda value: value == "Streaming",
        ) == "Streaming"
        try:
            virtual_mic.stream_file(os.path.join(fixtures_dir, "long_passage.wav"))
            assert await _wait_until(
                lambda: asyncio.sleep(0, result=server.sent_deltas),
                lambda value: value == 1,
                timeout=15,
            ) == 1
            portal_control.emit_activated()
            portal_control.emit_deactivated()
            assert await _wait_until(
                lambda: asyncio.sleep(0, result=server.disconnected),
                bool,
                timeout=5,
            )
        finally:
            virtual_mic.stop_playback()

        history = json.loads(await _wait_until(
            daemon.get_transcription_history,
            lambda value: "during drain" in value,
            timeout=5,
        ))
        saved = next(entry for entry in history if "during drain" in entry["text"])
        assert saved["outcome"] == "partial_transport_close"
        assert saved["pending_insertion"].strip() == "during drain"
        assert await _wait_until(
            daemon.get_state, lambda value: value == "Idle",
        ) == "Idle"
        assert daemon_process.poll() is None
    finally:
        server.close()
