# Voxkey

Linux voice dictation daemon for Wayland with multiple speech-to-text backends.

## Project structure

Rust workspace with three crates:
- `voxkey` (root) -- the daemon: records audio, runs transcription, injects text
- `voxkey-ipc` -- D-Bus IPC library shared between daemon and settings GUI
- `voxkey-settings` -- GTK settings GUI

## Transcription backends

Selectable per-user via the settings GUI or `~/.config/voxkey/config.toml`. Default is `whisper-cpp` (see `voxkey-ipc::TranscriberProvider`):
- **whisper.cpp** -- offline, invoked as a subprocess; user supplies the binary
- **Parakeet** -- offline, in-process via sherpa-onnx; 25 languages; CPU or CUDA; models downloaded on demand
- **Mistral** -- cloud HTTP API (batch)
- **Mistral Realtime** -- cloud WebSocket API (streaming, text injected as deltas arrive)

## Build and test

```bash
cargo build
cargo test          # Rust unit tests
```

Integration tests are Python/pytest in `tests/`:
```bash
pip install -r tests/requirements.txt
pytest tests/
```

Integration tests require a running D-Bus session and Wayland compositor.

## Fedora GNOME Boxes test VM

Use the Fedora GNOME Boxes VM for runtime, GUI, GNOME Shell, RPM, and any
task-specific testing that must not affect the host. The durable workflow is in
[`docs/fedora-vm-testing.md`](docs/fedora-vm-testing.md). Read it before working
with the VM. Access is password-free for agents (SSH key + GDM autologin +
limited NOPASSWD sudo); if that breaks, run `./scripts/vm-repair-access.sh`.
Never store VM passwords or other credentials in the repository.

## Releasing

The remote repo should only contain tagged, squash-committed versions. Before pushing a version deemed good enough to tag:

1. Squash everything between the last tag and HEAD into a single commit
2. Tag that commit with `vX.Y.Z`
3. Push the squashed commit and tag together

```bash
git reset --soft <last-tag>
git commit -m "Voxkey X.Y.Z: <summary>"
git tag vX.Y.Z
git push --force-with-lease && git push origin vX.Y.Z
```

Pushing the tag triggers CI to build the RPM and create a GitHub Release. The release notes are generated from the squashed commit message body. Write it strictly from the end user's perspective: what they can now do, what's fixed, what changed for them. Never include implementation details (libraries, file paths, build system changes, CI changes, code-level fixes). A user who doesn't read code should understand every line.

## Packaging

RPM spec is in `voxkey.spec`. Build artifacts go to `rpmbuild/`.

All local system installations must also go through an RPM so package ownership
stays correct. Use `./scripts/local-install.sh`; never use `cargo install` or
copy build artifacts directly into `/usr/bin`.
