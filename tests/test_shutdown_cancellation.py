# ABOUTME: Verifies graceful shutdown preempts an in-flight batch provider request.
# ABOUTME: Uses a delayed loopback HTTP endpoint so cancellation timing is deterministic.

import asyncio
import json
import os
import signal
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from helpers.dbus_portal import safe_introspect


DAEMON_BUS_NAME = "io.github.hy26v.Voxkey.Daemon"
DAEMON_OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
DAEMON_INTERFACE = "io.github.hy26v.Voxkey.Daemon1"


@pytest.fixture
def daemon_config(tmp_path, monkeypatch):
    """Expose the daemon's scratch directory for cancellation cleanup checks."""
    scratch = tmp_path / "voxkey-scratch"
    scratch.mkdir()
    monkeypatch.setenv("TMPDIR", str(scratch))
    yield scratch


class _DelayedServer(ThreadingHTTPServer):
    daemon_threads = True


class _DelayedTranscriber(BaseHTTPRequestHandler):
    def log_message(self, _format, *_args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        self.server.request_received.set()
        time.sleep(self.server.response_delay)
        body = b'{"text":"must never be inserted"}'
        try:
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass


async def _wait_until(get_value, predicate, timeout=10):
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        value = await get_value()
        if predicate(value):
            return value
        await asyncio.sleep(0.05)
    return await get_value()


async def _daemon_interface(dbus_session):
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


@pytest.mark.asyncio
async def test_sigterm_cancels_delayed_batch_request_promptly(
    daemon_process,
    daemon_config,
    dbus_session,
    portal_control,
    virtual_mic,
    fixtures_dir,
):
    assert daemon_process.reached_idle, (
        f"Daemon did not reach Idle: {daemon_process.startup_lines}"
    )
    server = _DelayedServer(("127.0.0.1", 0), _DelayedTranscriber)
    server.request_received = threading.Event()
    server.response_delay = 10
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()

    daemon = await _daemon_interface(dbus_session)
    transcriber = json.loads(await daemon.get_transcriber_config())
    transcriber["provider"] = "parakeet"
    transcriber["parakeet"] = {
        "model": "parakeet-tdt-0.6b-v3",
        "backend": "http",
        "endpoint": f"http://127.0.0.1:{server.server_port}/v1/audio/transcriptions",
        "execution_provider": "cpu",
    }

    try:
        await daemon.call_set_transcriber_config(json.dumps(transcriber))
        # The setter requests an asynchronous portal-session rebuild. Avoid
        # mistaking the old session's final Idle sample for the new one.
        await asyncio.sleep(0.5)

        async def session_ready():
            return await daemon.get_state(), await daemon.get_portal_connected()

        assert await _wait_until(
            session_ready,
            lambda value: value == ("Idle", True),
        ) == ("Idle", True)
        await asyncio.sleep(0.3)
        portal_control.clear_metrics()

        portal_control.emit_activated()
        assert await _wait_until(
            daemon.get_state,
            lambda value: value == "Recording",
        ) == "Recording"

        virtual_mic.stream_file(os.path.join(fixtures_dir, "hello.wav"))
        await asyncio.sleep(0.5)
        portal_control.emit_deactivated()
        await asyncio.sleep(0.1)
        portal_control.emit_activated()
        portal_control.emit_deactivated()

        assert await _wait_until(
            daemon.get_state,
            lambda value: value == "Transcribing",
        ) == "Transcribing"
        assert "remote_desktop" not in portal_control.active_session_types(), (
            "Batch transcription acquired desktop control before text existed"
        )
        assert await asyncio.to_thread(server.request_received.wait, 5), (
            "Delayed provider never received the transcription request"
        )

        started = time.monotonic()
        daemon_process.send_signal(signal.SIGTERM)
        exit_code = await asyncio.wait_for(
            asyncio.to_thread(daemon_process.wait),
            timeout=2,
        )
        elapsed = time.monotonic() - started

        assert exit_code == 0
        assert elapsed < 2, f"Graceful shutdown took {elapsed:.3f}s"
        closed = portal_control.closed_session_types()
        assert "remote_desktop" not in closed, (
            "Shutdown found a RemoteDesktop grant during batch transcription"
        )
        assert "shortcuts" in closed
        assert not list(daemon_config.glob("voxkey_*.wav")), (
            "cancelling transcription left captured audio in the scratch directory"
        )
    finally:
        virtual_mic.stop_playback()
        if daemon_process.poll() is None:
            daemon_process.send_signal(signal.SIGTERM)
            await asyncio.to_thread(daemon_process.wait)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=2)
