// ABOUTME: Builds the main settings window with status, configuration, and control groups.
// ABOUTME: Wires D-Bus property changes to widget updates and user actions to D-Bus method calls.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use adw::prelude::*;

use crate::daemon_client::{self, DaemonCommand, DaemonHandle, DaemonUpdate};
use crate::gui_settings;

pub fn build_window(app: &adw::Application) -> adw::ApplicationWindow {
    let (update_rx, handle) = daemon_client::connect();

    let toast_overlay = adw::ToastOverlay::new();

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

    let banner = adw::Banner::new("Daemon not running \u{2014} start voxkey first");
    banner.set_revealed(true);
    content.append(&banner);

    let scrolled = gtk4::ScrolledWindow::builder()
        .vexpand(true)
        .build();

    let clamp = adw::Clamp::builder()
        .maximum_size(600)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();

    let groups_box = gtk4::Box::new(gtk4::Orientation::Vertical, 24);

    // -- Status group --
    let status_group = adw::PreferencesGroup::builder()
        .title("Status")
        .build();

    let state_row = adw::ActionRow::builder()
        .title("State")
        .subtitle("Unknown")
        .build();
    let state_icon = gtk4::Image::from_icon_name("media-record-symbolic");
    state_icon.add_css_class("dim-label");
    state_row.add_prefix(&state_icon);

    let portal_row = adw::ActionRow::builder()
        .title("Portal")
        .subtitle("Unknown")
        .build();

    status_group.add(&state_row);
    status_group.add(&portal_row);
    groups_box.append(&status_group);

    // -- Dictation group --
    let dictation_group = adw::PreferencesGroup::builder()
        .title("Dictation")
        .build();

    let shortcut_label = gtk4::ShortcutLabel::new("");
    shortcut_label.set_valign(gtk4::Align::Center);

    let shortcut_row = adw::ActionRow::builder()
        .title("Shortcut")
        .subtitle("Click to change")
        .activatable(true)
        .build();
    shortcut_row.add_suffix(&shortcut_label);

    dictation_group.add(&shortcut_row);
    groups_box.append(&dictation_group);

    // -- Transcript group --
    let transcript_group = adw::PreferencesGroup::builder()
        .title("Last Transcript")
        .build();

    let copy_button = gtk4::Button::from_icon_name("edit-copy-symbolic");
    copy_button.set_valign(gtk4::Align::Center);
    copy_button.add_css_class("flat");
    transcript_group.set_header_suffix(Some(&copy_button));

    let transcript_view = gtk4::TextView::builder()
        .editable(true)
        .wrap_mode(gtk4::WrapMode::Word)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(12)
        .right_margin(12)
        .build();
    transcript_view.set_size_request(-1, 80);
    transcript_view.add_css_class("card");

    let transcript_buffer = transcript_view.buffer();

    transcript_group.add(&transcript_view);
    groups_box.append(&transcript_group);

    // -- Transcription group --
    let transcription_group = adw::PreferencesGroup::builder()
        .title("Transcription Engine")
        .build();

    let provider_model = gtk4::StringList::new(&[
        "whisper.cpp", "Mistral", "Mistral Realtime", "Parakeet v2", "Parakeet v3",
    ]);
    let provider_row = adw::ComboRow::builder()
        .title("Provider")
        .model(&provider_model)
        .build();

    transcription_group.add(&provider_row);

    // whisper.cpp sub-rows
    let command_row = adw::EntryRow::builder()
        .title("Command")
        .show_apply_button(true)
        .build();
    let args_row = adw::EntryRow::builder()
        .title("Arguments")
        .show_apply_button(true)
        .build();
    transcription_group.add(&command_row);
    transcription_group.add(&args_row);

    // Mistral / Mistral Realtime sub-rows (shared API key, provider-specific model list)
    let api_key_row = adw::PasswordEntryRow::builder()
        .title("API Key")
        .build();
    api_key_row.set_show_apply_button(true);

    let model_row = adw::EntryRow::builder()
        .title("Model")
        .show_apply_button(true)
        .build();
    let endpoint_row = adw::EntryRow::builder()
        .title("Endpoint")
        .show_apply_button(true)
        .build();
    transcription_group.add(&api_key_row);
    transcription_group.add(&model_row);
    transcription_group.add(&endpoint_row);

    // Parakeet sub-rows
    let execution_provider_model = gtk4::StringList::new(&["Auto", "CPU", "CUDA"]);
    let execution_provider_row = adw::ComboRow::builder()
        .title("Execution Provider")
        .model(&execution_provider_model)
        .build();

    let model_status_row = adw::ActionRow::builder()
        .title("Model Status")
        .subtitle("Unknown")
        .build();

    let download_button = gtk4::Button::with_label("Download");
    download_button.set_valign(gtk4::Align::Center);
    model_status_row.add_suffix(&download_button);

    let open_folder_button = gtk4::Button::from_icon_name("folder-open-symbolic");
    open_folder_button.set_valign(gtk4::Align::Center);
    open_folder_button.add_css_class("flat");
    model_status_row.add_suffix(&open_folder_button);

    let delete_model_button = gtk4::Button::from_icon_name("user-trash-symbolic");
    delete_model_button.set_valign(gtk4::Align::Center);
    delete_model_button.add_css_class("flat");
    delete_model_button.add_css_class("error");
    model_status_row.add_suffix(&delete_model_button);

    transcription_group.add(&execution_provider_row);
    transcription_group.add(&model_status_row);

    // Initially hide non-whisper.cpp rows (default provider)
    api_key_row.set_visible(false);
    model_row.set_visible(false);
    endpoint_row.set_visible(false);
    execution_provider_row.set_visible(false);
    model_status_row.set_visible(false);

    groups_box.append(&transcription_group);

    // Shared transcriber config state for building JSON from widgets
    let transcriber_state = Rc::new(RefCell::new(voxkey_ipc::TranscriberConfig::default()));
    // Guard to suppress send_transcriber_config during programmatic widget updates
    let updating_widgets = Rc::new(Cell::new(false));

    // -- Advanced group --
    let advanced_group = adw::PreferencesGroup::builder()
        .title("Advanced")
        .build();

    let reload_row = adw::ActionRow::builder()
        .title("Reload Configuration")
        .subtitle("Re-read config.toml from disk")
        .activatable(true)
        .build();
    let reload_icon = gtk4::Image::from_icon_name("view-refresh-symbolic");
    reload_row.add_suffix(&reload_icon);

    let clear_token_row = adw::ActionRow::builder()
        .title("Clear Portal Token")
        .subtitle("Force a fresh portal session on next restart")
        .activatable(true)
        .build();
    let clear_icon = gtk4::Image::from_icon_name("edit-clear-symbolic");
    clear_token_row.add_suffix(&clear_icon);

    let hide_on_close = Rc::new(Cell::new(gui_settings::load_hide_on_close()));

    let hide_on_close_row = adw::SwitchRow::builder()
        .title("Hide on close")
        .subtitle("Keep running in the background when the window is closed")
        .active(hide_on_close.get())
        .build();

    let hide_on_close_for_toggle = hide_on_close.clone();
    hide_on_close_row.connect_active_notify(move |row| {
        let value = row.is_active();
        hide_on_close_for_toggle.set(value);
        gui_settings::save_hide_on_close(value);
    });

    let quit_row = adw::ActionRow::builder()
        .title("Quit")
        .subtitle("Stop both the daemon and the settings app")
        .activatable(true)
        .build();
    let quit_icon = gtk4::Image::from_icon_name("application-exit-symbolic");
    quit_row.add_suffix(&quit_icon);

    advanced_group.add(&hide_on_close_row);
    advanced_group.add(&reload_row);
    advanced_group.add(&clear_token_row);
    advanced_group.add(&quit_row);
    groups_box.append(&advanced_group);

    clamp.set_child(Some(&groups_box));
    scrolled.set_child(Some(&clamp));
    content.append(&scrolled);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&adw::HeaderBar::new());
    toolbar_view.set_content(Some(&content));

    toast_overlay.set_child(Some(&toolbar_view));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Voxkey Settings")
        .default_width(480)
        .default_height(680)
        .content(&toast_overlay)
        .build();

    // -- Wire copy button --
    let buffer_for_copy = transcript_buffer.clone();
    let display = gdk::Display::default().expect("Could not get default display");
    let clipboard = display.clipboard();
    copy_button.connect_clicked(move |_| {
        let text = buffer_for_copy.text(
            &buffer_for_copy.start_iter(),
            &buffer_for_copy.end_iter(),
            false,
        );
        clipboard.set_text(&text);
    });

    // -- Wire up user actions --
    // Track the current trigger so the capture dialog knows the current value
    let current_trigger = Rc::new(RefCell::new(String::new()));
    wire_shortcut_capture(&shortcut_row, &shortcut_label, &current_trigger, &handle, &toast_overlay, &window);
    wire_transcriber_actions(
        &provider_row, &command_row, &args_row,
        &api_key_row, &model_row, &endpoint_row,
        &execution_provider_row, &model_status_row,
        &download_button, &delete_model_button, &open_folder_button,
        &transcriber_state, &updating_widgets, &handle,
    );
    wire_advanced_actions(&reload_row, &clear_token_row, &handle, &toast_overlay);

    // -- Wire quit button --
    let handle_for_quit = handle.clone();
    let app_for_quit = app.clone();
    quit_row.connect_activated(move |_| {
        handle_for_quit.send_quit_and_wait();
        app_for_quit.quit();
    });

    // -- Wire close-request for hide-on-close --
    let hide_on_close_for_close = hide_on_close.clone();
    window.connect_close_request(move |win| {
        if hide_on_close_for_close.get() {
            win.set_visible(false);
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });

    // -- Poll for D-Bus updates on the GTK main loop --
    let state_row = state_row.clone();
    let state_icon = state_icon.clone();
    let portal_row = portal_row.clone();
    let shortcut_label_update = shortcut_label.clone();
    let current_trigger_update = current_trigger.clone();
    let transcript_buffer = transcript_buffer.clone();
    let provider_row_update = provider_row.clone();
    let command_row_update = command_row.clone();
    let args_row_update = args_row.clone();
    let api_key_row_update = api_key_row.clone();
    let model_row_update = model_row.clone();
    let endpoint_row_update = endpoint_row.clone();
    let execution_provider_row_update = execution_provider_row.clone();
    let model_status_row_update = model_status_row.clone();
    let transcriber_state_update = transcriber_state.clone();
    let updating_widgets_poll = updating_widgets.clone();
    let banner = banner.clone();
    let toast_overlay_poll = toast_overlay.clone();
    let handle_poll = handle.clone();

    glib::timeout_add_local(Duration::from_millis(50), move || {
        while let Ok(update) = update_rx.try_recv() {
            match update {
                DaemonUpdate::Connected {
                    state,
                    shortcut_trigger,
                    transcriber_config,
                    portal_connected,
                    last_transcript,
                    last_error,
                } => {
                    banner.set_revealed(false);
                    update_state_row(&state_row, &state_icon, &state);
                    portal_row.set_subtitle(
                        if portal_connected { "Connected" } else { "Disconnected" },
                    );
                    shortcut_label_update.set_accelerator(&shortcut_trigger);
                    *current_trigger_update.borrow_mut() = shortcut_trigger;
                    if !last_transcript.is_empty() {
                        transcript_buffer.set_text(&last_transcript);
                    }
                    apply_transcriber_config_to_widgets(
                        &transcriber_config,
                        &provider_row_update,
                        &command_row_update,
                        &args_row_update,
                        &api_key_row_update,
                        &model_row_update,
                        &endpoint_row_update,
                        &execution_provider_row_update,
                        &model_status_row_update,
                        &transcriber_state_update,
                        &updating_widgets_poll,
                    );
                    if !last_error.is_empty() {
                        toast_overlay_poll.add_toast(adw::Toast::new(&last_error));
                    }
                    // Query model status if Parakeet is active
                    if let Ok(tc) = serde_json::from_str::<voxkey_ipc::TranscriberConfig>(&transcriber_config) {
                        if tc.provider == voxkey_ipc::TranscriberProvider::Parakeet {
                            handle_poll.send(DaemonCommand::ModelStatus(tc.parakeet.model.clone()));
                        }
                    }
                }
                DaemonUpdate::Disconnected => {
                    banner.set_revealed(true);
                    state_row.set_subtitle("Unknown");
                    portal_row.set_subtitle("Unknown");
                }
                DaemonUpdate::StateChanged(state) => {
                    update_state_row(&state_row, &state_icon, &state);
                }
                DaemonUpdate::PropertyChanged { name, value } => match name.as_str() {
                    "last_transcript" => {
                        transcript_buffer.set_text(&value);
                    }
                    "last_error" => {
                        if !value.is_empty() {
                            toast_overlay_poll.add_toast(adw::Toast::new(&value));
                        }
                    }
                    "portal_connected" => {
                        portal_row.set_subtitle(
                            if value == "true" { "Connected" } else { "Disconnected" },
                        );
                    }
                    "shortcut_trigger" => {
                        shortcut_label_update.set_accelerator(&value);
                        *current_trigger_update.borrow_mut() = value;
                    }
                    "transcriber_config" => {
                        apply_transcriber_config_to_widgets(
                            &value,
                            &provider_row_update,
                            &command_row_update,
                            &args_row_update,
                            &api_key_row_update,
                            &model_row_update,
                            &endpoint_row_update,
                            &execution_provider_row_update,
                            &model_status_row_update,
                            &transcriber_state_update,
                            &updating_widgets_poll,
                        );
                    }
                    _ => {}
                },
                DaemonUpdate::DownloadProgress { model_name, percent } => {
                    model_status_row_update.set_subtitle(
                        &format!("Downloading {model_name}... {percent}%"),
                    );
                    if percent >= 100 {
                        model_status_row_update.set_subtitle("Available");
                    }
                }
                DaemonUpdate::ModelStatusResult { status, .. } => {
                    let label = match status.as_str() {
                        "available" => "Available",
                        "downloading" => "Downloading...",
                        _ => "Not downloaded",
                    };
                    model_status_row_update.set_subtitle(label);
                }
            }
        }
        glib::ControlFlow::Continue
    });

    window
}

