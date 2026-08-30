# ABOUTME: Proves whole previews confirm stable prefixes and stop decoding from frame zero.
# ABOUTME: Runs the daemon through private D-Bus and PipeWire with a WAV-aware local backend.

import asyncio
import json
import os
import sys
import wave
from pathlib import Path

import pytest

from helpers.dbus_portal import safe_introspect


BUS = "io.github.hy26v.Voxkey.Daemon"
PATH = "/io/github/hy26v/Voxkey/Daemon"
IFACE = "io.github.hy26v.Voxkey.Daemon1"
TRANSCRIPT = (
    "one two three four five. six seven eight nine ten. "
    "eleven twelve thirteen fourteen fifteen. "
    "sixteen seventeen eighteen nineteen twenty. "
    "twenty-one twenty-two twenty-three twenty-four twenty-five."
)


@pytest.fixture
def daemon_config(isolated_voxkey_home, tmp_path, monkeypatch):
    log = tmp_path / "agreement-decodes.jsonl"
    monkeypatch.setenv("VOXKEY_AGREEMENT_LOG", str(log))
    backend = Path(__file__).parent / "helpers/agreement_backend.py"
    config = isolated_voxkey_home / "voxkey/config.toml"
    config.write_text(
        "\n".join(
            [
                '[transcriber]',
                'provider = "whisper-cpp"',
                '',
                '[transcriber.whisper_cpp]',
                f'command = {json.dumps(sys.executable)}',
                f'args = [{json.dumps(str(backend))}, "{{audio_file}}"]',
                '',
                '[preview]',
                'mode = "always"',
                'strategy = "whole"',
                'interval_ms = 3000',
                'max_audio_seconds = 0',
            ]
        )
        + "\n"
    )
    yield log


async def wait_until(get_value, predicate, timeout=15):
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        value = await get_value()
        if predicate(value):
            return value
        await asyncio.sleep(0.1)
    return await get_value()


def repeated_fixture(source, destination, copies=3):
    with wave.open(source, "rb") as reader:
        params = reader.getparams()
        frames = reader.readframes(reader.getnframes())
    with wave.open(str(destination), "wb") as writer:
        writer.setparams(params)
        writer.writeframes(frames * copies)


def observations(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line]


@pytest.mark.asyncio
async def test_three_pass_agreement_seeks_to_the_unconfirmed_tail(
    daemon_process,
    daemon_config,
    dbus_session,
    portal_control,
    virtual_mic,
    fixtures_dir,
    tmp_path,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    introspection = await safe_introspect(dbus_session, BUS, PATH)
    daemon = dbus_session.get_proxy_object(BUS, PATH, introspection).get_interface(IFACE)
    fixture = tmp_path / "long-agreement.wav"
    repeated_fixture(os.path.join(fixtures_dir, "long_passage.wav"), fixture)

    portal_control.emit_activated()
    assert await wait_until(daemon.get_state, lambda state: state == "Recording") == "Recording"
    try:
        virtual_mic.stream_file(str(fixture))

        async def logged_decodes():
            return observations(daemon_config)

        decodes = await wait_until(logged_decodes, lambda values: len(values) >= 4, timeout=20)
        assert len(decodes) >= 4
        assert await daemon.get_live_transcript() == TRANSCRIPT

        first_four = decodes[:4]
        intervals = [
            later["at"] - earlier["at"]
            for earlier, later in zip(first_four, first_four[1:])
        ]
        assert min(intervals) >= 2.5, f"preview interval was not enforced: {intervals}"

        sizes = [decode["frames"] for decode in first_four]
        assert all(decode["one_second_silent_tail"] for decode in first_four)
        assert sizes[0] < sizes[1] < sizes[2], sizes
        # Pass three confirms the first three sentences. This text-only test
        # backend has estimated timestamps, so pass four retains the safer
        # five-second lookback. It still grows by less than the three seconds
        # of newly captured audio instead of decoding from frame zero.
        assert sizes[3] <= sizes[2] + first_four[2]["rate"] * 11 // 4, sizes
    finally:
        virtual_mic.stop_playback()
        if await daemon.get_state() == "Recording":
            await daemon.call_cancel_dictation()
