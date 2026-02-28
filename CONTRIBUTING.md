# Contributing to Voxkey

Contributions are welcome. Contributions of new transcription backends are especially appreciated.

## Adding a Transcription Backend

Voxkey uses a provider-based architecture. Each transcription backend is an enum variant
in `Transcriber` with its own configuration struct. To add a new provider, touch these
files in order:

### 1. Define config types (`voxkey-ipc/src/lib.rs`)

- Add a config struct (e.g. `DeepgramConfig`) with fields like `api_key`, `model`, `endpoint`
- Implement `Default` with sensible defaults
- Add a variant to the `TranscriberProvider` enum
- Add the config struct as a field on `TranscriberConfig`

### 2. Implement the backend (`src/transcriber.rs`)

- Add a variant to the `Transcriber` enum holding the runtime fields it needs
- Handle the new provider in `from_config()`
- If this is a streaming/realtime provider, update `is_streaming()`
- Write the transcription function - for batch providers, accept a WAV path and return text;
  for streaming providers, see `src/streaming.rs` for the WebSocket pattern
- Add a match arm in `transcribe()`

### 3. Wire up the settings GUI (`voxkey-settings/src/window.rs`)

- Add the provider name to the combo row string list
- Add entry rows for provider-specific fields (API key, model, endpoint, etc.)
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

## General Guidelines

- Keep changes focused - one feature or fix per PR
- Follow the existing code style
- Add tests for new functionality
- Update the README Configuration section if you add new config fields

## Development Setup

```bash
sudo dnf install gtk4-devel libadwaita-devel alsa-lib-devel libxkbcommon-devel
cargo build
cargo test
```

## Opening Issues

- **Bug reports**: include your Fedora version, desktop environment, and steps to reproduce
- **New backend proposals**: open an issue describing the API and whether it's batch or streaming