fn update_state_row(row: &adw::ActionRow, icon: &gtk4::Image, state: &str) {
    row.set_subtitle(state);

    for class in &["success", "warning", "error", "dim-label"] {
        icon.remove_css_class(class);
    }

    match state {
        "Idle" => icon.add_css_class("dim-label"),
        "Recording" | "Streaming" => icon.add_css_class("error"),
        "Transcribing" | "Injecting" => icon.add_css_class("warning"),
        "RecoveringSession" => icon.add_css_class("error"),
        _ => icon.add_css_class("dim-label"),
    }
}

/// Convert a GDK key + modifiers into the portal trigger format: "<Control><Alt>d"
fn key_to_trigger(key: gdk::Key, modifiers: gdk::ModifierType) -> Option<String> {
    // Ignore lone modifier presses
    if matches!(
        key,
        gdk::Key::Shift_L
            | gdk::Key::Shift_R
            | gdk::Key::Control_L
            | gdk::Key::Control_R
            | gdk::Key::Alt_L
            | gdk::Key::Alt_R
            | gdk::Key::Super_L
            | gdk::Key::Super_R
            | gdk::Key::Meta_L
            | gdk::Key::Meta_R
            | gdk::Key::Hyper_L
            | gdk::Key::Hyper_R
            | gdk::Key::ISO_Level3_Shift
            | gdk::Key::Caps_Lock
            | gdk::Key::Num_Lock
    ) {
        return None;
    }

    let key_name = key.name()?;

    let mut parts = String::new();
    if modifiers.contains(gdk::ModifierType::CONTROL_MASK) {
        parts.push_str("<Control>");
    }
    if modifiers.contains(gdk::ModifierType::ALT_MASK) {
        parts.push_str("<Alt>");
    }
    if modifiers.contains(gdk::ModifierType::SHIFT_MASK) {
        parts.push_str("<Shift>");
    }
    if modifiers.contains(gdk::ModifierType::SUPER_MASK) {
        parts.push_str("<Super>");
    }
    parts.push_str(&key_name);

    Some(parts)
}

