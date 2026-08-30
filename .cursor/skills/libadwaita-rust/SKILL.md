---
name: libadwaita-rust
description: >-
  Build and change Voxkey's GTK4 + libadwaita Rust settings app. Use when
  editing voxkey-settings, Adw widgets (PreferencesGroup, ActionRow, SwitchRow,
  ComboRow, EntryRow, Banner, Toast, NavigationSplitView, StatusPage), gtk-rs,
  or settings window layout/behavior.
paths:
  - "voxkey-settings/**"
---

# Libadwaita + GTK4 Rust (Voxkey Settings)

Settings live in `voxkey-settings/` as an `adw::Application` talking to the daemon via `voxkey-ipc` / zbus.

## Stack pins (follow Cargo.toml)

- `gtk4` with feature `v4_14`
- `libadwaita` (`adw`) with feature `v1_6`
- Import style already used: `use libadwaita as adw;` plus `adw::prelude::*` / `gtk4::prelude::*`

## Established UI patterns in this repo

Prefer extending existing builders in `src/window.rs`, `history.rs`, `dictionary.rs` rather than inventing new shells.

| Need | Use |
|------|-----|
| App / window | `adw::Application`, `adw::ApplicationWindow` |
| Primary chrome | `adw::NavigationSplitView` + sidebar pages |
| Setting clusters | `adw::PreferencesGroup` |
| Rows | `ActionRow`, `SwitchRow`, `ComboRow`, `EntryRow`, `PasswordEntryRow` |
| Transient feedback | `adw::ToastOverlay` + `adw::Toast` |
| Persistent notice | `adw::Banner` (e.g. shell-extension restart) |
| Empty / blocked states | `adw::StatusPage` |
| Icons | `gtk4::Image::from_icon_name("…-symbolic")` |

Deep-link pages via `--page=` / `win.show-page` (`history`, `transcription`, `audio`, `dictionary`, `permissions`, `general`). Keep that list in sync when adding pages.

## Implementation rules

1. Match HIG writing and density — load skill `gnome-hig` for design questions.
2. Do not introduce custom CSS themes that fight Adwaita.
3. Keep daemon I/O off the GTK thread; follow existing async/`DaemonHandle` patterns.
4. Shell extension enablement/onboarding belongs in `shell_extension.rs`; do not duplicate Shell GJS logic in Settings beyond D-Bus / GNOME APIs already used.
5. After UI RPM installs, reopen Settings in the Fedora VM; extension updates
   need a reboot (agents) or logout/login — see `docs/fedora-vm-testing.md`.

## Docs

- [gtk-rs book: Libadwaita](https://gtk-rs.org/gtk4-rs/stable/latest/book/libadwaita.html)
- [gtk4-rs API](https://gtk-rs.org/gtk4-rs/stable/latest/docs/)
- [libadwaita C docs](https://gnome.pages.gitlab.gnome.org/libadwaita/) (widget behavior source of truth)
- Optional modern Rust walkthrough: https://fromthearchitect.dev/posts/gnome-rust-part-1-getting-started/

## More detail

- [references/resources.md](references/resources.md)
