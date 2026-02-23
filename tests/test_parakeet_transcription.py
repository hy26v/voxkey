# ABOUTME: Integration test for Parakeet transcription backend configuration.
# ABOUTME: Verifies config round-trip through D-Bus and model availability checks.

import json
import os

import pytest
import pytest_asyncio
from dbus_next.aio import MessageBus


DAEMON_BUS_NAME = "io.github.hy26v.Voxkey.Daemon"
DAEMON_OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
DAEMON_INTERFACE = "io.github.hy26v.Voxkey.Daemon1"


@pytest.fixture
def parakeet_model_available():
    """Skip test if no Parakeet model is downloaded."""
    data_dir = os.environ.get("XDG_DATA_HOME", os.path.expanduser("~/.local/share"))
    model_dir = os.path.join(data_dir, "voxkey", "models", "parakeet-tdt-0.6b-v3")
    required_files = ["encoder.int8.onnx", "decoder.int8.onnx", "joiner.int8.onnx", "tokens.txt"]
    if not all(os.path.exists(os.path.join(model_dir, f)) for f in required_files):
        pytest.skip("Parakeet v3 model not downloaded")


@pytest_asyncio.fixture
async def daemon_proxy(daemon_process):
    """Connect to the daemon's D-Bus interface on the mock portal bus."""
    bus = await MessageBus(bus_address=daemon_process.bus_address).connect()
    introspection = await bus.introspect(DAEMON_BUS_NAME, DAEMON_OBJECT_PATH)
    proxy = bus.get_proxy_object(DAEMON_BUS_NAME, DAEMON_OBJECT_PATH, introspection)
    iface = proxy.get_interface(DAEMON_INTERFACE)
    yield iface
    bus.disconnect()


@pytest.mark.asyncio
async def test_parakeet_config_round_trip(daemon_process, daemon_proxy):
    """Configure Parakeet provider and verify config persists through D-Bus."""
    assert daemon_process.reached_idle, "Daemon did not reach Idle"

    config = {
        "provider": "parakeet",
        "whisper_cpp": {"command": "whisper-cpp", "args": []},
        "mistral": {"api_key": "", "model": "voxtral-mini-2602", "endpoint": ""},
        "mistral_realtime": {"api_key": "", "model": "voxtral-mini-transcribe-realtime-2602", "endpoint": ""},
        "parakeet": {"model": "parakeet-tdt-0.6b-v3", "execution_provider": "cpu"},
    }
    await daemon_proxy.call_set_transcriber_config(json.dumps(config))

    result_json = await daemon_proxy.get_transcriber_config()
    result = json.loads(result_json)
    assert result["provider"] == "parakeet"
    assert result["parakeet"]["model"] == "parakeet-tdt-0.6b-v3"
    assert result["parakeet"]["execution_provider"] == "cpu"
