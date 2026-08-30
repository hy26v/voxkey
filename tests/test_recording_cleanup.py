# ABOUTME: Verifies abandoned recordings never leave captured audio on disk.
# ABOUTME: Interrupts the daemon mid-recording and inspects its private temp dir.

import os
import select
import signal
import subprocess
import time

import pytest


def _daemon_binary():
    return os.environ.get("VOXKEY_BIN", "voxkey")


def _start_daemon(bus_address, temp_dir, timeout=15):
    """Start the daemon with a private TMPDIR so its scratch files are visible."""
    env = os.environ.copy()
    env["DBUS_SESSION_BUS_ADDRESS"] = bus_address
    env["TMPDIR"] = str(temp_dir)

    proc = subprocess.Popen(
        [_daemon_binary()],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )

    deadline = time.monotonic() + timeout
    proc.reached_idle = False
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            break
        if select.select([proc.stderr], [], [], 0.5)[0]:
            line = proc.stderr.readline()
            if not line:
                break
            decoded = line.decode("utf-8", errors="replace").strip()
            if "STATE:" in decoded and "Idle" in decoded.split("STATE:")[-1]:
                proc.reached_idle = True
                break
    return proc


def _recordings_in(temp_dir):
    return sorted(
        name
        for name in os.listdir(temp_dir)
        if name.startswith("voxkey_") and name.endswith(".wav")
    )


def _stop(proc):
    if proc.poll() is None:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()


@pytest.mark.parametrize("shut_down", ["sigterm", "sigint"])
def test_interrupting_a_recording_leaves_no_audio_behind(
    mock_portal, virtual_mic, fixtures_dir, tmp_path, shut_down,
):
    """A recording abandoned by daemon shutdown must not persist to disk.

    The captured audio is private, so an interrupted dictation cannot leave a
    WAV of whatever the microphone heard sitting in the temp directory.
    """
    bus_address, controller, _ = mock_portal
    temp_dir = tmp_path / f"voxkey-tmp-{shut_down}"
    temp_dir.mkdir()

    proc = _start_daemon(bus_address, temp_dir)
    try:
        assert proc.reached_idle, "Daemon did not reach Idle"

        controller.emit_activated()
        virtual_mic.stream_file(os.path.join(fixtures_dir, "long_passage.wav"))
        time.sleep(1.5)

        assert _recordings_in(temp_dir), (
            "Expected an in-progress recording file; the test proves nothing "
            "if the daemon never started recording"
        )

        signal_number = signal.SIGTERM if shut_down == "sigterm" else signal.SIGINT
        proc.send_signal(signal_number)
        proc.wait(timeout=10)
    finally:
        virtual_mic.stop_playback()
        _stop(proc)

    leftovers = _recordings_in(temp_dir)
    assert not leftovers, (
        f"abandoned recording left captured audio on disk: {leftovers}"
    )
