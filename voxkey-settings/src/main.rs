// ABOUTME: Entry point for the voxkey settings GUI.
// ABOUTME: GTK4+libadwaita application for configuring and monitoring the voxkey daemon.

mod daemon_client;
mod dictionary;
mod gui_settings;
mod history;
mod menu;
mod model_library;
mod shell_extension;
mod window;

use adw::prelude::*;
use libadwaita as adw;

fn requested_page(arguments: &[std::ffi::OsString]) -> Option<String> {
    arguments
        .iter()
        .filter_map(|argument| argument.to_str())
        .find_map(|argument| argument.strip_prefix("--page="))
        .filter(|page| {
            matches!(
                *page,
                "history" | "transcription" | "audio" | "dictionary" | "permissions" | "general"
            )
        })
        .map(str::to_string)
}

fn present_window(app: &adw::Application, page: Option<&str>) {
    let page = page
        .map(str::to_owned)
        .unwrap_or_else(gui_settings::load_last_page);
    let window = app
        .windows()
        .first()
        .cloned()
        .unwrap_or_else(|| window::build_window(app).upcast());
    // GtkApplicationWindow exposes its `win` action namespace after it is
    // presented. This ordering matters on a cold command-line launch; an
    // already-running window has already completed that setup.
    window.present();
    if let Err(error) = window.activate_action("win.show-page", Some(&page.to_variant())) {
        tracing::warn!("Could not open settings page {page}: {error}");
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let app = adw::Application::builder()
        .application_id(voxkey_ipc::SETTINGS_BUS_NAME)
        .flags(gtk4::gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    app.connect_activate(|app| {
        present_window(app, None);
    });
    app.connect_command_line(|app, command_line| {
        let page = requested_page(&command_line.arguments());
        present_window(app, page.as_deref());
        gtk4::glib::ExitCode::SUCCESS
    });

    app.run();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_page_deep_link_accepts_known_pages() {
        let arguments = ["voxkey-settings".into(), "--page=permissions".into()];
        assert_eq!(requested_page(&arguments).as_deref(), Some("permissions"));
    }

    #[test]
    fn settings_page_deep_link_ignores_unknown_pages() {
        let arguments = ["voxkey-settings".into(), "--page=secrets".into()];
        assert_eq!(requested_page(&arguments), None);
    }
}
