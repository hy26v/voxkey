# ABOUTME: Verifies endpoint checks over D-Bus without persisting candidates or sending private data.
# ABOUTME: Uses a loopback transcription route so success and missing-path feedback are deterministic.

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from helpers.dbus_portal import safe_introspect


DAEMON_BUS_NAME = "io.github.hy26v.Voxkey.Daemon"
DAEMON_OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
DAEMON_INTERFACE = "io.github.hy26v.Voxkey.Daemon1"


class _EndpointHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        self.server.requests.append((self.path, dict(self.headers), body))
        if self.path == "/v1/audio/transcriptions":
            self.send_response(422)
        else:
            self.send_response(404)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def log_message(self, _format, *_args):
        pass


async def _daemon_proxy(dbus_session):
    introspection = await safe_introspect(
        dbus_session, DAEMON_BUS_NAME, DAEMON_OBJECT_PATH,
    )
    return dbus_session.get_proxy_object(
        DAEMON_BUS_NAME, DAEMON_OBJECT_PATH, introspection,
    ).get_interface(DAEMON_INTERFACE)


@pytest.mark.asyncio
async def test_endpoint_check_is_private_read_only_and_path_aware(
    daemon_process, dbus_session,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    server = ThreadingHTTPServer(("127.0.0.1", 0), _EndpointHandler)
    server.requests = []
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    try:
        daemon = await _daemon_proxy(dbus_session)
        original_json = await daemon.get_transcriber_config()
        candidate = json.loads(original_json)
        candidate["provider"] = "parakeet"
        candidate["parakeet"]["backend"] = "http"
        assert candidate["parakeet"]["allow_insecure_http"] is False

        candidate["parakeet"]["endpoint"] = (
            "http://192.168.1.132:8000/v1/audio/transcriptions"
        )
        blocked = json.loads(
            await daemon.call_check_transcriber_endpoint(json.dumps(candidate))
        )

        assert blocked["status"] == "failed"
        assert "Allow unencrypted LAN audio" in blocked["message"]
        assert await daemon.get_transcriber_config() == original_json

        candidate["parakeet"]["endpoint"] = (
            f"http://127.0.0.1:{server.server_port}/v1/audio/transcriptions"
        )

        reachable = json.loads(
            await daemon.call_check_transcriber_endpoint(json.dumps(candidate))
        )

        assert reachable["status"] == "reachable"
        assert await daemon.get_transcriber_config() == original_json
        path, headers, body = server.requests[-1]
        assert path == "/v1/audio/transcriptions"
        assert body == b""
        assert "Authorization" not in headers

        candidate["parakeet"]["endpoint"] = (
            f"http://127.0.0.1:{server.server_port}/missing"
        )
        missing = json.loads(
            await daemon.call_check_transcriber_endpoint(json.dumps(candidate))
        )

        assert missing["status"] == "failed"
        assert "URL path" in missing["message"]
        assert await daemon.get_transcriber_config() == original_json
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
