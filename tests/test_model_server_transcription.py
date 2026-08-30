# ABOUTME: Exercises an authenticated OpenAI-compatible model server through a full dictation cycle.
# ABOUTME: Uses isolated audio, D-Bus, config, history, and desktop insertion fixtures.

import asyncio
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
import queue
import threading

import pytest

from helpers.dbus_portal import safe_introspect


BUS = "io.github.hy26v.Voxkey.Daemon"
OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
IFACE = "io.github.hy26v.Voxkey.Daemon1"
TRANSCRIPT = "server route authenticated"


class _ModelServerHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        self.server.requests.put(
            {
                "path": self.path,
                "authorization": self.headers.get("Authorization"),
                "content_type": self.headers.get("Content-Type"),
                "body": body,
            }
        )
        response = json.dumps({"text": TRANSCRIPT}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)

    def log_message(self, _format, *_args):
        pass


@pytest.fixture
def model_server():
    server = ThreadingHTTPServer(("127.0.0.1", 0), _ModelServerHandler)
    server.requests = queue.Queue()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


@pytest.fixture
def daemon_config(isolated_voxkey_home, model_server):
    config = isolated_voxkey_home / "voxkey/config.toml"
    config.write_text(
        "\n".join(
            [
                "[transcriber]",
                'provider = "parakeet"',
                "",
                "[transcriber.parakeet]",
                'model = "server-model"',
                'backend = "http"',
                (
                    'endpoint = "http://127.0.0.1:'
                    f'{model_server.server_port}/v1/audio/transcriptions"'
                ),
                "allow_insecure_http = false",
                'execution_provider = "cpu"',
                'api_key = "server-token"',
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


@pytest.mark.asyncio
async def test_authenticated_model_server_transcribes_and_inserts(
    daemon_process,
    dbus_session,
    virtual_mic,
    fixtures_dir,
    model_server,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    introspection = await safe_introspect(dbus_session, BUS, OBJECT_PATH)
    daemon = dbus_session.get_proxy_object(
        BUS, OBJECT_PATH, introspection,
    ).get_interface(IFACE)

    public_config = json.loads(await daemon.get_transcriber_config())
    assert "api_key" not in public_config["parakeet"]

    completed = asyncio.get_running_loop().create_future()

    def on_transcription_complete(text):
        if not completed.done():
            completed.set_result(text)

    daemon.on_transcription_complete(on_transcription_complete)
    await daemon.call_start_dictation()
    assert await wait_until(
        daemon.get_state, lambda state: state == "Recording",
    ) == "Recording"

    try:
        virtual_mic.stream_file(os.path.join(fixtures_dir, "hello.wav"))
        await asyncio.to_thread(virtual_mic.wait_for_playback, 15)
    finally:
        virtual_mic.stop_playback()

    await daemon.call_stop_dictation()
    assert await asyncio.wait_for(completed, timeout=20) == TRANSCRIPT
    assert await wait_until(
        daemon.get_state, lambda state: state == "Idle",
    ) == "Idle"

    request = await asyncio.to_thread(model_server.requests.get, True, 5)
    assert request["path"] == "/v1/audio/transcriptions"
    assert request["authorization"] == "Bearer server-token"
    assert request["content_type"].startswith("multipart/form-data; boundary=")
    assert b'name="file"' in request["body"]
    assert b"RIFF" in request["body"]
    assert b'name="model"' in request["body"]
    assert b"server-model" in request["body"]

    history = json.loads(await daemon.get_transcription_history())
    saved = next(entry for entry in history if entry["text"] == TRANSCRIPT)
    assert saved["outcome"] == "completed"
    assert "pending_insertion" not in saved
    assert daemon_process.poll() is None
