<p align="center">
  <img src="logo.png" width="128" alt="Voxkey logo">
</p>

# Voxkey

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![GitHub release](https://img.shields.io/github/v/release/hy26v/voxkey)](https://github.com/hy26v/voxkey/releases)
[![GitHub stars](https://img.shields.io/github/stars/hy26v/voxkey)](https://github.com/hy26v/voxkey/stargazers)

Voice dictation for Wayland. Press a shortcut, speak, and text appears at your cursor.

Voxkey is a daemon that uses XDG Desktop Portal interfaces for global shortcuts
and compositor-tracked EIS keyboard injection. It does not use X11, clipboard
replacement, or the GNOME IBus input path.

> **Note:** Voxkey is developed and tested on Fedora running GNOME on Wayland. It may work on other distributions and compositors that support the required portal interfaces, but this is untested.

> **Post-incident status (2026-07-18):** The injector changes in this tree have
> passed offline protocol tests but have deliberately not been started in the
> affected graphical session. Keep the service disabled on a primary session
> until the build passes isolated GNOME/VM validation, including physical
> modifier checks after injection and shutdown.

## Features

- **Toggle dictation** with a global keyboard shortcut (default: `Super+Alt+D`),
  including safe single-button function and dedicated media keys
- **Text injection** directly at the cursor in any focused application
- **Multiple transcription backends:**
  - **Downloadable model library** with NVIDIA Nemotron 3.5, Parakeet Unified,
    and Parakeet v2/v3. Voxkey pins and verifies every file, lets you cancel a
    large download safely, and uses the streaming models to show text while you
    speak without sending audio away.
  - [whisper.cpp](https://github.com/ggerganov/whisper.cpp) - local, offline
  - [Mistral](https://docs.mistral.ai/) - cloud batch API
  - [Mistral Realtime](https://docs.mistral.ai/) - cloud streaming via WebSocket (text appears as you speak)
- **Adaptive GNOME settings app** with dedicated History, Transcription, Audio
  Input, Dictionary, Permissions, and General sections
- **Local dictation history** with search, copy, individual deletion, and clear-all;
  failed batch transcriptions retain their private WAV so you can retry with the
  current model or open the recording folder for manual upload
- **Selectable audio input** that fails safely instead of recording from a different
  microphone when the saved device is unavailable
- **Dictionary tools** for recurring word replacements and transcription vocabulary
- **Live corrected previews** in the GNOME status capsule; on-device providers
  periodically re-transcribe the growing recording, while realtime providers
  publish their incoming deltas. Every preview includes dictionary replacements
  and may revise earlier text before the final result is inserted. Configurable
  under `[preview]`, and off by default for metered network providers.
- **Session persistence** - portal permissions survive reboots
- **Fail-closed portal handling** - input/session errors stop the daemon instead
  of automatically recreating a virtual keyboard or retrying partial text
- **Tracked keycode injection** - characters and required Shift/AltGr keys are
  sent as explicit EIS keycodes using the compositor-provided keymap; the EIS
  connection remains neutral while idle and is disconnected when its portal
  session ends or any input error occurs

Characters that are not representable by the active compositor keymap are
rejected before the first key in that batch is sent. GNOME 50 exposes a
keyboard-capable EIS device here, not a keymap-independent text device.

## Requirements

- **Fedora** with **GNOME on Wayland** (other distributions and compositors are untested)
- Portal backends providing:
  - `org.freedesktop.portal.GlobalShortcuts` (v1+)
  - `org.freedesktop.portal.RemoteDesktop` (v2+) with keyboard device support
    and `ConnectToEIS`

## Installation

### Fedora (RPM)

Download the latest `.rpm` from
[GitHub Releases](https://github.com/hy26v/voxkey/releases):

```bash
sudo dnf install ./voxkey-*.rpm
```

Open "Voxkey" from your app launcher. The application starts its user service
automatically. Closing the window keeps both running when **Keep Running in
Background** is enabled; choose **Quit Voxkey**, or turn that option off and
close the window, to stop both.

On its first launch in GNOME, Voxkey also enables its packaged Shell extension
so the interactive Quick Settings controls and recording capsule are available
immediately. If the current GNOME Shell session has not discovered the extension
yet, Voxkey displays a persistent reminder to save your work and log out and
back in. Its **Log Out…** action always opens GNOME's confirmation dialog; Voxkey
never ends the session automatically. The capsule is limited to recording and
transcribing: it shows Cancel and Finish controls, elapsed capture time,
live microphone level, and corrected transcript previews, then disappears
before text insertion. The tile menu also provides copy/reinsert-last-transcript
actions and error recovery. That onboarding happens only once; if you later
disable the extension in GNOME Extensions, Voxkey respects that choice.

The service remains disabled as a login unit. For emergency or administrative
use, it can also be masked. Voxkey will respect the mask and offer an explicit
**Allow & Start** action instead of silently overriding it:

```bash
systemctl --user disable --now voxkey
systemctl --user mask voxkey
```

### Emergency keyboard recovery

If the graphical session stops accepting keyboard input, recover from a
remote shell (SSH) or TTY by running the recovery script. It stops and masks
Voxkey (stopping alone is not sufficient — D-Bus can reactivate it), restarts
the portal services that may still own virtual-keyboard state, and reports
what virtual input state remains so you can attach it to a bug report:

```bash
voxkey-keyboard-recovery
```

The command is included in the RPM. From a checkout, run
`./scripts/keyboard-recovery`. The script is idempotent,
never restarts IBus, and never changes your input sources.

Do not restart IBus as a first recovery step. On GNOME Wayland, restarting it
can leave GNOME Shell without its dynamically registered XKB engine and make
the lockout worse. During the 2026-07-18 incident, ordinary typing was only
partially restored and Shift remained broken; no reliable in-session modifier
reset was found, and the machine was ultimately rebooted. A logout-only reset
was not tested. Preserve unsaved work and input-source settings before taking
either action. Do not start this build in a primary session until it has passed
isolated GNOME validation.

See the [2026-07-18 incident report](docs/incidents/2026-07-18-keyboard-lockup.md)
for the evidence, failed recovery attempts, and the post-reboot
cross-application clipboard cleanup that restored copy and paste.

Maintainers: the automated, disposable-GNOME, soak, and stopping criteria are
defined in the [release-readiness gate](docs/release-readiness.md).

### Building from Source

**Requirements:**
- Rust toolchain (edition 2024)
- System libraries: GTK 4.14+, libadwaita 1.6+, ALSA, libxkbcommon

On Fedora:
```bash
sudo dnf install rust cargo gcc gcc-c++ clang-libs \
    gtk4-devel libadwaita-devel alsa-lib-devel libxkbcommon-devel \
    rpm-build systemd-rpm-macros ImageMagick git curl
```

Build a uniquely versioned development RPM and install it through DNF:

```bash
./scripts/local-install.sh
```

This is also the supported installation path for local development builds. It
keeps every installed file owned by the `voxkey` RPM, upgrades an existing
local build, and restarts the user service only if it was already running. Use
`./scripts/local-install.sh --build-only` when you only want the RPM artifact;
the script prints its path after a successful build.

## Configuration

Configuration lives at `~/.config/voxkey/config.toml`. All fields are optional - sensible defaults are used when omitted.

```toml
[shortcut]
trigger = "<Super><Alt>d"

[transcriber]
provider = "parakeet"  # or "whisper-cpp", "mistral", "mistral-realtime"

[transcriber.parakeet]
model = "nemotron-3.5-asr-streaming-0.6b"
backend = "local"  # "http" uses an OpenAI-compatible transcription server
endpoint = "https://speech.example.com/v1/audio/transcriptions"
# allow_insecure_http = true  # private IPs only; audio, transcripts, and keys are unencrypted
# execution_provider = "cuda"  # local backend only: "auto", "cpu", or "cuda"

[transcriber.whisper_cpp]
command = "whisper-cpp"
args = ["-m", "/path/to/model.bin", "{audio_file}"]

[transcriber.mistral]
# model = "voxtral-mini-2602"       # optional, shown as default
# endpoint = ""  # optional Mistral-compatible override; empty uses the Mistral API

[transcriber.mistral_realtime]
# model = "voxtral-mini-transcribe-realtime-2602"
# endpoint = ""

[audio]
sample_rate = 16000
channels = 1
tail_capture_ms = 1000 # extra recording time after the shortcut is released, to avoid clipping the last word
max_recording_seconds = 600 # hard duration limit; WAV capture is also capped at 64 MiB
input_device = ""      # exact microphone name, or empty to follow the system default
mute_output_while_recording = false # opt in to muting the current PipeWire/PulseAudio sink

[injection]
typing_delay_ms = 0  # optional delay between EIS key taps

[preview]
mode = "auto"           # auto | always | never
interval_ms = 1000      # how often a new preview hypothesis may be requested
max_audio_seconds = 0   # max unconfirmed window; 0 keeps long dictations live
```

Choose and download local models in **Transcription → Model library**. The
current curated catalog, licenses, server contract, and models evaluated for
future support are documented in [Model library](docs/model-library.md).

Set cloud API keys and optional transcription-server keys in Voxkey Settings.
They are stored in the desktop keyring, not in `config.toml`. Existing
plaintext keys are supported only for migration; Voxkey makes the
configuration private before reading them.

When you add or change a Mistral Batch, Mistral Realtime, or transcription-server
endpoint in Settings, Voxkey checks that exact address before saving it. The
check sends no recording or API key. If the server cannot be reached, the
address stays in the field so you can correct it or try again.

Voxkey blocks unencrypted model-server HTTP outside this computer by default. If a
trusted server on your private network does not support HTTPS, turn on **Allow
unencrypted LAN audio** beside its endpoint, then run **Check & Save**. This
permission accepts only literal private IPv4 addresses (such as `192.168.x.x`)
and IPv6 unique-local addresses; public addresses and hostnames still require
HTTPS. Recordings, returned transcripts, and an optional API key can be
observed in transit while this option is enabled.

Previews confirm only text that agrees across consecutive passes, then seek to
the first unconfirmed word with a short audio lookback. Decode work therefore
stays bounded during normal long dictations. `mode = "auto"` limits repeated
requests to providers that run on your machine (whisper.cpp and downloaded
models).
Cloud and self-hosted HTTP providers bill and rate-limit per request; choose
`"always"` only if you accept roughly one request per `interval_ms` for the
length of each recording. Realtime providers publish their own deltas and
ignore this section.

The **Live Feedback** group in General starts with three presets. **Automatic**
is recommended: local streaming models show their own live hypotheses, local
batch models use revisable previews, and network batch models wait until
recording stops. **Always Live** enables repeated previews for compatible
network models and clearly warns that recent audio will be sent repeatedly.
**Final Only** waits until recording stops for every batch model.

Turn on **Expert Mode** in General to adjust preview timing and stabilization,
text-insertion timing, capture details, model identifiers, server endpoints,
and processor selection. Optional defaults stay out of the way when Expert Mode
is off, while customized settings remain visible so the active configuration is
never obscured.

The `{audio_file}` placeholder in whisper-cpp args is replaced with the path to the recorded WAV file.
If the placeholder is omitted, the WAV path is appended as the final argument.

Values explicitly set in `config.toml` are normally preserved across upgrades.
The former `<Super>space` default is the one exception because it collides with
GNOME's input-source switcher; it is migrated in memory to `<Super><Alt>d`.

The shortcut can be one button when that button is safe to reserve globally.
Voxkey accepts unmodified function keys (`F1` through `F35`), `Pause`, `Print`,
and dedicated hardware keys such as Record, Dictate, or microphone-mute. A
programmable mouse or keyboard button mapped to an otherwise unused key such
as `F24` is a practical choice. Bare letters, digits, punctuation, Space,
Return, navigation keys, and modifier-only presses are intentionally rejected
because reserving them globally would interfere with normal typing or desktop
navigation. GNOME's portal remains the final authority and can reject a key
that conflicts with an existing desktop shortcut.

The Settings app displays both the configured accelerator and the binding
description returned by the desktop. If the binding is changed in GNOME's
Global Shortcuts settings, Voxkey follows the portal update without requiring
a restart.

Common settings (shortcut, transcription provider, API keys, microphone, and
dictionary) can be changed through the settings GUI while Voxkey is Idle. Stop
or cancel an active dictation first so changing a setting can never discard it.
Advanced settings such as `tail_capture_ms`, `max_recording_seconds`, and
`mute_output_while_recording` are config-file-only and require restarting the
daemon (or using "Reload Config") to take effect.

## Usage

1. Open Voxkey from your app launcher, or run `voxkey-settings`; the service starts automatically
2. Configure your transcription provider and API key
3. Press `Super+Alt+D` to start dictating, press again to stop
4. Transcribed text is typed into the focused application

## Architecture

```
voxkey/
├── src/           Dictation daemon (shortcuts, recording, transcription, injection)
├── voxkey-ipc/    Shared D-Bus interface types and proxy definitions
└── voxkey-settings/  GTK4+libadwaita settings GUI
```

The daemon, settings GUI, and GNOME Shell extension communicate over D-Bus
(`io.github.hy26v.Voxkey.Daemon`). The daemon manages portal sessions, audio
recording, transcription dispatch, text injection, and serialized control
requests. The settings GUI sends configuration updates and owns the service
lifecycle; the Shell extension subscribes to state and telemetry and provides
the everyday dictation controls. The daemon monitors the GUI's D-Bus
application name so it also shuts down after a GUI crash or forced termination.

## Contributing

Contributions are welcome, especially new transcription backends. See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## Star History

[![Star History Chart](https://api.star-history.com/svg?repos=hy26v/voxkey&type=Date)](https://star-history.com/#hy26v/voxkey&Date)

## License

[MIT](LICENSE)
