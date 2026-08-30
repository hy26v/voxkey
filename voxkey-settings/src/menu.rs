// ABOUTME: Builds the primary menu button shown in the settings window header bar.
// ABOUTME: Registers the About and Quit application actions, including the Ctrl+Q accelerator.

use adw::prelude::*;
use gtk4::prelude::*;
use libadwaita as adw;

pub const PAGE_SHORTCUTS: [(&str, &str); 6] = [
    ("history", "<Control>1"),
    ("transcription", "<Control>2"),
    ("audio", "<Control>3"),
    ("dictionary", "<Control>4"),
    ("general", "<Control>5"),
    ("permissions", "<Control>6"),
];
pub const GENERAL_SETTINGS_ACCELERATOR: &str = "<Control>comma";
pub const CLOSE_WINDOW_ACCELERATOR: &str = "<Control>w";
pub const MAIN_MENU_ACCELERATOR: &str = "F10";
pub const COPY_LATEST_ACCELERATOR: &str = "<Control><Shift>c";

const PAGE_SHORTCUT_TITLES: [&str; 6] = [
    "Open history",
    "Open transcription",
    "Open audio input",
    "Open dictionary",
    "Open general",
    "Open permissions",
];

pub fn register_page_shortcuts(app: &adw::Application) {
    for (page, accelerator) in PAGE_SHORTCUTS {
        let action = format!("win.show-page::{page}");
        if page == "general" {
            app.set_accels_for_action(&action, &[accelerator, GENERAL_SETTINGS_ACCELERATOR]);
        } else {
            app.set_accels_for_action(&action, &[accelerator]);
        }
    }
}

/// Register `app.about` and `app.quit` on `app`, bind `<Control>q` to
/// `app.quit`, and return a `MenuButton` with both actions in its menu.
pub fn setup_primary_menu(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
) -> gtk4::MenuButton {
    let close_action = gtk4::gio::SimpleAction::new("close-window", None);
    let window_for_close = window.clone();
    close_action.connect_activate(move |_, _| {
        window_for_close.close();
    });
    window.add_action(&close_action);
    app.set_accels_for_action("win.close-window", &[CLOSE_WINDOW_ACCELERATOR]);

    let shortcuts_action = gtk4::gio::SimpleAction::new("shortcuts", None);
    let app_for_shortcuts = app.clone();
    let window_for_shortcuts = window.clone();
    shortcuts_action.connect_activate(move |_, _| {
        show_shortcuts_window(&app_for_shortcuts, &window_for_shortcuts);
    });
    app.add_action(&shortcuts_action);
    app.set_accels_for_action("app.shortcuts", &["<Control>question"]);

    let about_action = gtk4::gio::SimpleAction::new("about", None);
    let window_for_about = window.clone();
    about_action.connect_activate(move |_, _| {
        show_about_dialog(&window_for_about);
    });
    app.add_action(&about_action);

    let quit_action = gtk4::gio::SimpleAction::new("quit", None);
    let app_for_quit = app.clone();
    quit_action.connect_activate(move |_, _| {
        app_for_quit.quit();
    });
    app.add_action(&quit_action);
    app.set_accels_for_action("app.quit", &["<Control>q"]);

    let menu = gtk4::gio::Menu::new();
    let help_section = gtk4::gio::Menu::new();
    help_section.append(Some("Keyboard shortcuts"), Some("app.shortcuts"));
    menu.append_section(None, &help_section);
    let dictation_section = gtk4::gio::Menu::new();
    dictation_section.append(Some("Copy latest transcription"), Some("win.copy-latest"));
    menu.append_section(None, &dictation_section);
    let application_section = gtk4::gio::Menu::new();
    application_section.append(Some("About Voxkey"), Some("app.about"));
    application_section.append(Some("Quit"), Some("app.quit"));
    menu.append_section(None, &application_section);

    let button = gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .primary(true)
        .tooltip_text("Main menu")
        .build();
    button.update_property(&[gtk4::accessible::Property::Label("Main menu")]);

    let open_menu_action = gtk4::gio::SimpleAction::new("open-menu", None);
    let button_for_action = button.clone();
    open_menu_action.connect_activate(move |_, _| {
        button_for_action.popup();
    });
    window.add_action(&open_menu_action);
    app.set_accels_for_action("win.open-menu", &[MAIN_MENU_ACCELERATOR]);

    button
}

