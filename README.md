<p align="center">
  <img src="logo.png" width="128" alt="Voxkey logo">
</p>

# Voxkey

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![GitHub release](https://img.shields.io/github/v/release/hy26v/voxkey)](https://github.com/hy26v/voxkey/releases)
[![GitHub stars](https://img.shields.io/github/stars/hy26v/voxkey)](https://github.com/hy26v/voxkey/stargazers)

Voice dictation for Wayland. Press a shortcut, speak, and text appears at your cursor.

Voxkey is a daemon that uses XDG Desktop Portal interfaces for global shortcuts and keyboard injection - no X11, no clipboard hacks, no virtual keyboard tools.

> **Note:** Voxkey is developed and tested on Fedora running GNOME on Wayland. It may work on other distributions and compositors that support the required portal interfaces, but this is untested.

## Features

- **Toggle dictation** with a global keyboard shortcut (default: `Super+Space`)
- **Text injection** directly at the cursor in any focused application
- **Multiple transcription backends:**
  - [NVIDIA Parakeet](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/nemo/models/parakeet-tdt-0.6b) - local, offline, runs entirely on your machine
  - [whisper.cpp](https://github.com/ggerganov/whisper.cpp) - local, offline
  - [Mistral](https://docs.mistral.ai/) - cloud batch API
  - [Mistral Realtime](https://docs.mistral.ai/) - cloud streaming via WebSocket (text appears as you speak)
- **Settings GUI** (GTK4 + libadwaita) for live configuration
- **Session persistence** - portal permissions survive reboots
- **Automatic recovery** on portal errors

## Requirements

- **Fedora** with **GNOME on Wayland** (other distributions and compositors are untested)
- Portal backends providing:
  - `org.freedesktop.portal.GlobalShortcuts` (v1+)
  - `org.freedesktop.portal.RemoteDesktop` (v2+) with keyboard device support

## Installation

### Fedora (RPM)

Download the latest `.rpm` from
[GitHub Releases](https://github.com/hy26v/voxkey/releases):

```bash
sudo dnf install ./voxkey-*.rpm
```

Then enable the daemon to start on login:

```bash
systemctl --user enable --now voxkey
```

Open "Voxkey" from your app launcher to configure transcription settings.

### Building from Source

**Requirements:**
- Rust toolchain (edition 2024)
- System libraries: GTK 4.14+, libadwaita 1.6+, ALSA, libxkbcommon

On Fedora:
```bash
sudo dnf install gtk4-devel libadwaita-devel alsa-lib-devel libxkbcommon-devel
```

```bash
cargo build --release
cargo install --path .
cargo install --path voxkey-settings
```

## Configuration

Configuration lives at `~/.config/voxkey/config.toml`. All fields are optional - sensible defaults are used when omitted.

```toml
[shortcut]
trigger = "<Super>space"

[transcriber]
provider = "parakeet"  # or "whisper-cpp", "mistral", "mistral-realtime"

[transcriber.parakeet]
# model = "parakeet-tdt-0.6b-v2"  # download from the Settings app

[transcriber.whisper_cpp]
command = "whisper-cpp"
args = ["-m", "/path/to/model.bin", "{audio_file}"]

[transcriber.mistral]
api_key = "your-api-key"
# model = "voxtral-mini-2602"       # optional, shown as default
# endpoint = ""                      # optional, uses Mistral API

[transcriber.mistral_realtime]
api_key = "your-api-key"
# model = "voxtral-mini-transcribe-realtime-2602"
# endpoint = ""

[audio]
sample_rate = 16000
channels = 1
```

The `{audio_file}` placeholder in whisper-cpp args is replaced with the path to the recorded WAV file.

All configuration can also be changed at runtime through the settings GUI.

## Usage

1. Start the daemon: `systemctl --user start voxkey` (or run `voxkey` directly)
2. Open settings: search "Voxkey" in your app launcher, or run `voxkey-settings`
3. Configure your transcription provider and API key
4. Press `Super+Space` to start dictating, press again to stop
5. Transcribed text is typed into the focused application

## Architecture

```
voxkey/
├── src/           Dictation daemon (shortcuts, recording, transcription, injection)
├── voxkey-ipc/    Shared D-Bus interface types and proxy definitions
└── voxkey-settings/  GTK4+libadwaita settings GUI
```

The daemon and settings GUI communicate over D-Bus (`io.github.hy26v.Voxkey.Daemon`). The daemon manages portal sessions, audio recording, transcription dispatch, and text injection. The settings GUI subscribes to property changes and sends configuration updates.

## Contributing

Contributions are welcome, especially new transcription backends. See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## Star History

[![Star History Chart](https://api.star-history.com/svg?repos=hy26v/voxkey&type=Date)](https://star-history.com/#hy26v/voxkey&Date)

## License

[MIT](LICENSE)
