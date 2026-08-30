# ABOUTME: Verifies silence-segmented previews keep committed text and never blank the live transcript.
# ABOUTME: Feeds two utterances split by a long pause through an energy-aware mock backend.

import asyncio
import json
import os
import sys
import wave

import pytest

from helpers.dbus_portal import safe_introspect


DAEMON_BUS_NAME = "io.github.hy26v.Voxkey.Daemon"
DAEMON_OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
DAEMON_INTERFACE = "io.github.hy26v.Voxkey.Daemon1"

PAUSE_SECONDS = 2.0


@pytest.fixture
def daemon_config(isolated_voxkey_home):
    """These behaviors are specific to the segmented preview strategy."""
    config_path = isolated_voxkey_home / "voxkey" / "config.toml"
    config_path.write_text('[preview]\nstrategy = "segmented"\n')
    yield

# Deterministic stand-in for whisper.cpp. `hello.wav` contains three voiced
# 100ms windows and `hello_world.wav` contains six, so the count identifies the
# first fixture, the second fixture, or a snapshot containing both. Unlike a
# midpoint heuristic, the result is invariant to seek position and silence pad.
BACKEND_SCRIPT = """
import sys, wave, struct, math

path = sys.argv[1]
reader = wave.open(path)
channels = reader.getnchannels()
rate = reader.getframerate()
# Product snapshots intentionally end in one second of punctuation-capture
# silence. Exclude that synthetic tail: this backend is meant to classify the
# segmentation ranges, not the padding policy.
frames = max(0, reader.getnframes() - rate)
data = reader.readframes(frames)
reader.close()
samples = struct.unpack("<%dh" % (frames * channels), data)

def rms(chunk):
    if not chunk:
        return 0.0
    total = sum((sample / 32768.0) ** 2 for sample in chunk)
    return math.sqrt(total / len(chunk))

window = int(rate * 0.1) * channels
voiced_windows = 0
offset = 0
while offset + window <= len(samples):
    if rms(samples[offset:offset + window]) >= 0.02:
        voiced_windows += 1
    offset += window

if voiced_windows >= 8:
    print("alpha beta")
elif voiced_windows >= 5:
    print("beta")
elif voiced_windows:
    print("alpha")
"""


async def _wait_until(get_value, predicate, timeout=10):
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        value = await get_value()
        if predicate(value):
            return value
        await asyncio.sleep(0.1)
    return await get_value()


def _build_pause_fixture(fixtures_dir, tmp_path):
    """Concatenate two compatible utterances with a long silent gap between them."""
    first_path = os.path.join(fixtures_dir, "hello.wav")
    second_path = os.path.join(fixtures_dir, "hello_world.wav")
    with wave.open(first_path) as first:
        params = first.getparams()
        first_frames = first.readframes(first.getnframes())
    with wave.open(second_path) as second:
        assert (
            second.getnchannels() == params.nchannels
            and second.getframerate() == params.framerate
            and second.getsampwidth() == params.sampwidth
        ), "pause fixture sources must share a PCM format"
        second_frames = second.readframes(second.getnframes())

    silence = b"\x00" * int(params.framerate * PAUSE_SECONDS) * params.sampwidth * params.nchannels
    fixture_path = tmp_path / "pause_fixture.wav"
    with wave.open(str(fixture_path), "wb") as out:
        out.setparams(params)
        out.writeframes(first_frames + silence + second_frames)
    return str(fixture_path)


@pytest.mark.asyncio
async def test_paused_utterances_accumulate_without_blank_previews(
    daemon_process,
    dbus_session,
    portal_control,
    virtual_mic,
    fixtures_dir,
    tmp_path,
):
    assert daemon_process.reached_idle, "Daemon did not reach Idle"

    introspection = await safe_introspect(
        dbus_session,
        DAEMON_BUS_NAME,
        DAEMON_OBJECT_PATH,
    )
    daemon = dbus_session.get_proxy_object(
        DAEMON_BUS_NAME,
        DAEMON_OBJECT_PATH,
        introspection,
    ).get_interface(DAEMON_INTERFACE)

    transcriber = json.loads(await daemon.get_transcriber_config())
    transcriber["provider"] = "whisper-cpp"
    transcriber["whisper_cpp"] = {
        "command": sys.executable,
        "args": ["-c", BACKEND_SCRIPT, "{audio_file}"],
    }
    await daemon.call_set_transcriber_config(json.dumps(transcriber))

    # The configuration write requests a daemon session rebuild. Require a stable
    # connected/idle observation so activation targets the rebuilt session.
    await asyncio.sleep(0.5)

    async def session_ready():
        return await daemon.get_portal_connected(), await daemon.get_state()

    assert await _wait_until(
        session_ready, lambda value: value == (True, "Idle"),
    ) == (True, "Idle")
    await asyncio.sleep(0.3)

    fixture_path = _build_pause_fixture(fixtures_dir, tmp_path)

    portal_control.emit_activated()
    assert await _wait_until(
        daemon.get_state, lambda value: value == "Recording",
    ) == "Recording"

    try:
        virtual_mic.stream_file(fixture_path)

        # The first utterance must surface, then the second must join it. Once
        # "alpha" is visible the transcript is never allowed to go blank again:
        # a blank here is the exact regression segmentation is meant to fix.
        seen_alpha = False
        saw_combined = False
        deadline = asyncio.get_running_loop().time() + 30
        while asyncio.get_running_loop().time() < deadline:
            live = await daemon.get_live_transcript()
            if seen_alpha:
                assert live != "", "live transcript went empty after 'alpha'"
                if live == "alpha beta":
                    saw_combined = True
                    break
            elif live == "alpha":
                seen_alpha = True
            await asyncio.sleep(0.1)

        assert seen_alpha, "live transcript never showed the first utterance"
        assert saw_combined, "live transcript never combined both utterances"

        # Let the fixture finish playing so the tail holds the whole recording
        # before the final transcription is assembled.
        await asyncio.to_thread(virtual_mic.wait_for_playback, 15)
    finally:
        virtual_mic.stop_playback()

    # The signal is emitted when the transcript is published, before injection
    # runs, and survives the session loss that follows in this EIS-less mock.
    received = asyncio.get_running_loop().create_future()

    def on_transcription_complete(text):
        if not received.done():
            received.set_result(text)

    daemon.on_transcription_complete(on_transcription_complete)

    # Toggle the shortcut off to stop the recording.
    portal_control.emit_deactivated()
    await asyncio.sleep(0.1)
    portal_control.emit_activated()

    final = await asyncio.wait_for(received, timeout=20)
    assert final == "alpha beta"