fn show_shortcuts_window(app: &adw::Application, parent: &adw::ApplicationWindow) {
    let navigation_group = gtk4::ShortcutsGroup::builder().title("Navigation").build();
    for ((_, accelerator), title) in PAGE_SHORTCUTS.iter().zip(PAGE_SHORTCUT_TITLES) {
        let shortcut = gtk4::ShortcutsShortcut::builder()
            .accelerator(*accelerator)
            .title(title)
            .build();
        navigation_group.add_shortcut(&shortcut);
    }

    let application_group = gtk4::ShortcutsGroup::builder().title("Application").build();
    let show_shortcuts = gtk4::ShortcutsShortcut::builder()
        .accelerator("<Control>question")
        .title("Show keyboard shortcuts")
        .build();
    application_group.add_shortcut(&show_shortcuts);
    let search_history = gtk4::ShortcutsShortcut::builder()
        .accelerator("<Control>f")
        .title("Search history or dictionary")
        .build();
    application_group.add_shortcut(&search_history);
    let open_general = gtk4::ShortcutsShortcut::builder()
        .accelerator(GENERAL_SETTINGS_ACCELERATOR)
        .title("Open general settings")
        .build();
    application_group.add_shortcut(&open_general);
    let close_window = gtk4::ShortcutsShortcut::builder()
        .accelerator(CLOSE_WINDOW_ACCELERATOR)
        .title("Close window")
        .build();
    application_group.add_shortcut(&close_window);
    let open_menu = gtk4::ShortcutsShortcut::builder()
        .accelerator(MAIN_MENU_ACCELERATOR)
        .title("Open main menu")
        .build();
    application_group.add_shortcut(&open_menu);
    let copy_latest = gtk4::ShortcutsShortcut::builder()
        .accelerator(COPY_LATEST_ACCELERATOR)
        .title("Copy latest transcription")
        .build();
    application_group.add_shortcut(&copy_latest);
    let quit = gtk4::ShortcutsShortcut::builder()
        .accelerator("<Control>q")
        .title("Quit")
        .build();
    application_group.add_shortcut(&quit);

    let section = gtk4::ShortcutsSection::builder()
        .section_name("shortcuts")
        .title("Keyboard shortcuts")
        .build();
    section.add_group(&navigation_group);
    section.add_group(&application_group);

    let shortcuts = gtk4::ShortcutsWindow::builder()
        .application(app)
        .transient_for(parent)
        .modal(true)
        .title("Keyboard shortcuts")
        .default_width(560)
        .default_height(480)
        .build();
    shortcuts.add_section(&section);
    shortcuts.present();
}

fn show_about_dialog(window: &adw::ApplicationWindow) {
    let about = adw::AboutDialog::builder()
        .application_name("Voxkey")
        .application_icon("io.github.hy26v.Voxkey")
        .version(env!("CARGO_PKG_VERSION"))
        .developers(vec!["Daniel".to_string()])
        .website("https://github.com/hy26v/voxkey")
        .issue_url("https://github.com/hy26v/voxkey/issues")
        .license_type(gtk4::License::MitX11)
        .build();
    about.present(Some(window));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_shortcuts_are_unique_and_cover_primary_destinations() {
        let pages: Vec<_> = PAGE_SHORTCUTS.iter().map(|(page, _)| *page).collect();
        let accelerators: Vec<_> = PAGE_SHORTCUTS
            .iter()
            .map(|(_, accelerator)| *accelerator)
            .collect();

        assert_eq!(
            pages,
            [
                "history",
                "transcription",
                "audio",
                "dictionary",
                "general",
                "permissions"
            ]
        );
        assert_eq!(
            accelerators
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            accelerators.len()
        );
        assert_eq!(GENERAL_SETTINGS_ACCELERATOR, "<Control>comma");
        assert_eq!(CLOSE_WINDOW_ACCELERATOR, "<Control>w");
        assert_eq!(MAIN_MENU_ACCELERATOR, "F10");
        assert_eq!(COPY_LATEST_ACCELERATOR, "<Control><Shift>c");
    }
}