/// Wire the shortcut row to open a key capture dialog on click.
fn wire_shortcut_capture(
    shortcut_row: &adw::ActionRow,
    shortcut_label: &gtk4::ShortcutLabel,
    current_trigger: &Rc<RefCell<String>>,
    handle: &DaemonHandle,
    toast_overlay: &adw::ToastOverlay,
    parent_window: &adw::ApplicationWindow,
) {
    let shortcut_label = shortcut_label.clone();
    let current_trigger = current_trigger.clone();
    let handle = handle.clone();
    let toast_overlay = toast_overlay.clone();
    let parent_window = parent_window.clone();

    shortcut_row.connect_activated(move |_| {
        show_shortcut_capture_dialog(
            &parent_window,
            &shortcut_label,
            &current_trigger,
            &handle,
            &toast_overlay,
        );
    });
}

fn show_shortcut_capture_dialog(
    parent: &adw::ApplicationWindow,
    shortcut_label: &gtk4::ShortcutLabel,
    current_trigger: &Rc<RefCell<String>>,
    handle: &DaemonHandle,
    toast_overlay: &adw::ToastOverlay,
) {
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(360)
        .default_height(200)
        .title("Set Shortcut")
        .build();

    let dialog_content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&adw::HeaderBar::new());

    let status_page = adw::StatusPage::builder()
        .icon_name("preferences-desktop-keyboard-shortcuts-symbolic")
        .title("Press a shortcut")
        .description("Press Escape to cancel")
        .build();

    toolbar_view.set_content(Some(&status_page));
    dialog_content.append(&toolbar_view);
    dialog.set_content(Some(&dialog_content));

    let key_controller = gtk4::EventControllerKey::new();

    let dialog_ref = dialog.clone();
    let shortcut_label = shortcut_label.clone();
    let current_trigger = current_trigger.clone();
    let handle = handle.clone();
    let toast_overlay = toast_overlay.clone();

    key_controller.connect_key_pressed(move |_, key, _, modifiers| {
        // Escape cancels
        if key == gdk::Key::Escape {
            dialog_ref.close();
            return glib::Propagation::Stop;
        }

        if let Some(trigger) = key_to_trigger(key, modifiers) {
            shortcut_label.set_accelerator(&trigger);
            *current_trigger.borrow_mut() = trigger.clone();
            handle.send(DaemonCommand::SetShortcut(trigger));
            toast_overlay.add_toast(adw::Toast::new("Shortcut updated"));
            dialog_ref.close();
            return glib::Propagation::Stop;
        }

        glib::Propagation::Proceed
    });

    dialog.add_controller(key_controller);
    dialog.present();
}

