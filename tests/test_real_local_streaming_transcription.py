# ABOUTME: Exercises a downloaded local streaming model through recording, live decode, and insertion.
# ABOUTME: Keeps model files, audio, D-Bus, EIS, config, and history inside the isolated test stack.

import asyncio
from collections import Counter
import json
import os
import shutil
import time
from pathlib import Path

import pytest

from helpers.dbus_portal import safe_introspect


BUS = "io.github.hy26v.Voxkey.Daemon"
OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
IFACE = "io.github.hy26v.Voxkey.Daemon1"
MODEL_NAME = "nemotron-3.5-asr-streaming-0.6b"
REQUIRED_MODEL_FILES = (
    "encoder.int8.onnx",
    "decoder.int8.onnx",
    "joiner.int8.onnx",
    "tokens.txt",
)


@pytest.fixture
def daemon_config(isolated_voxkey_home):
    source = os.environ.get("VOXKEY_TEST_LOCAL_STREAMING_MODEL_DIR")
    if not source:
        pytest.skip(
            "set VOXKEY_TEST_LOCAL_STREAMING_MODEL_DIR for the real-model run"
        )
    source = Path(source).resolve()
    if not all((source / name).is_file() for name in REQUIRED_MODEL_FILES):
        pytest.skip("the configured local streaming model is incomplete")

    models = isolated_voxkey_home / "data/voxkey/models"
    models.mkdir(parents=True, exist_ok=True)
    isolated_model = models / MODEL_NAME
    if not isolated_model.exists():
        isolated_model.mkdir()
        for name in REQUIRED_MODEL_FILES:
            try:
                os.link(source / name, isolated_model / name)
            except OSError:
                shutil.copyfile(source / name, isolated_model / name)
    elif not all((isolated_model / name).is_file() for name in REQUIRED_MODEL_FILES):
        pytest.fail("the isolated local streaming model is incomplete")

    config = isolated_voxkey_home / "voxkey/config.toml"
    config.write_text(
        "\n".join(
            [
                "[transcriber]",
                'provider = "parakeet"',
                "",
                "[transcriber.parakeet]",
                f'model = "{MODEL_NAME}"',
                'execution_provider = "cpu"',
            ]
        )
        + "\n"
    )
    yield


async def wait_until(get_value, predicate, timeout=20):
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        value = await get_value()
        if predicate(value):
            return value
        await asyncio.sleep(0.1)
    return await get_value()


def normalized_words(text):
    return [
        word.strip(".,!?;:").casefold()
        for word in text.split()
        if word.strip(".,!?;:")
    ]


def word_f1(reference, hypothesis):
    reference = normalized_words(reference)
    hypothesis = normalized_words(hypothesis)
    remaining = Counter(reference)
    matches = 0
    for word in hypothesis:
        if remaining[word] > 0:
            remaining[word] -= 1
            matches += 1
    precision = matches / max(1, len(hypothesis))
    recall = matches / max(1, len(reference))
    return (
        0.0
        if precision + recall == 0
        else 2 * precision * recall / (precision + recall)
    )


@pytest.mark.asyncio
async def test_real_local_streaming_model_transcribes_and_inserts(
    daemon_process,
    dbus_session,
    virtual_mic,
    fixtures_dir,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    introspection = await safe_introspect(dbus_session, BUS, OBJECT_PATH)
    daemon = dbus_session.get_proxy_object(
        BUS, OBJECT_PATH, introspection,
    ).get_interface(IFACE)

    completed = asyncio.get_running_loop().create_future()

    def on_transcription_complete(text):
        if not completed.done():
            completed.set_result(text)

    daemon.on_transcription_complete(on_transcription_complete)
    await daemon.call_start_dictation()
    active_state = await wait_until(
        daemon.get_state,
        lambda state: state in {"Connecting", "Recording", "Streaming"},
    )
    assert active_state in {"Connecting", "Recording", "Streaming"}

    fixture = os.path.join(fixtures_dir, "long_passage.wav")
    playback_started = time.monotonic()
    try:
        virtual_mic.stream_file(fixture)
        live = await wait_until(
            daemon.get_live_transcript,
            lambda text: bool(text.strip()),
            timeout=90,
        )
        preview_latency = time.monotonic() - playback_started
        assert live.strip(), "the streaming recognizer produced no live transcript"
        assert preview_latency < 90, f"first transcript took {preview_latency:.2f}s"
        await asyncio.to_thread(virtual_mic.wait_for_playback, 15)
    finally:
        virtual_mic.stop_playback()

    await daemon.call_stop_dictation()
    final = await asyncio.wait_for(completed, timeout=90)
    reference = (
        "The quick brown fox jumps over the lazy dog. "
        "She sells seashells by the seashore."
    )
    assert word_f1(reference, final) >= 0.8, final
    assert await wait_until(
        daemon.get_state, lambda state: state == "Idle", timeout=15,
    ) == "Idle"

    history = json.loads(await daemon.get_transcription_history())
    saved = next(entry for entry in history if entry["text"] == final)
    assert saved["outcome"] == "completed"
    assert "pending_insertion" not in saved
    assert await daemon.get_portal_connected() is True
    assert daemon_process.poll() is None
