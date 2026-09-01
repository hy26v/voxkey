# ABOUTME: Verifies a provider failure preserves the user's completed recording.
# ABOUTME: Exercises the daemon failure path with a deterministic rejecting backend.

import asyncio
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
import os
import sys
import threading
import wave
from pathlib import Path

import pytest

from helpers.dbus_portal import safe_introspect


DAEMON_BUS_NAME = "io.github.hy26v.Voxkey.Daemon"
DAEMON_OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
DAEMON_INTERFACE = "io.github.hy26v.Voxkey.Daemon1"


class _BoundedParakeetHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers["Content-Length"])
        body = self.rfile.read(length)
        wav_start = body.index(b"RIFF")
        with wave.open(io.BytesIO(body[wav_start:]), "rb") as recording:
            duration = recording.getnframes() / recording.getframerate()

        with self.server.result_lock:
            self.server.durations.append(duration)

        if duration > 120:
            status = 422
            response = {
                "error": {"message": "The WAV duration exceeds the configured limit."},
            }
        else:
            status = 200
            response = {"text": self.server.transcript_for_duration(duration)}
        payload = json.dumps(response).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, _format, *_args):
        pass


@pytest.fixture
def daemon_config(isolated_voxkey_home):
    """Use a batch backend that always rejects the finalized recording."""
    path = isolated_voxkey_home / "voxkey" / "config.toml"
    path.write_text(
        f'''[transcriber]
provider = "whisper-cpp"

[transcriber.whisper_cpp]
command = {json.dumps(sys.executable)}
args = [
  "-c",
  "import sys; print('deterministic rejection', file=sys.stderr); sys.exit(42)",
  "{{audio_file}}",
]

[preview]
mode = "never"

[audio]
no_speech_guard = false
'''
    )
    os.chmod(path, 0o600)
    yield


async def _wait_until(get_value, predicate, timeout=10):
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        value = await get_value()
        if predicate(value):
            return value
        await asyncio.sleep(0.05)
    return await get_value()


async def _daemon(dbus_session):
    introspection = await safe_introspect(
        dbus_session,
        DAEMON_BUS_NAME,
        DAEMON_OBJECT_PATH,
    )
    return dbus_session.get_proxy_object(
        DAEMON_BUS_NAME,
        DAEMON_OBJECT_PATH,
        introspection,
    ).get_interface(DAEMON_INTERFACE)


async def _create_failed_recording(daemon):
    await daemon.call_start_dictation()
    await asyncio.sleep(0.1)
    await daemon.call_stop_dictation()

    error = await _wait_until(
        daemon.get_last_error,
        lambda value: "deterministic rejection" in value,
    )
    assert "saved in History" in error, error
    history = json.loads(await daemon.get_transcription_history())
    assert history, "the failed transcription disappeared from History"
    return history[0]


@pytest.mark.asyncio
async def test_rejected_transcription_preserves_a_recoverable_wav(
    daemon_process,
    dbus_session,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    daemon = await _daemon(dbus_session)
    failed = await _create_failed_recording(daemon)
    assert failed["outcome"] == "failed"
    assert "deterministic rejection" in failed["error"]

    audio_path = Path(failed["audio_path"])
    assert audio_path.is_file(), "the failed recording was deleted"
    assert audio_path.is_relative_to(Path(os.environ["XDG_STATE_HOME"]) / "voxkey")
    with wave.open(str(audio_path), "rb") as recording:
        assert recording.getnchannels() > 0
        assert recording.getframerate() > 0

    await daemon.call_delete_history_entry(failed["id"])
    assert not audio_path.exists(), "deleting the failure left its private WAV behind"


@pytest.mark.asyncio
async def test_saved_recording_can_be_retried_with_the_current_batch_provider(
    daemon_process,
    dbus_session,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    daemon = await _daemon(dbus_session)
    failed = await _create_failed_recording(daemon)
    audio_path = Path(failed["audio_path"])

    transcriber = json.loads(await daemon.get_transcriber_config())
    transcriber["whisper_cpp"] = {
        "command": sys.executable,
        "args": ["-c", "print('recovered transcript')", "{audio_file}"],
    }
    await daemon.call_set_transcriber_config(json.dumps(transcriber))
    await asyncio.sleep(0.5)

    async def session_ready():
        return await daemon.get_portal_connected(), await daemon.get_state()

    assert await _wait_until(
        session_ready,
        lambda value: value == (True, "Idle"),
    ) == (True, "Idle")
    await asyncio.sleep(0.2)

    await daemon.call_retry_history_entry(failed["id"])
    transcript = await _wait_until(
        daemon.get_last_transcript,
        lambda value: value == "recovered transcript",
    )
    assert transcript == "recovered transcript"

    history = json.loads(await daemon.get_transcription_history())
    assert history[0]["text"] == "recovered transcript"
    assert history[0]["outcome"] == "completed"
    assert any(entry["id"] == failed["id"] for entry in history)
    assert audio_path.is_file(), "retrying destroyed the recoverable source recording"

    await daemon.call_clear_transcription_history()
    assert not audio_path.exists()
    assert json.loads(await daemon.get_transcription_history()) == []


@pytest.mark.asyncio
async def test_long_saved_recording_is_chunked_without_losing_its_ends(
    daemon_process,
    dbus_session,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    daemon = await _daemon(dbus_session)
    failed = await _create_failed_recording(daemon)
    audio_path = Path(failed["audio_path"])

    # This is a real 121-second WAV but only a few KiB, so the regression
    # reaches the duration boundary without making the integration suite wait.
    with wave.open(str(audio_path), "wb") as recording:
        recording.setnchannels(1)
        recording.setsampwidth(2)
        recording.setframerate(10)
        recording.writeframes(bytes(1_210 * 2))

    server = ThreadingHTTPServer(("127.0.0.1", 0), _BoundedParakeetHandler)
    server.durations = []
    server.result_lock = threading.Lock()
    server.transcript_for_duration = lambda duration: (
        "the opening paragraph boundary words"
        if duration > 100
        else "boundary words the closing paragraph"
    )
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()

    try:
        transcriber = json.loads(await daemon.get_transcriber_config())
        transcriber["provider"] = "parakeet"
        transcriber["parakeet"] = {
            "model": "parakeet-tdt-0.6b-v3",
            "backend": "http",
            "endpoint": (
                f"http://127.0.0.1:{server.server_port}/v1/audio/transcriptions"
            ),
            "allow_insecure_http": False,
            "execution_provider": "cpu",
        }
        await daemon.call_set_transcriber_config(json.dumps(transcriber))
        await asyncio.sleep(0.5)

        async def session_ready():
            return await daemon.get_portal_connected(), await daemon.get_state()

        assert await _wait_until(
            session_ready,
            lambda value: value == (True, "Idle"),
        ) == (True, "Idle")
        await asyncio.sleep(0.2)

        await daemon.call_retry_history_entry(failed["id"])
        transcript = await _wait_until(
            daemon.get_last_transcript,
            lambda value: value.endswith("the closing paragraph"),
        )
        assert transcript == (
            "the opening paragraph boundary words the closing paragraph"
        )
        assert await _wait_until(
            daemon.get_state,
            lambda value: value == "Idle",
        ) == "Idle"

        with server.result_lock:
            durations = list(server.durations)
        assert len(durations) == 2, durations
        assert all(duration <= 120 for duration in durations), durations
        assert sum(durations) > 121, "adjacent chunks did not overlap"

        history = json.loads(await daemon.get_transcription_history())
        assert history[0]["outcome"] == "completed"
        assert history[0]["text"] == transcript
    finally:
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=5)
