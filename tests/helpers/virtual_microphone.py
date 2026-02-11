# ABOUTME: Virtual microphone using PipeWire/PulseAudio for streaming pre-recorded audio.
# ABOUTME: Creates a null sink whose monitor source acts as a microphone for recording apps.

import os
import signal
import subprocess
import time


class VirtualMicrophone:
    """A virtual microphone backed by PipeWire's PulseAudio compatibility.

    Creates a null audio sink via pactl. PipeWire auto-creates a .monitor
    source for the sink. Audio played into the sink (via pw-cat) appears
    as microphone input on the monitor source.
    """

    def __init__(self, sink_name="voxkey_test_mic"):
        self._sink_name = sink_name
        self._monitor_source = f"{sink_name}.monitor"
        self._module_id = None
        self._original_default_source = None
        self._playback_proc = None
        self._cleanup_stale_modules()
        self._create_null_sink()
        self._set_as_default_source()

    def _cleanup_stale_modules(self):
        """Unload any leftover null-sink modules from previous test runs."""
        result = subprocess.run(
            ["pactl", "list", "modules", "short"],
            capture_output=True, text=True, timeout=5,
        )
        for line in result.stdout.splitlines():
            parts = line.split("\t")
            if len(parts) >= 3 and self._sink_name in parts[2]:
                module_id = parts[0]
                subprocess.run(
                    ["pactl", "unload-module", module_id],
                    capture_output=True, timeout=5,
                )

    def _create_null_sink(self):
        """Create a PulseAudio null sink via pactl. PipeWire provides the monitor source."""
        result = subprocess.run(
            [
                "pactl", "load-module", "module-null-sink",
                f"sink_name={self._sink_name}",
                f"sink_properties=device.description=VoxkeyTestMic",
            ],
            capture_output=True, text=True, timeout=5,
        )
        module_id = result.stdout.strip()
        if module_id:
            self._module_id = module_id
        time.sleep(0.3)

    def _set_as_default_source(self):
        """Set the null sink's monitor as the default audio source."""
        # Save original default source for restoration
        result = subprocess.run(
            ["pactl", "get-default-source"],
            capture_output=True, text=True, timeout=5,
        )
        self._original_default_source = result.stdout.strip()

        subprocess.run(
            ["pactl", "set-default-source", self._monitor_source],
            capture_output=True, timeout=5,
        )

    def stream_file(self, wav_path):
        """Stream a WAV file through the virtual microphone.

        The audio is played into the null sink. Any application recording
        from the monitor source (set as default) receives this audio.
        """
        if not os.path.exists(wav_path):
            raise FileNotFoundError(f"Audio fixture not found: {wav_path}")

        self._playback_proc = subprocess.Popen(
            [
                "pw-cat", "--playback",
                "--target", self._sink_name,
                wav_path,
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

    def wait_for_playback(self, timeout=30):
        """Wait for the current audio playback to finish."""
        if self._playback_proc:
            self._playback_proc.wait(timeout=timeout)
            self._playback_proc = None

    def stop_playback(self):
        """Stop any ongoing audio playback."""
        if self._playback_proc:
            self._playback_proc.send_signal(signal.SIGTERM)
            try:
                self._playback_proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self._playback_proc.kill()
            self._playback_proc = None

    def close(self):
        """Restore original default source and unload the null sink module."""
        self.stop_playback()

        if self._original_default_source:
            subprocess.run(
                ["pactl", "set-default-source", self._original_default_source],
                capture_output=True, timeout=5,
            )

        if self._module_id:
            result = subprocess.run(
                ["pactl", "unload-module", self._module_id],
                capture_output=True, text=True, timeout=5,
            )
            if result.returncode != 0:
                import warnings
                warnings.warn(
                    f"Failed to unload module {self._module_id}: {result.stderr.strip()}"
                )
            self._module_id = None

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()

    def __del__(self):
        if self._module_id is not None:
            self.close()
