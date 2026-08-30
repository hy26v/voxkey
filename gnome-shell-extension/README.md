# Voxkey GNOME Shell Extension

Adds a Voxkey tile to GNOME Quick Settings that controls the running daemon
and serves as the primary day-to-day dictation interface.

## What it shows

- Click the tile to start dictation while idle or finish the active recording.
- The submenu provides explicit Start, Finish, and Cancel actions. Those calls
  enter the daemon's serialized event loop alongside the global shortcut, so
  they cannot bypass its state machine.
- Subtitle describes the current activity in user-facing terms: Ready,
  Listening, Listening and transcribing, Transcribing, Typing transcription,
  Restoring desktop access, or "Desktop access needed".
- The tile lights up while a dictation is in progress.
- An interactive status capsule appears 24 px above the usable
  bottom edge while the daemon is recording, streaming, or transcribing. It
  disappears before insertion begins, so completed dictation leaves no
  overlay behind. The capsule stays centered on the display containing the
  focused app, falls back to the primary display when no app is focused,
  avoids reserved panel/dock space, and remains visible over fullscreen
  windows. In the GNOME overview it moves above the app dash instead of
  covering it.
- During capture, the capsule shows elapsed time, the configured provider and
  microphone, and a meter driven by real microphone samples. Cancel and
  Finish are available without opening Quick Settings. The final capture
  duration remains visible while transcription finishes.
- A multi-line area shows the current corrected transcription hypothesis. Realtime
  providers update it from incoming deltas; batch providers repeatedly process
  the growing recording and replace the preview as their hypothesis changes.
  Dictionary replacements are applied before every preview is displayed.
- The submenu previews the last completed transcript before offering copy or
  reinsert actions; selecting the preview opens its History. The active provider,
  microphone, and desktop shortcut rows open the matching Settings page directly.
- Recoverable daemon failures remain available in Quick Settings. Selecting the
  summary opens Settings, where the full details can be viewed; Copy Error and
  Dismiss actions remain alongside it without reopening the recording capsule.

## Cancellation behavior

Batch cancellation deletes the temporary recording and aborts its pending
transcription. Realtime cancellation discards provider output that has not yet
been committed. Text already inserted as completed realtime deltas cannot be
removed safely, so Voxkey leaves it in the focused application.

The extension never synthesizes the global shortcut. Its D-Bus control calls
are acknowledged by the same daemon event loop that processes portal shortcut
events, including session-generation checks during screen lock and recovery.

## Install (per-user)

```bash
make -C gnome-shell-extension install
```

Then in a GNOME session, log out and back in (or run
`Alt+F2 -> r` on X11) and enable the extension via the Extensions app
or `gnome-extensions enable voxkey@hy26v.github.io`.

The Fedora RPM installs this extension system-wide. On the first launch of the
packaged Voxkey settings UI, Voxkey asks GNOME Shell to enable it for the
current user and records that successful onboarding. A later manual disable is
not overridden.

## Uninstall

```bash
make -C gnome-shell-extension uninstall
```

## GNOME Shell version

GNOME 45 through 50 are declared compatibility targets in `metadata.json`.
Older shells (43, 44) used a different Quick Settings API and are not
supported. GNOME 50 is exercised in the disposable release-gate VM; the other
declared versions still have static packaging coverage only
(`gnome-extensions pack`, JSON/JS syntax).
