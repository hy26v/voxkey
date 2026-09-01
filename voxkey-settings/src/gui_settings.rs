// ABOUTME: Persists GUI-specific settings like hide-on-close and expert mode.
// ABOUTME: Uses a plain text file in XDG_CONFIG_HOME/voxkey/.

use std::path::PathBuf;

const HIDE_ON_CLOSE_FILE: &str = "hide_on_close";
const EXPERT_MODE_FILE: &str = "expert_mode";
const SHELL_EXTENSION_ONBOARDED_FILE: &str = "shell_extension_onboarded";
const LAST_PAGE_FILE: &str = "last_page";
const DICTIONARY_TAB_FILE: &str = "dictionary_tab";
const WINDOW_STATE_FILE: &str = "window_state";
const HISTORY_FILTER_FILE: &str = "history_filter";
const DEFAULT_PAGE: &str = "history";
const DEFAULT_DICTIONARY_TAB: &str = "replacements";
const DEFAULT_HISTORY_FILTER: &str = "all";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowState {
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

fn path() -> Option<PathBuf> {
    path_from(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

pub fn config_directory() -> Option<PathBuf> {
    config_directory_from(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

fn config_directory_from(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    path_from(xdg_config_home, home)
        .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
}

fn path_from(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let config_dir = xdg_config_home
        .filter(|path| !path.is_empty() && std::path::Path::new(path).is_absolute())
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(|path| !path.is_empty() && std::path::Path::new(path).is_absolute())
                .map(|path| PathBuf::from(path).join(".config"))
        })?;
    Some(config_dir.join("voxkey").join(HIDE_ON_CLOSE_FILE))
}

fn shell_extension_onboarded_path() -> Option<PathBuf> {
    path().map(|path| path.with_file_name(SHELL_EXTENSION_ONBOARDED_FILE))
}

fn expert_mode_path() -> Option<PathBuf> {
    path().map(|path| path.with_file_name(EXPERT_MODE_FILE))
}

fn last_page_path() -> Option<PathBuf> {
    path().map(|path| path.with_file_name(LAST_PAGE_FILE))
}

fn dictionary_tab_path() -> Option<PathBuf> {
    path().map(|path| path.with_file_name(DICTIONARY_TAB_FILE))
}

fn window_state_path() -> Option<PathBuf> {
    path().map(|path| path.with_file_name(WINDOW_STATE_FILE))
}

fn history_filter_path() -> Option<PathBuf> {
    path().map(|path| path.with_file_name(HISTORY_FILTER_FILE))
}

fn known_page(page: &str) -> Option<&str> {
    matches!(
        page,
        "history" | "transcription" | "audio" | "dictionary" | "permissions" | "general"
    )
    .then_some(page)
}

fn known_dictionary_tab(tab: &str) -> Option<&str> {
    matches!(tab, "replacements" | "vocabulary").then_some(tab)
}

fn known_history_filter(filter: &str) -> Option<&str> {
    matches!(
        filter,
        "all" | "pinned" | "completed" | "needs-attention" | "cancelled"
    )
    .then_some(filter)
}

fn load_bool_setting(path: Option<PathBuf>, default: bool) -> bool {
    path.and_then(|path| std::fs::read_to_string(path).ok())
        .map(|value| match value.trim() {
            "true" => true,
            "false" => false,
            _ => default,
        })
        .unwrap_or(default)
}

fn save_bool_setting(path: Option<PathBuf>, value: bool) {
    let Some(path) = path else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, if value { "true" } else { "false" });
}

pub fn load_hide_on_close() -> bool {
    load_bool_setting(path(), true)
}

pub fn save_hide_on_close(value: bool) {
    save_bool_setting(path(), value);
}

pub fn load_expert_mode() -> bool {
    load_bool_setting(expert_mode_path(), false)
}

pub fn save_expert_mode(value: bool) {
    save_bool_setting(expert_mode_path(), value);
}

pub fn load_last_page() -> String {
    last_page_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|page| known_page(page.trim()).map(str::to_string))
        .unwrap_or_else(|| DEFAULT_PAGE.to_string())
}

pub fn save_last_page(page: &str) {
    let Some(page) = known_page(page) else {
        return;
    };
    let Some(path) = last_page_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("{page}\n"));
}

pub fn load_dictionary_tab() -> String {
    dictionary_tab_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|tab| known_dictionary_tab(tab.trim()).map(str::to_string))
        .unwrap_or_else(|| DEFAULT_DICTIONARY_TAB.to_string())
}

pub fn save_dictionary_tab(tab: &str) {
    let Some(tab) = known_dictionary_tab(tab) else {
        return;
    };
    let Some(path) = dictionary_tab_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("{tab}\n"));
}

pub fn load_history_filter() -> String {
    history_filter_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|filter| known_history_filter(filter.trim()).map(str::to_string))
        .unwrap_or_else(|| DEFAULT_HISTORY_FILTER.to_string())
}

pub fn save_history_filter(filter: &str) {
    let Some(filter) = known_history_filter(filter) else {
        return;
    };
    let Some(path) = history_filter_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("{filter}\n"));
}

fn parse_window_state(value: &str) -> Option<WindowState> {
    let mut values = value.split_whitespace();
    let width = values.next()?.parse().ok()?;
    let height = values.next()?.parse().ok()?;
    let maximized = match values.next()? {
        "true" => true,
        "false" => false,
        _ => return None,
    };
    if values.next().is_some()
        || !(360..=16_384).contains(&width)
        || !(300..=16_384).contains(&height)
    {
        return None;
    }
    Some(WindowState {
        width,
        height,
        maximized,
    })
}