/// Set entry text, showing `default_text` dimmed when `value` is empty or matches the default.
fn set_entry_with_default(row: &adw::EntryRow, value: &str, default_text: &str) {
    let is_default = value.is_empty() || value == default_text;
    let display = if value.is_empty() { default_text } else { value };
    set_entry_text_without_apply(row, display);
    if let Some(delegate) = row.delegate() {
        delegate.set_opacity(if is_default { 0.55 } else { 1.0 });
    }
}

/// Set entry text without triggering the apply button.
/// Toggling show_apply_button off→on after set_text() makes libadwaita
/// snapshot the current text as the "applied" baseline.
fn set_entry_text_without_apply(row: &adw::EntryRow, text: &str) {
    row.set_show_apply_button(false);
    row.set_text(text);
    row.set_show_apply_button(true);
}

/// Same as set_entry_text_without_apply but for PasswordEntryRow.
fn set_password_entry_text_without_apply(row: &adw::PasswordEntryRow, text: &str) {
    row.set_show_apply_button(false);
    row.set_text(text);
    row.set_show_apply_button(true);
}

/// Parse transcriber config JSON and update all transcriber widgets.
fn apply_transcriber_config_to_widgets(
    config_json: &str,
    provider_row: &adw::ComboRow,
    command_row: &adw::EntryRow,
    args_row: &adw::EntryRow,
    api_key_row: &adw::PasswordEntryRow,
    model_row: &adw::EntryRow,
    endpoint_row: &adw::EntryRow,
    execution_provider_row: &adw::ComboRow,
    model_status_row: &adw::ActionRow,
    state: &Rc<RefCell<voxkey_ipc::TranscriberConfig>>,
    updating_widgets: &Rc<Cell<bool>>,
) {
    let Ok(tc) = serde_json::from_str::<voxkey_ipc::TranscriberConfig>(config_json) else {
        return;
    };

    // Suppress notify handlers from sending config back to daemon while we update widgets.
    updating_widgets.set(true);

    // Update state BEFORE touching widgets. provider_row.set_selected() fires
    // connect_selected_notify which reads from state — it must see current values.
    *state.borrow_mut() = tc.clone();

    let provider_idx = match tc.provider {
        voxkey_ipc::TranscriberProvider::WhisperCpp => 0u32,
        voxkey_ipc::TranscriberProvider::Mistral => 1,
        voxkey_ipc::TranscriberProvider::MistralRealtime => 2,
        voxkey_ipc::TranscriberProvider::Parakeet => {
            if tc.parakeet.model == "parakeet-tdt-0.6b-v2" { 3 } else { 4 }
        }
    };
    provider_row.set_selected(provider_idx);

    // Set entry text and reset the "applied text" baseline so the apply button
    // stays hidden. Toggling show_apply_button off→on after set_text() snapshots
    // the current text as the new baseline in libadwaita.
    set_entry_text_without_apply(command_row, &tc.whisper_cpp.command);
    set_entry_text_without_apply(args_row, &tc.whisper_cpp.args.join(" "));

    // Show API key, model, and endpoint from the active Mistral provider
    let is_whisper = tc.provider == voxkey_ipc::TranscriberProvider::WhisperCpp;
    let is_parakeet = tc.provider == voxkey_ipc::TranscriberProvider::Parakeet;
    let is_mistral_api = !is_whisper && !is_parakeet;

    if is_mistral_api {
        let (active_api_key, active_model, active_endpoint, default_model, default_endpoint) = match tc.provider {
            voxkey_ipc::TranscriberProvider::MistralRealtime => {
                (&tc.mistral_realtime.api_key, &tc.mistral_realtime.model, &tc.mistral_realtime.endpoint,
                 voxkey_ipc::MistralRealtimeConfig::DEFAULT_MODEL, voxkey_ipc::MistralRealtimeConfig::DEFAULT_ENDPOINT)
            }
            _ => {
                (&tc.mistral.api_key, &tc.mistral.model, &tc.mistral.endpoint,
                 voxkey_ipc::MistralConfig::DEFAULT_MODEL, voxkey_ipc::MistralConfig::DEFAULT_ENDPOINT)
            }
        };
        set_password_entry_text_without_apply(api_key_row, active_api_key);
        set_entry_with_default(model_row, active_model, default_model);
        set_entry_with_default(endpoint_row, active_endpoint, default_endpoint);
    }

    if is_parakeet {
        let ep_idx = match tc.parakeet.execution_provider {
            voxkey_ipc::ExecutionProviderChoice::Auto => 0u32,
            voxkey_ipc::ExecutionProviderChoice::Cpu => 1,
            voxkey_ipc::ExecutionProviderChoice::Cuda => 2,
        };
        execution_provider_row.set_selected(ep_idx);
    }

    // Toggle visibility
    command_row.set_visible(is_whisper);
    args_row.set_visible(is_whisper);
    api_key_row.set_visible(is_mistral_api);
    model_row.set_visible(is_mistral_api);
    endpoint_row.set_visible(is_mistral_api);
    execution_provider_row.set_visible(is_parakeet);
    model_status_row.set_visible(is_parakeet);

    updating_widgets.set(false);
}

