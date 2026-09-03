# Contributing to Voxkey

Contributions are welcome. Contributions of new transcription backends are especially appreciated.

## Adding a Transcription Backend

Voxkey uses a provider-based architecture. Each transcription backend is an enum variant
in `Transcriber` with its own configuration struct. To add a new provider, touch these
files in order:

### 1. Define config types (`voxkey-ipc/src/lib.rs`)

- Add an entry to `CLOUD_PROVIDERS` in `voxkey-ipc/src/cloud.rs` with the live
  protocol, default endpoint, model, and keyring service
- Add a variant to the `TranscriberProvider` enum if this is a named service
- Reuse `CloudSttConfig` for non-secret fields (`model`, `endpoint`) unless the
  provider needs a dedicated struct
- Implement `Default` with sensible defaults
- Add the config struct as a field on `TranscriberConfig` if it is not already
  covered by the shared cloud catalog

### 2. Implement the backend (`src/transcriber.rs`)

- Add a variant to the `Transcriber` enum holding the runtime fields it needs
- Handle the new provider in `from_config()`
- If this is a streaming/realtime provider, update `is_streaming()`
- Write the transcription function - for batch providers, accept a WAV path and return text;
  for streaming providers, see `src/streaming.rs` for the WebSocket pattern
- Add a match arm in `transcribe()`

### 3. Wire up the settings GUI (`voxkey-settings/src/window.rs`)

- Add the provider name to the combo row string list
- Add entry rows for provider-specific fields (model, endpoint, etc.)
- If the provider needs a credential, add an explicitly allowlisted Secret
  Service entry and use the shared API-key controls. Credentials must be
  injected only into the daemon's runtime copy: never persist them in
  `config.toml` or expose them through D-Bus configuration JSON.
- Update `apply_transcriber_config_to_widgets()` to show/hide fields for your provider
- Update `wire_transcriber_actions()` to read/write your provider's config

### 4. Add tests

- Config round-trip test in `voxkey-ipc`
- `from_config()` variant creation test in `src/transcriber.rs`
- Integration test if feasible

### Key design notes

- **All provider configs coexist.** The config file holds settings for every provider
  simultaneously. Switching providers doesn't lose settings for other providers.
- **Batch vs. streaming.** Batch providers receive a WAV file path and return text.
  Streaming providers receive audio chunks over a WebSocket and emit text incrementally.
  The daemon routes to different code paths based on `is_streaming()`.
- **Provider names** use kebab-case in config files (e.g. `"my-provider"`).
- **Provider credentials are runtime-only.** The system keyring is the source
  of truth; configuration structs and debug output must remain secret-free.

## General Guidelines

- Keep changes focused - one feature or fix per PR
- Follow the existing code style
- Add tests for new functionality
- Update the README Configuration section if you add new config fields

## Development Setup

```bash
sudo dnf install \
  gtk4-devel libadwaita-devel alsa-lib-devel libxkbcommon-devel \
  clippy rustfmt python3-pytest python3-pytest-asyncio python3-dbus-next \
  python3-websockets \
  pulseaudio-utils pipewire-utils
cargo build
```

Before opening a pull request, run the same automated gate as CI:

```bash
./scripts/verify all
```

The `all` gate delegates its integration phase to `./scripts/ci-integration`,
which starts a private PipeWire/PulseAudio-compatible session in addition to
its private D-Bus portal and isolated Voxkey state. Direct `pytest` execution
is refused before the virtual microphone can change the caller's live default
input. In a headless Fedora container, use
`dbus-run-session -- ./scripts/ci-integration`.

Release candidates have an additional disposable-GNOME test and soak gate; see
[Release readiness](docs/release-readiness.md).

## Preview quality harness

The live preview is user-visible output, so changes to segmentation, preview
strategies, or decoding must be graded against the committed baselines in
`scripts/preview_baselines/`. The grading logic (normalization, stability
lag, baseline regression policy, fixture ground truth) runs in the normal
integration suite. The full sweep additionally drives a real whisper.cpp
binary over the fixture audio and needs a local whisper.cpp build and model:

```bash
VOXKEY_TEST_WHISPER_BIN=/path/to/whisper-cli \
VOXKEY_TEST_WHISPER_MODEL=/path/to/ggml-model.bin \
  ./scripts/ci-integration tests/test_real_whisper_preview_quality.py
```

A run that intentionally improves quality updates a baseline with
`scripts/preview_quality.py --update-baseline`; commit the updated baseline
in the same change and say so in the PR description.

## Opening Issues

- **Bug reports**: use the bug template; include your Fedora version, desktop
  environment, and steps to reproduce
- **Keyboard/input problems** (no text injected, wrong characters, stuck
  modifiers, lockup): use the keyboard-issue template — it collects the
  compositor, portal, and journalctl details needed to act on these reports
- **New backend proposals**: open an issue describing the API and whether it's batch or streaming
