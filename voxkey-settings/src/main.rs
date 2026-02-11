// ABOUTME: Entry point for the voxkey settings GUI.
// ABOUTME: GTK4+libadwaita application for configuring and monitoring the voxkey daemon.

mod daemon_client;
mod gui_settings;
mod window;

use libadwaita as adw;
use adw::prelude::*;

const APP_ID: &str = "io.github.hy26v.Voxkey";

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .build();

    app.connect_activate(|app| {
        if let Some(win) = app.windows().first() {
            win.set_visible(true);
            win.present();
        } else {
            let win = window::build_window(app);
            win.present();
        }
    });

    app.run();
}
