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

    let provider_model = gtk4::StringList::new(&["whisper.cpp", "Mistral", "Mistral Realtime"]);
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

    // Initially hide Mistral rows (default is whisper.cpp)
    api_key_row.set_visible(false);
    model_row.set_visible(false);
    endpoint_row.set_visible(false);

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
    let transcriber_state_update = transcriber_state.clone();
    let updating_widgets_poll = updating_widgets.clone();
    let banner = banner.clone();
    let toast_overlay_poll = toast_overlay.clone();

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
                        &transcriber_state_update,
                        &updating_widgets_poll,
                    );
                    if !last_error.is_empty() {
                        toast_overlay_poll.add_toast(adw::Toast::new(&last_error));
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
                            &transcriber_state_update,
                            &updating_widgets_poll,
                        );
                    }
                    _ => {}
                },
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
    };
    provider_row.set_selected(provider_idx);

    // Set entry text and reset the "applied text" baseline so the apply button
    // stays hidden. Toggling show_apply_button off→on after set_text() snapshots
    // the current text as the new baseline in libadwaita.
    set_entry_text_without_apply(command_row, &tc.whisper_cpp.command);
    set_entry_text_without_apply(args_row, &tc.whisper_cpp.args.join(" "));

    // Show API key, model, and endpoint from the active provider
    let (active_api_key, active_model, active_endpoint, default_model, default_endpoint) = match tc.provider {
        voxkey_ipc::TranscriberProvider::WhisperCpp => {
            (&tc.mistral.api_key, &tc.mistral.model, &tc.mistral.endpoint,
             voxkey_ipc::MistralConfig::DEFAULT_MODEL, voxkey_ipc::MistralConfig::DEFAULT_ENDPOINT)
        }
        voxkey_ipc::TranscriberProvider::Mistral => {
            (&tc.mistral.api_key, &tc.mistral.model, &tc.mistral.endpoint,
             voxkey_ipc::MistralConfig::DEFAULT_MODEL, voxkey_ipc::MistralConfig::DEFAULT_ENDPOINT)
        }
        voxkey_ipc::TranscriberProvider::MistralRealtime => {
            (&tc.mistral_realtime.api_key, &tc.mistral_realtime.model, &tc.mistral_realtime.endpoint,
             voxkey_ipc::MistralRealtimeConfig::DEFAULT_MODEL, voxkey_ipc::MistralRealtimeConfig::DEFAULT_ENDPOINT)
        }
    };
    set_password_entry_text_without_apply(api_key_row, active_api_key);
    set_entry_with_default(model_row, active_model, default_model);
    set_entry_with_default(endpoint_row, active_endpoint, default_endpoint);

    // Toggle visibility
    let is_whisper = tc.provider == voxkey_ipc::TranscriberProvider::WhisperCpp;
    command_row.set_visible(is_whisper);
    args_row.set_visible(is_whisper);
    api_key_row.set_visible(!is_whisper);
    model_row.set_visible(!is_whisper);
    endpoint_row.set_visible(!is_whisper);

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
        let state = state.clone();
        let updating_widgets = updating_widgets.clone();
        let handle = handle.clone();
        provider_row.connect_selected_notify(move |row| {
            if updating_widgets.get() {
                return;
            }
            let provider = match row.selected() {
                0 => voxkey_ipc::TranscriberProvider::WhisperCpp,
                2 => voxkey_ipc::TranscriberProvider::MistralRealtime,
                _ => voxkey_ipc::TranscriberProvider::Mistral,
            };
            let is_whisper = provider == voxkey_ipc::TranscriberProvider::WhisperCpp;
            let is_realtime = provider == voxkey_ipc::TranscriberProvider::MistralRealtime;
            command_row.set_visible(is_whisper);
            args_row.set_visible(is_whisper);
            api_key_row.set_visible(!is_whisper);
            model_row.set_visible(!is_whisper);
            endpoint_row.set_visible(!is_whisper);

            state.borrow_mut().provider = provider;

            // Show fields from the active provider config
            {
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
