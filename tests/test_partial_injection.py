# ABOUTME: Proves a mid-injection physical modifier records and retries only the untyped suffix.
# ABOUTME: Drives the daemon through the isolated portal's real EIS protocol peer.

import asyncio
import json
import os
import signal
import subprocess
import sys
from pathlib import Path

import pytest

from helpers.dbus_portal import safe_introspect


DAEMON_BUS_NAME = "io.github.hy26v.Voxkey.Daemon"
DAEMON_OBJECT_PATH = "/io/github/hy26v/Voxkey/Daemon"
DAEMON_INTERFACE = "io.github.hy26v.Voxkey.Daemon1"


@pytest.fixture
def daemon_config(isolated_voxkey_home):
    """Use deterministic two-character output and a visible inter-key delay."""
    path = isolated_voxkey_home / "voxkey" / "config.toml"
    path.write_text(
        f'''[injection]
typing_delay_ms = 50

[transcriber]
provider = "whisper-cpp"

[transcriber.whisper_cpp]
command = {json.dumps(sys.executable)}
args = ["-c", "print('ab')", "{{audio_file}}"]
'''
    )
    os.chmod(path, 0o600)
    yield


async def _daemon_proxy(bus):
    introspection = await safe_introspect(
        bus, DAEMON_BUS_NAME, DAEMON_OBJECT_PATH,
    )
    return bus.get_proxy_object(
        DAEMON_BUS_NAME, DAEMON_OBJECT_PATH, introspection,
    ).get_interface(DAEMON_INTERFACE)


async def _wait_for_ready_daemon(bus, timeout=15):
    deadline = asyncio.get_running_loop().time() + timeout
    last_error = None
    while asyncio.get_running_loop().time() < deadline:
        try:
            proxy = await _daemon_proxy(bus)
            if await proxy.get_portal_connected() and await proxy.get_state() == "Idle":
                return proxy
        except Exception as error:
            last_error = error
        await asyncio.sleep(0.05)
    raise AssertionError(f"replacement daemon did not become ready: {last_error}")


async def _wait_for(predicate, timeout=15):
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        value = predicate()
        if value is not None:
            return value
        await asyncio.sleep(0.05)
    return predicate()


def _history_entry(path):
    if not path.is_file():
        return None
    entries = json.loads(path.read_text())
    return entries[0] if entries else None


def _eis_events(path, expected_count):
    if not path.is_file():
        return None
    events = path.read_text().splitlines()
    return events if len(events) >= expected_count else None


@pytest.mark.asyncio
async def test_partial_injection_fails_closed_and_retries_only_the_suffix(
    daemon_process,
    dbus_session,
    mock_portal,
    monkeypatch,
    tmp_path,
):
    assert daemon_process.reached_idle, daemon_process.startup_lines
    event_log = tmp_path / "eis-events.log"
    history_path = Path(os.environ["XDG_STATE_HOME"]) / "voxkey" / "history.json"
    monkeypatch.setenv("VOXKEY_TEST_EIS_EVENT_LOG", str(event_log))
    monkeypatch.setenv("VOXKEY_TEST_EIS_MODIFIER_AFTER_FIRST_TAP", "1")

    daemon = await _daemon_proxy(dbus_session)
    await daemon.call_start_dictation()
    await daemon.call_stop_dictation()

    exit_code = await asyncio.to_thread(daemon_process.wait, 15)
    assert exit_code != 0, "a partial compositor write did not fail closed"

    first_entry = await _wait_for(
        lambda: _history_entry(history_path),
    )
    assert first_entry is not None, "partial transcript was not saved"
    assert first_entry["text"] == "ab"
    assert first_entry["outcome"] == "completed"
    assert first_entry["pending_insertion"] == "b"
    assert await _wait_for(lambda: _eis_events(event_log, 2)) == [
        "30 Press",
        "30 Released",
    ]

    # The next explicit retry must use the saved suffix, not duplicate the
    # prefix that already reached the compositor before the modifier arrived.
    monkeypatch.delenv("VOXKEY_TEST_EIS_MODIFIER_AFTER_FIRST_TAP")
    bus_address, _, _ = mock_portal
    env = os.environ.copy()
    env["DBUS_SESSION_BUS_ADDRESS"] = bus_address
    replacement = subprocess.Popen(
        [os.environ.get("VOXKEY_BIN", "voxkey")],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    try:
        retry_daemon = await _wait_for_ready_daemon(dbus_session)
        await retry_daemon.call_insert_last_transcript()
        assert await _wait_for(lambda: _eis_events(event_log, 4)) == [
            "30 Press",
            "30 Released",
            "48 Press",
            "48 Released",
        ]
        retried_entry = await _wait_for(
            lambda: (
                entry
                if (entry := _history_entry(history_path)) is not None
                and "pending_insertion" not in entry
                else None
            )
        )
        assert retried_entry is not None, "successful suffix retry was not persisted"
    finally:
        if replacement.poll() is None:
            replacement.send_signal(signal.SIGTERM)
            try:
                await asyncio.to_thread(replacement.wait, 10)
            except subprocess.TimeoutExpired:
                replacement.kill()
                await asyncio.to_thread(replacement.wait)
