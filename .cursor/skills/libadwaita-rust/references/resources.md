# Libadwaita / gtk-rs resources

## Official

- [GNOME HIG](https://developer.gnome.org/hig/) (design; use with skill `gnome-hig`)
- [libadwaita documentation](https://gnome.pages.gitlab.gnome.org/libadwaita/)
- [GTK 4 documentation](https://docs.gtk.org/gtk4/)
- [gtk-rs GTK4 book](https://gtk-rs.org/gtk4-rs/stable/latest/book/)
- [Libadwaita chapter (gtk-rs book)](https://gtk-rs.org/gtk4-rs/stable/latest/book/libadwaita.html)
- [gtk4-rs API docs](https://gtk-rs.org/gtk4-rs/stable/latest/docs/)
- [libadwaita-rs API docs](https://world.pages.gitlab.gnome.org/Rust/libadwaita-rs/stable/latest/docs/libadwaita/)

## Widgets Voxkey already uses heavily

- PreferencesGroup, ActionRow, SwitchRow, ComboRow, EntryRow, PasswordEntryRow
- Toast / ToastOverlay
- Banner
- StatusPage
- NavigationSplitView
- ApplicationWindow

Check libadwaita docs for the exact widget before adding a novel pattern.

## Project entry points

- `voxkey-settings/src/main.rs` — application + `--page=` deep links
- `voxkey-settings/src/window.rs` — main UI construction
- `voxkey-settings/src/shell_extension.rs` — Shell extension onboarding from Settings
- `voxkey-settings/Cargo.toml` — crate feature pins
