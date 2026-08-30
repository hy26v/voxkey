# Changelog

All notable changes to Voxkey are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); each entry is written
from the user's perspective.

## [Unreleased]

### Changed

- Large local-model downloads can now be cancelled from either model view in
  Settings. Voxkey stops the transfer safely, removes the incomplete file, and
  keeps any model files that already finished so a later retry can reuse them.
- Settings now stays responsive while checking large local models. Repeated
  checks for the same model share one result, and integrity scans run one at a
  time instead of competing for disk access.
- Slow model verification no longer fails on the ordinary five-second command
  deadline. If a check still cannot finish, both model views now offer a clear
  **Retry check** action instead of remaining disabled.
- Settings remains responsive while checking a transcription server or waiting
  for the desktop keyring. Other changes and service controls continue working,
  while credential saves and removals stay ordered and obsolete status checks
  no longer delay the current operation.
- Existing errors no longer reappear as fresh alerts whenever Settings opens,
  and repeated copies of the same failure alert only once. A warning beside
  **General** keeps unresolved details discoverable, while dismissing an older
  message can no longer erase a newer error.
- Model downloads now leave their busy state reliably when they finish, are
  cancelled, or fail. Both model views show the failed attempt and an immediate
  retry action, even when the same download error happens more than once.
- Custom Mistral Batch, Mistral Realtime, and Parakeet server endpoints are now
  checked before they are saved. Settings shows connection progress and keeps
  the address ready to correct or retry when the server cannot be reached.
- Parakeet servers on a trusted private network can now use unencrypted HTTP
  after you explicitly allow it beside the endpoint. Voxkey keeps it off by
  default, warns that audio and transcripts are unencrypted, and still requires
  HTTPS for public addresses and Mistral.
- Settings now offers simple **Automatic**, **Always Live**, and **Final Only**
  feedback presets. Expert Mode reveals detailed model, preview, audio, typing,
  and troubleshooting controls, while custom values remain visible so they are
  never hidden by accident.
- Dictation status and service startup problems now use concise, action-oriented
  language instead of internal service states and technical error messages.
- When GNOME cannot enable Voxkey's Shell controls immediately, the settings
  app now explains that a logout is required and leaves the decision to you.
  Choosing **Log Out…** still opens GNOME's confirmation dialog.

### Security

- API keys are no longer readable from Voxkey's D-Bus interface: the settings
  app can only see whether a key is stored, replace it, or remove it — the key
  itself never leaves the daemon.
- Mistral Realtime endpoints must use encrypted `wss://` unless they point at
  this machine (`localhost`, `127.0.0.1`, or `[::1]`), so the API key cannot
  travel over an unencrypted connection.

## [0.6.2] - 2026-08-13

Recording-shortcut changes now take effect on GNOME.

- Replace the active GNOME portal binding when selecting a new recording shortcut
- Retire the previous shortcut so only the newly selected key toggles dictation
- Repair configurations that displayed a different shortcut from GNOME's active binding

## [0.6.1] - 2026-08-13

One-button dictation and an updated local Parakeet runtime.

- Use a safe function or dedicated media key by itself as the recording shortcut
- See the active GNOME portal binding and follow desktop-side shortcut changes live
- Run local Parakeet transcription directly through the current sherpa-onnx runtime
- Remove packaged private inference libraries cleanly when uninstalling Voxkey

## [0.6.0] - 2026-08-13

A richer GNOME experience with safer, more reliable dictation.

- See recording status and corrected live previews in the GNOME Shell status capsule
- Manage transcription history, dictionary rules, models, microphones, and permissions in the redesigned settings app
- Keep Mistral API keys securely in the system keyring
- Recover safely from portal, recording, transcription, and screen-lock failures
- Use stronger shortcut validation and fail-closed keyboard injection

## [0.5.0] - 2026-03-01

Reliability improvements and configurable text insertion speed.

- Choose how fast text is inserted at your cursor, from 0 to 50 milliseconds per character
- Dictation recovers gracefully from transient errors instead of requiring a restart

## [0.4.0] - 2026-03-01

Faster text insertion.

- Text appears at the cursor more quickly after you stop speaking

## [0.3.0] - 2026-02-28

Dictation continues working after the screen unlocks.

- Voxkey reconnects automatically after a screen lock or compositor restart, without needing to restart the daemon

## [0.2.0] - 2026-02-23

Offline dictation with Parakeet.

- Speak in 25 languages with high-accuracy local transcription powered by NVIDIA Parakeet
- Download Parakeet models directly from the settings app, with progress shown live
- Choose between CPU and CUDA acceleration for transcription
- No internet connection or API key required for Parakeet

## [0.1.0] - 2026-02-11

Initial release.