pub fn load_window_state() -> Option<WindowState> {
    window_state_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|value| parse_window_state(&value))
}

pub fn save_window_state(state: WindowState) {
    let Some(path) = window_state_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        path,
        format!("{} {} {}\n", state.width, state.height, state.maximized),
    );
}

pub fn shell_extension_onboarded() -> bool {
    shell_extension_onboarded_path().is_some_and(|path| path.is_file())
}

pub fn mark_shell_extension_onboarded() -> std::io::Result<()> {
    let path = shell_extension_onboarded_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No absolute user configuration directory is available",
        )
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, "enabled\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_settings_path_does_not_panic_without_home() {
        let result = std::panic::catch_unwind(|| path_from(None, None));

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);
        assert_eq!(
            path_from(Some(""), Some("/home/test")),
            Some(PathBuf::from("/home/test/.config/voxkey/hide_on_close"))
        );
        assert_eq!(
            path_from(Some("relative-config"), Some("/home/test")),
            Some(PathBuf::from("/home/test/.config/voxkey/hide_on_close"))
        );
    }

    #[test]
    fn relative_home_does_not_redirect_gui_settings_into_the_working_directory() {
        assert_eq!(path_from(None, Some("relative-home")), None);
        assert_eq!(path_from(None, Some("  \t")), None);
    }

    #[test]
    fn shell_extension_marker_is_next_to_other_gui_settings() {
        let hide_on_close = path_from(Some("/config"), Some("/home/test")).unwrap();

        assert_eq!(
            hide_on_close.with_file_name(SHELL_EXTENSION_ONBOARDED_FILE),
            PathBuf::from("/config/voxkey/shell_extension_onboarded")
        );
    }

    #[test]
    fn configuration_directory_contains_daemon_and_gui_settings() {
        assert_eq!(
            config_directory_from(Some("/config"), Some("/home/test")),
            Some(PathBuf::from("/config/voxkey"))
        );
        assert_eq!(config_directory_from(None, None), None);
    }

    #[test]
    fn expert_mode_is_stored_next_to_other_gui_settings() {
        let hide_on_close = path_from(Some("/config"), Some("/home/test")).unwrap();

        assert_eq!(
            hide_on_close.with_file_name(EXPERT_MODE_FILE),
            PathBuf::from("/config/voxkey/expert_mode")
        );
    }

    #[test]
    fn last_page_accepts_only_pages_the_window_can_show() {
        assert_eq!(known_page("history"), Some("history"));
        assert_eq!(known_page("permissions"), Some("permissions"));
        assert_eq!(known_page(" history "), None);
        assert_eq!(known_page("secrets"), None);
    }

    #[test]
    fn last_page_is_stored_next_to_other_gui_settings() {
        let hide_on_close = path_from(Some("/config"), Some("/home/test")).unwrap();

        assert_eq!(
            hide_on_close.with_file_name(LAST_PAGE_FILE),
            PathBuf::from("/config/voxkey/last_page")
        );
    }

    #[test]
    fn dictionary_tab_accepts_only_visible_dictionary_views() {
        assert_eq!(known_dictionary_tab("replacements"), Some("replacements"));
        assert_eq!(known_dictionary_tab("vocabulary"), Some("vocabulary"));
        assert_eq!(known_dictionary_tab(" replacements "), None);
        assert_eq!(known_dictionary_tab("phrases"), None);
    }

    #[test]
    fn dictionary_tab_is_stored_next_to_other_gui_settings() {
        let hide_on_close = path_from(Some("/config"), Some("/home/test")).unwrap();

        assert_eq!(
            hide_on_close.with_file_name(DICTIONARY_TAB_FILE),
            PathBuf::from("/config/voxkey/dictionary_tab")
        );
    }

    #[test]
    fn window_state_is_stored_next_to_other_gui_settings() {
        let hide_on_close = path_from(Some("/config"), Some("/home/test")).unwrap();

        assert_eq!(
            hide_on_close.with_file_name(WINDOW_STATE_FILE),
            PathBuf::from("/config/voxkey/window_state")
        );
    }

    #[test]
    fn window_state_accepts_only_sensible_complete_values() {
        assert_eq!(
            parse_window_state("980 700 true\n"),
            Some(WindowState {
                width: 980,
                height: 700,
                maximized: true,
            })
        );
        assert_eq!(parse_window_state("200 700 false"), None);
        assert_eq!(parse_window_state("980 700 maybe"), None);
        assert_eq!(parse_window_state("980 700 false extra"), None);
    }

    #[test]
    fn history_filter_accepts_only_visible_filter_choices() {
        for filter in ["all", "pinned", "completed", "needs-attention", "cancelled"] {
            assert_eq!(known_history_filter(filter), Some(filter));
        }
        assert_eq!(known_history_filter("issues"), None);
        assert_eq!(known_history_filter(" completed "), None);
    }

    #[test]
    fn history_filter_is_stored_next_to_other_gui_settings() {
        let hide_on_close = path_from(Some("/config"), Some("/home/test")).unwrap();

        assert_eq!(
            hide_on_close.with_file_name(HISTORY_FILTER_FILE),
            PathBuf::from("/config/voxkey/history_filter")
        );
    }
}
