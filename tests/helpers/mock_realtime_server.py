# ABOUTME: Mock Mistral realtime WebSocket server for streaming transcription tests.
# ABOUTME: Speaks the session.created / input_audio.append / text.delta protocol.

import asyncio
import json
import threading

from websockets.asyncio.server import serve
from websockets.exceptions import ConnectionClosed


class MockRealtimeServer:
    """A minimal stand-in for Mistral's realtime transcription endpoint.

    Emits the configured transcript deltas once the daemon starts sending
    audio, which is what makes the daemon's streaming path observable without
    contacting a paid cloud API.
    """

    def __init__(
        self,
        deltas,
        delta_gap=0.05,
        fail_after_deltas=None,
        disconnect_after_deltas=False,
        graceful_close_after_deltas=False,
        graceful_close_on_audio_end=False,
    ):
        self._deltas = list(deltas)
        self._delta_gap = delta_gap
        self._fail_after_deltas = fail_after_deltas
        self._disconnect_after_deltas = disconnect_after_deltas
        self._graceful_close_after_deltas = graceful_close_after_deltas
        self._graceful_close_on_audio_end = graceful_close_on_audio_end
        self._port = None
        self._loop = asyncio.new_event_loop()
        self._started = threading.Event()
        self.received_audio_chunks = 0
        self.session_updates = []
        self.sent_deltas = 0
        self.sent_failure = False
        self.disconnected = False

        self._thread = threading.Thread(target=self._serve_forever, daemon=True)
        self._thread.start()
        if not self._started.wait(timeout=10):
            raise RuntimeError("mock realtime server did not start")

    @property
    def url(self):
        return f"ws://127.0.0.1:{self._port}/v1/audio/transcriptions/realtime"

    async def _handle(self, connection):
        try:
            await connection.send(json.dumps({"type": "session.created"}))
            emitted = False
            async for raw in connection:
                message = json.loads(raw)
                kind = message.get("type")
                if kind == "session.update":
                    self.session_updates.append(message)
                elif kind == "input_audio.append":
                    self.received_audio_chunks += 1
                    if not emitted:
                        emitted = True
                        for delta in self._deltas:
                            await connection.send(json.dumps({
                                "type": "transcription.text.delta",
                                "text": delta,
                            }))
                            self.sent_deltas += 1
                            await asyncio.sleep(self._delta_gap)
                        if self._fail_after_deltas is not None:
                            await connection.send(json.dumps({
                                "type": "error",
                                "text": self._fail_after_deltas,
                            }))
                            self.sent_failure = True
                        if self._disconnect_after_deltas:
                            self.disconnected = True
                            connection.transport.abort()
                            return
                        if self._graceful_close_after_deltas:
                            self.disconnected = True
                            await connection.close()
                            return
                elif kind == "input_audio.end" and self._graceful_close_on_audio_end:
                    self.disconnected = True
                    await connection.close()
                    return
        except ConnectionClosed:
            # Normal fixture teardown can retire the daemon before the mock's
            # peer close frame is flushed. The product-side assertions decide
            # whether a transport close was expected for each test.
            return

    def _serve_forever(self):
        asyncio.set_event_loop(self._loop)

        async def main():
            self._shutdown = asyncio.Event()
            async with await serve(self._handle, "127.0.0.1", 0) as server:
                self._port = server.sockets[0].getsockname()[1]
                self._started.set()
                await self._shutdown.wait()

        try:
            self._loop.run_until_complete(main())
        finally:
            self._started.set()
            self._loop.close()

    def close(self):
        self._loop.call_soon_threadsafe(self._shutdown.set)
        self._thread.join(timeout=5)
