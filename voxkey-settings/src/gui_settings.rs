// ABOUTME: Persists GUI-specific settings like the hide-on-close preference.
// ABOUTME: Uses a plain text file in XDG_CONFIG_HOME/voxkey/.

use std::path::PathBuf;

fn path() -> PathBuf {
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").expect("HOME not set");
            PathBuf::from(home).join(".config")
        });
    config_dir.join("voxkey").join("hide_on_close")
}

pub fn load_hide_on_close() -> bool {
    std::fs::read_to_string(path())
        .map(|s| s.trim() != "false")
        .unwrap_or(true)
}

pub fn save_hide_on_close(value: bool) {
    let p = path();
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(p, if value { "true" } else { "false" });
}