/// Build the current TranscriberConfig from shared state and send it to the daemon.
fn send_transcriber_config(state: &Rc<RefCell<voxkey_ipc::TranscriberConfig>>, handle: &DaemonHandle) {
    let config = state.borrow().clone();
    if let Ok(json) = serde_json::to_string(&config) {
        handle.send(DaemonCommand::SetTranscriberConfig(json));
    }
}

fn wire_transcriber_actions(
    provider_row: &adw::ComboRow,
    command_row: &adw::EntryRow,
    args_row: &adw::EntryRow,
    api_key_row: &adw::PasswordEntryRow,
    model_row: &adw::EntryRow,
    endpoint_row: &adw::EntryRow,
    execution_provider_row: &adw::ComboRow,
    model_status_row: &adw::ActionRow,
    download_button: &gtk4::Button,
    delete_model_button: &gtk4::Button,
    open_folder_button: &gtk4::Button,
    state: &Rc<RefCell<voxkey_ipc::TranscriberConfig>>,
    updating_widgets: &Rc<Cell<bool>>,
    handle: &DaemonHandle,
) {
    // Provider combo: toggle visibility, update fields, and send config
    {
        let command_row = command_row.clone();
        let args_row = args_row.clone();
        let api_key_row = api_key_row.clone();
        let model_row = model_row.clone();
        let endpoint_row = endpoint_row.clone();
        let execution_provider_row = execution_provider_row.clone();
        let model_status_row = model_status_row.clone();
        let state = state.clone();
        let updating_widgets = updating_widgets.clone();
        let handle = handle.clone();
        provider_row.connect_selected_notify(move |row| {
            if updating_widgets.get() {
                return;
            }
            let selected = row.selected();
            let is_parakeet = selected == 3 || selected == 4;

            if is_parakeet {
                let model_name = if selected == 3 {
                    "parakeet-tdt-0.6b-v2"
                } else {
                    "parakeet-tdt-0.6b-v3"
                };
                state.borrow_mut().provider = voxkey_ipc::TranscriberProvider::Parakeet;
                state.borrow_mut().parakeet.model = model_name.to_string();
            } else {
                let provider = match selected {
                    0 => voxkey_ipc::TranscriberProvider::WhisperCpp,
                    2 => voxkey_ipc::TranscriberProvider::MistralRealtime,
                    _ => voxkey_ipc::TranscriberProvider::Mistral,
                };
                state.borrow_mut().provider = provider;
            }

            let provider = state.borrow().provider.clone();
            let is_whisper = provider == voxkey_ipc::TranscriberProvider::WhisperCpp;
            let is_mistral_api = !is_whisper && !is_parakeet;

            command_row.set_visible(is_whisper);
            args_row.set_visible(is_whisper);
            api_key_row.set_visible(is_mistral_api);
            model_row.set_visible(is_mistral_api);
            endpoint_row.set_visible(is_mistral_api);
            execution_provider_row.set_visible(is_parakeet);
            model_status_row.set_visible(is_parakeet);

            if is_mistral_api {
                let is_realtime = provider == voxkey_ipc::TranscriberProvider::MistralRealtime;
                let st = state.borrow();
                let (key, model, endpoint, default_model, default_endpoint) = if is_realtime {
                    (&st.mistral_realtime.api_key, &st.mistral_realtime.model, &st.mistral_realtime.endpoint,
                     voxkey_ipc::MistralRealtimeConfig::DEFAULT_MODEL, voxkey_ipc::MistralRealtimeConfig::DEFAULT_ENDPOINT)
                } else {
                    (&st.mistral.api_key, &st.mistral.model, &st.mistral.endpoint,
                     voxkey_ipc::MistralConfig::DEFAULT_MODEL, voxkey_ipc::MistralConfig::DEFAULT_ENDPOINT)
                };
                set_password_entry_text_without_apply(&api_key_row, key);
                set_entry_with_default(&model_row, model, default_model);
                set_entry_with_default(&endpoint_row, endpoint, default_endpoint);
            }

            if is_parakeet {
                let model_name = state.borrow().parakeet.model.clone();
                handle.send(DaemonCommand::ModelStatus(model_name));
            }

            send_transcriber_config(&state, &handle);
        });
    }

    // whisper.cpp command apply
    {
        let state = state.clone();
        let handle = handle.clone();
        command_row.connect_apply(move |row| {
            state.borrow_mut().whisper_cpp.command = row.text().to_string();
            send_transcriber_config(&state, &handle);
        });
    }

    // whisper.cpp args apply
    {
        let state = state.clone();
        let handle = handle.clone();
        args_row.connect_apply(move |row| {
            state.borrow_mut().whisper_cpp.args = row
                .text()
                .split_whitespace()
                .map(|s| s.to_string())
                .collect();
            send_transcriber_config(&state, &handle);
        });
    }

    // API key apply (writes to active provider's config)
    {
        let state = state.clone();
        let handle = handle.clone();
        api_key_row.connect_apply(move |row| {
            let key = row.text().to_string();
            let mut st = state.borrow_mut();
            match st.provider {
                voxkey_ipc::TranscriberProvider::MistralRealtime => {
                    st.mistral_realtime.api_key = key;
                }
                _ => {
                    st.mistral.api_key = key;
                }
            }
            drop(st);
            send_transcriber_config(&state, &handle);
        });
    }

    // Model entry (writes to active provider's config)
    {
        let state = state.clone();
        let handle = handle.clone();
        model_row.connect_apply(move |row| {
            let model = row.text().to_string();
            let mut st = state.borrow_mut();
            match st.provider {
                voxkey_ipc::TranscriberProvider::MistralRealtime => {
                    st.mistral_realtime.model = model;
                }
                _ => {
                    st.mistral.model = model;
                }
            }
            drop(st);
            send_transcriber_config(&state, &handle);
        });
    }

    // Endpoint entry (writes to active provider's config, empty when matching default)
    {
        let state = state.clone();
        let handle = handle.clone();
        endpoint_row.connect_apply(move |row| {
            let raw = row.text().to_string();
            let mut st = state.borrow_mut();
            match st.provider {
                voxkey_ipc::TranscriberProvider::MistralRealtime => {
                    let default = voxkey_ipc::MistralRealtimeConfig::DEFAULT_ENDPOINT;
                    st.mistral_realtime.endpoint = if raw == default { String::new() } else { raw };
                }
                _ => {
                    let default = voxkey_ipc::MistralConfig::DEFAULT_ENDPOINT;
                    st.mistral.endpoint = if raw == default { String::new() } else { raw };
                }
            }
            drop(st);
            send_transcriber_config(&state, &handle);
        });
    }

    // Execution provider combo (Parakeet)
    {
        let state = state.clone();
        let handle = handle.clone();
        let updating_widgets = updating_widgets.clone();
        execution_provider_row.connect_selected_notify(move |row| {
            if updating_widgets.get() {
                return;
            }
            let ep = match row.selected() {
                1 => voxkey_ipc::ExecutionProviderChoice::Cpu,
                2 => voxkey_ipc::ExecutionProviderChoice::Cuda,
                _ => voxkey_ipc::ExecutionProviderChoice::Auto,
            };
            state.borrow_mut().parakeet.execution_provider = ep;
            send_transcriber_config(&state, &handle);
        });
    }

    // Download button
    {
        let state = state.clone();
        let handle = handle.clone();
        download_button.connect_clicked(move |_| {
            let model_name = state.borrow().parakeet.model.clone();
            handle.send(DaemonCommand::DownloadModel(model_name));
        });
    }

    // Open folder button
    {
        let handle = handle.clone();
        open_folder_button.connect_clicked(move |_| {
            handle.send(DaemonCommand::OpenModelsDir);
        });
    }

    // Delete button
    {
        let state = state.clone();
        let handle = handle.clone();
        let model_status_row = model_status_row.clone();
        delete_model_button.connect_clicked(move |_| {
            let model_name = state.borrow().parakeet.model.clone();
            handle.send(DaemonCommand::DeleteModel(model_name));
            model_status_row.set_subtitle("Not downloaded");
        });
    }
}

fn wire_advanced_actions(
    reload_row: &adw::ActionRow,
    clear_token_row: &adw::ActionRow,
    handle: &DaemonHandle,
    toast_overlay: &adw::ToastOverlay,
) {
    let handle_clone = handle.clone();
    let toast_clone = toast_overlay.clone();
    reload_row.connect_activated(move |_| {
        handle_clone.send(DaemonCommand::ReloadConfig);
        toast_clone.add_toast(adw::Toast::new("Configuration reloaded"));
    });

    let handle_clone = handle.clone();
    let toast_clone = toast_overlay.clone();
    clear_token_row.connect_activated(move |_| {
        handle_clone.send(DaemonCommand::ClearRestoreToken);
        toast_clone.add_toast(adw::Toast::new("Portal token cleared"));
    });
}
