---
name: gnome-shell-extensions
description: >-
  Develop and review the Voxkey GNOME Shell extension (Quick Settings tile,
  recording capsule, D-Bus control). Use when editing gnome-shell-extension/,
  extension.js, metadata.json, stylesheet.css, Shell GJS, QuickMenuToggle,
  SystemIndicator, or EGO/extension review practices.
paths:
  - "gnome-shell-extension/**"
  - "voxkey-settings/src/shell_extension.rs"
  - "docs/fedora-vm-testing.md"
  - "tests/test_packaging_safety.py"
---

# GNOME Shell Extensions (Voxkey)

Voxkey ships an ESM Shell extension (GNOME 45–50) that talks to the daemon over D-Bus and exposes a Quick Settings control surface.

## Project facts

| Item | Value |
|------|--------|
| Path | `gnome-shell-extension/` |
| UUID | `voxkey@hy26v.github.io` |
| Entry | `extension.js` (default-export `Extension` subclass) |
| Install | `make -C gnome-shell-extension install` |
| Shell targets | 45–50 in `metadata.json` |
| D-Bus | `io.github.hy26v.Voxkey.Daemon` / `/io/github/hy26v/Voxkey/Daemon` / `io.github.hy26v.Voxkey.Daemon1` |
| UI API | `QuickMenuToggle` + `SystemIndicator` via `quickSettings.js` |

## Hard rules (EGO / gjs.guide best practices)

These are required quality rules for Shell extension code, including AI-assisted edits:

1. Create/connect only in `enable()`; fully reverse in `disable()` (destroy actors, remove timeouts, drop refs).
2. No speculative `try/catch` around `destroy`/`disconnect`/`GLib.Source.remove`.
3. No `?.()` or `typeof === 'function'` guards on APIs that exist on the targeted Shell version.
4. No `_destroyed` / `_enabled` boolean lifecycle flags; null the instance after destroy.
5. Override widget `destroy()`; do not connect a `destroy` signal solely for cleanup.
6. Symbolic `St.Icon` / `icon_name` only — no emoji icons, no ASCII progress bars.
7. Prefer D-Bus to the Voxkey daemon over spawning subprocesses from the Shell process.
8. Keep `enable()`/`disable()` adjacent and the entry class small; prefer modules over one giant file when splitting.
9. Remove an existing timeout before creating a replacement; put remove+add next to each other.
10. Do not leave placeholder empty `enable`/`disable` stubs.

Authoritative LLM-oriented reference (raw Markdown for agents):

https://gitlab.gnome.org/World/javascript/gjs-guide/-/raw/main/docs/extensions/review-guidelines/best-practices.md

Also read: [EGO review guidelines](https://gjs.guide/extensions/review-guidelines/review-guidelines.html)

## Voxkey-specific guidance

- The extension must not synthesize the global shortcut; call daemon methods (`StartDictation`, `StopDictation`, `CancelDictation`, …) and stay on the daemon state machine.
- Respect focus delay before insertion (`QUICK_SETTINGS_FOCUS_DELAY_MS`) when finishing from Quick Settings.
- Capsule is for capture lifecycle only (recording/streaming/transcribing); do not keep it up through inject/error-only states.
- Match state strings with `src/state.rs`.
- On Wayland, reload requires a new Shell session — prefer reboot for agents
  (GDM autologin); see `docs/fedora-vm-testing.md`. Do not claim a settings
  restart reloads the extension.

## When changing Shell APIs

Check the matching upgrade guide before using new APIs or style class names:

- https://gjs.guide/extensions/upgrading/gnome-shell-45.html … through …
- https://gjs.guide/extensions/upgrading/gnome-shell-50.html

Quick Settings topic (closest to this extension): https://gjs.guide/extensions/topics/quick-settings.html

## More detail

- [references/resources.md](references/resources.md)
- [references/best-practices-checklist.md](references/best-practices-checklist.md)
