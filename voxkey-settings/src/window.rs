// ABOUTME: Builds the main settings window with status, configuration, and control groups.
// ABOUTME: Wires D-Bus property changes to widget updates and user actions to D-Bus method calls.

use std::cell::{Cell, RefCell};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::rc::Rc;

use adw::prelude::*;
use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;

use crate::daemon_client::{self, DaemonCommand, DaemonHandle, DaemonSnapshot, DaemonUpdate};
use crate::gui_settings;

// AdwStatusPage needs enough vertical room for its icon, title, wrapped
// instructions, and the transient window's header bar at 100% scaling.
const SHORTCUT_DIALOG_DEFAULT_WIDTH: i32 = 480;
const SHORTCUT_DIALOG_DEFAULT_HEIGHT: i32 = 420;
const DEFAULT_WINDOW_WIDTH: i32 = 980;
const DEFAULT_WINDOW_HEIGHT: i32 = 700;
const SHELL_EXTENSION_RESTART_NOTICE: &str =
    "Save your work, then log out and back in to add Voxkey controls to GNOME Shell.";
const SHELL_EXTENSION_RESTART_ACTION: &str = "Log Out…";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointKind {
    MistralBatch,
    MistralRealtime,
    ParakeetHttp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewPreset {
    Automatic,
    AlwaysLive,
    FinalOnly,
    Custom,
}

const LOCAL_MODEL_CHOICE_START: u32 = 3;
const STANDARD_TRANSCRIBER_CHOICES: u32 =
    LOCAL_MODEL_CHOICE_START + voxkey_ipc::model_library::LOCAL_MODELS.len() as u32;
const CUSTOM_PARAKEET_CHOICE: u32 = STANDARD_TRANSCRIBER_CHOICES;

fn local_model_for_choice(choice: u32) -> Option<&'static voxkey_ipc::model_library::LocalModel> {
    choice
        .checked_sub(LOCAL_MODEL_CHOICE_START)
        .and_then(|index| voxkey_ipc::model_library::LOCAL_MODELS.get(index as usize))
}

fn choice_for_local_model(model_id: &str) -> Option<u32> {
    voxkey_ipc::model_library::LOCAL_MODELS
        .iter()
        .position(|model| model.id == model_id)
        .map(|index| LOCAL_MODEL_CHOICE_START + index as u32)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TranscriberChoicePresentation {
    selected: u32,
    show_custom_parakeet: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TranscriberSetupCopy {
    title: &'static str,
    description: &'static str,
}

fn transcriber_setup_copy(selected: u32, parakeet_backend_selected: u32) -> TranscriberSetupCopy {
    match selected {
        0 => TranscriberSetupCopy {
            title: "Whisper.cpp setup",
            description: "Choose its executable and model; expert mode adds command arguments",
        },
        1 => TranscriberSetupCopy {
            title: "Mistral setup",
            description: "Store your API key and choose the batch transcription model",
        },
        2 => TranscriberSetupCopy {
            title: "Mistral Realtime setup",
            description: "Store your API key and choose the realtime transcription model",
        },
        3.. if parakeet_backend_selected == 1 => TranscriberSetupCopy {
            title: "Model server setup",
            description: "Connect an OpenAI-compatible endpoint for finished recordings",
        },
        _ => TranscriberSetupCopy {
            title: "Local model setup",
            description: "Download and run speech models privately on this computer",
        },
    }
}

fn apply_transcriber_setup_copy(
    group: &adw::PreferencesGroup,
    selected: u32,
    parakeet_backend_selected: u32,
) {
    let copy = transcriber_setup_copy(selected, parakeet_backend_selected);
    group.set_title(copy.title);
    group.set_description(Some(copy.description));
}

fn transcriber_location_icon_name(selected: u32, parakeet_backend_selected: u32) -> &'static str {
    match selected {
        1 | 2 => "network-server-symbolic",
        3.. if parakeet_backend_selected == 1 => "network-server-symbolic",
        _ => "computer-symbolic",
    }
}

fn execution_provider_subtitle(selected: u32) -> &'static str {
    match selected {
        1 => "Always process speech with the CPU",
        2 => "Use CUDA acceleration with a supported NVIDIA GPU",
        _ => "Use NVIDIA CUDA when available, otherwise use the CPU",
    }
}

fn transcriber_choice_presentation(
    config: &voxkey_ipc::TranscriberConfig,
) -> TranscriberChoicePresentation {
    let selected = match config.provider {
        voxkey_ipc::TranscriberProvider::WhisperCpp => 0,
        voxkey_ipc::TranscriberProvider::Mistral => 1,
        voxkey_ipc::TranscriberProvider::MistralRealtime => 2,
        voxkey_ipc::TranscriberProvider::Parakeet => {
            choice_for_local_model(&config.parakeet.model).unwrap_or(CUSTOM_PARAKEET_CHOICE)
        }
    };
    TranscriberChoicePresentation {
        selected,
        show_custom_parakeet: selected == CUSTOM_PARAKEET_CHOICE,
    }
}

fn sync_custom_parakeet_choice(model: &gtk4::StringList, show: bool) {
    while model.n_items() > STANDARD_TRANSCRIBER_CHOICES {
        model.remove(STANDARD_TRANSCRIBER_CHOICES);
    }
    if show {
        model.append("Custom model or server ID");
    }
}

fn set_monospace_entry_text(row: &adw::EntryRow) {
    let attributes = gtk4::pango::AttrList::new();
    attributes.insert(gtk4::pango::AttrString::new_family("monospace"));
    row.set_attributes(Some(&attributes));
}

/// One endpoint field and its persistent, inline connectivity feedback. Each
/// network provider gets its own row so results stay attached to the address
/// they describe when the user switches models.
#[derive(Clone)]
struct EndpointEditor {
    kind: EndpointKind,
    entry: adw::EntryRow,
    insecure_http_row: Option<adw::SwitchRow>,
    status: adw::ActionRow,
    status_icon: gtk4::Image,
    spinner: gtk4::Spinner,
    check_button: gtk4::Button,
    request_id: Rc<Cell<u64>>,
    permission_dirty: Rc<Cell<bool>>,
}

impl EndpointEditor {
    fn new(kind: EndpointKind, title: &str) -> Self {
        let entry = adw::EntryRow::builder()
            .title(title)
            .input_purpose(gtk4::InputPurpose::Url)
            .input_hints(gtk4::InputHints::NO_EMOJI | gtk4::InputHints::NO_SPELLCHECK)
            .enable_emoji_completion(false)
            .show_apply_button(true)
            .build();
        set_monospace_entry_text(&entry);
        let entry_icon = gtk4::Image::from_icon_name("network-server-symbolic");
        entry_icon.add_css_class("dim-label");
        entry.add_prefix(&entry_icon);
        let insecure_http_row = (kind == EndpointKind::ParakeetHttp).then(|| {
            let row = adw::SwitchRow::builder()
                .title("Allow unencrypted LAN audio")
                .subtitle(
                    "Audio, transcripts, and any API key travel without encryption. Use only with a trusted server on a private network.",
                )
                .subtitle_lines(2)
                .build();
            let icon = gtk4::Image::from_icon_name("dialog-warning-symbolic");
            icon.add_css_class("warning");
            row.add_prefix(&icon);
            row
        });
        let status = adw::ActionRow::builder()
            .title("Server connection")
            .subtitle("Check the address before Voxkey saves it")
            .subtitle_lines(2)
            .use_markup(false)
            .build();
        let status_icon = gtk4::Image::from_icon_name("network-server-symbolic");
        status.add_prefix(&status_icon);
        let spinner = gtk4::Spinner::new();
        spinner.set_valign(gtk4::Align::Center);
        spinner.set_visible(false);
        status.add_suffix(&spinner);
        let check_button = gtk4::Button::with_label("Check");
        check_button.set_valign(gtk4::Align::Center);
        check_button.add_css_class("suggested-action");
        status.add_suffix(&check_button);

        Self {
            kind,
            entry,
            insecure_http_row,
            status,
            status_icon,
            spinner,
            check_button,
            request_id: Rc::new(Cell::new(0)),
            permission_dirty: Rc::new(Cell::new(false)),
        }
    }

    fn set_visible(&self, visible: bool) {
        self.entry.set_visible(visible);
        if let Some(row) = &self.insecure_http_row {
            row.set_visible(visible);
        }
        self.status.set_visible(visible);
    }

    fn set_controls_sensitive(&self, sensitive: bool) {
        self.entry.set_sensitive(sensitive);
        if let Some(row) = &self.insecure_http_row {
            row.set_sensitive(sensitive);
        }
    }

    fn sync_insecure_http_permission(&self, allowed: bool, preserve_pending: bool) {
        let Some(row) = &self.insecure_http_row else {
            return;
        };
        if preserve_pending && self.permission_dirty.get() {
            return;
        }
        row.set_active(allowed);
        self.permission_dirty.set(false);
    }

    fn next_request(&self) -> u64 {
        let next = self.request_id.get().wrapping_add(1).max(1);
        self.request_id.set(next);
        next
    }

    fn request_is_current(&self, request_id: u64) -> bool {
        self.request_id.get() == request_id
    }

    fn set_icon(&self, icon_name: &str, style: Option<&str>) {
        for class in ["accent", "success", "warning", "error"] {
            self.status_icon.remove_css_class(class);
        }
        self.status_icon.set_icon_name(Some(icon_name));
        if let Some(style) = style {
            self.status_icon.add_css_class(style);
        }
    }

    fn set_check_emphasized(&self, emphasized: bool) {
        self.check_button.remove_css_class("flat");
        self.check_button.remove_css_class("suggested-action");
        self.check_button.add_css_class(if emphasized {
            "suggested-action"
        } else {
            "flat"
        });
    }

    fn show_idle(&self) {
        self.status.set_title("Address changed");
        self.status
            .set_subtitle("Check the address before Voxkey saves it");
        self.set_icon("network-server-symbolic", None);
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.check_button.set_label("Check and save");
        self.set_check_emphasized(true);
        self.check_button.set_visible(true);
        self.set_controls_sensitive(true);
    }

    fn show_saved(&self) {
        if self.kind == EndpointKind::ParakeetHttp && self.entry.text().trim().is_empty() {
            self.status.set_title("Server address needed");
            self.status
                .set_subtitle("Enter the transcription server address, then check and save it");
            self.set_icon("network-server-symbolic", None);
            self.check_button.set_label("Check and save");
            self.set_check_emphasized(true);
        } else {
            self.status.set_title("Server address saved");
            self.status
                .set_subtitle("Voxkey will use this address for new dictations");
            self.set_icon("object-select-symbolic", Some("success"));
            self.check_button.set_label("Check");
            self.set_check_emphasized(false);
        }
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.check_button.set_visible(true);
        self.set_controls_sensitive(true);
    }

    fn show_permission_changed(&self, allowed: bool) {
        if allowed {
            self.status.set_title("Unencrypted LAN audio selected");
            self.status
                .set_subtitle("Check the server to save this permission and address");
            self.set_icon("dialog-warning-symbolic", Some("warning"));
            self.check_button.set_label("Check and save");
            self.set_check_emphasized(true);
        } else {
            self.status.set_title("Unencrypted LAN audio blocked");
            self.status
                .set_subtitle("Unencrypted private addresses require the switch above");
            self.set_icon("network-server-symbolic", None);
            self.check_button.set_label("Check");
            self.set_check_emphasized(false);
        }
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.check_button.set_visible(true);
        self.set_controls_sensitive(true);
    }

    fn show_checking(&self) {
        self.status.set_title("Checking server…");
        self.status
            .set_subtitle("Contacting the server without sending audio or credentials");
        self.set_icon("network-transmit-receive-symbolic", Some("accent"));
        self.check_button.set_visible(false);
        self.spinner.set_visible(true);
        self.spinner.start();
        self.set_controls_sensitive(false);
    }

    fn show_saving(&self) {
        self.status.set_title("Saving address…");
        self.status
            .set_subtitle("The server responded; updating Voxkey settings");
    }

    fn show_reachable(&self, message: &str) {
        self.status.set_title("Server reachable");
        self.status.set_subtitle(message);
        self.set_icon("object-select-symbolic", Some("success"));
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.check_button.set_label("Check again");
        self.set_check_emphasized(false);
        self.check_button.set_visible(true);
        self.set_controls_sensitive(true);
    }

    fn show_failed(&self, message: &str) {
        self.status.set_title("Server not ready");
        self.status.set_subtitle(message);
        self.set_icon("dialog-error-symbolic", Some("error"));
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.check_button.set_label("Try again");
        self.set_check_emphasized(true);
        self.check_button.set_visible(true);
        self.set_controls_sensitive(true);
    }
}

/// Return the keyring service used by the active transcription path. Local
/// models deliberately return None so they never wake or prompt the keyring.
fn transcriber_api_service(config: &voxkey_ipc::TranscriberConfig) -> Option<&'static str> {
    match config.provider {
        voxkey_ipc::TranscriberProvider::Mistral => Some(voxkey_ipc::API_KEY_SERVICE_MISTRAL),
        voxkey_ipc::TranscriberProvider::MistralRealtime => {
            Some(voxkey_ipc::API_KEY_SERVICE_MISTRAL_REALTIME)
        }
        voxkey_ipc::TranscriberProvider::Parakeet
            if config.parakeet.backend == voxkey_ipc::ParakeetBackend::Http =>
        {
            Some(voxkey_ipc::API_KEY_SERVICE_MODEL_SERVER)
        }
        _ => None,
    }
}

fn api_key_provider_name(provider: &voxkey_ipc::TranscriberProvider) -> Option<&'static str> {
    match provider {
        voxkey_ipc::TranscriberProvider::Mistral => Some("Mistral"),
        voxkey_ipc::TranscriberProvider::MistralRealtime => Some("Mistral Realtime"),
        voxkey_ipc::TranscriberProvider::Parakeet => Some("Model server"),
        _ => None,
    }
}

fn api_key_saved_message(provider: &voxkey_ipc::TranscriberProvider) -> Option<String> {
    api_key_provider_name(provider).map(|name| format!("{name} API key saved"))
}

/// Whether a keyring entry exists for the provider the row currently belongs
/// to, or None when that provider does not use an API key.
fn api_key_status_for_provider(
    service: &str,
    present: bool,
    config: &voxkey_ipc::TranscriberConfig,
) -> Option<bool> {
    (transcriber_api_service(config) == Some(service)).then_some(present)
}

fn api_key_status_for_request(
    service: &str,
    present: bool,
    config: &voxkey_ipc::TranscriberConfig,
    request_id: u64,
    current_request_id: u64,
) -> Option<bool> {
    if request_id != current_request_id {
        return None;
    }
    api_key_status_for_provider(service, present, config)
}

fn advance_api_key_request(request_id: &Cell<u64>) -> u64 {
    let next = request_id.get().wrapping_add(1);
    request_id.set(next);
    next
}

fn normalized_api_key_input(input: &str) -> String {
    input.trim().to_string()
}

fn api_key_entry_title(present: Option<bool>) -> &'static str {
    if present == Some(true) {
        "Replace API key"
    } else {
        "API key"
    }
}

fn request_api_key_status(service: Option<&str>, request_id: &Cell<u64>, handle: &DaemonHandle) {
    let request_id = advance_api_key_request(request_id);
    if let Some(service) = service {
        handle.send(DaemonCommand::HasApiKey {
            service: service.to_string(),
            request_id,
        });
    }
}

/// Reflect whether a key is stored. The entry row itself stays empty — it only
/// accepts a new key to replace the stored one, and the status row says which
/// state applies. No-op if `service` does not match the active provider, which
/// also avoids stale asynchronous replies.
#[allow(clippy::too_many_arguments)]
fn apply_api_key_status(
    service: &str,
    present: bool,
    request_id: u64,
    current_request_id: u64,
    status_row: &adw::ActionRow,
    entry_row: &adw::PasswordEntryRow,
    remove_button: &gtk4::Button,
    stored_state: &Rc<Cell<Option<bool>>>,
    provider_state: &Rc<RefCell<voxkey_ipc::TranscriberConfig>>,
) {
    let config = provider_state.borrow();
    let Some(present) =
        api_key_status_for_request(service, present, &config, request_id, current_request_id)
    else {
        return;
    };
    stored_state.set(Some(present));
    update_api_key_row_state(status_row, entry_row, remove_button, Some(present));
}

fn toast_after_success(
    completion: daemon_client::CommandCompletion,
    toast_overlay: &adw::ToastOverlay,
    message: &'static str,
) {
    let toast_overlay = toast_overlay.clone();
    glib::spawn_future_local(async move {
        if completion.wait().await.is_ok() {
            toast_overlay.add_toast(adw::Toast::new(message));
        }
    });
}

fn build_shell_extension_restart_banner(toast_overlay: &adw::ToastOverlay) -> adw::Banner {
    let banner = adw::Banner::new(SHELL_EXTENSION_RESTART_NOTICE);
    banner.set_button_label(Some(SHELL_EXTENSION_RESTART_ACTION));
    banner.set_revealed(false);

    let toast_overlay = toast_overlay.clone();
    banner.connect_button_clicked(move |_| {
        if let Err(error) = crate::shell_extension::request_logout() {
            tracing::warn!("Could not open GNOME's logout confirmation: {error}");
            toast_overlay.add_toast(adw::Toast::new(
                "Could not open the logout dialog. Use the system menu to log out.",
            ));
        }
    });

    banner
}

pub fn build_window(app: &adw::Application) -> adw::ApplicationWindow {
    let (mut update_rx, handle) = daemon_client::connect();
    let expert_mode = Rc::new(Cell::new(gui_settings::load_expert_mode()));
    let toast_overlay = adw::ToastOverlay::new();
    let stack = gtk4::Stack::builder()
        .transition_type(gtk4::StackTransitionType::Crossfade)
        .transition_duration(160)
        .build();

    // History is the first screen: Voxkey intentionally has no dashboard.
    let history_page = crate::history::build_history_page(&handle, &toast_overlay);
    stack.add_named(&history_page.widget, Some("history"));

    // -- AI Models page --
    let models_box = gtk4::Box::new(gtk4::Orientation::Vertical, 24);
    let transcription_group = adw::PreferencesGroup::builder()
        .title("Transcription")
        .description("Choose where and how Voxkey turns speech into text")
        .build();

    let provider_model = gtk4::StringList::new(&[
        "Whisper.cpp (local)",
        "Mistral (cloud)",
        "Mistral Realtime (cloud)",
    ]);
    for model in voxkey_ipc::model_library::LOCAL_MODELS {
        provider_model.append(&format!("{} ({})", model.name, model.language_summary));
    }
    let provider_row = adw::ComboRow::builder()
        .title("Transcription engine")
        .subtitle("Runs locally; recorded audio stays on this computer")
        .subtitle_lines(2)
        .model(&provider_model)
        .build();
    let provider_icon = gtk4::Image::from_icon_name("computer-symbolic");
    provider_row.add_prefix(&provider_icon);

    transcription_group.add(&provider_row);

    let model_configuration_group = adw::PreferencesGroup::new();
    apply_transcriber_setup_copy(&model_configuration_group, provider_row.selected(), 0);

    let command_row = adw::EntryRow::builder()
        .title("Whisper executable")
        .show_apply_button(true)
        .build();
    set_monospace_entry_text(&command_row);
    let choose_command_button = gtk4::Button::with_label("Choose…");
    choose_command_button.set_tooltip_text(Some("Choose the whisper.cpp executable"));
    choose_command_button.set_valign(gtk4::Align::Center);
    update_whisper_command_action(&choose_command_button, "whisper-cpp");
    command_row.add_suffix(&choose_command_button);
    let whisper_model_row = adw::ActionRow::builder()
        .title("Whisper model")
        .subtitle("Choose a model file to use Whisper")
        .subtitle_lines(2)
        .use_markup(false)
        .build();
    let choose_whisper_model_button = gtk4::Button::with_label("Choose…");
    choose_whisper_model_button.set_tooltip_text(Some("Choose a whisper.cpp model file"));
    choose_whisper_model_button.set_valign(gtk4::Align::Center);
    choose_whisper_model_button.add_css_class("flat");
    whisper_model_row.add_suffix(&choose_whisper_model_button);
    whisper_model_row.set_activatable_widget(Some(&choose_whisper_model_button));
    let args_row = adw::EntryRow::builder()
        .title("Command arguments")
        .show_apply_button(true)
        .build();
    set_monospace_entry_text(&args_row);
    model_configuration_group.add(&command_row);
    model_configuration_group.add(&whisper_model_row);
    model_configuration_group.add(&args_row);

    // Mistral / Mistral Realtime sub-rows (shared API key, provider-specific model list)
    let api_key_status_row = adw::ActionRow::builder()
        .title("API key")
        .subtitle("Checking for a stored key…")
        .subtitle_lines(2)
        .build();
    let api_key_remove_button = gtk4::Button::with_label("Remove");
    api_key_remove_button.set_valign(gtk4::Align::Center);
    api_key_remove_button.add_css_class("flat");
    api_key_remove_button.add_css_class("destructive-action");
    api_key_remove_button.set_visible(false);
    api_key_status_row.add_suffix(&api_key_remove_button);
    let api_key_row = adw::PasswordEntryRow::builder()
        .title(api_key_entry_title(None))
        .build();
    api_key_row.set_show_apply_button(true);

    let model_row = adw::EntryRow::builder()
        .title("Cloud model")
        .show_apply_button(true)
        .build();
    set_monospace_entry_text(&model_row);
    let batch_endpoint = EndpointEditor::new(EndpointKind::MistralBatch, "Mistral batch server");
    let realtime_endpoint =
        EndpointEditor::new(EndpointKind::MistralRealtime, "Mistral Realtime server");
    model_configuration_group.add(&api_key_status_row);
    model_configuration_group.add(&api_key_row);
    model_configuration_group.add(&model_row);
    model_configuration_group.add(&batch_endpoint.entry);
    model_configuration_group.add(&batch_endpoint.status);
    model_configuration_group.add(&realtime_endpoint.entry);
    model_configuration_group.add(&realtime_endpoint.status);

    // Downloadable local-model sub-rows
    let parakeet_backend_model = gtk4::StringList::new(&["On this computer", "On a server"]);
    let parakeet_backend_row = adw::ComboRow::builder()
        .title("Run model")
        .model(&parakeet_backend_model)
        .build();
    {
        let model_configuration_group = model_configuration_group.clone();
        let parakeet_backend_row = parakeet_backend_row.clone();
        provider_row.connect_selected_notify(move |row| {
            apply_transcriber_setup_copy(
                &model_configuration_group,
                row.selected(),
                parakeet_backend_row.selected(),
            );
        });
    }
    {
        let model_configuration_group = model_configuration_group.clone();
        let provider_row = provider_row.clone();
        parakeet_backend_row.connect_selected_notify(move |row| {
            apply_transcriber_setup_copy(
                &model_configuration_group,
                provider_row.selected(),
                row.selected(),
            );
        });
    }
    {
        let provider_icon = provider_icon.clone();
        let parakeet_backend_row = parakeet_backend_row.clone();
        provider_row.connect_selected_notify(move |row| {
            provider_icon.set_icon_name(Some(transcriber_location_icon_name(
                row.selected(),
                parakeet_backend_row.selected(),
            )));
        });
    }
    {
        let provider_icon = provider_icon.clone();
        let provider_row = provider_row.clone();
        parakeet_backend_row.connect_selected_notify(move |row| {
            provider_icon.set_icon_name(Some(transcriber_location_icon_name(
                provider_row.selected(),
                row.selected(),
            )));
        });
    }
    let parakeet_endpoint = EndpointEditor::new(EndpointKind::ParakeetHttp, "Transcription server");
    let execution_provider_model =
        gtk4::StringList::new(&["Automatic", "CPU", "NVIDIA GPU (CUDA)"]);
    let execution_provider_row = adw::ComboRow::builder()
        .title("Processor")
        .subtitle(execution_provider_subtitle(0))
        .subtitle_lines(2)
        .model(&execution_provider_model)
        .build();

    let model_status_row = adw::ActionRow::builder()
        .title("Local model")
        .subtitle("Checking model…")
        .title_lines(2)
        .use_markup(false)
        .build();

    let model_download_progress = gtk4::ProgressBar::builder()
        .valign(gtk4::Align::Center)
        .width_request(96)
        .visible(false)
        .build();
    model_download_progress.set_tooltip_text(Some("Model download progress"));
    model_download_progress
        .update_property(&[gtk4::accessible::Property::Label("Model download progress")]);
    model_status_row.add_suffix(&model_download_progress);

    let download_button = gtk4::Button::with_label(parakeet_model_action_label(
        voxkey_ipc::ParakeetConfig::DEFAULT_MODEL,
    ));
    download_button.set_valign(gtk4::Align::Center);
    download_button.add_css_class("suggested-action");
    download_button.set_visible(false);
    model_status_row.add_suffix(&download_button);

    let open_folder_button = gtk4::Button::from_icon_name("folder-open-symbolic");
    open_folder_button.set_valign(gtk4::Align::Center);
    open_folder_button.add_css_class("flat");
    open_folder_button.set_tooltip_text(Some("Open model folder"));
    open_folder_button.set_visible(expert_mode.get());
    open_folder_button.update_property(&[gtk4::accessible::Property::Label("Open model folder")]);
    model_status_row.add_suffix(&open_folder_button);

    let delete_model_button = gtk4::Button::from_icon_name("user-trash-symbolic");
    delete_model_button.set_valign(gtk4::Align::Center);
    delete_model_button.add_css_class("flat");
    delete_model_button.add_css_class("destructive-action");
    delete_model_button.set_tooltip_text(Some("Delete downloaded model"));
    delete_model_button.set_visible(false);
    delete_model_button
        .update_property(&[gtk4::accessible::Property::Label("Delete downloaded model")]);
    model_status_row.add_suffix(&delete_model_button);

    model_configuration_group.add(&parakeet_backend_row);
    model_configuration_group.add(&parakeet_endpoint.entry);
    if let Some(row) = &parakeet_endpoint.insecure_http_row {
        model_configuration_group.add(row);
    }
    model_configuration_group.add(&parakeet_endpoint.status);
    model_configuration_group.add(&execution_provider_row);
    model_configuration_group.add(&model_status_row);

    // Initially hide non-whisper.cpp rows (default provider)
    args_row.set_visible(expert_mode.get());
    api_key_status_row.set_visible(false);
    api_key_row.set_visible(false);
    model_row.set_visible(false);
    batch_endpoint.set_visible(false);
    realtime_endpoint.set_visible(false);
    parakeet_backend_row.set_visible(false);
    parakeet_endpoint.set_visible(false);
    execution_provider_row.set_visible(false);
    model_status_row.set_visible(false);

    models_box.append(&transcription_group);
    models_box.append(&model_configuration_group);
    let model_library = Rc::new(crate::model_library::ModelLibrary::new(
        &handle,
        &toast_overlay,
    ));
    models_box.append(&model_library.group);
    stack.add_named(&scroll_clamped(&models_box, 720), Some("models"));

    // Shared transcriber config state for building JSON from widgets
    let transcriber_state = Rc::new(RefCell::new(voxkey_ipc::TranscriberConfig::default()));
    // The first daemon snapshot populates empty widgets. Later snapshots merge
    // around any text the user is still editing instead of discarding it.
    let transcriber_widgets_initialized = Rc::new(Cell::new(false));
    // Guard to suppress send_transcriber_config during programmatic widget updates
    let updating_widgets = Rc::new(Cell::new(false));
    // Whether a keyring entry exists for the active provider, or `None` while
    // that status is being checked. The secret value itself is never sent over
    // D-Bus; the row only accepts a replacement.
    let api_key_stored = Rc::new(Cell::new(None));
    // Monotonically identifies the newest keyring status read. Editing the field
    // or changing providers invalidates every older asynchronous reply.
    let api_key_request_id = Rc::new(Cell::new(0_u64));

    // -- Audio Input page --
    let audio_box = gtk4::Box::new(gtk4::Orientation::Vertical, 24);
    let audio_group = adw::PreferencesGroup::builder()
        .title("Audio input")
        .description("Select the microphone used for new dictations")
        .build();
    let audio_device_model = gtk4::StringList::new(&["System default"]);
    let audio_device_row = adw::ComboRow::builder()
        .title("Microphone")
        .subtitle("Follow the current system default")
        .subtitle_lines(2)
        .model(&audio_device_model)
        .build();
    audio_device_row.add_prefix(&gtk4::Image::from_icon_name(
        "audio-input-microphone-symbolic",
    ));
    audio_group.add(&audio_device_row);

    let refresh_audio_icon = gtk4::Image::from_icon_name("view-refresh-symbolic");
    let refresh_audio_spinner = gtk4::Spinner::new();
    let refresh_audio_stack = gtk4::Stack::builder()
        .transition_type(gtk4::StackTransitionType::Crossfade)
        .transition_duration(100)
        .build();
    refresh_audio_stack.add_named(&refresh_audio_icon, Some("idle"));
    refresh_audio_stack.add_named(&refresh_audio_spinner, Some("refreshing"));
    refresh_audio_stack.set_visible_child_name("idle");
    let refresh_audio_button = gtk4::Button::builder()
        .child(&refresh_audio_stack)
        .tooltip_text("Refresh microphones")
        .valign(gtk4::Align::Center)
        .build();
    refresh_audio_button
        .update_property(&[gtk4::accessible::Property::Label("Refresh microphones")]);
    refresh_audio_button.add_css_class("flat");
    audio_group.set_header_suffix(Some(&refresh_audio_button));
    audio_box.append(&audio_group);

    let recording_format_group = adw::PreferencesGroup::builder()
        .title("Capture details")
        .description("Sample rate and channels used with the current transcription engine")
        .build();
    let recording_format_row = adw::ActionRow::builder()
        .title("Capture format")
        .subtitle("Loading…")
        .build();
    recording_format_row.add_prefix(&gtk4::Image::from_icon_name("audio-x-generic-symbolic"));
    recording_format_group.add(&recording_format_row);
    recording_format_group.set_visible(expert_mode.get());
    audio_box.append(&recording_format_group);
    stack.add_named(&scroll_clamped(&audio_box, 720), Some("audio"));

    let audio_devices = Rc::new(RefCell::new(Vec::<String>::new()));
    let updating_audio_widgets = Rc::new(Cell::new(false));
    {
        let audio_devices = audio_devices.clone();
        let updating_audio_widgets = updating_audio_widgets.clone();
        let handle = handle.clone();
        let toast_overlay = toast_overlay.clone();
        audio_device_row.connect_selected_notify(move |row| {
            if updating_audio_widgets.get() {
                return;
            }
            let selected = row.selected() as usize;
            let device = if selected == 0 {
                String::new()
            } else {
                audio_devices
                    .borrow()
                    .get(selected - 1)
                    .cloned()
                    .unwrap_or_default()
            };
            let previous_subtitle = row.subtitle().unwrap_or_default();
            row.set_subtitle("Saving microphone…");
            row.set_sensitive(false);
            let completion = handle.send(DaemonCommand::SetAudioInputDevice(device));
            let refresh_handle = handle.clone();
            let row = row.clone();
            let toast_overlay = toast_overlay.clone();
            glib::spawn_future_local(async move {
                let saved = completion.wait().await.is_ok();
                row.set_subtitle(&previous_subtitle);
                row.set_sensitive(true);
                if saved {
                    toast_overlay.add_toast(adw::Toast::new("Microphone changed"));
                } else {
                    toast_overlay.add_toast(adw::Toast::new(
                        "Could not change the microphone. Try again.",
                    ));
                    refresh_handle.send(DaemonCommand::RefreshAudioInputDevices);
                }
            });
        });
    }
    {
        let handle = handle.clone();
        let toast_overlay = toast_overlay.clone();
        let audio_group = audio_group.clone();
        let refresh_audio_stack = refresh_audio_stack.clone();
        let refresh_audio_spinner = refresh_audio_spinner.clone();
        refresh_audio_button.connect_clicked(move |button| {
            button.set_sensitive(false);
            button.set_tooltip_text(Some("Refreshing microphones"));
            button.update_property(&[gtk4::accessible::Property::Label("Refreshing microphones")]);
            refresh_audio_stack.set_visible_child_name("refreshing");
            refresh_audio_spinner.start();
            let completion = handle.send(DaemonCommand::RefreshAudioInputDevices);
            let button = button.clone();
            let refresh_audio_stack = refresh_audio_stack.clone();
            let refresh_audio_spinner = refresh_audio_spinner.clone();
            let toast_overlay = toast_overlay.clone();
            let audio_group = audio_group.clone();
            glib::spawn_future_local(async move {
                let refreshed = completion.wait().await.is_ok();
                refresh_audio_spinner.stop();
                refresh_audio_stack.set_visible_child_name("idle");
                button.set_tooltip_text(Some("Refresh microphones"));
                button.update_property(&[gtk4::accessible::Property::Label("Refresh microphones")]);
                button.set_sensitive(true);
                if refreshed {
                    toast_overlay.add_toast(adw::Toast::new("Microphones refreshed"));
                } else {
                    audio_group.set_description(Some(microphone_refresh_failure_description()));
                    toast_overlay.add_toast(microphone_refresh_failure_toast(&button));
                }
            });
        });
    }

    // -- Dictionary page --
    let dictionary_config = Rc::new(RefCell::new(voxkey_ipc::DictionaryConfig::default()));
    let dictionary_page =
        crate::dictionary::build_dictionary_page(dictionary_config, handle.clone(), &toast_overlay);
    stack.add_named(&dictionary_page.widget, Some("dictionary"));

    // -- General Settings page --
    let settings_box = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
    let status_group = adw::PreferencesGroup::builder().title("Status").build();
    let state_row = adw::ActionRow::builder()
        .title("Checking dictation…")
        .subtitle("Connecting to Voxkey")
        .subtitle_lines(2)
        .build();
    let state_icon = gtk4::Image::from_icon_name("media-record-symbolic");
    state_icon.add_css_class("accent");
    state_row.add_prefix(&state_icon);
    let state_spinner = gtk4::Spinner::new();
    state_spinner.set_valign(gtk4::Align::Center);
    state_spinner.start();
    state_row.add_suffix(&state_spinner);
    let portal_row = adw::ActionRow::builder()
        .title("Type into other apps")
        .subtitle("Checking desktop access…")
        .subtitle_lines(2)
        .build();
    portal_row.add_prefix(&gtk4::Image::from_icon_name(
        "preferences-system-privacy-symbolic",
    ));
    let portal_details_icon = gtk4::Image::from_icon_name("go-next-symbolic");
    portal_details_icon.add_css_class("dim-label");
    portal_details_icon.set_visible(false);
    portal_row.add_suffix(&portal_details_icon);
    let error_row = adw::ActionRow::builder()
        .title("Needs attention")
        .subtitle_lines(3)
        .use_markup(false)
        .activatable(true)
        .visible(false)
        .build();
    let error_icon = gtk4::Image::from_icon_name("dialog-error-symbolic");
    error_icon.add_css_class("error");
    error_row.add_prefix(&error_icon);
    let error_details_icon = gtk4::Image::from_icon_name("go-next-symbolic");
    error_details_icon.add_css_class("dim-label");
    error_row.add_suffix(&error_details_icon);
    status_group.add(&state_row);
    status_group.add(&portal_row);
    status_group.add(&error_row);
    settings_box.append(&status_group);

    let dictation_group = adw::PreferencesGroup::builder()
        .title("Start dictating")
        .description("Use one shortcut to start listening, then press it again when you finish")
        .build();
    let shortcut_label = gtk4::ShortcutLabel::new("");
    shortcut_label.set_valign(gtk4::Align::Center);
    let shortcut_row = adw::ActionRow::builder()
        .title("Keyboard shortcut")
        .subtitle("Choose a function key, media key, or key combination")
        .subtitle_lines(2)
        .use_markup(false)
        .activatable(true)
        .build();
    shortcut_row.set_tooltip_text(Some("Change keyboard shortcut"));
    shortcut_row.update_property(&[gtk4::accessible::Property::Description(
        "Activate to choose a new dictation shortcut",
    )]);
    shortcut_row.add_prefix(&gtk4::Image::from_icon_name(
        "preferences-desktop-keyboard-shortcuts-symbolic",
    ));
    shortcut_row.add_suffix(&shortcut_label);
    let shortcut_details_icon = gtk4::Image::from_icon_name("go-next-symbolic");
    shortcut_details_icon.add_css_class("dim-label");
    shortcut_details_icon.set_accessible_role(gtk4::AccessibleRole::Presentation);
    shortcut_row.add_suffix(&shortcut_details_icon);
    dictation_group.add(&shortcut_row);
    settings_box.append(&dictation_group);

    let preview_group = adw::PreferencesGroup::builder()
        .title("Live feedback")
        .description("Choose how much text Voxkey shows while you speak")
        .build();
    let preview_preset_model =
        gtk4::StringList::new(&["Automatic", "Always live", "Final only", "Custom"]);
    let preview_preset_row = adw::ComboRow::builder()
        .title("Feedback preset")
        .subtitle("Recommended — adapts to local and network models")
        .subtitle_lines(2)
        .model(&preview_preset_model)
        .build();
    preview_preset_row.add_prefix(&gtk4::Image::from_icon_name("view-reveal-symbolic"));
    preview_group.add(&preview_preset_row);
    settings_box.append(&preview_group);

    let preview_advanced_group = adw::PreferencesGroup::builder()
        .title("Fine-tune live feedback")
        .description("Detailed controls for performance, server load, and text stability")
        .build();
    let preview_mode_model = gtk4::StringList::new(&["Automatic", "Always", "Never"]);
    let preview_mode_row = adw::ComboRow::builder()
        .title("When to show live text")
        .subtitle("On for local models; off for network models")
        .subtitle_lines(2)
        .model(&preview_mode_model)
        .build();
    preview_advanced_group.add(&preview_mode_row);

    let preview_strategy_model = gtk4::StringList::new(&["Stable context", "Pause segments"]);
    let preview_strategy_row = adw::ComboRow::builder()
        .title("How text stabilizes")
        .subtitle("Keep agreed text and recheck only the uncertain tail")
        .subtitle_lines(2)
        .model(&preview_strategy_model)
        .build();
    preview_advanced_group.add(&preview_strategy_row);

    let preview_interval_adjustment = gtk4::Adjustment::new(
        1.0,
        voxkey_ipc::PreviewConfig::MIN_INTERVAL_MS as f64 / 1000.0,
        u32::MAX as f64 / 1000.0,
        0.25,
        1.0,
        0.0,
    );
    let preview_interval_row = adw::SpinRow::builder()
        .title("Update frequency")
        .subtitle("Seconds between preview updates; lower values use more processor and network")
        .subtitle_lines(2)
        .adjustment(&preview_interval_adjustment)
        .digits(2)
        .build();
    add_spin_row_unit(&preview_interval_row, "s");
    preview_advanced_group.add(&preview_interval_row);

    let preview_audio_limit_adjustment = gtk4::Adjustment::new(
        0.0,
        0.0,
        voxkey_ipc::PreviewConfig::MAX_AUDIO_SECONDS as f64,
        5.0,
        30.0,
        0.0,
    );
    let preview_audio_limit_row = adw::SpinRow::builder()
        .title("Audio per update")
        .subtitle("Seconds of unconfirmed audio per request; 0 keeps it unlimited")
        .subtitle_lines(2)
        .adjustment(&preview_audio_limit_adjustment)
        .snap_to_ticks(true)
        .build();
    add_spin_row_unit(&preview_audio_limit_row, "s");
    preview_advanced_group.add(&preview_audio_limit_row);

    let preview_state = Rc::new(RefCell::new(voxkey_ipc::PreviewConfig::default()));
    let updating_preview_widgets = Rc::new(Cell::new(false));

    let insertion_group = adw::PreferencesGroup::builder()
        .title("Typing timing")
        .description("Compatibility control for applications that miss very fast keystrokes")
        .build();
    let typing_delay_adjustment = gtk4::Adjustment::new(
        voxkey_ipc::InjectionConfig::default().typing_delay_ms as f64,
        0.0,
        voxkey_ipc::InjectionConfig::MAX_TYPING_DELAY_MS as f64,
        1.0,
        5.0,
        0.0,
    );
    let typing_delay_row = adw::SpinRow::builder()
        .title("Delay between keystrokes")
        .subtitle("Milliseconds between typed characters; 0 is fastest")
        .subtitle_lines(2)
        .adjustment(&typing_delay_adjustment)
        .build();
    add_spin_row_unit(&typing_delay_row, "ms");
    insertion_group.add(&typing_delay_row);

    let injection_state = Rc::new(RefCell::new(voxkey_ipc::InjectionConfig::default()));
    let updating_injection_widgets = Rc::new(Cell::new(false));

    let behavior_group = adw::PreferencesGroup::builder()
        .title("Application")
        .build();
    let hide_on_close = Rc::new(Cell::new(gui_settings::load_hide_on_close()));
    let hide_on_close_row = adw::SwitchRow::builder()
        .title("Keep running in background")
        .subtitle("Keep dictation available after closing this window")
        .active(hide_on_close.get())
        .build();
    {
        let hide_on_close = hide_on_close.clone();
        hide_on_close_row.connect_active_notify(move |row| {
            let value = row.is_active();
            hide_on_close.set(value);
            gui_settings::save_hide_on_close(value);
        });
    }
    behavior_group.add(&hide_on_close_row);

    let expert_mode_row = adw::SwitchRow::builder()
        .title("Expert mode")
        .subtitle("Show detailed preview, audio, typing, and troubleshooting controls")
        .subtitle_lines(2)
        .active(expert_mode.get())
        .build();
    behavior_group.add(&expert_mode_row);
    settings_box.append(&behavior_group);

    let advanced_group = adw::PreferencesGroup::builder()
        .title("Troubleshooting")
        .description("Use these actions when configuration or desktop permission changes")
        .build();

    let open_config_row = adw::ActionRow::builder()
        .title("Open configuration folder")
        .subtitle("View config.toml and other Voxkey settings")
        .activatable(true)
        .build();
    let open_config_icon = gtk4::Image::from_icon_name("folder-open-symbolic");
    open_config_icon.set_accessible_role(gtk4::AccessibleRole::Presentation);
    open_config_row.add_suffix(&open_config_icon);

    let reload_row = adw::ActionRow::builder()
        .title("Reload configuration")
        .subtitle("Reload settings from the configuration file on this computer")
        .activatable(true)
        .build();
    let reload_icon = gtk4::Image::from_icon_name("view-refresh-symbolic");
    reload_row.add_suffix(&reload_icon);

    let clear_token_row = adw::ActionRow::builder()
        .title("Reset desktop permission")
        .subtitle("Forget the saved desktop permission and ask again")
        .activatable(true)
        .build();
    let clear_icon = gtk4::Image::from_icon_name("edit-clear-symbolic");
    clear_token_row.add_suffix(&clear_icon);

    advanced_group.add(&open_config_row);
    advanced_group.add(&reload_row);
    advanced_group.add(&clear_token_row);

    preview_advanced_group.set_visible(expert_mode.get());
    insertion_group.set_visible(expert_mode.get());
    advanced_group.set_visible(expert_mode.get());
    {
        let expert_mode = expert_mode.clone();
        let preview_advanced_group = preview_advanced_group.clone();
        let insertion_group = insertion_group.clone();
        let advanced_group = advanced_group.clone();
        let recording_format_group = recording_format_group.clone();
        let transcriber_state = transcriber_state.clone();
        let command_row = command_row.clone();
        let whisper_model_row = whisper_model_row.clone();
        let args_row = args_row.clone();
        let api_key_status_row = api_key_status_row.clone();
        let api_key_row = api_key_row.clone();
        let model_row = model_row.clone();
        let batch_endpoint = batch_endpoint.clone();
        let realtime_endpoint = realtime_endpoint.clone();
        let parakeet_backend_row = parakeet_backend_row.clone();
        let parakeet_endpoint = parakeet_endpoint.clone();
        let execution_provider_row = execution_provider_row.clone();
        let model_status_row = model_status_row.clone();
        let open_folder_button = open_folder_button.clone();
        let download_button = download_button.clone();
        expert_mode_row.connect_active_notify(move |row| {
            let active = row.is_active();
            expert_mode.set(active);
            gui_settings::save_expert_mode(active);
            preview_advanced_group.set_visible(active);
            insertion_group.set_visible(active);
            advanced_group.set_visible(active);
            recording_format_group.set_visible(active);
            let current_action = if download_button.is_visible()
                && download_button.label().as_deref() == ModelStatusAction::OpenFolder.label()
            {
                ModelStatusAction::OpenFolder
            } else {
                ModelStatusAction::None
            };
            open_folder_button
                .set_visible(parakeet_model_folder_icon_visible(active, current_action));
            apply_transcriber_visibility(
                &transcriber_state.borrow(),
                active,
                &command_row,
                &whisper_model_row,
                &args_row,
                &api_key_status_row,
                &api_key_row,
                &model_row,
                &batch_endpoint,
                &realtime_endpoint,
                &parakeet_backend_row,
                &parakeet_endpoint,
                &execution_provider_row,
                &model_status_row,
            );
        });
    }
    settings_box.append(&preview_advanced_group);
    settings_box.append(&insertion_group);
    settings_box.append(&advanced_group);
    stack.add_named(&scroll_clamped(&settings_box, 720), Some("settings"));

    // Until the first daemon snapshot arrives, these widgets contain display
    // defaults rather than the user's authoritative settings. Keep them
    // readable but non-editable so a click cannot queue a change based on
    // placeholder state. Local-only application preferences remain usable.
    let daemon_backed_controls = vec![
        models_box.clone().upcast::<gtk4::Widget>(),
        audio_box.clone().upcast(),
        dictionary_page.widget.clone(),
        dictation_group.clone().upcast(),
        preview_group.clone().upcast(),
        preview_advanced_group.clone().upcast(),
        insertion_group.clone().upcast(),
        reload_row.clone().upcast(),
        clear_token_row.clone().upcast(),
    ];
    apply_daemon_control_state(&daemon_backed_controls, false, "Unavailable");

    // -- Permissions page --
    let retry_permission_button = gtk4::Button::with_label("Request desktop access");
    retry_permission_button.add_css_class("suggested-action");
    retry_permission_button.set_halign(gtk4::Align::Center);
    retry_permission_button.set_sensitive(false);
    retry_permission_button.set_visible(false);
    let permission_status = adw::StatusPage::builder()
        .icon_name("preferences-system-privacy-symbolic")
        .title("Allow Voxkey to type for you")
        .description(
            "GNOME asks for permission before Voxkey can type a transcription into another app.",
        )
        .child(&retry_permission_button)
        .build();
    stack.add_named(&permission_status, Some("permissions"));

    // -- Adaptive split navigation --
    let primary_list = gtk4::ListBox::new();
    primary_list.set_selection_mode(gtk4::SelectionMode::Single);
    primary_list.add_css_class("navigation-sidebar");
    let history_nav = navigation_row("History", "document-open-recent-symbolic");
    let models_nav = navigation_row("Transcription", "system-run-symbolic");
    let audio_nav = navigation_row("Audio input", "audio-input-microphone-symbolic");
    let dictionary_nav = navigation_row("Dictionary", "accessories-dictionary-symbolic");
    primary_list.append(&history_nav);
    primary_list.append(&models_nav);
    primary_list.append(&audio_nav);
    primary_list.append(&dictionary_nav);

    let secondary_list = gtk4::ListBox::new();
    secondary_list.set_selection_mode(gtk4::SelectionMode::Single);
    secondary_list.add_css_class("navigation-sidebar");
    let permissions_nav = navigation_row("Permissions", "preferences-system-privacy-symbolic");
    let settings_nav = navigation_row("General", "preferences-system-symbolic");
    secondary_list.append(&permissions_nav);
    secondary_list.append(&settings_nav);

    {
        let secondary_list = secondary_list.clone();
        let permissions_nav = permissions_nav.clone();
        portal_row.connect_activated(move |_| {
            secondary_list.select_row(Some(&permissions_nav));
        });
    }

    let last_error_state = Rc::new(RefCell::new(String::new()));
    let daemon_available = Rc::new(Cell::new(false));
    {
        let last_error_state = last_error_state.clone();
        let daemon_available = daemon_available.clone();
        let handle = handle.clone();
        let toast_overlay = toast_overlay.clone();
        error_row.connect_activated(move |row| {
            let error = last_error_state.borrow().clone();
            if error.is_empty() {
                return;
            }
            let dialog =
                build_error_details_dialog(&error, &handle, &toast_overlay, daemon_available.get());
            if let Some(root) = row.root() {
                dialog.present(Some(&root));
            }
        });
    }

    let sidebar_content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    primary_list.set_margin_top(12);
    primary_list.set_margin_start(8);
    primary_list.set_margin_end(8);
    sidebar_content.append(&primary_list);
    let sidebar_spacer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    sidebar_spacer.set_vexpand(true);
    sidebar_content.append(&sidebar_spacer);
    let separator = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    separator.set_margin_start(12);
    separator.set_margin_end(12);
    sidebar_content.append(&separator);
    secondary_list.set_margin_top(8);
    secondary_list.set_margin_bottom(8);
    secondary_list.set_margin_start(8);
    secondary_list.set_margin_end(8);
    sidebar_content.append(&secondary_list);

    let sidebar_title = adw::WindowTitle::new("Voxkey", "Voice dictation");
    let sidebar_header = adw::HeaderBar::builder()
        .title_widget(&sidebar_title)
        .show_end_title_buttons(false)
        .build();
    let sidebar_toolbar = adw::ToolbarView::new();
    sidebar_toolbar.add_top_bar(&sidebar_header);
    sidebar_toolbar.set_content(Some(&sidebar_content));

    let content_title = adw::WindowTitle::new("History", "");
    let content_header = adw::HeaderBar::builder()
        .title_widget(&content_title)
        .show_start_title_buttons(false)
        .build();
    let banner = adw::Banner::new("Starting Voxkey…");
    banner.set_revealed(true);
    let shell_extension_restart_banner = build_shell_extension_restart_banner(&toast_overlay);
    let allow_unmask_on_start = Rc::new(Cell::new(false));
    let cancel_from_banner_available = Rc::new(Cell::new(false));
    let cancel_from_banner_pending = Rc::new(Cell::new(false));
    {
        let handle = handle.clone();
        let allow_unmask_on_start = allow_unmask_on_start.clone();
        let cancel_available = cancel_from_banner_available.clone();
        let cancel_pending = cancel_from_banner_pending.clone();
        banner.connect_button_clicked(move |banner| {
            if cancel_available.get() {
                if cancel_pending.replace(true) {
                    return;
                }
                cancel_available.set(false);
                banner.set_title("Cancelling dictation…");
                banner.set_button_label(None);
                let completion = handle.send(DaemonCommand::CancelDictation);
                let banner = banner.clone();
                let cancel_available = cancel_available.clone();
                let cancel_pending = cancel_pending.clone();
                glib::spawn_future_local(async move {
                    if completion.wait().await.is_err() && cancel_pending.replace(false) {
                        cancel_available.set(true);
                        banner.set_title("Could not cancel the current dictation. Try again.");
                        banner.set_button_label(Some("Try again"));
                        banner.set_revealed(true);
                    }
                });
                return;
            }
            let unmask = allow_unmask_on_start.get();
            banner.set_title(if unmask {
                "Allowing and starting Voxkey…"
            } else {
                "Starting Voxkey…"
            });
            banner.set_button_label(None);
            handle.start_service(unmask);
        });
    }
    stack.set_vexpand(true);
    let content_toolbar = adw::ToolbarView::new();
    content_toolbar.add_top_bar(&content_header);
    // Recovery banners belong to the fixed chrome so a page with a tall
    // minimum size cannot squeeze them out of a compact window.
    content_toolbar.add_top_bar(&banner);
    content_toolbar.add_top_bar(&shell_extension_restart_banner);
    content_toolbar.set_content(Some(&stack));

    let sidebar_page = adw::NavigationPage::new(&sidebar_toolbar, "Voxkey");
    let content_page = adw::NavigationPage::new(&content_toolbar, "Settings");
    let split_view = adw::NavigationSplitView::new();
    split_view.set_sidebar(Some(&sidebar_page));
    split_view.set_content(Some(&content_page));
    split_view.set_min_sidebar_width(210.0);
    split_view.set_max_sidebar_width(260.0);
    split_view.set_sidebar_width_fraction(0.25);
    toast_overlay.set_child(Some(&split_view));

    let restored_window_state =
        gui_settings::load_window_state().unwrap_or(gui_settings::WindowState {
            width: DEFAULT_WINDOW_WIDTH,
            height: DEFAULT_WINDOW_HEIGHT,
            maximized: false,
        });
    let last_normal_window_size = Rc::new(Cell::new((
        restored_window_state.width,
        restored_window_state.height,
    )));
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Voxkey")
        .default_width(restored_window_state.width)
        .default_height(restored_window_state.height)
        .content(&toast_overlay)
        .build();
    if restored_window_state.maximized {
        window.maximize();
    }

    let show_page_action =
        gtk4::gio::SimpleAction::new("show-page", Some(gtk4::glib::VariantTy::STRING));
    {
        let primary_list = primary_list.clone();
        let secondary_list = secondary_list.clone();
        let history_nav = history_nav.clone();
        let models_nav = models_nav.clone();
        let audio_nav = audio_nav.clone();
        let dictionary_nav = dictionary_nav.clone();
        let permissions_nav = permissions_nav.clone();
        let settings_nav = settings_nav.clone();
        let stack = stack.clone();
        let content_title = content_title.clone();
        let split_view = split_view.clone();
        show_page_action.connect_activate(move |_, parameter| {
            let Some(page) = parameter.and_then(gtk4::glib::Variant::str) else {
                return;
            };
            let (list, row, other_list, name, title) = match page {
                "transcription" => (
                    &primary_list,
                    &models_nav,
                    &secondary_list,
                    "models",
                    "Transcription",
                ),
                "audio" => (
                    &primary_list,
                    &audio_nav,
                    &secondary_list,
                    "audio",
                    "Audio input",
                ),
                "dictionary" => (
                    &primary_list,
                    &dictionary_nav,
                    &secondary_list,
                    "dictionary",
                    "Dictionary",
                ),
                "permissions" => (
                    &secondary_list,
                    &permissions_nav,
                    &primary_list,
                    "permissions",
                    "Permissions",
                ),
                "general" => (
                    &secondary_list,
                    &settings_nav,
                    &primary_list,
                    "settings",
                    "General",
                ),
                _ => (
                    &primary_list,
                    &history_nav,
                    &secondary_list,
                    "history",
                    "History",
                ),
            };
            other_list.unselect_all();
            list.select_row(Some(row));
            stack.set_visible_child_name(name);
            content_title.set_title(title);
            split_view.set_show_content(true);
            gui_settings::save_last_page(page);
        });
    }
    window.add_action(&show_page_action);
    crate::menu::register_page_shortcuts(app);

    let search_history_action = gtk4::gio::SimpleAction::new("search-history", None);
    {
        let show_page_action = show_page_action.clone();
        let stack = stack.clone();
        let search_entry = history_page.search_entry.clone();
        let focus_dictionary_search = dictionary_page.focus_search.clone();
        search_history_action.connect_activate(move |_, _| {
            if stack.visible_child_name().as_deref() == Some("dictionary") {
                focus_dictionary_search();
                return;
            }
            show_page_action.activate(Some(&"history".to_variant()));
            if search_entry.is_visible() {
                search_entry.grab_focus();
            }
        });
    }
    window.add_action(&search_history_action);
    app.set_accels_for_action("win.search-history", &["<Control>f"]);

    window.add_action(&history_page.copy_latest_action);
    app.set_accels_for_action("win.copy-latest", &[crate::menu::COPY_LATEST_ACCELERATOR]);

    let menu_button = crate::menu::setup_primary_menu(app, &window);
    content_header.pack_end(&menu_button);

    // All graceful application exits converge here. If the process is killed
    // too abruptly for this hook to run, the daemon-side D-Bus lifecycle
    // monitor provides the same shutdown request independently.
    let handle_for_shutdown = handle.clone();
    let window_for_shutdown = window.downgrade();
    let last_normal_window_size_for_shutdown = last_normal_window_size.clone();
    app.connect_shutdown(move |_| {
        if let Some(window) = window_for_shutdown.upgrade() {
            save_window_state(&window, &last_normal_window_size_for_shutdown);
        }
        handle_for_shutdown.send_quit_and_wait();
    });

    let breakpoint = adw::Breakpoint::new(
        adw::BreakpointCondition::parse("max-width: 760px").expect("valid breakpoint"),
    );
    let collapsed = true.to_value();
    breakpoint.add_setter(&split_view, "collapsed", Some(&collapsed));
    window.add_breakpoint(breakpoint);

    let narrow_breakpoint = adw::Breakpoint::new(
        adw::BreakpointCondition::parse("max-width: 500px").expect("valid breakpoint"),
    );
    narrow_breakpoint.add_setter(&split_view, "collapsed", Some(&collapsed));
    let hide_dictionary_switcher = false.to_value();
    narrow_breakpoint.add_setter(
        &dictionary_page.switcher,
        "visible",
        Some(&hide_dictionary_switcher),
    );
    let reveal_dictionary_switcher_bar = true.to_value();
    narrow_breakpoint.add_setter(
        &dictionary_page.switcher_bar,
        "reveal",
        Some(&reveal_dictionary_switcher_bar),
    );
    let hide_shortcut_label = false.to_value();
    narrow_breakpoint.add_setter(&shortcut_label, "visible", Some(&hide_shortcut_label));
    window.add_breakpoint(narrow_breakpoint);

    {
        let secondary_list = secondary_list.clone();
        let models_nav = models_nav.clone();
        let audio_nav = audio_nav.clone();
        let dictionary_nav = dictionary_nav.clone();
        let stack = stack.clone();
        let content_title = content_title.clone();
        let split_view = split_view.clone();
        primary_list.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            secondary_list.unselect_all();
            let (name, title) = if row == &models_nav {
                ("models", "Transcription")
            } else if row == &audio_nav {
                ("audio", "Audio input")
            } else if row == &dictionary_nav {
                ("dictionary", "Dictionary")
            } else {
                ("history", "History")
            };
            stack.set_visible_child_name(name);
            content_title.set_title(title);
            split_view.set_show_content(true);
            gui_settings::save_last_page(match name {
                "models" => "transcription",
                name => name,
            });
        });
    }
    {
        let primary_list = primary_list.clone();
        let stack = stack.clone();
        let content_title = content_title.clone();
        let split_view = split_view.clone();
        let permissions_nav = permissions_nav.clone();
        secondary_list.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            primary_list.unselect_all();
            let (name, title) = if row == &permissions_nav {
                ("permissions", "Permissions")
            } else {
                ("settings", "General")
            };
            stack.set_visible_child_name(name);
            content_title.set_title(title);
            split_view.set_show_content(true);
            gui_settings::save_last_page(if name == "settings" { "general" } else { name });
        });
    }
    {
        let secondary_list = secondary_list.clone();
        let settings_nav = settings_nav.clone();
        history_page.empty_action.connect_clicked(move |_| {
            secondary_list.select_row(Some(&settings_nav));
        });
    }
    // -- Wire up user actions --
    wire_shortcut_capture(&shortcut_row, &handle, &toast_overlay, &window);
    wire_whisper_command_picker(
        &choose_command_button,
        &command_row,
        &whisper_model_row,
        &transcriber_state,
        &updating_widgets,
        &handle,
        &toast_overlay,
        &window,
    );
    wire_whisper_model_picker(
        &choose_whisper_model_button,
        &whisper_model_row,
        &args_row,
        &transcriber_state,
        &updating_widgets,
        &handle,
        &toast_overlay,
        &window,
    );
    wire_transcriber_actions(
        &provider_row,
        &command_row,
        &choose_command_button,
        &whisper_model_row,
        &args_row,
        &api_key_status_row,
        &api_key_row,
        &api_key_remove_button,
        &model_row,
        &batch_endpoint,
        &realtime_endpoint,
        &parakeet_backend_row,
        &parakeet_endpoint,
        &execution_provider_row,
        &model_status_row,
        &model_download_progress,
        &download_button,
        &delete_model_button,
        &open_folder_button,
        &transcriber_state,
        &updating_widgets,
        &api_key_stored,
        &api_key_request_id,
        &expert_mode,
        &handle,
        &toast_overlay,
    );
    wire_preview_actions(
        &preview_preset_row,
        &preview_mode_row,
        &preview_strategy_row,
        &preview_interval_row,
        &preview_audio_limit_row,
        &expert_mode_row,
        &preview_state,
        &transcriber_state,
        &updating_preview_widgets,
        &handle,
    );
    wire_advanced_actions(
        &open_config_row,
        &reload_row,
        &clear_token_row,
        &handle,
        &toast_overlay,
    );
    wire_injection_actions(
        &typing_delay_row,
        &injection_state,
        &updating_injection_widgets,
        &handle,
    );

    // -- Wire close-request for hide-on-close --
    let hide_on_close_for_close = hide_on_close.clone();
    let last_normal_window_size_for_close = last_normal_window_size.clone();
    window.connect_close_request(move |win| {
        save_window_state(win, &last_normal_window_size_for_close);
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
    let state_spinner = state_spinner.clone();
    let portal_row = portal_row.clone();
    let portal_details_icon_update = portal_details_icon.clone();
    let error_row_update = error_row.clone();
    let last_error_state_update = last_error_state.clone();
    let shortcut_row_update = shortcut_row.clone();
    let shortcut_label_update = shortcut_label.clone();
    let provider_row_update = provider_row.clone();
    let provider_model_update = provider_model.clone();
    let command_row_update = command_row.clone();
    let choose_command_button_update = choose_command_button.clone();
    let whisper_model_row_update = whisper_model_row.clone();
    let args_row_update = args_row.clone();
    let api_key_status_row_update = api_key_status_row.clone();
    let api_key_row_update = api_key_row.clone();
    let api_key_remove_button_update = api_key_remove_button.clone();
    let model_row_update = model_row.clone();
    let batch_endpoint_update = batch_endpoint.clone();
    let realtime_endpoint_update = realtime_endpoint.clone();
    let parakeet_backend_row_update = parakeet_backend_row.clone();
    let parakeet_endpoint_update = parakeet_endpoint.clone();
    let execution_provider_row_update = execution_provider_row.clone();
    let model_status_row_update = model_status_row.clone();
    let model_download_progress_update = model_download_progress.clone();
    let download_button_update = download_button.clone();
    let delete_model_button_update = delete_model_button.clone();
    let open_folder_button_update = open_folder_button.clone();
    let model_library_update = model_library.clone();
    let transcriber_state_update = transcriber_state.clone();
    let transcriber_widgets_initialized_update = transcriber_widgets_initialized.clone();
    let updating_widgets_update = updating_widgets.clone();
    let api_key_stored_for_update = api_key_stored.clone();
    let api_key_request_id_for_update = api_key_request_id.clone();
    let expert_mode_update = expert_mode.clone();
    let typing_delay_row_update = typing_delay_row.clone();
    let injection_state_update = injection_state.clone();
    let updating_injection_widgets_update = updating_injection_widgets.clone();
    let preview_preset_row_update = preview_preset_row.clone();
    let preview_mode_row_update = preview_mode_row.clone();
    let preview_strategy_row_update = preview_strategy_row.clone();
    let preview_interval_row_update = preview_interval_row.clone();
    let preview_audio_limit_row_update = preview_audio_limit_row.clone();
    let preview_state_update = preview_state.clone();
    let updating_preview_widgets_update = updating_preview_widgets.clone();
    let history_apply_update = history_page.apply_json.clone();
    let history_empty_action_update = history_page.empty_action.clone();
    let history_set_actions_available_update = history_page.set_actions_available.clone();
    let dictionary_apply_update = dictionary_page.apply_json.clone();
    let audio_group_update = audio_group.clone();
    let audio_device_row_update = audio_device_row.clone();
    let audio_devices_update = audio_devices.clone();
    let updating_audio_widgets_update = updating_audio_widgets.clone();
    let recording_format_row_update = recording_format_row.clone();
    let permissions_nav_update = permissions_nav.clone();
    let permission_status_update = permission_status.clone();
    let retry_permission_button_update = retry_permission_button.clone();
    let permission_daemon_state = Rc::new(RefCell::new("Unavailable".to_string()));
    let permission_daemon_state_update = permission_daemon_state.clone();
    let permission_portal_connected = Rc::new(Cell::new(false));
    let permission_portal_connected_update = permission_portal_connected.clone();
    let permission_daemon_available_update = daemon_available.clone();
    {
        let permission_portal_connected = permission_portal_connected.clone();
        let handle = handle.clone();
        let toast_overlay = toast_overlay.clone();
        retry_permission_button.connect_clicked(move |button| {
            if permission_portal_connected.get() {
                if let Some(root) = button.root() {
                    present_reset_desktop_permission_dialog(&root, &handle, &toast_overlay);
                }
            } else {
                let completion = handle.send(DaemonCommand::ClearRestoreToken);
                toast_after_success(completion, &toast_overlay, "Requesting desktop access…");
            }
        });
    }
    let banner = banner.clone();
    let allow_unmask_on_start_update = allow_unmask_on_start.clone();
    let cancel_from_banner_available_update = cancel_from_banner_available.clone();
    let cancel_from_banner_pending_update = cancel_from_banner_pending.clone();
    let toast_overlay_update = toast_overlay.clone();
    let handle_update = handle.clone();
    let daemon_backed_controls_update = daemon_backed_controls;

    glib::spawn_future_local(async move {
        let mut current_sample_rate = 0;
        let mut current_channels = 0;
        while let Some(update) = update_rx.recv().await {
            match update {
                DaemonUpdate::Connected(snapshot) => {
                    let DaemonSnapshot {
                        state,
                        shortcut_trigger,
                        shortcut_description,
                        transcriber_config,
                        injection_config,
                        preview_config,
                        dictionary_config,
                        transcription_history,
                        audio_input_devices,
                        audio_input_device,
                        sample_rate,
                        channels,
                        portal_connected,
                        last_transcript,
                        last_error,
                    } = *snapshot;
                    apply_daemon_control_state(&daemon_backed_controls_update, true, &state);
                    history_set_actions_available_update(daemon_controls_are_editable(
                        true, &state,
                    ));
                    history_empty_action_update.set_label(history_empty_action_label(true, &state));
                    allow_unmask_on_start_update.set(false);
                    apply_connected_banner_state(
                        &banner,
                        &state,
                        &cancel_from_banner_available_update,
                        &cancel_from_banner_pending_update,
                    );
                    *permission_daemon_state_update.borrow_mut() = state.clone();
                    permission_portal_connected_update.set(portal_connected);
                    permission_daemon_available_update.set(true);
                    update_state_row(
                        &state_row,
                        &state_icon,
                        &state_spinner,
                        &state,
                        !last_error.trim().is_empty(),
                    );
                    apply_portal_state(
                        portal_connected,
                        true,
                        &state,
                        &portal_row,
                        &portal_details_icon_update,
                        &permissions_nav_update,
                        &permission_status_update,
                        &retry_permission_button_update,
                    );
                    shortcut_label_update.set_accelerator(&shortcut_trigger);
                    shortcut_row_update
                        .set_subtitle(&shortcut_subtitle(&shortcut_description, true));
                    let _ = last_transcript;
                    history_apply_update(&transcription_history);
                    dictionary_apply_update(&dictionary_config);
                    apply_audio_devices_to_widgets(
                        &audio_input_devices,
                        &audio_input_device,
                        &audio_group_update,
                        &audio_device_row_update,
                        &audio_devices_update,
                        &updating_audio_widgets_update,
                    );
                    current_sample_rate = sample_rate;
                    current_channels = channels;
                    recording_format_row_update
                        .set_subtitle(&recording_format_description(sample_rate, channels));
                    apply_transcriber_config_to_widgets(
                        &transcriber_config,
                        &provider_row_update,
                        &provider_model_update,
                        &command_row_update,
                        &choose_command_button_update,
                        &whisper_model_row_update,
                        &args_row_update,
                        &api_key_status_row_update,
                        &api_key_row_update,
                        &api_key_remove_button_update,
                        &model_row_update,
                        &batch_endpoint_update,
                        &realtime_endpoint_update,
                        &parakeet_backend_row_update,
                        &parakeet_endpoint_update,
                        &execution_provider_row_update,
                        &model_status_row_update,
                        &download_button_update,
                        &transcriber_state_update,
                        &updating_widgets_update,
                        &api_key_stored_for_update,
                        &expert_mode_update,
                        transcriber_widgets_initialized_update.replace(true),
                    );
                    apply_preview_config_to_widgets(
                        &preview_config,
                        &preview_preset_row_update,
                        &preview_mode_row_update,
                        &preview_strategy_row_update,
                        &preview_interval_row_update,
                        &preview_audio_limit_row_update,
                        &preview_state_update,
                        &transcriber_state_update,
                        &updating_preview_widgets_update,
                    );
                    if !last_error.is_empty() {
                        toast_overlay_update.add_toast(last_error_toast());
                    }
                    apply_last_error(&last_error, &error_row_update, &last_error_state_update);
                    let (parsed_transcriber_config, parsed_injection_config) =
                        parse_initial_config_sections(&transcriber_config, &injection_config);
                    model_library_update.set_selected(parsed_transcriber_config.as_ref().and_then(
                        |config| {
                            (config.provider == voxkey_ipc::TranscriberProvider::Parakeet)
                                .then_some(config.parakeet.model.as_str())
                        },
                    ));
                    model_library_update.request_statuses(&handle_update);
                    // Query downloaded-model status only for the local Parakeet backend.
                    if let Some(model_name) = parsed_transcriber_config
                        .as_ref()
                        .and_then(local_parakeet_model_name)
                    {
                        handle_update.send(DaemonCommand::ModelStatus(model_name.to_string()));
                    }
                    if let Some(ic) = parsed_injection_config {
                        apply_injection_config_to_widgets_from_config(
                            &ic,
                            &typing_delay_row_update,
                            &injection_state_update,
                            &updating_injection_widgets_update,
                        );
                    }
                    // Ask whether the active provider has a key stored. The key
                    // value itself never crosses D-Bus.
                    let config = transcriber_state_update.borrow();
                    let service = transcriber_api_service(&config);
                    request_api_key_status(service, &api_key_request_id_for_update, &handle_update);
                }
                DaemonUpdate::Disconnected {
                    message,
                    can_unmask,
                } => {
                    apply_daemon_control_state(
                        &daemon_backed_controls_update,
                        false,
                        "Unavailable",
                    );
                    history_set_actions_available_update(false);
                    history_empty_action_update
                        .set_label(history_empty_action_label(false, "Unavailable"));
                    allow_unmask_on_start_update.set(can_unmask);
                    cancel_from_banner_available_update.set(false);
                    cancel_from_banner_pending_update.set(false);
                    banner.set_title(&message);
                    banner.set_button_label(Some(if can_unmask {
                        "Allow and start"
                    } else {
                        "Start Voxkey"
                    }));
                    banner.set_revealed(true);
                    *permission_daemon_state_update.borrow_mut() = "Unavailable".to_string();
                    permission_portal_connected_update.set(false);
                    permission_daemon_available_update.set(false);
                    update_state_row(
                        &state_row,
                        &state_icon,
                        &state_spinner,
                        "Unavailable",
                        !last_error_state_update.borrow().trim().is_empty(),
                    );
                    shortcut_row_update.set_subtitle(&shortcut_subtitle("", false));
                    apply_portal_state(
                        false,
                        false,
                        "Unavailable",
                        &portal_row,
                        &portal_details_icon_update,
                        &permissions_nav_update,
                        &permission_status_update,
                        &retry_permission_button_update,
                    );
                }
                DaemonUpdate::ServiceStarting { unmasking } => {
                    apply_daemon_control_state(
                        &daemon_backed_controls_update,
                        false,
                        "StartingService",
                    );
                    history_set_actions_available_update(false);
                    history_empty_action_update
                        .set_label(history_empty_action_label(false, "StartingService"));
                    allow_unmask_on_start_update.set(false);
                    cancel_from_banner_available_update.set(false);
                    cancel_from_banner_pending_update.set(false);
                    banner.set_title(if unmasking {
                        "Allowing and starting Voxkey…"
                    } else {
                        "Starting Voxkey…"
                    });
                    banner.set_button_label(None);
                    banner.set_revealed(true);
                    *permission_daemon_state_update.borrow_mut() = "StartingService".to_string();
                    permission_portal_connected_update.set(false);
                    permission_daemon_available_update.set(false);
                    update_state_row(
                        &state_row,
                        &state_icon,
                        &state_spinner,
                        "StartingService",
                        !last_error_state_update.borrow().trim().is_empty(),
                    );
                    apply_portal_state(
                        false,
                        false,
                        "StartingService",
                        &portal_row,
                        &portal_details_icon_update,
                        &permissions_nav_update,
                        &permission_status_update,
                        &retry_permission_button_update,
                    );
                }
                DaemonUpdate::StateChanged(state) => {
                    apply_daemon_control_state(&daemon_backed_controls_update, true, &state);
                    history_set_actions_available_update(daemon_controls_are_editable(
                        true, &state,
                    ));
                    history_empty_action_update.set_label(history_empty_action_label(true, &state));
                    apply_connected_banner_state(
                        &banner,
                        &state,
                        &cancel_from_banner_available_update,
                        &cancel_from_banner_pending_update,
                    );
                    *permission_daemon_state_update.borrow_mut() = state.clone();
                    update_state_row(
                        &state_row,
                        &state_icon,
                        &state_spinner,
                        &state,
                        !last_error_state_update.borrow().trim().is_empty(),
                    );
                    apply_portal_state(
                        permission_portal_connected_update.get(),
                        permission_daemon_available_update.get(),
                        &state,
                        &portal_row,
                        &portal_details_icon_update,
                        &permissions_nav_update,
                        &permission_status_update,
                        &retry_permission_button_update,
                    );
                }
                DaemonUpdate::PropertyChanged { name, value } => match name.as_str() {
                    "last_transcript" => {}
                    "transcription_history" => history_apply_update(&value),
                    "last_error" => {
                        apply_last_error(&value, &error_row_update, &last_error_state_update);
                        update_state_row(
                            &state_row,
                            &state_icon,
                            &state_spinner,
                            permission_daemon_state_update.borrow().as_str(),
                            !value.trim().is_empty(),
                        );
                        if !value.is_empty() {
                            toast_overlay_update.add_toast(last_error_toast());
                            if value.starts_with("Download failed:") {
                                model_library_update.request_statuses(&handle_update);
                            }
                        }
                    }
                    "portal_connected" => {
                        let portal_connected = value == "true";
                        permission_portal_connected_update.set(portal_connected);
                        let daemon_state = permission_daemon_state_update.borrow();
                        apply_portal_state(
                            portal_connected,
                            permission_daemon_available_update.get(),
                            daemon_state.as_str(),
                            &portal_row,
                            &portal_details_icon_update,
                            &permissions_nav_update,
                            &permission_status_update,
                            &retry_permission_button_update,
                        );
                    }
                    "shortcut_trigger" => {
                        shortcut_label_update.set_accelerator(&value);
                    }
                    "shortcut_description" => {
                        shortcut_row_update.set_subtitle(&shortcut_subtitle(&value, true));
                    }
                    "transcriber_config" => {
                        let previous_service =
                            transcriber_api_service(&transcriber_state_update.borrow());
                        apply_transcriber_config_to_widgets(
                            &value,
                            &provider_row_update,
                            &provider_model_update,
                            &command_row_update,
                            &choose_command_button_update,
                            &whisper_model_row_update,
                            &args_row_update,
                            &api_key_status_row_update,
                            &api_key_row_update,
                            &api_key_remove_button_update,
                            &model_row_update,
                            &batch_endpoint_update,
                            &realtime_endpoint_update,
                            &parakeet_backend_row_update,
                            &parakeet_endpoint_update,
                            &execution_provider_row_update,
                            &model_status_row_update,
                            &download_button_update,
                            &transcriber_state_update,
                            &updating_widgets_update,
                            &api_key_stored_for_update,
                            &expert_mode_update,
                            transcriber_widgets_initialized_update.replace(true),
                        );
                        let service = transcriber_api_service(&transcriber_state_update.borrow());
                        if previous_service != service {
                            request_api_key_status(
                                service,
                                &api_key_request_id_for_update,
                                &handle_update,
                            );
                        }
                        update_preview_widget_context(
                            &preview_state_update.borrow(),
                            &transcriber_state_update.borrow(),
                            &preview_preset_row_update,
                            &preview_mode_row_update,
                            &preview_strategy_row_update,
                            &preview_interval_row_update,
                            &preview_audio_limit_row_update,
                            &updating_preview_widgets_update,
                        );
                        {
                            let config = transcriber_state_update.borrow();
                            model_library_update.set_selected(
                                (config.provider == voxkey_ipc::TranscriberProvider::Parakeet)
                                    .then_some(config.parakeet.model.as_str()),
                            );
                        }
                        let model_name = {
                            let config = transcriber_state_update.borrow();
                            local_parakeet_model_name(&config).map(str::to_owned)
                        };
                        if let Some(model_name) = model_name {
                            apply_model_status(
                                "checking",
                                &model_name,
                                &model_status_row_update,
                                &model_download_progress_update,
                                &download_button_update,
                                &delete_model_button_update,
                                &open_folder_button_update,
                                expert_mode_update.get(),
                            );
                            handle_update.send(DaemonCommand::ModelStatus(model_name));
                        }
                    }
                    "injection_config" => {
                        apply_injection_config_to_widgets(
                            &value,
                            &typing_delay_row_update,
                            &injection_state_update,
                            &updating_injection_widgets_update,
                        );
                    }
                    "preview_config" => {
                        apply_preview_config_to_widgets(
                            &value,
                            &preview_preset_row_update,
                            &preview_mode_row_update,
                            &preview_strategy_row_update,
                            &preview_interval_row_update,
                            &preview_audio_limit_row_update,
                            &preview_state_update,
                            &transcriber_state_update,
                            &updating_preview_widgets_update,
                        );
                    }
                    "dictionary_config" => {
                        dictionary_apply_update(&value);
                    }
                    "audio_input_device" => {
                        handle_update.send(DaemonCommand::RefreshAudioInputDevices);
                    }
                    "sample_rate" => {
                        if let Ok(sample_rate) = value.parse() {
                            current_sample_rate = sample_rate;
                            recording_format_row_update.set_subtitle(
                                &recording_format_description(
                                    current_sample_rate,
                                    current_channels,
                                ),
                            );
                        }
                    }
                    "channels" => {
                        if let Ok(channels) = value.parse() {
                            current_channels = channels;
                            recording_format_row_update.set_subtitle(
                                &recording_format_description(
                                    current_sample_rate,
                                    current_channels,
                                ),
                            );
                        }
                    }
                    _ => {}
                },
                DaemonUpdate::DownloadProgress {
                    model_name,
                    percent,
                } => {
                    model_library_update.set_progress(&model_name, percent);
                    if model_name == transcriber_state_update.borrow().parakeet.model
                        && model_status_row_update.subtitle().as_deref()
                            != Some("Cancelling download…")
                    {
                        apply_model_status(
                            "downloading",
                            &model_name,
                            &model_status_row_update,
                            &model_download_progress_update,
                            &download_button_update,
                            &delete_model_button_update,
                            &open_folder_button_update,
                            expert_mode_update.get(),
                        );
                        model_status_row_update.set_subtitle(&format!("Downloading… {percent}%"));
                        update_model_download_progress(&model_download_progress_update, percent);
                        if percent >= 100 {
                            apply_model_status(
                                "available",
                                &model_name,
                                &model_status_row_update,
                                &model_download_progress_update,
                                &download_button_update,
                                &delete_model_button_update,
                                &open_folder_button_update,
                                expert_mode_update.get(),
                            );
                        }
                    }
                }
                DaemonUpdate::ModelStatusResult { model_name, status } => {
                    model_library_update.set_status(&model_name, &status);
                    if model_name == transcriber_state_update.borrow().parakeet.model {
                        apply_model_status(
                            &status,
                            &model_name,
                            &model_status_row_update,
                            &model_download_progress_update,
                            &download_button_update,
                            &delete_model_button_update,
                            &open_folder_button_update,
                            expert_mode_update.get(),
                        );
                    }
                }
                DaemonUpdate::ApiKeyStatus {
                    service,
                    present,
                    request_id,
                } => {
                    let was_updating = updating_widgets_update.replace(true);
                    apply_api_key_status(
                        &service,
                        present,
                        request_id,
                        api_key_request_id_for_update.get(),
                        &api_key_status_row_update,
                        &api_key_row_update,
                        &api_key_remove_button_update,
                        &api_key_stored_for_update,
                        &transcriber_state_update,
                    );
                    updating_widgets_update.set(was_updating);
                }
                DaemonUpdate::AudioDevices {
                    devices_json,
                    selected_device,
                } => {
                    apply_audio_devices_to_widgets(
                        &devices_json,
                        &selected_device,
                        &audio_group_update,
                        &audio_device_row_update,
                        &audio_devices_update,
                        &updating_audio_widgets_update,
                    );
                }
                DaemonUpdate::CommandFailed { operation, message } => {
                    let detail = sanitize_command_failure(&message);
                    toast_overlay_update
                        .add_toast(adw::Toast::new(&format!("{operation} failed. {detail}")));
                    if matches!(
                        operation.as_str(),
                        "Download model" | "Cancel model download"
                    ) {
                        model_library_update.request_statuses(&handle_update);
                    }
                }
            }
        }
    });

    let shell_extension_restart_banner_for_onboarding = shell_extension_restart_banner.clone();
    crate::shell_extension::onboard_once(move || {
        shell_extension_restart_banner_for_onboarding.set_revealed(true);
    });

    window
}

fn apply_last_error(message: &str, row: &adw::ActionRow, state: &Rc<RefCell<String>>) {
    *state.borrow_mut() = message.to_string();
    row.set_subtitle(message);
    row.set_visible(!message.trim().is_empty());
}

/// Strip D-Bus / implementation prefixes so toasts stay actionable for users.
fn sanitize_command_failure(message: &str) -> String {
    let trimmed = message
        .trim()
        .trim_start_matches("GDBus.Error:")
        .trim_start_matches("org.freedesktop.DBus.Error.Failed:")
        .trim_start_matches("org.freedesktop.DBus.Error.InvalidArgs:")
        .trim();
    let without_named_error = match trimmed.split_once(": ") {
        Some((prefix, rest)) if prefix.starts_with("org.") || prefix.starts_with("io.github.") => {
            rest.trim()
        }
        _ => trimmed,
    };
    if without_named_error.is_empty() {
        "Try again, or check General for details.".to_string()
    } else {
        without_named_error.to_string()
    }
}

fn last_error_toast() -> adw::Toast {
    let target = "general".to_variant();
    adw::Toast::builder()
        .title("Voxkey needs attention")
        .button_label("View details")
        .action_name("win.show-page")
        .action_target(&target)
        .priority(adw::ToastPriority::High)
        .build()
}

fn build_error_details_dialog(
    message: &str,
    handle: &DaemonHandle,
    toast_overlay: &adw::ToastOverlay,
    daemon_available: bool,
) -> adw::AlertDialog {
    let text_view = gtk4::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk4::WrapMode::WordChar)
        .left_margin(14)
        .right_margin(14)
        .top_margin(12)
        .bottom_margin(12)
        .build();
    text_view.buffer().set_text(message);

    let scrolled = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .min_content_height(100)
        .max_content_height(300)
        .propagate_natural_height(true)
        .child(&text_view)
        .build();
    scrolled.add_css_class("card");

    let dialog = adw::AlertDialog::builder()
        .heading("Voxkey needs attention")
        .extra_child(&scrolled)
        .build();
    dialog.add_response("close", "Close");
    dialog.add_response("copy", "Copy details");
    dialog.add_response("dismiss", "Dismiss error");
    dialog.set_response_enabled("dismiss", daemon_available);
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");

    let message = message.to_string();
    let toast_overlay_for_copy = toast_overlay.clone();
    dialog.connect_response(Some("copy"), move |_, _| {
        if let Some(display) = gtk4::gdk::Display::default() {
            display.clipboard().set_text(&message);
            toast_overlay_for_copy.add_toast(adw::Toast::new("Error details copied"));
        }
    });

    let handle = handle.clone();
    let toast_overlay = toast_overlay.clone();
    dialog.connect_response(Some("dismiss"), move |_, _| {
        let completion = handle.send(DaemonCommand::ClearLastError);
        toast_after_success(completion, &toast_overlay, "Error dismissed");
    });

    dialog
}

fn navigation_row(title: &str, icon_name: &str) -> gtk4::ListBoxRow {
    let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    content.set_margin_top(8);
    content.set_margin_bottom(8);
    content.set_margin_start(10);
    content.set_margin_end(10);
    let icon = gtk4::Image::from_icon_name(icon_name);
    icon.set_pixel_size(20);
    content.append(&icon);
    let label = gtk4::Label::builder()
        .label(title)
        .xalign(0.0)
        .hexpand(true)
        .build();
    content.append(&label);
    gtk4::ListBoxRow::builder()
        .child(&content)
        .selectable(true)
        .activatable(true)
        .build()
}

fn scroll_clamped(content: &impl IsA<gtk4::Widget>, maximum_size: i32) -> gtk4::Widget {
    let clamp = adw::Clamp::builder()
        .maximum_size(maximum_size)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(18)
        .margin_end(18)
        .build();
    clamp.set_child(Some(content));
    let scrolled = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build();
    scrolled.upcast()
}

fn save_window_state(window: &adw::ApplicationWindow, last_normal_size: &Cell<(i32, i32)>) {
    if !window.is_maximized() {
        let size = (window.width(), window.height());
        if size.0 >= 360 && size.1 >= 300 {
            last_normal_size.set(size);
        }
    }
    let (width, height) = last_normal_size.get();
    gui_settings::save_window_state(gui_settings::WindowState {
        width,
        height,
        maximized: window.is_maximized(),
    });
}

fn daemon_controls_are_editable(daemon_available: bool, daemon_state: &str) -> bool {
    daemon_available && daemon_state == "Idle"
}

fn apply_daemon_control_state(
    controls: &[gtk4::Widget],
    daemon_available: bool,
    daemon_state: &str,
) {
    let editable = daemon_controls_are_editable(daemon_available, daemon_state);
    for control in controls {
        control.set_sensitive(editable);
    }
}

fn settings_lock_message(daemon_state: &str) -> Option<&'static str> {
    match daemon_state {
        "Idle" => None,
        "Connecting" | "Recording" | "Streaming" => {
            Some("Finish or cancel the current dictation to change settings.")
        }
        "Transcribing" => {
            Some("Wait for transcription to finish, or cancel it, to change settings.")
        }
        "Injecting" => Some("Wait for Voxkey to finish typing before changing settings."),
        "RecoveringSession" => Some("Controls unlock after Voxkey restores desktop access."),
        _ => Some("Controls unlock when Voxkey is ready."),
    }
}

fn history_empty_action_label(daemon_available: bool, daemon_state: &str) -> &'static str {
    if !daemon_available {
        "Open General"
    } else if daemon_state == "Idle" {
        "View dictation shortcut"
    } else {
        "View dictation status"
    }
}

fn settings_lock_action_label(daemon_state: &str) -> Option<&'static str> {
    matches!(
        daemon_state,
        "Connecting" | "Recording" | "Streaming" | "Transcribing"
    )
    .then_some("Cancel dictation")
}

fn apply_connected_banner_state(
    banner: &adw::Banner,
    daemon_state: &str,
    cancel_available: &Cell<bool>,
    cancel_pending: &Cell<bool>,
) {
    cancel_pending.set(false);
    if let Some(message) = settings_lock_message(daemon_state) {
        banner.set_title(message);
        let action_label = settings_lock_action_label(daemon_state);
        cancel_available.set(action_label.is_some());
        banner.set_button_label(action_label);
        banner.set_revealed(true);
    } else {
        cancel_available.set(false);
        banner.set_button_label(None);
        banner.set_revealed(false);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct PermissionPagePresentation {
    title: &'static str,
    description: &'static str,
    status_subtitle: &'static str,
    icon: &'static str,
    action: PermissionPageAction,
    action_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionPageAction {
    None,
    Request,
    Reset,
}

fn permission_page_presentation(
    portal_connected: bool,
    daemon_available: bool,
    daemon_state: &str,
) -> PermissionPagePresentation {
    if portal_connected {
        PermissionPagePresentation {
            title: "Desktop access ready",
            description: "Voxkey can type transcriptions into other apps.",
            status_subtitle: "Ready to type transcriptions into other apps",
            icon: "object-select-symbolic",
            action: PermissionPageAction::Reset,
            action_enabled: daemon_available && daemon_state == "Idle",
        }
    } else if !daemon_available {
        PermissionPagePresentation {
            title: "Start Voxkey first",
            description: "Voxkey must be running before desktop access can be requested.",
            status_subtitle: "Available after Voxkey starts",
            icon: "system-run-symbolic",
            action: PermissionPageAction::None,
            action_enabled: false,
        }
    } else if daemon_state == "RecoveringSession" {
        PermissionPagePresentation {
            title: "Restoring desktop access",
            description: "Voxkey is reconnecting automatically. No action is needed.",
            status_subtitle: "Restoring desktop access automatically",
            icon: "view-refresh-symbolic",
            action: PermissionPageAction::None,
            action_enabled: false,
        }
    } else if daemon_state != "Idle" {
        PermissionPagePresentation {
            title: "Finish the current dictation first",
            description: "Desktop access can be requested when Voxkey is ready.",
            status_subtitle: "Desktop access can be requested when Voxkey is ready",
            icon: "media-playback-pause-symbolic",
            action: PermissionPageAction::Request,
            action_enabled: false,
        }
    } else {
        PermissionPagePresentation {
            title: "Allow Voxkey to type for you",
            description: "GNOME asks for permission before Voxkey can type a transcription into another app.",
            status_subtitle: "Permission needed before Voxkey can type for you",
            icon: "preferences-system-privacy-symbolic",
            action: PermissionPageAction::Request,
            action_enabled: true,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_portal_state(
    portal_connected: bool,
    daemon_available: bool,
    daemon_state: &str,
    portal_row: &adw::ActionRow,
    portal_details_icon: &gtk4::Image,
    permissions_nav: &gtk4::ListBoxRow,
    permission_status: &adw::StatusPage,
    retry_button: &gtk4::Button,
) {
    // Permissions remains a useful status destination after access succeeds.
    // Keeping both routes visible lets people verify or reset desktop access
    // without needing to know the Ctrl+6 shortcut.
    permissions_nav.set_visible(true);
    portal_row.set_activatable(true);
    portal_details_icon.set_visible(true);
    let presentation =
        permission_page_presentation(portal_connected, daemon_available, daemon_state);
    portal_row.set_subtitle(presentation.status_subtitle);
    retry_button.remove_css_class("suggested-action");
    retry_button.set_visible(presentation.action != PermissionPageAction::None);
    retry_button.set_sensitive(presentation.action_enabled);
    match presentation.action {
        PermissionPageAction::None => {}
        PermissionPageAction::Request => {
            retry_button.set_label("Request desktop access");
            retry_button.add_css_class("suggested-action");
        }
        PermissionPageAction::Reset => retry_button.set_label("Reset desktop access…"),
    }
    permission_status.set_title(presentation.title);
    permission_status.set_description(Some(presentation.description));
    permission_status.set_icon_name(Some(presentation.icon));
}

fn apply_audio_devices_to_widgets(
    devices_json: &str,
    selected_device: &str,
    group: &adw::PreferencesGroup,
    row: &adw::ComboRow,
    devices_state: &Rc<RefCell<Vec<String>>>,
    updating: &Rc<Cell<bool>>,
) {
    let Ok(devices) = serde_json::from_str::<Vec<String>>(devices_json) else {
        return;
    };
    let presentation = audio_device_presentation(&devices, selected_device);
    group.set_description(Some(&microphone_count_description(devices.len())));
    updating.set(true);
    *devices_state.borrow_mut() = presentation.values;
    let label_refs: Vec<&str> = presentation.labels.iter().map(String::as_str).collect();
    row.set_model(Some(&gtk4::StringList::new(&label_refs)));
    row.set_selected(presentation.selected);
    row.set_subtitle(presentation.subtitle);
    row.set_sensitive(presentation.selectable);
    updating.set(false);
}

fn add_spin_row_unit(row: &adw::SpinRow, unit: &str) {
    let label = gtk4::Label::new(Some(unit));
    label.add_css_class("dim-label");
    label.set_valign(gtk4::Align::Center);
    label.set_accessible_role(gtk4::AccessibleRole::Presentation);
    row.add_suffix(&label);
}

fn microphone_count_description(count: usize) -> String {
    match count {
        0 => "No microphones found · Connect one, then refresh".to_string(),
        1 => "1 microphone available".to_string(),
        count => format!("{count} microphones available"),
    }
}

fn microphone_refresh_failure_description() -> &'static str {
    "Could not refresh microphones · Check the service and try again"
}

fn microphone_refresh_failure_toast(retry_button: &gtk4::Button) -> adw::Toast {
    let toast = adw::Toast::builder()
        .title("Could not refresh microphones")
        .button_label("Try again")
        .priority(adw::ToastPriority::High)
        .build();
    let retry_button = retry_button.clone();
    toast.connect_button_clicked(move |_| retry_button.emit_clicked());
    toast
}

#[derive(Debug, PartialEq, Eq)]
struct AudioDevicePresentation {
    labels: Vec<String>,
    values: Vec<String>,
    selected: u32,
    subtitle: &'static str,
    selectable: bool,
}

fn recording_format_description(sample_rate: u32, channels: u16) -> String {
    let rate = if sample_rate >= 1_000 {
        let whole = sample_rate / 1_000;
        let remainder = sample_rate % 1_000;
        if remainder == 0 {
            format!("{whole} kHz")
        } else {
            let fraction = format!("{remainder:03}");
            format!("{whole}.{} kHz", fraction.trim_end_matches('0'))
        }
    } else {
        format!("{sample_rate} Hz")
    };
    let channels = match channels {
        1 => "Mono".to_string(),
        2 => "Stereo".to_string(),
        count => format!("{count} channels"),
    };
    format!("{rate} · {channels}")
}

fn audio_device_presentation(devices: &[String], selected_device: &str) -> AudioDevicePresentation {
    let available_index = devices.iter().position(|device| device == selected_device);
    let unavailable = !selected_device.is_empty() && available_index.is_none();

    let no_microphones = devices.is_empty() && selected_device.is_empty();
    let mut values = devices.to_vec();
    let mut labels = vec![if no_microphones {
        "No microphones found".to_string()
    } else {
        "System default".to_string()
    }];
    if unavailable {
        values.insert(0, selected_device.to_string());
        labels.push(format!("{selected_device} (unavailable)"));
    }
    labels.extend(devices.iter().cloned());

    let selected = if unavailable {
        1
    } else {
        available_index.map_or(0, |index| index as u32 + 1)
    };
    let subtitle = if unavailable {
        "Choose an available microphone or use the system default"
    } else if no_microphones {
        "Connect a microphone, then use Refresh"
    } else if selected == 0 {
        "Follow the current system default"
    } else {
        "Use this microphone for every dictation"
    };

    AudioDevicePresentation {
        labels,
        values,
        selected,
        subtitle,
        selectable: !no_microphones,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelStatusAction {
    None,
    Download,
    CancelDownload,
    OpenFolder,
}

impl ModelStatusAction {
    fn label(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Download => Some("Download"),
            Self::CancelDownload => Some("Cancel download"),
            Self::OpenFolder => Some("Open model folder"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ModelStatusPresentation {
    subtitle: String,
    show_download_progress: bool,
    action: ModelStatusAction,
    show_delete: bool,
}

fn model_status_presentation(status: &str, model_name: &str) -> ModelStatusPresentation {
    match status {
        "available" => ModelStatusPresentation {
            subtitle: "Available on this computer".to_string(),
            show_download_progress: false,
            action: ModelStatusAction::None,
            show_delete: true,
        },
        "downloading" => ModelStatusPresentation {
            subtitle: "Downloading…".to_string(),
            show_download_progress: true,
            action: ModelStatusAction::CancelDownload,
            show_delete: false,
        },
        "cancelling" => ModelStatusPresentation {
            subtitle: "Cancelling download…".to_string(),
            show_download_progress: false,
            action: ModelStatusAction::None,
            show_delete: false,
        },
        "deleting" => ModelStatusPresentation {
            subtitle: "Deleting model…".to_string(),
            show_download_progress: false,
            action: ModelStatusAction::None,
            show_delete: false,
        },
        "checking" => ModelStatusPresentation {
            subtitle: "Checking model…".to_string(),
            show_download_progress: false,
            action: ModelStatusAction::None,
            show_delete: false,
        },
        _ => ModelStatusPresentation {
            subtitle: parakeet_model_download_description(model_name)
                .unwrap_or_else(|| "Custom model files not found".to_string()),
            show_download_progress: false,
            action: if parakeet_model_can_download(model_name) {
                ModelStatusAction::Download
            } else {
                ModelStatusAction::OpenFolder
            },
            show_delete: false,
        },
    }
}

fn local_parakeet_model_name(config: &voxkey_ipc::TranscriberConfig) -> Option<&str> {
    (config.provider == voxkey_ipc::TranscriberProvider::Parakeet
        && config.parakeet.backend == voxkey_ipc::ParakeetBackend::Local)
        .then_some(config.parakeet.model.as_str())
}

fn parakeet_model_can_download(model_name: &str) -> bool {
    voxkey_ipc::model_library::local_model(model_name).is_some()
}

fn parakeet_model_download_description(model_name: &str) -> Option<String> {
    voxkey_ipc::model_library::local_model(model_name)
        .map(|model| format!("Not downloaded · {} download", model.download_size()))
}

fn parakeet_model_action_label(model_name: &str) -> &'static str {
    if parakeet_model_can_download(model_name) {
        ModelStatusAction::Download.label().unwrap()
    } else {
        ModelStatusAction::OpenFolder.label().unwrap()
    }
}

fn parakeet_model_folder_icon_visible(expert_mode: bool, action: ModelStatusAction) -> bool {
    expert_mode && action != ModelStatusAction::OpenFolder
}

fn parakeet_model_display_name(model_name: &str) -> &str {
    voxkey_ipc::model_library::local_model(model_name)
        .map(|model| model.name)
        .unwrap_or(model_name)
}

fn parakeet_model_status_title(model_name: &str) -> String {
    match voxkey_ipc::model_library::local_model(model_name) {
        Some(model) => format!("{} model", model.name),
        None if model_name.trim().is_empty() => "Custom open model".to_string(),
        None => format!("Custom model: {model_name}"),
    }
}

fn model_download_fraction(percent: u8) -> f64 {
    f64::from(percent.min(100)) / 100.0
}

fn update_model_download_progress(progress: &gtk4::ProgressBar, percent: u8) {
    let percent = percent.min(100);
    progress.set_visible(true);
    progress.set_fraction(model_download_fraction(percent));
    progress.set_tooltip_text(Some(&format!("Model download: {percent}%")));
}

#[allow(clippy::too_many_arguments)]
fn apply_model_status(
    status: &str,
    model_name: &str,
    row: &adw::ActionRow,
    download_progress: &gtk4::ProgressBar,
    download_button: &gtk4::Button,
    delete_button: &gtk4::Button,
    open_folder_button: &gtk4::Button,
    expert_mode: bool,
) {
    let presentation = model_status_presentation(status, model_name);
    row.set_title(&parakeet_model_status_title(model_name));
    row.set_subtitle(&presentation.subtitle);
    if presentation.show_download_progress && !download_progress.is_visible() {
        download_progress.set_fraction(0.0);
        download_progress.set_tooltip_text(Some("Model download starting"));
    }
    download_progress.set_visible(presentation.show_download_progress);
    download_button.remove_css_class("suggested-action");
    download_button.remove_css_class("destructive-action");
    if let Some(label) = presentation.action.label() {
        download_button.set_label(label);
        download_button.set_visible(true);
        if matches!(
            presentation.action,
            ModelStatusAction::Download | ModelStatusAction::OpenFolder
        ) {
            download_button.add_css_class("suggested-action");
        }
    } else {
        download_button.set_visible(false);
    }
    delete_button.set_visible(presentation.show_delete);
    open_folder_button.set_visible(parakeet_model_folder_icon_visible(
        expert_mode,
        presentation.action,
    ));
}

#[derive(Debug, PartialEq, Eq)]
struct DictationStatusPresentation {
    title: &'static str,
    subtitle: &'static str,
    icon: &'static str,
    style: &'static str,
    busy: bool,
}

fn dictation_status_presentation(
    state: &str,
    has_unresolved_error: bool,
) -> DictationStatusPresentation {
    if state == "Idle" && has_unresolved_error {
        return DictationStatusPresentation {
            title: "Ready to try again",
            subtitle: "Review the issue below before your next dictation",
            icon: "view-refresh-symbolic",
            style: "accent",
            busy: false,
        };
    }

    match state {
        "StartingService" => DictationStatusPresentation {
            title: "Starting Voxkey…",
            subtitle: "Getting ready to dictate",
            icon: "system-run-symbolic",
            style: "accent",
            busy: true,
        },
        "Idle" => DictationStatusPresentation {
            title: "Ready to dictate",
            subtitle: "Press your shortcut whenever you want to speak",
            icon: "object-select-symbolic",
            style: "success",
            busy: false,
        },
        "Connecting" => DictationStatusPresentation {
            title: "Starting live transcription…",
            subtitle: "Connecting to the transcription server",
            icon: "network-transmit-receive-symbolic",
            style: "accent",
            busy: true,
        },
        "Recording" => DictationStatusPresentation {
            title: "Listening…",
            subtitle: "Press your shortcut again when you finish",
            icon: "audio-input-microphone-symbolic",
            style: "accent",
            busy: true,
        },
        "Streaming" => DictationStatusPresentation {
            title: "Listening and transcribing…",
            subtitle: "Live text is being prepared while you speak",
            icon: "audio-input-microphone-symbolic",
            style: "accent",
            busy: true,
        },
        "Transcribing" => DictationStatusPresentation {
            title: "Transcribing…",
            subtitle: "Turning your recording into text",
            icon: "document-edit-symbolic",
            style: "accent",
            busy: true,
        },
        "Injecting" => DictationStatusPresentation {
            title: "Typing your words…",
            subtitle: "Sending the transcription to the active app",
            icon: "input-keyboard-symbolic",
            style: "accent",
            busy: true,
        },
        "RecoveringSession" => DictationStatusPresentation {
            title: "Restoring desktop access…",
            subtitle: "Voxkey is reconnecting before the next dictation",
            icon: "dialog-warning-symbolic",
            style: "warning",
            busy: true,
        },
        "Unavailable" => DictationStatusPresentation {
            title: "Voxkey is unavailable",
            subtitle: "Start Voxkey from the message above",
            icon: "dialog-warning-symbolic",
            style: "warning",
            busy: false,
        },
        _ => DictationStatusPresentation {
            title: "Checking dictation…",
            subtitle: "Waiting for Voxkey",
            icon: "media-record-symbolic",
            style: "dim-label",
            busy: true,
        },
    }
}

fn update_state_row(
    row: &adw::ActionRow,
    icon: &gtk4::Image,
    spinner: &gtk4::Spinner,
    state: &str,
    has_unresolved_error: bool,
) {
    let presentation = dictation_status_presentation(state, has_unresolved_error);
    row.set_title(presentation.title);
    row.set_subtitle(presentation.subtitle);
    icon.set_icon_name(Some(presentation.icon));

    for class in &["accent", "success", "warning", "error", "dim-label"] {
        icon.remove_css_class(class);
    }
    icon.add_css_class(presentation.style);

    spinner.set_visible(presentation.busy);
    if presentation.busy {
        spinner.start();
    } else {
        spinner.stop();
    }
}

fn shortcut_subtitle(description: &str, daemon_available: bool) -> String {
    let description = description.trim();
    if !description.is_empty() {
        readable_shortcut(description)
    } else if daemon_available {
        "Choose the shortcut you want to use for dictation".to_string()
    } else {
        "Available after Voxkey starts".to_string()
    }
}

fn readable_shortcut(description: &str) -> String {
    let (prefix, accelerator) = description
        .strip_prefix("Press ")
        .map_or(("", description), |accelerator| ("Press ", accelerator));
    let mut remainder = accelerator;
    let mut parts = Vec::new();

    while let Some(after_open) = remainder.strip_prefix('<') {
        let Some(end) = after_open.find('>') else {
            return description.to_string();
        };
        parts.push(readable_shortcut_part(&after_open[..end]));
        remainder = &after_open[end + 1..];
    }

    if parts.is_empty() || remainder.is_empty() {
        return description.to_string();
    }

    parts.push(readable_shortcut_key(remainder));
    format!("{prefix}{}", parts.join(" + "))
}

fn readable_shortcut_part(part: &str) -> String {
    match part.to_ascii_lowercase().as_str() {
        "control" | "primary" | "ctrl" => "Ctrl".to_string(),
        "alt" | "mod1" => "Alt".to_string(),
        "shift" => "Shift".to_string(),
        "super" | "mod4" => "Super".to_string(),
        "meta" => "Meta".to_string(),
        "hyper" => "Hyper".to_string(),
        _ => part.to_string(),
    }
}

fn readable_shortcut_key(key: &str) -> String {
    match key.to_ascii_lowercase().as_str() {
        "space" => "Space".to_string(),
        "return" => "Enter".to_string(),
        "escape" => "Esc".to_string(),
        _ if key.len() == 1 => key.to_ascii_uppercase(),
        _ => key.to_string(),
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

    // GTK's runtime keysym-name table can lag behind the key constants in its
    // headers. Keep the newer voice/recording keys capturable by using their
    // shortcuts-spec identifiers when `gdk_keyval_name()` cannot name them.
    let key_name = match key {
        gdk::Key::Assistant => "XF86Assistant".into(),
        gdk::Key::Dictate => "XF86Dictate".into(),
        gdk::Key::MacroRecordStart => "XF86MacroRecordStart".into(),
        gdk::Key::MacroRecordStop => "XF86MacroRecordStop".into(),
        gdk::Key::PauseRecord => "XF86PauseRecord".into(),
        gdk::Key::StopRecord => "XF86StopRecord".into(),
        gdk::Key::VoiceCommand => "XF86VoiceCommand".into(),
        gdk::Key::Voicemail => "XF86Voicemail".into(),
        _ => key.name()?,
    };

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
    if modifiers.contains(gdk::ModifierType::META_MASK) {
        parts.push_str("<Meta>");
    }
    if modifiers.contains(gdk::ModifierType::HYPER_MASK) {
        parts.push_str("<Hyper>");
    }
    parts.push_str(&key_name);

    Some(parts)
}

fn should_cancel_shortcut_capture(key: gdk::Key, modifiers: gdk::ModifierType) -> bool {
    let accelerator_modifiers = gdk::ModifierType::CONTROL_MASK
        | gdk::ModifierType::ALT_MASK
        | gdk::ModifierType::SHIFT_MASK
        | gdk::ModifierType::SUPER_MASK
        | gdk::ModifierType::META_MASK
        | gdk::ModifierType::HYPER_MASK;
    key == gdk::Key::Escape && !modifiers.intersects(accelerator_modifiers)
}

fn shortcut_validation_description(error: &str) -> String {
    let error = error.trim();
    let separator = if matches!(error.chars().last(), Some('.' | '!' | '?')) {
        " "
    } else {
        ". "
    };
    format!("{error}{separator}Press another shortcut, or press Escape to cancel.")
}

fn shortcut_save_failure_description(error: &str) -> String {
    let message = error
        .split_once("Desktop rejected shortcut:")
        .map(|(_, reason)| reason.trim())
        .filter(|reason| !reason.is_empty())
        .unwrap_or("Voxkey could not save that shortcut.");
    shortcut_validation_description(message)
}

/// Wire the shortcut row to open a key capture dialog on click.
fn wire_shortcut_capture(
    shortcut_row: &adw::ActionRow,
    handle: &DaemonHandle,
    toast_overlay: &adw::ToastOverlay,
    parent_window: &adw::ApplicationWindow,
) {
    let handle = handle.clone();
    let toast_overlay = toast_overlay.clone();
    let parent_window = parent_window.clone();

    shortcut_row.connect_activated(move |_| {
        show_shortcut_capture_dialog(&parent_window, &handle, &toast_overlay);
    });
}

fn show_shortcut_capture_dialog(
    parent: &adw::ApplicationWindow,
    handle: &DaemonHandle,
    toast_overlay: &adw::ToastOverlay,
) {
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(SHORTCUT_DIALOG_DEFAULT_WIDTH)
        .default_height(SHORTCUT_DIALOG_DEFAULT_HEIGHT)
        .title("Set shortcut")
        .build();

    let dialog_content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&adw::HeaderBar::new());

    let use_default_button = gtk4::Button::with_label("Use default shortcut");
    use_default_button.set_halign(gtk4::Align::Center);
    let status_page = adw::StatusPage::builder()
        .icon_name("preferences-desktop-keyboard-shortcuts-symbolic")
        .title("Press a shortcut")
        .description(
            "Press a function or media key, or hold a modifier and press another key. Escape to cancel.",
        )
        .child(&use_default_button)
        .build();

    toolbar_view.set_content(Some(&status_page));
    dialog_content.append(&toolbar_view);
    dialog.set_content(Some(&dialog_content));

    let key_controller = gtk4::EventControllerKey::new();

    let dialog_ref = dialog.clone();
    let handle = handle.clone();
    let toast_overlay = toast_overlay.clone();
    let status_page_for_capture = status_page.clone();
    let saving = Rc::new(Cell::new(false));

    {
        let dialog = dialog.clone();
        let handle = handle.clone();
        let toast_overlay = toast_overlay.clone();
        let status_page = status_page.clone();
        let saving = saving.clone();
        use_default_button.connect_clicked(move |_| {
            if saving.replace(true) {
                return;
            }
            status_page.set_icon_name(Some("document-save-symbolic"));
            status_page.set_title("Restoring default shortcut…");
            status_page.set_description(Some("Checking that Alt + Super + D is available"));
            let completion = handle.send(DaemonCommand::SetShortcut(
                voxkey_ipc::DEFAULT_SHORTCUT_TRIGGER.to_string(),
            ));
            let dialog = dialog.clone();
            let status_page = status_page.clone();
            let toast_overlay = toast_overlay.clone();
            let saving = saving.clone();
            glib::spawn_future_local(async move {
                match completion.wait().await {
                    Ok(()) => {
                        toast_overlay.add_toast(adw::Toast::new("Default shortcut restored"));
                        dialog.close();
                    }
                    Err(error) => {
                        saving.set(false);
                        status_page.set_icon_name(Some("dialog-warning-symbolic"));
                        status_page.set_title("Try another shortcut");
                        status_page
                            .set_description(Some(&shortcut_save_failure_description(&error)));
                    }
                }
            });
        });
    }

    key_controller.connect_key_pressed(move |_, key, _, modifiers| {
        if saving.get() {
            return glib::Propagation::Stop;
        }

        // Escape cancels
        if should_cancel_shortcut_capture(key, modifiers) {
            dialog_ref.close();
            return glib::Propagation::Stop;
        }

        if let Some(trigger) = key_to_trigger(key, modifiers) {
            if let Err(error) = voxkey_ipc::validate_shortcut_trigger(&trigger) {
                status_page_for_capture.set_icon_name(Some("dialog-warning-symbolic"));
                status_page_for_capture.set_title("Try another shortcut");
                status_page_for_capture
                    .set_description(Some(&shortcut_validation_description(&error)));
                return glib::Propagation::Stop;
            }
            saving.set(true);
            status_page_for_capture.set_icon_name(Some("document-save-symbolic"));
            status_page_for_capture.set_title("Saving shortcut…");
            status_page_for_capture
                .set_description(Some("Checking that this shortcut is available"));
            let completion = handle.send(DaemonCommand::SetShortcut(trigger));
            let dialog = dialog_ref.clone();
            let status_page = status_page_for_capture.clone();
            let toast_overlay = toast_overlay.clone();
            let saving = saving.clone();
            glib::spawn_future_local(async move {
                match completion.wait().await {
                    Ok(()) => {
                        toast_overlay.add_toast(adw::Toast::new("Shortcut updated"));
                        dialog.close();
                    }
                    Err(error) => {
                        saving.set(false);
                        status_page.set_icon_name(Some("dialog-warning-symbolic"));
                        status_page.set_title("Try another shortcut");
                        status_page
                            .set_description(Some(&shortcut_save_failure_description(&error)));
                    }
                }
            });
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
    let display = if value.is_empty() {
        default_text
    } else {
        value
    };
    set_entry_text_without_apply(row, display);
    if let Some(delegate) = row.delegate() {
        delegate.set_opacity(if is_default { 0.55 } else { 1.0 });
    }
}

fn displayed_entry_value(value: &str, default_text: &str) -> String {
    if value.is_empty() {
        default_text.to_string()
    } else {
        value.to_string()
    }
}

fn should_replace_entry(preserve_dirty: bool, displayed: &str, previous: &str) -> bool {
    !preserve_dirty || displayed == previous
}

fn set_entry_text_if_clean(
    row: &adw::EntryRow,
    previous: &str,
    current: &str,
    preserve_dirty: bool,
) {
    if should_replace_entry(preserve_dirty, &row.text(), previous) {
        set_entry_text_without_apply(row, current);
    }
}

fn set_entry_with_default_if_clean(
    row: &adw::EntryRow,
    previous: &str,
    current: &str,
    default_text: &str,
    preserve_dirty: bool,
) {
    let previous = displayed_entry_value(previous, default_text);
    if should_replace_entry(preserve_dirty, &row.text(), &previous) {
        set_entry_with_default(row, current, default_text);
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

/// Re-establish the saved baseline after EntryRow hid its apply button for an
/// invalid submission, then restore the rejected text as a pending edit.
fn keep_entry_edit_pending(row: &adw::EntryRow, saved: &str, entered: &str) {
    row.set_show_apply_button(false);
    row.set_text(saved);
    row.set_show_apply_button(true);
    row.set_text(entered);
}

/// Reflect whether a key is stored for the row's provider. The entry row stays
/// empty either way; the status row's subtitle and the Remove button carry the
/// state.
fn update_api_key_row_state(
    status_row: &adw::ActionRow,
    entry_row: &adw::PasswordEntryRow,
    remove_button: &gtk4::Button,
    present: Option<bool>,
) {
    entry_row.set_title(api_key_entry_title(present));
    entry_row.set_sensitive(true);
    entry_row.set_show_apply_button(false);
    entry_row.set_text("");
    entry_row.set_show_apply_button(true);
    status_row.set_subtitle(match present {
        Some(true) => "Key stored. Enter a new key below to replace it.",
        Some(false) => "No key stored. Enter a key below to use cloud transcription.",
        None => "Checking for a stored key…",
    });
    remove_button.set_visible(present == Some(true));
}

fn show_api_key_operation(
    status_row: &adw::ActionRow,
    entry_row: &adw::PasswordEntryRow,
    remove_button: &gtk4::Button,
    message: &str,
) {
    status_row.set_subtitle(message);
    entry_row.set_sensitive(false);
    remove_button.set_visible(false);
}

fn keep_api_key_edit_pending(entry_row: &adw::PasswordEntryRow, entered: &str) {
    entry_row.set_show_apply_button(false);
    entry_row.set_text("");
    entry_row.set_show_apply_button(true);
    entry_row.set_text(entered);
}

fn api_key_operation_is_current(
    service: &str,
    config: &voxkey_ipc::TranscriberConfig,
    operation_id: u64,
    current_request_id: u64,
) -> bool {
    operation_id == current_request_id && transcriber_api_service(config) == Some(service)
}

fn format_whisper_args(args: &[String]) -> String {
    args.iter()
        .map(|argument| {
            if !argument.is_empty()
                && argument.chars().all(|character| {
                    !character.is_whitespace() && !matches!(character, '\'' | '"' | '\\')
                })
            {
                argument.clone()
            } else {
                format!(
                    "\"{}\"",
                    argument.replace('\\', "\\\\").replace('"', "\\\"")
                )
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_whisper_args(text: &str) -> Result<Vec<String>, String> {
    #[derive(Clone, Copy)]
    enum Quote {
        Single,
        Double,
    }

    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;

    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if escaped {
            current.push(character);
            escaped = false;
            started = true;
            continue;
        }

        match quote {
            Some(Quote::Single) => {
                if character == '\'' {
                    quote = None;
                } else {
                    current.push(character);
                }
            }
            Some(Quote::Double) => match character {
                '"' => quote = None,
                '\\' if matches!(characters.peek().copied(), Some('"' | '\\')) => escaped = true,
                '\\' => current.push(character),
                _ => current.push(character),
            },
            None => match character {
                '\'' => {
                    quote = Some(Quote::Single);
                    started = true;
                }
                '"' => {
                    quote = Some(Quote::Double);
                    started = true;
                }
                '\\' => {
                    escaped = true;
                    started = true;
                }
                character if character.is_whitespace() => {
                    if started {
                        arguments.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                _ => {
                    current.push(character);
                    started = true;
                }
            },
        }
    }

    if escaped {
        return Err("trailing escape".to_string());
    }
    if quote.is_some() {
        return Err("unterminated quote".to_string());
    }
    if started {
        arguments.push(current);
    }
    Ok(arguments)
}

fn parse_initial_config_sections(
    transcriber_json: &str,
    injection_json: &str,
) -> (
    Option<voxkey_ipc::TranscriberConfig>,
    Option<voxkey_ipc::InjectionConfig>,
) {
    let transcriber = serde_json::from_str::<voxkey_ipc::TranscriberConfig>(transcriber_json).ok();
    let injection = serde_json::from_str::<voxkey_ipc::InjectionConfig>(injection_json).ok();
    (transcriber, injection)
}

#[derive(Debug, PartialEq, Eq)]
struct TranscriberVisibility {
    whisper_command: bool,
    whisper_model: bool,
    whisper_arguments: bool,
    api_key: bool,
    model_name: bool,
    batch_endpoint: bool,
    realtime_endpoint: bool,
    parakeet_backend: bool,
    parakeet_endpoint: bool,
    execution_provider: bool,
    model_status: bool,
}

fn whisper_model_path(args: &[String]) -> Option<&str> {
    args.iter().enumerate().find_map(|(index, argument)| {
        if matches!(argument.as_str(), "-m" | "--model") {
            args.get(index + 1).map(String::as_str)
        } else {
            argument
                .strip_prefix("--model=")
                .or_else(|| argument.strip_prefix("-m="))
        }
    })
}

fn whisper_args_with_model(args: &[String], model_path: &str) -> Vec<String> {
    let mut updated = args.to_vec();
    for index in 0..updated.len() {
        if matches!(updated[index].as_str(), "-m" | "--model") {
            if let Some(value) = updated.get_mut(index + 1) {
                *value = model_path.to_string();
            } else {
                updated.push(model_path.to_string());
            }
            return updated;
        }
        if updated[index].starts_with("--model=") {
            updated[index] = format!("--model={model_path}");
            return updated;
        }
        if updated[index].starts_with("-m=") {
            updated[index] = format!("-m={model_path}");
            return updated;
        }
    }
    updated.splice(0..0, ["--model".to_string(), model_path.to_string()]);
    updated
}

fn whisper_has_advanced_arguments(args: &[String]) -> bool {
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        if matches!(argument.as_str(), "-m" | "--model") {
            index += 2;
        } else if argument.starts_with("--model=")
            || argument.starts_with("-m=")
            || argument == "{audio_file}"
        {
            index += 1;
        } else {
            return true;
        }
    }
    false
}

fn is_standard_whisper_command(command: &str) -> bool {
    Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "whisper-cpp" | "whisper-cli"))
}

fn update_whisper_model_row(row: &adw::ActionRow, config: &voxkey_ipc::TranscriberConfig) {
    row.set_use_markup(true);
    row.set_subtitle(&whisper_model_subtitle_markup(config));
}

fn whisper_model_subtitle(config: &voxkey_ipc::TranscriberConfig) -> String {
    match whisper_model_path(&config.whisper_cpp.args).filter(|path| !path.trim().is_empty()) {
        Some(path) if Path::new(path).is_absolute() && !Path::new(path).is_file() => {
            format!("Model file not found: {path}")
        }
        Some(path) => path.to_string(),
        None if is_standard_whisper_command(&config.whisper_cpp.command) => {
            "Choose a model file to use Whisper".to_string()
        }
        None => "No model file chosen; your Whisper program may supply one".to_string(),
    }
}

fn whisper_model_subtitle_markup(config: &voxkey_ipc::TranscriberConfig) -> String {
    let Some(path) =
        whisper_model_path(&config.whisper_cpp.args).filter(|path| !path.trim().is_empty())
    else {
        return glib::markup_escape_text(&whisper_model_subtitle(config)).to_string();
    };
    let missing = Path::new(path).is_absolute() && !Path::new(path).is_file();
    let path = glib::markup_escape_text(path);
    let value = format!("<span font_family=\"monospace\">{path}</span>");
    if missing {
        format!("Model file not found: {value}")
    } else {
        value
    }
}

fn transcriber_visibility(
    config: &voxkey_ipc::TranscriberConfig,
    expert_mode: bool,
) -> TranscriberVisibility {
    let is_whisper = config.provider == voxkey_ipc::TranscriberProvider::WhisperCpp;
    let is_parakeet = config.provider == voxkey_ipc::TranscriberProvider::Parakeet;
    let is_mistral_batch = config.provider == voxkey_ipc::TranscriberProvider::Mistral;
    let is_mistral_realtime = config.provider == voxkey_ipc::TranscriberProvider::MistralRealtime;
    let is_mistral_api = is_mistral_batch || is_mistral_realtime;
    let is_parakeet_http =
        is_parakeet && config.parakeet.backend == voxkey_ipc::ParakeetBackend::Http;
    let is_parakeet_local = is_parakeet && !is_parakeet_http;
    let custom_mistral_model = if is_mistral_realtime {
        config.mistral_realtime.model != voxkey_ipc::MistralRealtimeConfig::DEFAULT_MODEL
    } else {
        config.mistral.model != voxkey_ipc::MistralConfig::DEFAULT_MODEL
    };

    TranscriberVisibility {
        whisper_command: is_whisper,
        whisper_model: is_whisper,
        whisper_arguments: is_whisper
            && (expert_mode || whisper_has_advanced_arguments(&config.whisper_cpp.args)),
        api_key: is_mistral_api || is_parakeet_http,
        model_name: is_mistral_api && (expert_mode || custom_mistral_model),
        batch_endpoint: is_mistral_batch && (expert_mode || !config.mistral.endpoint.is_empty()),
        realtime_endpoint: is_mistral_realtime
            && (expert_mode || !config.mistral_realtime.endpoint.is_empty()),
        parakeet_backend: is_parakeet && (expert_mode || is_parakeet_http),
        parakeet_endpoint: is_parakeet_http,
        execution_provider: is_parakeet_local
            && (expert_mode
                || config.parakeet.execution_provider != voxkey_ipc::ExecutionProviderChoice::Auto),
        model_status: is_parakeet_local,
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_transcriber_visibility(
    config: &voxkey_ipc::TranscriberConfig,
    expert_mode: bool,
    command_row: &adw::EntryRow,
    whisper_model_row: &adw::ActionRow,
    args_row: &adw::EntryRow,
    api_key_status_row: &adw::ActionRow,
    api_key_row: &adw::PasswordEntryRow,
    model_row: &adw::EntryRow,
    batch_endpoint: &EndpointEditor,
    realtime_endpoint: &EndpointEditor,
    parakeet_backend_row: &adw::ComboRow,
    parakeet_endpoint: &EndpointEditor,
    execution_provider_row: &adw::ComboRow,
    model_status_row: &adw::ActionRow,
) {
    let visibility = transcriber_visibility(config, expert_mode);
    command_row.set_visible(visibility.whisper_command);
    whisper_model_row.set_visible(visibility.whisper_model);
    args_row.set_visible(visibility.whisper_arguments);
    api_key_status_row.set_visible(visibility.api_key);
    api_key_row.set_visible(visibility.api_key);
    model_row.set_visible(visibility.model_name);
    batch_endpoint.set_visible(visibility.batch_endpoint);
    realtime_endpoint.set_visible(visibility.realtime_endpoint);
    parakeet_backend_row.set_visible(visibility.parakeet_backend);
    parakeet_endpoint.set_visible(visibility.parakeet_endpoint);
    execution_provider_row.set_visible(visibility.execution_provider);
    model_status_row.set_visible(visibility.model_status);
}

/// Parse transcriber config JSON and update all transcriber widgets.
// GTK signal wiring naturally passes the related widget set together. Keeping
// the concrete widget types visible here makes these updates straightforward
// to audit and avoids a stateful abstraction around UI objects.
#[allow(clippy::too_many_arguments)]
fn apply_transcriber_config_to_widgets(
    config_json: &str,
    provider_row: &adw::ComboRow,
    provider_model: &gtk4::StringList,
    command_row: &adw::EntryRow,
    choose_command_button: &gtk4::Button,
    whisper_model_row: &adw::ActionRow,
    args_row: &adw::EntryRow,
    api_key_status_row: &adw::ActionRow,
    api_key_row: &adw::PasswordEntryRow,
    api_key_remove_button: &gtk4::Button,
    model_row: &adw::EntryRow,
    batch_endpoint: &EndpointEditor,
    realtime_endpoint: &EndpointEditor,
    parakeet_backend_row: &adw::ComboRow,
    parakeet_endpoint: &EndpointEditor,
    execution_provider_row: &adw::ComboRow,
    model_status_row: &adw::ActionRow,
    download_button: &gtk4::Button,
    state: &Rc<RefCell<voxkey_ipc::TranscriberConfig>>,
    updating_widgets: &Rc<Cell<bool>>,
    api_key_stored: &Rc<Cell<Option<bool>>>,
    expert_mode: &Rc<Cell<bool>>,
    preserve_dirty: bool,
) {
    let Ok(tc) = serde_json::from_str::<voxkey_ipc::TranscriberConfig>(config_json) else {
        return;
    };

    let previous = state.borrow().clone();
    let provider_changed = previous.provider != tc.provider;

    // Suppress notify handlers from sending config back to daemon while we update widgets.
    updating_widgets.set(true);

    // Update state BEFORE touching widgets. provider_row.set_selected() fires
    // connect_selected_notify which reads from state — it must see current values.
    *state.borrow_mut() = tc.clone();

    let choice = transcriber_choice_presentation(&tc);
    sync_custom_parakeet_choice(provider_model, choice.show_custom_parakeet);
    provider_row.set_selected(choice.selected);
    provider_row.set_subtitle(transcriber_location_subtitle(&tc));
    download_button.set_label(parakeet_model_action_label(&tc.parakeet.model));

    // Set entry text and reset the "applied text" baseline so the apply button
    // stays hidden. Toggling show_apply_button off→on after set_text() snapshots
    // the current text as the new baseline in libadwaita.
    set_entry_text_if_clean(
        command_row,
        &previous.whisper_cpp.command,
        &tc.whisper_cpp.command,
        preserve_dirty,
    );
    update_whisper_command_action(choose_command_button, &command_row.text());
    set_entry_text_if_clean(
        args_row,
        &format_whisper_args(&previous.whisper_cpp.args),
        &format_whisper_args(&tc.whisper_cpp.args),
        preserve_dirty,
    );
    update_whisper_model_row(whisper_model_row, &tc);
    set_entry_with_default_if_clean(
        &batch_endpoint.entry,
        &previous.mistral.endpoint,
        &tc.mistral.endpoint,
        voxkey_ipc::MistralConfig::DEFAULT_ENDPOINT,
        preserve_dirty,
    );
    set_entry_with_default_if_clean(
        &realtime_endpoint.entry,
        &previous.mistral_realtime.endpoint,
        &tc.mistral_realtime.endpoint,
        voxkey_ipc::MistralRealtimeConfig::DEFAULT_ENDPOINT,
        preserve_dirty,
    );
    set_entry_text_if_clean(
        &parakeet_endpoint.entry,
        &previous.parakeet.endpoint,
        &tc.parakeet.endpoint,
        preserve_dirty,
    );
    parakeet_endpoint.sync_insecure_http_permission(
        tc.parakeet.allow_insecure_http,
        preserve_dirty && !provider_changed,
    );
    if !preserve_dirty || provider_changed {
        batch_endpoint.show_saved();
        realtime_endpoint.show_saved();
        parakeet_endpoint.show_saved();
    }
    parakeet_backend_row.set_selected(match tc.parakeet.backend {
        voxkey_ipc::ParakeetBackend::Local => 0,
        voxkey_ipc::ParakeetBackend::Http => 1,
    });

    // Show only controls belonging to the selected model and backend.
    let is_whisper = tc.provider == voxkey_ipc::TranscriberProvider::WhisperCpp;
    let is_parakeet = tc.provider == voxkey_ipc::TranscriberProvider::Parakeet;
    let is_mistral_api = !is_whisper && !is_parakeet;
    let uses_api_key =
        is_mistral_api || (is_parakeet && tc.parakeet.backend == voxkey_ipc::ParakeetBackend::Http);
    let is_parakeet_local =
        is_parakeet && tc.parakeet.backend == voxkey_ipc::ParakeetBackend::Local;

    if provider_changed {
        api_key_stored.set(None);
    }

    if uses_api_key && (!preserve_dirty || provider_changed) {
        // The daemon publishes config with the key redacted, so the row is
        // only cleared when it is not holding a pending replacement edit.
        update_api_key_row_state(
            api_key_status_row,
            api_key_row,
            api_key_remove_button,
            api_key_stored.get(),
        );
    }

    if is_mistral_api {
        let (active_model, default_model) = match tc.provider {
            voxkey_ipc::TranscriberProvider::MistralRealtime => (
                &tc.mistral_realtime.model,
                voxkey_ipc::MistralRealtimeConfig::DEFAULT_MODEL,
            ),
            _ => (&tc.mistral.model, voxkey_ipc::MistralConfig::DEFAULT_MODEL),
        };
        let previous_model = match previous.provider {
            voxkey_ipc::TranscriberProvider::MistralRealtime => &previous.mistral_realtime.model,
            _ => &previous.mistral.model,
        };
        set_entry_with_default_if_clean(
            model_row,
            previous_model,
            active_model,
            default_model,
            preserve_dirty && !provider_changed,
        );
    }

    if is_parakeet_local {
        let ep_idx = match tc.parakeet.execution_provider {
            voxkey_ipc::ExecutionProviderChoice::Auto => 0u32,
            voxkey_ipc::ExecutionProviderChoice::Cpu => 1,
            voxkey_ipc::ExecutionProviderChoice::Cuda => 2,
        };
        execution_provider_row.set_selected(ep_idx);
        execution_provider_row.set_subtitle(execution_provider_subtitle(ep_idx));
    }

    apply_transcriber_visibility(
        &tc,
        expert_mode.get(),
        command_row,
        whisper_model_row,
        args_row,
        api_key_status_row,
        api_key_row,
        model_row,
        batch_endpoint,
        realtime_endpoint,
        parakeet_backend_row,
        parakeet_endpoint,
        execution_provider_row,
        model_status_row,
    );

    updating_widgets.set(false);
}

/// Build the current TranscriberConfig from shared state and send it to the daemon.
fn send_transcriber_config(
    state: &Rc<RefCell<voxkey_ipc::TranscriberConfig>>,
    handle: &DaemonHandle,
) {
    let config = state.borrow().clone();
    if let Ok(json) = serde_json::to_string(&config) {
        handle.send(DaemonCommand::SetTranscriberConfig(json));
    }
}

fn transcriber_location_subtitle(config: &voxkey_ipc::TranscriberConfig) -> &'static str {
    match config.provider {
        voxkey_ipc::TranscriberProvider::WhisperCpp => {
            "Runs locally; recorded audio stays on this computer"
        }
        voxkey_ipc::TranscriberProvider::Parakeet => match config.parakeet.backend {
            voxkey_ipc::ParakeetBackend::Local => {
                "Runs locally; recorded audio stays on this computer"
            }
            voxkey_ipc::ParakeetBackend::Http => {
                "Sends each finished recording to your transcription server"
            }
        },
        voxkey_ipc::TranscriberProvider::Mistral => "Sends each finished recording to Mistral",
        voxkey_ipc::TranscriberProvider::MistralRealtime => {
            "Streams audio to Mistral while you speak"
        }
    }
}

fn normalized_endpoint(raw: &str, default_endpoint: &str) -> String {
    let trimmed = raw.trim();
    if trimmed == default_endpoint {
        String::new()
    } else {
        trimmed.to_string()
    }
}

impl EndpointKind {
    fn saved_display(self, config: &voxkey_ipc::TranscriberConfig) -> String {
        match self {
            Self::MistralBatch => displayed_entry_value(
                &config.mistral.endpoint,
                voxkey_ipc::MistralConfig::DEFAULT_ENDPOINT,
            ),
            Self::MistralRealtime => displayed_entry_value(
                &config.mistral_realtime.endpoint,
                voxkey_ipc::MistralRealtimeConfig::DEFAULT_ENDPOINT,
            ),
            Self::ParakeetHttp => config.parakeet.endpoint.clone(),
        }
    }

    fn candidate(
        self,
        current: &voxkey_ipc::TranscriberConfig,
        entered: &str,
    ) -> Result<(voxkey_ipc::TranscriberConfig, String), String> {
        let mut candidate = current.clone();
        // Connectivity checks never need provider credentials. Keep that
        // invariant even if a legacy in-memory config somehow contains one.
        candidate.mistral.api_key.clear();
        candidate.mistral_realtime.api_key.clear();
        candidate.parakeet.api_key.clear();
        let stored = match self {
            Self::MistralBatch => {
                candidate.provider = voxkey_ipc::TranscriberProvider::Mistral;
                let endpoint =
                    normalized_endpoint(entered, voxkey_ipc::MistralConfig::DEFAULT_ENDPOINT);
                candidate.mistral.endpoint = endpoint.clone();
                endpoint
            }
            Self::MistralRealtime => {
                candidate.provider = voxkey_ipc::TranscriberProvider::MistralRealtime;
                let endpoint = normalized_endpoint(
                    entered,
                    voxkey_ipc::MistralRealtimeConfig::DEFAULT_ENDPOINT,
                );
                candidate.mistral_realtime.endpoint = endpoint.clone();
                endpoint
            }
            Self::ParakeetHttp => {
                let endpoint = entered.trim().to_string();
                if endpoint.is_empty() {
                    return Err(
                        "Enter the transcription server address before checking it.".to_string()
                    );
                }
                candidate.provider = voxkey_ipc::TranscriberProvider::Parakeet;
                candidate.parakeet.backend = voxkey_ipc::ParakeetBackend::Http;
                candidate.parakeet.endpoint = endpoint.clone();
                endpoint
            }
        };
        Ok((candidate, stored))
    }

    fn stored_value(self, config: &voxkey_ipc::TranscriberConfig) -> String {
        match self {
            Self::MistralBatch => config.mistral.endpoint.clone(),
            Self::MistralRealtime => config.mistral_realtime.endpoint.clone(),
            Self::ParakeetHttp => config.parakeet.endpoint.clone(),
        }
    }

    fn set_stored_value(self, config: &mut voxkey_ipc::TranscriberConfig, value: String) {
        match self {
            Self::MistralBatch => config.mistral.endpoint = value,
            Self::MistralRealtime => config.mistral_realtime.endpoint = value,
            Self::ParakeetHttp => config.parakeet.endpoint = value,
        }
    }

    fn insecure_http_allowed(self, config: &voxkey_ipc::TranscriberConfig) -> bool {
        self == Self::ParakeetHttp && config.parakeet.allow_insecure_http
    }

    fn set_insecure_http_allowed(self, config: &mut voxkey_ipc::TranscriberConfig, allowed: bool) {
        if self == Self::ParakeetHttp {
            config.parakeet.allow_insecure_http = allowed;
        }
    }

    fn set_saved_entry(self, row: &adw::EntryRow, stored: &str) {
        match self {
            Self::MistralBatch => {
                set_entry_with_default(row, stored, voxkey_ipc::MistralConfig::DEFAULT_ENDPOINT)
            }
            Self::MistralRealtime => set_entry_with_default(
                row,
                stored,
                voxkey_ipc::MistralRealtimeConfig::DEFAULT_ENDPOINT,
            ),
            Self::ParakeetHttp => set_entry_text_without_apply(row, stored),
        }
    }
}

fn endpoint_check_command_error(error: &str) -> &'static str {
    if error.contains("UnknownMethod") || error.contains("unknown method") {
        "Connectivity checks need the current Voxkey. Close and reopen settings, then try again."
    } else if error.contains("owner changed")
        || error.contains("channel closed")
        || error.contains("not available")
    {
        "Voxkey restarted during the check. Try again."
    } else {
        "The connectivity check could not finish. Check that Voxkey is running, then try again."
    }
}

fn endpoint_save_error(error: &str) -> &'static str {
    if error.contains("Stop or cancel dictation") || error.contains("while Voxkey is") {
        "The server responded, but the address was not saved. Stop dictation, then try again."
    } else {
        "The server responded, but Voxkey could not save the address. Try again."
    }
}

fn endpoint_requires_save(
    previous: &str,
    checked: &str,
    previous_insecure_http: bool,
    checked_insecure_http: bool,
) -> bool {
    previous != checked || previous_insecure_http != checked_insecure_http
}

fn wire_endpoint_editor(
    editor: &EndpointEditor,
    state: &Rc<RefCell<voxkey_ipc::TranscriberConfig>>,
    updating_widgets: &Rc<Cell<bool>>,
    handle: &DaemonHandle,
) {
    {
        let entry = editor.entry.clone();
        editor.check_button.connect_clicked(move |_| {
            entry.emit_by_name::<()>("apply", &[]);
        });
    }

    if let Some(permission_row) = editor.insecure_http_row.clone() {
        let editor = editor.clone();
        let state = state.clone();
        let updating_widgets = updating_widgets.clone();
        let handle = handle.clone();
        permission_row.connect_active_notify(move |row| {
            if updating_widgets.get() {
                return;
            }
            debug_assert_eq!(editor.kind, EndpointKind::ParakeetHttp);
            let allowed = row.is_active();
            let persisted = state.borrow().parakeet.allow_insecure_http;
            editor.permission_dirty.set(allowed != persisted);
            editor.next_request();
            if allowed == persisted
                && editor.entry.text() == editor.kind.saved_display(&state.borrow())
            {
                editor.show_saved();
                return;
            }
            editor.show_permission_changed(allowed);
            // Enabling plaintext transport remains pending until the endpoint
            // passes its check, so consent and address are saved atomically.
            // Revoking consent is applied immediately for safety.
            if !allowed && persisted {
                state.borrow_mut().parakeet.allow_insecure_http = false;
                editor.permission_dirty.set(false);
                send_transcriber_config(&state, &handle);
            }
        });
    }

    {
        let editor = editor.clone();
        let entry = editor.entry.clone();
        let state = state.clone();
        let updating_widgets = updating_widgets.clone();
        entry.connect_changed(move |_| {
            if updating_widgets.get() {
                return;
            }
            editor.next_request();
            let saved = editor.kind.saved_display(&state.borrow());
            let saved_permission = editor.kind.insecure_http_allowed(&state.borrow());
            let displayed_permission = editor
                .insecure_http_row
                .as_ref()
                .is_some_and(|row| row.is_active());
            if editor.entry.text() == saved
                && saved_permission == displayed_permission
                && !editor.permission_dirty.get()
            {
                editor.show_saved();
            } else {
                editor.show_idle();
            }
        });
    }

    {
        let editor = editor.clone();
        let state = state.clone();
        let updating_widgets = updating_widgets.clone();
        let handle = handle.clone();
        editor.entry.clone().connect_apply(move |row| {
            if updating_widgets.get() {
                return;
            }

            let entered = row.text().to_string();
            let (mut candidate, stored) = match editor.kind.candidate(&state.borrow(), &entered) {
                Ok(candidate) => candidate,
                Err(message) => {
                    let saved = editor.kind.saved_display(&state.borrow());
                    keep_entry_edit_pending(row, &saved, &entered);
                    editor.show_failed(&message);
                    return;
                }
            };
            let checked_insecure_http = editor
                .insecure_http_row
                .as_ref()
                .is_some_and(|row| row.is_active());
            editor
                .kind
                .set_insecure_http_allowed(&mut candidate, checked_insecure_http);
            let Ok(candidate_json) = serde_json::to_string(&candidate) else {
                editor.show_failed("Voxkey could not prepare this address for checking.");
                return;
            };
            let saved_display = editor.kind.saved_display(&state.borrow());
            let previous_stored = editor.kind.stored_value(&state.borrow());
            let previous_insecure_http = editor.kind.insecure_http_allowed(&state.borrow());
            let request_id = editor.next_request();
            editor.show_checking();

            let completion = handle.send(DaemonCommand::CheckEndpoint(candidate_json));
            let editor = editor.clone();
            let state = state.clone();
            let handle = handle.clone();
            glib::spawn_future_local(async move {
                let result = completion.wait_endpoint_check().await;
                if !editor.request_is_current(request_id) {
                    editor.set_controls_sensitive(true);
                    return;
                }

                let report = match result {
                    Ok(report) => report,
                    Err(error) => {
                        keep_entry_edit_pending(&editor.entry, &saved_display, &entered);
                        editor.show_failed(endpoint_check_command_error(&error));
                        return;
                    }
                };
                if report.status == voxkey_ipc::EndpointCheckStatus::Failed {
                    keep_entry_edit_pending(&editor.entry, &saved_display, &entered);
                    editor.show_failed(&report.message);
                    return;
                }

                if !endpoint_requires_save(
                    &previous_stored,
                    &stored,
                    previous_insecure_http,
                    checked_insecure_http,
                ) {
                    editor.kind.set_saved_entry(&editor.entry, &stored);
                    editor.permission_dirty.set(false);
                    editor.show_reachable(&report.message);
                    return;
                }

                editor.show_saving();
                {
                    let mut state = state.borrow_mut();
                    editor.kind.set_stored_value(&mut state, stored.clone());
                    editor
                        .kind
                        .set_insecure_http_allowed(&mut state, checked_insecure_http);
                }
                let config_json = match serde_json::to_string(&*state.borrow()) {
                    Ok(config_json) => config_json,
                    Err(_) => {
                        let mut state = state.borrow_mut();
                        editor
                            .kind
                            .set_stored_value(&mut state, previous_stored.clone());
                        editor
                            .kind
                            .set_insecure_http_allowed(&mut state, previous_insecure_http);
                        drop(state);
                        keep_entry_edit_pending(&editor.entry, &saved_display, &entered);
                        editor.show_failed("Voxkey could not prepare this address for saving.");
                        return;
                    }
                };
                let save = handle.send(DaemonCommand::SaveCheckedEndpoint(config_json));
                match save.wait().await {
                    Ok(()) => {
                        editor.kind.set_saved_entry(&editor.entry, &stored);
                        editor.permission_dirty.set(false);
                        editor.show_reachable(&report.message);
                    }
                    Err(error) => {
                        let mut state = state.borrow_mut();
                        editor.kind.set_stored_value(&mut state, previous_stored);
                        editor
                            .kind
                            .set_insecure_http_allowed(&mut state, previous_insecure_http);
                        drop(state);
                        keep_entry_edit_pending(&editor.entry, &saved_display, &entered);
                        editor.show_failed(endpoint_save_error(&error));
                    }
                }
            });
        });
    }
}

fn whisper_executable_error(path: &Path) -> Option<&'static str> {
    let Ok(metadata) = std::fs::metadata(path) else {
        return Some("The selected file could not be opened");
    };
    if !metadata.is_file() {
        return Some("Choose a file, not a folder");
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        return Some("The selected file is not executable");
    }
    None
}

fn whisper_command_available(command: &str) -> bool {
    let trimmed = command.trim();
    !trimmed.is_empty() && trimmed == command && glib::find_program_in_path(trimmed).is_some()
}

fn update_whisper_command_action(button: &gtk4::Button, command: &str) {
    if whisper_command_available(command) {
        button.set_label("Choose…");
        button.set_tooltip_text(Some("Choose a different whisper.cpp executable"));
        button.remove_css_class("suggested-action");
        button.add_css_class("flat");
    } else {
        button.set_label("Choose executable…");
        button.set_tooltip_text(Some("whisper.cpp is not available; choose its executable"));
        button.remove_css_class("flat");
        button.add_css_class("suggested-action");
    }
}

fn whisper_model_file_error(path: &Path) -> Option<&'static str> {
    let Ok(metadata) = std::fs::metadata(path) else {
        return Some("The selected model could not be opened");
    };
    if !metadata.is_file() {
        return Some("Choose a model file, not a folder");
    }
    if metadata.len() == 0 {
        return Some("The selected model file is empty");
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn wire_whisper_command_picker(
    button: &gtk4::Button,
    command_row: &adw::EntryRow,
    whisper_model_row: &adw::ActionRow,
    state: &Rc<RefCell<voxkey_ipc::TranscriberConfig>>,
    updating_widgets: &Rc<Cell<bool>>,
    handle: &DaemonHandle,
    toast_overlay: &adw::ToastOverlay,
    parent: &adw::ApplicationWindow,
) {
    let command_row = command_row.clone();
    let whisper_model_row = whisper_model_row.clone();
    let state = state.clone();
    let updating_widgets = updating_widgets.clone();
    let handle = handle.clone();
    let toast_overlay = toast_overlay.clone();
    let parent = parent.clone();
    button.connect_clicked(move |button| {
        let command_action = button.clone();
        let dialog = gtk4::FileDialog::builder()
            .title("Choose Whisper executable")
            .accept_label("Choose")
            .modal(true)
            .build();
        let current_path = std::path::PathBuf::from(command_row.text().as_str());
        if current_path.is_file() {
            dialog.set_initial_file(Some(&gtk4::gio::File::for_path(current_path)));
        }

        let command_row = command_row.clone();
        let whisper_model_row = whisper_model_row.clone();
        let state = state.clone();
        let updating_widgets = updating_widgets.clone();
        let handle = handle.clone();
        let toast_overlay = toast_overlay.clone();
        let parent = parent.clone();
        glib::spawn_future_local(async move {
            let file = match dialog.open_future(Some(&parent)).await {
                Ok(file) => file,
                Err(error)
                    if error.matches(gtk4::DialogError::Cancelled)
                        || error.matches(gtk4::DialogError::Dismissed)
                        || error.matches(gtk4::gio::IOErrorEnum::Cancelled) =>
                {
                    return;
                }
                Err(error) => {
                    tracing::warn!("Could not open the Whisper executable chooser: {error}");
                    toast_overlay.add_toast(adw::Toast::new(
                        "Could not open the file chooser. Try typing the path instead.",
                    ));
                    return;
                }
            };
            let Some(path) = file.path() else {
                toast_overlay.add_toast(adw::Toast::new(
                    "Choose an executable stored on this computer",
                ));
                return;
            };
            if let Some(message) = whisper_executable_error(&path) {
                toast_overlay.add_toast(adw::Toast::new(message));
                return;
            }
            if updating_widgets.get() {
                return;
            }

            let command = path.to_string_lossy().into_owned();
            state.borrow_mut().whisper_cpp.command = command.clone();
            set_entry_text_without_apply(&command_row, &command);
            update_whisper_command_action(&command_action, &command);
            update_whisper_model_row(&whisper_model_row, &state.borrow());
            send_transcriber_config(&state, &handle);
        });
    });
}

#[allow(clippy::too_many_arguments)]
fn wire_whisper_model_picker(
    button: &gtk4::Button,
    whisper_model_row: &adw::ActionRow,
    args_row: &adw::EntryRow,
    state: &Rc<RefCell<voxkey_ipc::TranscriberConfig>>,
    updating_widgets: &Rc<Cell<bool>>,
    handle: &DaemonHandle,
    toast_overlay: &adw::ToastOverlay,
    parent: &adw::ApplicationWindow,
) {
    let whisper_model_row = whisper_model_row.clone();
    let args_row = args_row.clone();
    let state = state.clone();
    let updating_widgets = updating_widgets.clone();
    let handle = handle.clone();
    let toast_overlay = toast_overlay.clone();
    let parent = parent.clone();
    button.connect_clicked(move |_| {
        let dialog = gtk4::FileDialog::builder()
            .title("Choose Whisper model")
            .accept_label("Choose")
            .modal(true)
            .build();
        if let Ok(args) = parse_whisper_args(&args_row.text())
            && let Some(current_path) = whisper_model_path(&args)
            && Path::new(current_path).is_file()
        {
            dialog.set_initial_file(Some(&gtk4::gio::File::for_path(current_path)));
        }

        let whisper_model_row = whisper_model_row.clone();
        let args_row = args_row.clone();
        let state = state.clone();
        let updating_widgets = updating_widgets.clone();
        let handle = handle.clone();
        let toast_overlay = toast_overlay.clone();
        let parent = parent.clone();
        glib::spawn_future_local(async move {
            let file = match dialog.open_future(Some(&parent)).await {
                Ok(file) => file,
                Err(error)
                    if error.matches(gtk4::DialogError::Cancelled)
                        || error.matches(gtk4::DialogError::Dismissed)
                        || error.matches(gtk4::gio::IOErrorEnum::Cancelled) =>
                {
                    return;
                }
                Err(error) => {
                    tracing::warn!("Could not open the Whisper model chooser: {error}");
                    toast_overlay.add_toast(adw::Toast::new(
                        "Could not open the file chooser. Try again.",
                    ));
                    return;
                }
            };
            let Some(path) = file.path() else {
                toast_overlay.add_toast(adw::Toast::new("Choose a model stored on this computer"));
                return;
            };
            if let Some(message) = whisper_model_file_error(&path) {
                toast_overlay.add_toast(adw::Toast::new(message));
                return;
            }
            if updating_widgets.get() {
                return;
            }
            let entered_args = match parse_whisper_args(&args_row.text()) {
                Ok(args) => args,
                Err(_) => {
                    toast_overlay.add_toast(adw::Toast::new(
                        "Fix command arguments before choosing a model",
                    ));
                    return;
                }
            };

            let model_path = path.to_string_lossy().into_owned();
            let args = whisper_args_with_model(&entered_args, &model_path);
            state.borrow_mut().whisper_cpp.args = args.clone();
            set_entry_text_without_apply(&args_row, &format_whisper_args(&args));
            update_whisper_model_row(&whisper_model_row, &state.borrow());
            send_transcriber_config(&state, &handle);
        });
    });
}

// All controls in this group share one configuration snapshot and daemon
// handle; listing them explicitly documents which signals this function owns.
#[allow(clippy::too_many_arguments)]
fn wire_transcriber_actions(
    provider_row: &adw::ComboRow,
    command_row: &adw::EntryRow,
    choose_command_button: &gtk4::Button,
    whisper_model_row: &adw::ActionRow,
    args_row: &adw::EntryRow,
    api_key_status_row: &adw::ActionRow,
    api_key_row: &adw::PasswordEntryRow,
    api_key_remove_button: &gtk4::Button,
    model_row: &adw::EntryRow,
    batch_endpoint: &EndpointEditor,
    realtime_endpoint: &EndpointEditor,
    parakeet_backend_row: &adw::ComboRow,
    parakeet_endpoint: &EndpointEditor,
    execution_provider_row: &adw::ComboRow,
    model_status_row: &adw::ActionRow,
    model_download_progress: &gtk4::ProgressBar,
    download_button: &gtk4::Button,
    delete_model_button: &gtk4::Button,
    open_folder_button: &gtk4::Button,
    state: &Rc<RefCell<voxkey_ipc::TranscriberConfig>>,
    updating_widgets: &Rc<Cell<bool>>,
    api_key_stored: &Rc<Cell<Option<bool>>>,
    api_key_request_id: &Rc<Cell<u64>>,
    expert_mode: &Rc<Cell<bool>>,
    handle: &DaemonHandle,
    toast_overlay: &adw::ToastOverlay,
) {
    // Provider combo: toggle visibility, update fields, and send config
    {
        let command_row = command_row.clone();
        let whisper_model_row = whisper_model_row.clone();
        let args_row = args_row.clone();
        let api_key_row = api_key_row.clone();
        let model_row = model_row.clone();
        let batch_endpoint = batch_endpoint.clone();
        let realtime_endpoint = realtime_endpoint.clone();
        let parakeet_backend_row = parakeet_backend_row.clone();
        let parakeet_endpoint = parakeet_endpoint.clone();
        let execution_provider_row = execution_provider_row.clone();
        let model_status_row = model_status_row.clone();
        let model_download_progress = model_download_progress.clone();
        let download_button = download_button.clone();
        let delete_model_button = delete_model_button.clone();
        let open_folder_button = open_folder_button.clone();
        let state = state.clone();
        let updating_widgets = updating_widgets.clone();
        let api_key_status_row = api_key_status_row.clone();
        let api_key_row = api_key_row.clone();
        let api_key_remove_button = api_key_remove_button.clone();
        let api_key_stored = api_key_stored.clone();
        let api_key_request_id = api_key_request_id.clone();
        let expert_mode = expert_mode.clone();
        let handle = handle.clone();
        provider_row.connect_selected_notify(move |row| {
            if updating_widgets.get() {
                return;
            }
            let selected = row.selected();
            let selected_model = local_model_for_choice(selected).map(|model| model.id);
            let is_parakeet = selected_model.is_some() || selected == CUSTOM_PARAKEET_CHOICE;

            if is_parakeet {
                let mut config = state.borrow_mut();
                config.provider = voxkey_ipc::TranscriberProvider::Parakeet;
                if let Some(model_name) = selected_model {
                    config.parakeet.model = model_name.to_string();
                }
                download_button.set_label(parakeet_model_action_label(&config.parakeet.model));
            } else {
                let provider = match selected {
                    0 => voxkey_ipc::TranscriberProvider::WhisperCpp,
                    2 => voxkey_ipc::TranscriberProvider::MistralRealtime,
                    _ => voxkey_ipc::TranscriberProvider::Mistral,
                };
                state.borrow_mut().provider = provider;
            }

            let provider = state.borrow().provider.clone();
            let is_mistral_api = matches!(
                provider,
                voxkey_ipc::TranscriberProvider::Mistral
                    | voxkey_ipc::TranscriberProvider::MistralRealtime
            );
            let uses_api_key = transcriber_api_service(&state.borrow()).is_some();
            let is_parakeet_local = is_parakeet
                && state.borrow().parakeet.backend == voxkey_ipc::ParakeetBackend::Local;

            {
                let config = state.borrow();
                row.set_subtitle(transcriber_location_subtitle(&config));
                apply_transcriber_visibility(
                    &config,
                    expert_mode.get(),
                    &command_row,
                    &whisper_model_row,
                    &args_row,
                    &api_key_status_row,
                    &api_key_row,
                    &model_row,
                    &batch_endpoint,
                    &realtime_endpoint,
                    &parakeet_backend_row,
                    &parakeet_endpoint,
                    &execution_provider_row,
                    &model_status_row,
                );
            }

            if uses_api_key {
                // The replacement-only editor must not carry another
                // service's key status while the daemon checks this one.
                api_key_stored.set(None);
                update_api_key_row_state(
                    &api_key_status_row,
                    &api_key_row,
                    &api_key_remove_button,
                    None,
                );
            }

            if is_mistral_api {
                let is_realtime = provider == voxkey_ipc::TranscriberProvider::MistralRealtime;
                let st = state.borrow();
                let (model, default_model) = if is_realtime {
                    (
                        &st.mistral_realtime.model,
                        voxkey_ipc::MistralRealtimeConfig::DEFAULT_MODEL,
                    )
                } else {
                    (&st.mistral.model, voxkey_ipc::MistralConfig::DEFAULT_MODEL)
                };
                set_entry_with_default(&model_row, model, default_model);
            }

            if is_parakeet_local {
                let model_name = state.borrow().parakeet.model.clone();
                apply_model_status(
                    "checking",
                    &model_name,
                    &model_status_row,
                    &model_download_progress,
                    &download_button,
                    &delete_model_button,
                    &open_folder_button,
                    expert_mode.get(),
                );
                handle.send(DaemonCommand::ModelStatus(model_name));
            }

            send_transcriber_config(&state, &handle);

            // Refresh the stored-key status for the new active provider.
            let service = transcriber_api_service(&state.borrow());
            request_api_key_status(service, &api_key_request_id, &handle);
        });
    }

    // whisper.cpp command apply
    {
        let state = state.clone();
        let handle = handle.clone();
        let updating_widgets = updating_widgets.clone();
        let choose_command_button = choose_command_button.clone();
        let whisper_model_row = whisper_model_row.clone();
        command_row.connect_apply(move |row| {
            if updating_widgets.get() {
                return;
            }
            state.borrow_mut().whisper_cpp.command = row.text().to_string();
            update_whisper_command_action(&choose_command_button, &row.text());
            update_whisper_model_row(&whisper_model_row, &state.borrow());
            send_transcriber_config(&state, &handle);
        });
    }

    // whisper.cpp args apply
    {
        let state = state.clone();
        let handle = handle.clone();
        let updating_widgets = updating_widgets.clone();
        let toast_overlay = toast_overlay.clone();
        let whisper_model_row = whisper_model_row.clone();
        args_row.connect_apply(move |row| {
            if updating_widgets.get() {
                return;
            }
            let entered = row.text().to_string();
            match parse_whisper_args(&entered) {
                Ok(args) => {
                    state.borrow_mut().whisper_cpp.args = args;
                    update_whisper_model_row(&whisper_model_row, &state.borrow());
                    send_transcriber_config(&state, &handle);
                }
                Err(error) => {
                    tracing::warn!("Invalid whisper.cpp arguments: {error}");
                    let saved = format_whisper_args(&state.borrow().whisper_cpp.args);
                    keep_entry_edit_pending(row, &saved, &entered);
                    toast_overlay.add_toast(adw::Toast::new(&format!(
                        "Invalid whisper.cpp arguments: {error}"
                    )));
                }
            }
        });
    }

    // Any actual typing makes an outstanding keyring read stale. Otherwise a
    // slow reply can replace a key the user has just entered but not applied.
    {
        let request_id = api_key_request_id.clone();
        let updating_widgets = updating_widgets.clone();
        api_key_row.connect_changed(move |_| {
            if !updating_widgets.get() {
                advance_api_key_request(&request_id);
            }
        });
    }

    // API key apply: write the typed value to the system keyring via D-Bus,
    // never the persisted config. The row is replacement-only: an empty apply
    // is ignored — removal has its own explicit button.
    {
        let state = state.clone();
        let handle = handle.clone();
        let stored = api_key_stored.clone();
        let request_id = api_key_request_id.clone();
        let updating_widgets = updating_widgets.clone();
        let api_key_status_row = api_key_status_row.clone();
        let remove_button = api_key_remove_button.clone();
        let toast_overlay = toast_overlay.clone();
        api_key_row.connect_apply(move |row| {
            if updating_widgets.get() {
                return;
            }
            let typed = normalized_api_key_input(&row.text());
            if typed.is_empty() {
                return;
            }
            let Some(service) = transcriber_api_service(&state.borrow()).map(str::to_string) else {
                return;
            };
            let previously_stored = stored.get();
            show_api_key_operation(&api_key_status_row, row, &remove_button, "Saving API key…");
            let operation_id = advance_api_key_request(&request_id);
            let completion = handle.send(DaemonCommand::SetApiKey {
                service: service.clone(),
                key: typed.clone(),
            });
            let state = state.clone();
            let handle = handle.clone();
            let stored = stored.clone();
            let request_id = request_id.clone();
            let api_key_status_row = api_key_status_row.clone();
            let api_key_row = row.clone();
            let remove_button = remove_button.clone();
            let toast_overlay = toast_overlay.clone();
            glib::spawn_future_local(async move {
                let result = completion.wait().await;
                let config = state.borrow();
                let provider = config.provider.clone();
                if !api_key_operation_is_current(&service, &config, operation_id, request_id.get())
                {
                    return;
                }
                drop(config);
                if result.is_err() {
                    stored.set(previously_stored);
                    update_api_key_row_state(
                        &api_key_status_row,
                        &api_key_row,
                        &remove_button,
                        previously_stored,
                    );
                    keep_api_key_edit_pending(&api_key_row, &typed);
                    return;
                }
                stored.set(Some(true));
                update_api_key_row_state(
                    &api_key_status_row,
                    &api_key_row,
                    &remove_button,
                    Some(true),
                );
                if let Some(message) = api_key_saved_message(&provider) {
                    toast_overlay.add_toast(adw::Toast::new(&message));
                }
                request_api_key_status(Some(&service), &request_id, &handle);
            });
        });
    }

    // API key removal is irreversible because the secret itself never enters
    // the settings process. Confirm first, then keep the row busy until the
    // daemon has either removed the key or reported a retryable failure.
    {
        let state = state.clone();
        let handle = handle.clone();
        let stored = api_key_stored.clone();
        let request_id = api_key_request_id.clone();
        let api_key_status_row = api_key_status_row.clone();
        let api_key_row = api_key_row.clone();
        let remove_button = api_key_remove_button.clone();
        let toast_overlay = toast_overlay.clone();
        api_key_remove_button.connect_clicked(move |button| {
            let config = state.borrow();
            let provider = config.provider.clone();
            let Some(service) = transcriber_api_service(&config).map(str::to_string) else {
                return;
            };
            drop(config);
            let Some(provider_name) = api_key_provider_name(&provider) else {
                return;
            };

            let dialog = adw::AlertDialog::builder()
                .heading(format!("Remove {provider_name} API key?"))
                .body("Authenticated transcription will stop working until you enter a new key.")
                .build();
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("remove", "Remove key");
            dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");

            let state = state.clone();
            let handle = handle.clone();
            let stored = stored.clone();
            let request_id = request_id.clone();
            let api_key_status_row = api_key_status_row.clone();
            let api_key_row = api_key_row.clone();
            let remove_button = remove_button.clone();
            let toast_overlay = toast_overlay.clone();
            dialog.connect_response(Some("remove"), move |_, _| {
                let pending_key = api_key_row.text().to_string();
                show_api_key_operation(
                    &api_key_status_row,
                    &api_key_row,
                    &remove_button,
                    "Removing API key…",
                );
                let operation_id = advance_api_key_request(&request_id);
                let completion = handle.send(DaemonCommand::ClearApiKey {
                    service: service.clone(),
                });
                let state = state.clone();
                let handle = handle.clone();
                let stored = stored.clone();
                let request_id = request_id.clone();
                let api_key_status_row = api_key_status_row.clone();
                let api_key_row = api_key_row.clone();
                let remove_button = remove_button.clone();
                let toast_overlay = toast_overlay.clone();
                let service = service.clone();
                glib::spawn_future_local(async move {
                    let result = completion.wait().await;
                    let config = state.borrow();
                    let is_current = api_key_operation_is_current(
                        &service,
                        &config,
                        operation_id,
                        request_id.get(),
                    );
                    drop(config);
                    if result.is_err() {
                        if !is_current {
                            return;
                        }
                        stored.set(Some(true));
                        update_api_key_row_state(
                            &api_key_status_row,
                            &api_key_row,
                            &remove_button,
                            Some(true),
                        );
                        if !pending_key.is_empty() {
                            keep_api_key_edit_pending(&api_key_row, &pending_key);
                        }
                        return;
                    }
                    toast_overlay.add_toast(adw::Toast::new("API key removed"));
                    if is_current {
                        stored.set(Some(false));
                        update_api_key_row_state(
                            &api_key_status_row,
                            &api_key_row,
                            &remove_button,
                            Some(false),
                        );
                        request_api_key_status(Some(&service), &request_id, &handle);
                    }
                });
            });
            if let Some(root) = button.root() {
                dialog.present(Some(&root));
            }
        });
    }

    // Model entry (writes to active provider's config)
    {
        let state = state.clone();
        let handle = handle.clone();
        let updating_widgets = updating_widgets.clone();
        model_row.connect_apply(move |row| {
            if updating_widgets.get() {
                return;
            }
            let model = row.text().to_string();
            let mut st = state.borrow_mut();
            match st.provider {
                voxkey_ipc::TranscriberProvider::MistralRealtime => {
                    st.mistral_realtime.model = model;
                }
                voxkey_ipc::TranscriberProvider::Mistral => {
                    st.mistral.model = model;
                }
                _ => return,
            }
            drop(st);
            send_transcriber_config(&state, &handle);
        });
    }

    wire_endpoint_editor(batch_endpoint, state, updating_widgets, handle);
    wire_endpoint_editor(realtime_endpoint, state, updating_widgets, handle);
    wire_endpoint_editor(parakeet_endpoint, state, updating_widgets, handle);

    // Parakeet backend selection controls whether inference is local or sent
    // to the model-specific HTTP endpoint.
    {
        let provider_row = provider_row.clone();
        let parakeet_endpoint = parakeet_endpoint.clone();
        let api_key_status_row = api_key_status_row.clone();
        let api_key_row = api_key_row.clone();
        let api_key_remove_button = api_key_remove_button.clone();
        let api_key_stored = api_key_stored.clone();
        let api_key_request_id = api_key_request_id.clone();
        let execution_provider_row = execution_provider_row.clone();
        let model_status_row = model_status_row.clone();
        let model_download_progress = model_download_progress.clone();
        let download_button = download_button.clone();
        let delete_model_button = delete_model_button.clone();
        let open_folder_button = open_folder_button.clone();
        let state = state.clone();
        let expert_mode = expert_mode.clone();
        let handle = handle.clone();
        let updating_widgets = updating_widgets.clone();
        parakeet_backend_row.connect_selected_notify(move |row| {
            if updating_widgets.get() {
                return;
            }
            let backend = if row.selected() == 1 {
                voxkey_ipc::ParakeetBackend::Http
            } else {
                voxkey_ipc::ParakeetBackend::Local
            };
            state.borrow_mut().parakeet.backend = backend;
            let is_active = state.borrow().provider == voxkey_ipc::TranscriberProvider::Parakeet;
            let is_http = is_active && backend == voxkey_ipc::ParakeetBackend::Http;
            let is_local = is_active && !is_http;
            row.set_visible(is_active && (expert_mode.get() || is_http));
            parakeet_endpoint.set_visible(is_http);
            api_key_status_row.set_visible(is_http);
            api_key_row.set_visible(is_http);
            api_key_stored.set(None);
            update_api_key_row_state(
                &api_key_status_row,
                &api_key_row,
                &api_key_remove_button,
                None,
            );
            let api_service = is_http.then_some(voxkey_ipc::API_KEY_SERVICE_MODEL_SERVER);
            request_api_key_status(api_service, &api_key_request_id, &handle);
            if is_http {
                parakeet_endpoint.show_saved();
            }
            execution_provider_row.set_visible(
                is_local
                    && (expert_mode.get()
                        || state.borrow().parakeet.execution_provider
                            != voxkey_ipc::ExecutionProviderChoice::Auto),
            );
            model_status_row.set_visible(is_local);
            provider_row.set_subtitle(transcriber_location_subtitle(&state.borrow()));
            if is_local {
                let model_name = state.borrow().parakeet.model.clone();
                apply_model_status(
                    "checking",
                    &model_name,
                    &model_status_row,
                    &model_download_progress,
                    &download_button,
                    &delete_model_button,
                    &open_folder_button,
                    expert_mode.get(),
                );
                handle.send(DaemonCommand::ModelStatus(model_name));
            }
            send_transcriber_config(&state, &handle);
        });
    }

    // Execution provider combo (Parakeet)
    {
        let state = state.clone();
        let expert_mode = expert_mode.clone();
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
            row.set_subtitle(execution_provider_subtitle(row.selected()));
            state.borrow_mut().parakeet.execution_provider = ep;
            row.set_visible(expert_mode.get() || ep != voxkey_ipc::ExecutionProviderChoice::Auto);
            send_transcriber_config(&state, &handle);
        });
    }

    // Download button
    {
        let state = state.clone();
        let handle = handle.clone();
        let model_status_row = model_status_row.clone();
        let model_download_progress = model_download_progress.clone();
        let delete_model_button = delete_model_button.clone();
        let open_folder_button = open_folder_button.clone();
        let expert_mode = expert_mode.clone();
        let toast_overlay = toast_overlay.clone();
        download_button.connect_clicked(move |button| {
            let model_name = state.borrow().parakeet.model.clone();
            if button.label().as_deref() == ModelStatusAction::CancelDownload.label() {
                apply_model_status(
                    "cancelling",
                    &model_name,
                    &model_status_row,
                    &model_download_progress,
                    button,
                    &delete_model_button,
                    &open_folder_button,
                    expert_mode.get(),
                );
                let completion =
                    handle.send(DaemonCommand::CancelModelDownload(model_name.clone()));
                let handle = handle.clone();
                let toast_overlay = toast_overlay.clone();
                glib::spawn_future_local(async move {
                    let cancelled = completion.wait().await.is_ok();
                    handle.send(DaemonCommand::ModelStatus(model_name.clone()));
                    if cancelled {
                        toast_overlay.add_toast(adw::Toast::new(&format!(
                            "{} download cancelled",
                            parakeet_model_display_name(&model_name)
                        )));
                    }
                });
                return;
            }
            if button.label().as_deref() == ModelStatusAction::OpenFolder.label() {
                handle.send(DaemonCommand::OpenModelsDir);
                return;
            }
            apply_model_status(
                "downloading",
                &model_name,
                &model_status_row,
                &model_download_progress,
                button,
                &delete_model_button,
                &open_folder_button,
                expert_mode.get(),
            );
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
        let model_download_progress = model_download_progress.clone();
        let download_button = download_button.clone();
        let delete_model_button_for_result = delete_model_button.clone();
        let open_folder_button = open_folder_button.clone();
        let expert_mode = expert_mode.clone();
        let toast_overlay = toast_overlay.clone();
        delete_model_button.connect_clicked(move |button| {
            let model_name = state.borrow().parakeet.model.clone();
            let dialog = adw::AlertDialog::builder()
                .heading(format!(
                    "Delete {}?",
                    parakeet_model_display_name(&model_name)
                ))
                .heading_use_markup(false)
                .body("The model files will need to be downloaded again before local transcription can use this model.")
                .build();
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("delete", "Delete");
            dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");

            let handle = handle.clone();
            let model_status_row = model_status_row.clone();
            let model_download_progress = model_download_progress.clone();
            let download_button = download_button.clone();
            let delete_model_button = delete_model_button_for_result.clone();
            let open_folder_button = open_folder_button.clone();
            let expert_mode = expert_mode.clone();
            let toast_overlay = toast_overlay.clone();
            let state = state.clone();
            dialog.connect_response(None, move |_, response| {
                if response != "delete" {
                    return;
                }
                apply_model_status(
                    "deleting",
                    &model_name,
                    &model_status_row,
                    &model_download_progress,
                    &download_button,
                    &delete_model_button,
                    &open_folder_button,
                    expert_mode.get(),
                );
                let completion = handle.send(DaemonCommand::DeleteModel(model_name.clone()));
                let model_status_row = model_status_row.clone();
                let model_download_progress = model_download_progress.clone();
                let download_button = download_button.clone();
                let delete_model_button = delete_model_button.clone();
                let open_folder_button = open_folder_button.clone();
                let expert_mode = expert_mode.clone();
                let toast_overlay = toast_overlay.clone();
                let model_name = model_name.clone();
                let state = state.clone();
                glib::spawn_future_local(async move {
                    let (status, deleted) = if completion.wait().await.is_ok() {
                        ("not_downloaded", true)
                    } else {
                        ("available", false)
                    };
                    if state.borrow().parakeet.model == model_name {
                        apply_model_status(
                            status,
                            &model_name,
                            &model_status_row,
                            &model_download_progress,
                            &download_button,
                            &delete_model_button,
                            &open_folder_button,
                            expert_mode.get(),
                        );
                    }
                    if deleted {
                        toast_overlay.add_toast(adw::Toast::new(&format!(
                            "{} deleted",
                            parakeet_model_display_name(&model_name)
                        )));
                    }
                });
            });
            dialog.present(Some(&button.root().expect("button must belong to the window")));
        });
    }
}

/// Parse injection config JSON and update the typing delay widget.
fn apply_injection_config_to_widgets(
    config_json: &str,
    typing_delay_row: &adw::SpinRow,
    state: &Rc<RefCell<voxkey_ipc::InjectionConfig>>,
    updating_widgets: &Rc<Cell<bool>>,
) {
    let Ok(ic) = serde_json::from_str::<voxkey_ipc::InjectionConfig>(config_json) else {
        return;
    };

    apply_injection_config_to_widgets_from_config(&ic, typing_delay_row, state, updating_widgets);
}

fn apply_injection_config_to_widgets_from_config(
    config: &voxkey_ipc::InjectionConfig,
    typing_delay_row: &adw::SpinRow,
    state: &Rc<RefCell<voxkey_ipc::InjectionConfig>>,
    updating_widgets: &Rc<Cell<bool>>,
) {
    updating_widgets.set(true);
    *state.borrow_mut() = config.clone();
    typing_delay_row.set_value(config.typing_delay_ms as f64);
    updating_widgets.set(false);
}

/// Build the current InjectionConfig from shared state and send it to the daemon.
fn send_injection_config(state: &Rc<RefCell<voxkey_ipc::InjectionConfig>>, handle: &DaemonHandle) {
    let config = state.borrow().clone();
    if let Ok(json) = serde_json::to_string(&config) {
        handle.send(DaemonCommand::SetInjectionConfig(json));
    }
}

fn wire_injection_actions(
    typing_delay_row: &adw::SpinRow,
    state: &Rc<RefCell<voxkey_ipc::InjectionConfig>>,
    updating_widgets: &Rc<Cell<bool>>,
    handle: &DaemonHandle,
) {
    let state = state.clone();
    let updating_widgets = updating_widgets.clone();
    let handle = handle.clone();
    typing_delay_row.connect_value_notify(move |row| {
        if updating_widgets.get() {
            return;
        }
        state.borrow_mut().typing_delay_ms = row.value() as u32;
        send_injection_config(&state, &handle);
    });
}

fn preview_mode_from_selected(selected: u32) -> voxkey_ipc::PreviewMode {
    match selected {
        1 => voxkey_ipc::PreviewMode::Always,
        2 => voxkey_ipc::PreviewMode::Never,
        _ => voxkey_ipc::PreviewMode::Auto,
    }
}

fn selected_for_preview_mode(mode: voxkey_ipc::PreviewMode) -> u32 {
    match mode {
        voxkey_ipc::PreviewMode::Auto => 0,
        voxkey_ipc::PreviewMode::Always => 1,
        voxkey_ipc::PreviewMode::Never => 2,
    }
}

fn preview_strategy_from_selected(selected: u32) -> voxkey_ipc::PreviewStrategy {
    match selected {
        1 => voxkey_ipc::PreviewStrategy::Segmented,
        _ => voxkey_ipc::PreviewStrategy::Whole,
    }
}

fn selected_for_preview_strategy(strategy: voxkey_ipc::PreviewStrategy) -> u32 {
    match strategy {
        voxkey_ipc::PreviewStrategy::Whole => 0,
        voxkey_ipc::PreviewStrategy::Segmented => 1,
    }
}

impl PreviewPreset {
    fn from_config(config: &voxkey_ipc::PreviewConfig) -> Self {
        [Self::Automatic, Self::AlwaysLive, Self::FinalOnly]
            .into_iter()
            .find(|preset| preset.config().as_ref() == Some(config))
            .unwrap_or(Self::Custom)
    }

    fn from_selected(selected: u32) -> Self {
        match selected {
            1 => Self::AlwaysLive,
            2 => Self::FinalOnly,
            3 => Self::Custom,
            _ => Self::Automatic,
        }
    }

    fn selected(self) -> u32 {
        match self {
            Self::Automatic => 0,
            Self::AlwaysLive => 1,
            Self::FinalOnly => 2,
            Self::Custom => 3,
        }
    }

    fn config(self) -> Option<voxkey_ipc::PreviewConfig> {
        let mode = match self {
            Self::Automatic => voxkey_ipc::PreviewMode::Auto,
            Self::AlwaysLive => voxkey_ipc::PreviewMode::Always,
            Self::FinalOnly => voxkey_ipc::PreviewMode::Never,
            Self::Custom => return None,
        };
        Some(voxkey_ipc::PreviewConfig {
            mode,
            ..Default::default()
        })
    }
}

fn transcriber_runs_locally(config: &voxkey_ipc::TranscriberConfig) -> bool {
    match config.provider {
        voxkey_ipc::TranscriberProvider::WhisperCpp => true,
        voxkey_ipc::TranscriberProvider::Parakeet => {
            config.parakeet.backend == voxkey_ipc::ParakeetBackend::Local
        }
        voxkey_ipc::TranscriberProvider::Mistral
        | voxkey_ipc::TranscriberProvider::MistralRealtime => false,
    }
}

fn transcriber_is_realtime(config: &voxkey_ipc::TranscriberConfig) -> bool {
    config.provider == voxkey_ipc::TranscriberProvider::MistralRealtime
}

fn preview_mode_subtitle(
    mode: voxkey_ipc::PreviewMode,
    transcriber: &voxkey_ipc::TranscriberConfig,
) -> &'static str {
    if transcriber_is_realtime(transcriber) {
        return "Realtime models provide their own live text; this setting is ignored";
    }

    match (mode, transcriber_runs_locally(transcriber)) {
        (voxkey_ipc::PreviewMode::Auto, true) => "On for this local model",
        (voxkey_ipc::PreviewMode::Auto, false) => {
            "Off for this network model; choose Always to enable it"
        }
        (voxkey_ipc::PreviewMode::Always, true) => "Live text is generated while you dictate",
        (voxkey_ipc::PreviewMode::Always, false) => {
            "Growing audio is sent to the active server while you dictate"
        }
        (voxkey_ipc::PreviewMode::Never, _) => "No live text is generated while recording",
    }
}

fn preview_strategy_subtitle(strategy: voxkey_ipc::PreviewStrategy) -> &'static str {
    match strategy {
        voxkey_ipc::PreviewStrategy::Whole => {
            "Keep agreed text and recheck only the uncertain tail"
        }
        voxkey_ipc::PreviewStrategy::Segmented => {
            "Commit phrases at pauses to keep each preview request shorter"
        }
    }
}

fn preview_preset_subtitle(
    preset: PreviewPreset,
    transcriber: &voxkey_ipc::TranscriberConfig,
) -> &'static str {
    if transcriber_is_realtime(transcriber) {
        return "This realtime engine already provides live text";
    }

    match (preset, transcriber_runs_locally(transcriber)) {
        (PreviewPreset::Automatic, true) => "Recommended — balanced live text for local models",
        (PreviewPreset::Automatic, false) => "Recommended — avoids repeated network requests",
        (PreviewPreset::AlwaysLive, true) => "Shows stable text while you speak",
        (PreviewPreset::AlwaysLive, false) => {
            "Repeatedly sends recent audio to the server while you speak"
        }
        (PreviewPreset::FinalOnly, _) => "Shows the transcription after you stop recording",
        (PreviewPreset::Custom, _) => "Uses the detailed controls shown in expert mode",
    }
}

fn preview_controls_are_active(
    preview: &voxkey_ipc::PreviewConfig,
    transcriber: &voxkey_ipc::TranscriberConfig,
) -> bool {
    !transcriber_is_realtime(transcriber) && preview.allows(transcriber_runs_locally(transcriber))
}

#[allow(clippy::too_many_arguments)]
fn update_preview_widget_context(
    preview: &voxkey_ipc::PreviewConfig,
    transcriber: &voxkey_ipc::TranscriberConfig,
    preset_row: &adw::ComboRow,
    mode_row: &adw::ComboRow,
    strategy_row: &adw::ComboRow,
    interval_row: &adw::SpinRow,
    audio_limit_row: &adw::SpinRow,
    updating_widgets: &Rc<Cell<bool>>,
) {
    let preset = PreviewPreset::from_config(preview);
    let was_updating = updating_widgets.replace(true);
    preset_row.set_selected(preset.selected());
    updating_widgets.set(was_updating);
    preset_row.set_subtitle(preview_preset_subtitle(preset, transcriber));
    preset_row.set_sensitive(!transcriber_is_realtime(transcriber));

    mode_row.set_subtitle(preview_mode_subtitle(preview.mode, transcriber));
    strategy_row.set_subtitle(preview_strategy_subtitle(preview.strategy));

    let active = preview_controls_are_active(preview, transcriber);
    strategy_row.set_sensitive(active);
    interval_row.set_sensitive(active);
    audio_limit_row.set_sensitive(active);

    if !transcriber_runs_locally(transcriber) && !transcriber_is_realtime(transcriber) {
        interval_row
            .set_subtitle("Seconds between server requests; lower values increase server load");
        audio_limit_row
            .set_subtitle("Seconds of unconfirmed audio sent per request; 0 is unlimited");
    } else {
        interval_row.set_subtitle("Seconds between previews; lower values use more processor time");
        audio_limit_row
            .set_subtitle("Seconds of unconfirmed audio decoded per preview; 0 is unlimited");
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_preview_config_to_widgets(
    config_json: &str,
    preset_row: &adw::ComboRow,
    mode_row: &adw::ComboRow,
    strategy_row: &adw::ComboRow,
    interval_row: &adw::SpinRow,
    audio_limit_row: &adw::SpinRow,
    state: &Rc<RefCell<voxkey_ipc::PreviewConfig>>,
    transcriber_state: &Rc<RefCell<voxkey_ipc::TranscriberConfig>>,
    updating_widgets: &Rc<Cell<bool>>,
) {
    let Ok(config) = serde_json::from_str::<voxkey_ipc::PreviewConfig>(config_json) else {
        return;
    };

    updating_widgets.set(true);
    *state.borrow_mut() = config.clone();
    mode_row.set_selected(selected_for_preview_mode(config.mode));
    strategy_row.set_selected(selected_for_preview_strategy(config.strategy));
    interval_row.set_value(config.interval_ms as f64 / 1000.0);
    audio_limit_row.set_value(config.max_audio_seconds as f64);
    update_preview_widget_context(
        &config,
        &transcriber_state.borrow(),
        preset_row,
        mode_row,
        strategy_row,
        interval_row,
        audio_limit_row,
        updating_widgets,
    );
    updating_widgets.set(false);
}

fn send_preview_config(state: &Rc<RefCell<voxkey_ipc::PreviewConfig>>, handle: &DaemonHandle) {
    let config = state.borrow().clone();
    if let Ok(json) = serde_json::to_string(&config) {
        handle.send(DaemonCommand::SetPreviewConfig(json));
    }
}

#[allow(clippy::too_many_arguments)]
fn wire_preview_actions(
    preset_row: &adw::ComboRow,
    mode_row: &adw::ComboRow,
    strategy_row: &adw::ComboRow,
    interval_row: &adw::SpinRow,
    audio_limit_row: &adw::SpinRow,
    expert_mode_row: &adw::SwitchRow,
    state: &Rc<RefCell<voxkey_ipc::PreviewConfig>>,
    transcriber_state: &Rc<RefCell<voxkey_ipc::TranscriberConfig>>,
    updating_widgets: &Rc<Cell<bool>>,
    handle: &DaemonHandle,
) {
    {
        let mode_row = mode_row.clone();
        let strategy_row = strategy_row.clone();
        let interval_row = interval_row.clone();
        let audio_limit_row = audio_limit_row.clone();
        let expert_mode_row = expert_mode_row.clone();
        let state = state.clone();
        let transcriber_state = transcriber_state.clone();
        let updating_widgets = updating_widgets.clone();
        let handle = handle.clone();
        preset_row.connect_selected_notify(move |preset_row| {
            if updating_widgets.get() {
                return;
            }

            let preset = PreviewPreset::from_selected(preset_row.selected());
            let Some(config) = preset.config() else {
                expert_mode_row.set_active(true);
                preset_row.set_subtitle(preview_preset_subtitle(
                    PreviewPreset::Custom,
                    &transcriber_state.borrow(),
                ));
                return;
            };

            updating_widgets.set(true);
            *state.borrow_mut() = config.clone();
            mode_row.set_selected(selected_for_preview_mode(config.mode));
            strategy_row.set_selected(selected_for_preview_strategy(config.strategy));
            interval_row.set_value(config.interval_ms as f64 / 1000.0);
            audio_limit_row.set_value(config.max_audio_seconds as f64);
            update_preview_widget_context(
                &config,
                &transcriber_state.borrow(),
                preset_row,
                &mode_row,
                &strategy_row,
                &interval_row,
                &audio_limit_row,
                &updating_widgets,
            );
            updating_widgets.set(false);
            send_preview_config(&state, &handle);
        });
    }

    {
        let preset_row = preset_row.clone();
        let state = state.clone();
        let transcriber_state = transcriber_state.clone();
        let updating_widgets = updating_widgets.clone();
        let handle = handle.clone();
        let strategy_row = strategy_row.clone();
        let interval_row = interval_row.clone();
        let audio_limit_row = audio_limit_row.clone();
        mode_row.connect_selected_notify(move |mode_row| {
            if updating_widgets.get() {
                return;
            }
            state.borrow_mut().mode = preview_mode_from_selected(mode_row.selected());
            update_preview_widget_context(
                &state.borrow(),
                &transcriber_state.borrow(),
                &preset_row,
                mode_row,
                &strategy_row,
                &interval_row,
                &audio_limit_row,
                &updating_widgets,
            );
            send_preview_config(&state, &handle);
        });
    }

    {
        let preset_row = preset_row.clone();
        let state = state.clone();
        let transcriber_state = transcriber_state.clone();
        let updating_widgets = updating_widgets.clone();
        let handle = handle.clone();
        let mode_row = mode_row.clone();
        let interval_row = interval_row.clone();
        let audio_limit_row = audio_limit_row.clone();
        strategy_row.connect_selected_notify(move |row| {
            if updating_widgets.get() {
                return;
            }
            let strategy = preview_strategy_from_selected(row.selected());
            state.borrow_mut().strategy = strategy;
            update_preview_widget_context(
                &state.borrow(),
                &transcriber_state.borrow(),
                &preset_row,
                &mode_row,
                row,
                &interval_row,
                &audio_limit_row,
                &updating_widgets,
            );
            send_preview_config(&state, &handle);
        });
    }

    {
        let preset_row = preset_row.clone();
        let state = state.clone();
        let transcriber_state = transcriber_state.clone();
        let updating_widgets = updating_widgets.clone();
        let handle = handle.clone();
        let mode_row = mode_row.clone();
        let strategy_row = strategy_row.clone();
        let audio_limit_row = audio_limit_row.clone();
        interval_row.connect_value_notify(move |row| {
            if updating_widgets.get() {
                return;
            }
            state.borrow_mut().interval_ms = (row.value() * 1000.0).round().max(0.0) as u32;
            update_preview_widget_context(
                &state.borrow(),
                &transcriber_state.borrow(),
                &preset_row,
                &mode_row,
                &strategy_row,
                row,
                &audio_limit_row,
                &updating_widgets,
            );
            send_preview_config(&state, &handle);
        });
    }

    {
        let preset_row = preset_row.clone();
        let state = state.clone();
        let transcriber_state = transcriber_state.clone();
        let updating_widgets = updating_widgets.clone();
        let handle = handle.clone();
        let mode_row = mode_row.clone();
        let strategy_row = strategy_row.clone();
        let interval_row = interval_row.clone();
        audio_limit_row.connect_value_notify(move |row| {
            if updating_widgets.get() {
                return;
            }
            state.borrow_mut().max_audio_seconds = row.value().round().max(0.0) as u32;
            update_preview_widget_context(
                &state.borrow(),
                &transcriber_state.borrow(),
                &preset_row,
                &mode_row,
                &strategy_row,
                &interval_row,
                row,
                &updating_widgets,
            );
            send_preview_config(&state, &handle);
        });
    }
}

fn wire_advanced_actions(
    open_config_row: &adw::ActionRow,
    reload_row: &adw::ActionRow,
    clear_token_row: &adw::ActionRow,
    handle: &DaemonHandle,
    toast_overlay: &adw::ToastOverlay,
) {
    let toast_clone = toast_overlay.clone();
    open_config_row.connect_activated(move |row| {
        open_configuration_directory(row, &toast_clone);
    });

    let handle_clone = handle.clone();
    let toast_clone = toast_overlay.clone();
    reload_row.connect_activated(move |_| {
        let completion = handle_clone.send(DaemonCommand::ReloadConfig);
        toast_after_success(completion, &toast_clone, "Configuration reloaded");
    });

    let handle_clone = handle.clone();
    let toast_clone = toast_overlay.clone();
    clear_token_row.connect_activated(move |row| {
        if let Some(root) = row.root() {
            present_reset_desktop_permission_dialog(&root, &handle_clone, &toast_clone);
        }
    });
}

fn open_configuration_directory(row: &adw::ActionRow, toast_overlay: &adw::ToastOverlay) {
    let Some(path) = gui_settings::config_directory() else {
        toast_overlay.add_toast(adw::Toast::new("Could not find the configuration folder"));
        return;
    };

    let directory = gtk4::gio::File::for_path(path);
    let uri = directory.uri();
    let row = row.clone();
    let toast_overlay = toast_overlay.clone();
    row.set_sensitive(false);
    glib::spawn_future_local(async move {
        if let Err(error) = directory
            .make_directory_future(glib::Priority::DEFAULT)
            .await
            && !error.matches(gtk4::gio::IOErrorEnum::Exists)
        {
            tracing::warn!("Could not prepare the configuration folder: {error}");
            toast_overlay.add_toast(adw::Toast::new(
                "Could not prepare the configuration folder",
            ));
            row.set_sensitive(true);
            return;
        }

        if let Err(error) = gtk4::gio::AppInfo::launch_default_for_uri_future(
            &uri,
            None::<&gtk4::gio::AppLaunchContext>,
        )
        .await
        {
            tracing::warn!("Could not open the configuration folder: {error}");
            toast_overlay.add_toast(adw::Toast::new("Could not open the configuration folder"));
        }
        row.set_sensitive(true);
    });
}

fn present_reset_desktop_permission_dialog(
    parent: &impl IsA<gtk4::Widget>,
    handle: &DaemonHandle,
    toast_overlay: &adw::ToastOverlay,
) {
    let dialog = adw::AlertDialog::builder()
        .heading("Reset desktop access?")
        .body(
            "Voxkey will disconnect from desktop input and ask GNOME for permission again. Use this only if typing into other apps has stopped working.",
        )
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("reset", "Reset access");
    dialog.set_response_appearance("reset", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let handle = handle.clone();
    let toast_overlay = toast_overlay.clone();
    dialog.connect_response(Some("reset"), move |_, _| {
        let completion = handle.send(DaemonCommand::ClearRestoreToken);
        toast_after_success(
            completion,
            &toast_overlay,
            "Desktop access reset; reconnecting…",
        );
    });
    dialog.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initialize_adwaita() -> bool {
        static INITIALIZED_ON: std::sync::OnceLock<Option<std::thread::ThreadId>> =
            std::sync::OnceLock::new();
        let initialized_on =
            INITIALIZED_ON.get_or_init(|| adw::init().is_ok().then(|| std::thread::current().id()));
        initialized_on.as_ref() == Some(&std::thread::current().id())
    }

    #[test]
    fn transcription_setup_heading_tracks_the_selected_engine_family() {
        assert_eq!(transcriber_setup_copy(0, 0).title, "Whisper.cpp setup");
        assert_eq!(transcriber_setup_copy(1, 0).title, "Mistral setup");
        assert_eq!(transcriber_setup_copy(2, 0).title, "Mistral Realtime setup");
        assert_eq!(transcriber_setup_copy(3, 0).title, "Local model setup");
        assert_eq!(transcriber_setup_copy(5, 0).title, "Local model setup");
        assert_eq!(transcriber_setup_copy(5, 1).title, "Model server setup");
    }

    #[test]
    fn transcription_location_icon_distinguishes_computer_and_server_engines() {
        assert_eq!(transcriber_location_icon_name(0, 0), "computer-symbolic");
        assert_eq!(
            transcriber_location_icon_name(1, 0),
            "network-server-symbolic"
        );
        assert_eq!(transcriber_location_icon_name(4, 0), "computer-symbolic");
        assert_eq!(
            transcriber_location_icon_name(4, 1),
            "network-server-symbolic"
        );
    }

    #[test]
    fn processor_choices_explain_their_runtime_behavior() {
        assert!(execution_provider_subtitle(0).contains("when available"));
        assert!(execution_provider_subtitle(1).contains("CPU"));
        assert!(execution_provider_subtitle(2).contains("NVIDIA GPU"));
    }

    #[test]
    fn daemon_settings_are_editable_only_while_ready() {
        assert!(daemon_controls_are_editable(true, "Idle"));
        for state in [
            "Connecting",
            "Recording",
            "Streaming",
            "Transcribing",
            "Injecting",
            "RecoveringSession",
        ] {
            assert!(!daemon_controls_are_editable(true, state));
        }
        assert!(!daemon_controls_are_editable(false, "Idle"));
    }

    #[test]
    fn busy_settings_banner_explains_each_visible_lock() {
        assert_eq!(settings_lock_message("Idle"), None);
        assert_eq!(settings_lock_action_label("Idle"), None);
        assert!(
            settings_lock_message("Recording").is_some_and(|message| message.contains("cancel"))
        );
        assert_eq!(
            settings_lock_action_label("Recording"),
            Some("Cancel dictation")
        );
        assert!(
            settings_lock_message("Transcribing")
                .is_some_and(|message| message.contains("transcription"))
        );
        assert_eq!(
            settings_lock_action_label("Transcribing"),
            Some("Cancel dictation")
        );
        assert!(
            settings_lock_message("Injecting").is_some_and(|message| message.contains("typing"))
        );
        assert_eq!(settings_lock_action_label("Injecting"), None);
        assert!(
            settings_lock_message("RecoveringSession")
                .is_some_and(|message| message.contains("desktop access"))
        );
        assert_eq!(settings_lock_action_label("RecoveringSession"), None);
    }

    #[test]
    fn empty_history_action_matches_the_service_state() {
        assert_eq!(
            history_empty_action_label(false, "Unavailable"),
            "Open General"
        );
        assert_eq!(
            history_empty_action_label(true, "Recording"),
            "View dictation status"
        );
        assert_eq!(
            history_empty_action_label(true, "Idle"),
            "View dictation shortcut"
        );
    }

    #[test]
    fn unavailable_microphone_remains_the_visible_selection() {
        let devices = vec!["Built-in Microphone".to_string()];
        let presentation = audio_device_presentation(&devices, "USB Headset");

        assert_eq!(
            presentation.labels,
            vec![
                "System default".to_string(),
                "USB Headset (unavailable)".to_string(),
                "Built-in Microphone".to_string(),
            ]
        );
        assert_eq!(
            presentation.values,
            vec!["USB Headset".to_string(), "Built-in Microphone".to_string()]
        );
        assert_eq!(presentation.selected, 1);
        assert!(presentation.subtitle.contains("Choose"));
        assert!(presentation.selectable);
    }

    #[test]
    fn available_and_default_microphones_keep_their_expected_indices() {
        let devices = vec!["Built-in Microphone".to_string(), "USB Headset".to_string()];

        let available = audio_device_presentation(&devices, "USB Headset");
        assert_eq!(available.selected, 2);
        assert_eq!(available.values, devices);

        let default = audio_device_presentation(&available.values, "");
        assert_eq!(default.selected, 0);
        assert_eq!(default.labels[0], "System default");
        assert!(default.selectable);
    }

    #[test]
    fn recording_formats_use_familiar_audio_units() {
        assert_eq!(recording_format_description(16_000, 1), "16 kHz · Mono");
        assert_eq!(recording_format_description(44_100, 2), "44.1 kHz · Stereo");
        assert_eq!(
            recording_format_description(48_000, 6),
            "48 kHz · 6 channels"
        );
        assert_eq!(recording_format_description(800, 1), "800 Hz · Mono");
    }

    #[test]
    fn empty_microphone_list_explains_how_to_retry() {
        let presentation = audio_device_presentation(&[], "");

        assert_eq!(presentation.labels, vec!["No microphones found"]);
        assert_eq!(presentation.selected, 0);
        assert!(presentation.subtitle.contains("Refresh"));
        assert!(!presentation.selectable);
    }

    #[test]
    fn microphone_count_uses_live_singular_and_empty_guidance() {
        assert_eq!(
            microphone_count_description(0),
            "No microphones found · Connect one, then refresh"
        );
        assert_eq!(microphone_count_description(1), "1 microphone available");
        assert_eq!(microphone_count_description(3), "3 microphones available");
        assert!(microphone_refresh_failure_description().contains("try again"));
    }

    #[test]
    fn microphone_refresh_failure_toast_offers_an_immediate_retry() {
        if !initialize_adwaita() {
            return;
        }
        let retry_button = gtk4::Button::new();
        let retried = Rc::new(Cell::new(false));
        let retried_on_click = retried.clone();
        retry_button.connect_clicked(move |_| retried_on_click.set(true));

        let toast = microphone_refresh_failure_toast(&retry_button);
        assert_eq!(
            toast.title().as_deref(),
            Some("Could not refresh microphones")
        );
        assert_eq!(toast.button_label().as_deref(), Some("Try again"));
        assert_eq!(toast.priority(), adw::ToastPriority::High);
        toast.emit_by_name::<()>("button-clicked", &[]);
        assert!(retried.get());
    }

    #[test]
    fn custom_mistral_endpoint_override_is_trimmed() {
        assert_eq!(
            normalized_endpoint(
                " https://mistral.example.test/v1/audio/transcriptions ",
                voxkey_ipc::MistralConfig::DEFAULT_ENDPOINT,
            ),
            "https://mistral.example.test/v1/audio/transcriptions"
        );
    }

    #[test]
    fn displayed_default_endpoint_is_stored_as_empty_override() {
        assert!(
            normalized_endpoint(
                voxkey_ipc::MistralConfig::DEFAULT_ENDPOINT,
                voxkey_ipc::MistralConfig::DEFAULT_ENDPOINT,
            )
            .is_empty()
        );
    }

    #[test]
    fn endpoint_candidate_is_normalized_without_credentials_or_mutating_saved_state() {
        let mut saved = voxkey_ipc::TranscriberConfig {
            provider: voxkey_ipc::TranscriberProvider::Mistral,
            ..Default::default()
        };
        saved.mistral.endpoint = "https://saved.example.test/v1".to_string();
        saved.mistral.api_key = "legacy-secret".to_string();
        saved.mistral_realtime.api_key = "other-secret".to_string();

        let (candidate, stored) = EndpointKind::MistralBatch
            .candidate(&saved, " https://new.example.test/v1/audio/transcriptions ")
            .unwrap();

        assert_eq!(stored, "https://new.example.test/v1/audio/transcriptions");
        assert_eq!(candidate.mistral.endpoint, stored);
        assert!(candidate.mistral.api_key.is_empty());
        assert!(candidate.mistral_realtime.api_key.is_empty());
        assert_eq!(saved.mistral.endpoint, "https://saved.example.test/v1");
        assert_eq!(saved.mistral.api_key, "legacy-secret");
    }

    #[test]
    fn blank_parakeet_endpoint_is_rejected_before_a_network_request() {
        let saved = voxkey_ipc::TranscriberConfig::default();

        let error = EndpointKind::ParakeetHttp
            .candidate(&saved, " \t ")
            .unwrap_err();

        assert!(error.contains("server address"), "{error}");
    }

    #[test]
    fn checking_an_unchanged_endpoint_does_not_restart_the_daemon_to_save_it_again() {
        assert!(!endpoint_requires_save(
            "https://speech.example.test/v1",
            "https://speech.example.test/v1",
            false,
            false,
        ));
        assert!(endpoint_requires_save(
            "https://old.example.test/v1",
            "https://new.example.test/v1",
            false,
            false,
        ));
        assert!(endpoint_requires_save(
            "http://192.168.1.132:8000/v1/audio/transcriptions",
            "http://192.168.1.132:8000/v1/audio/transcriptions",
            false,
            true,
        ));
    }

    #[test]
    fn insecure_http_permission_belongs_only_to_parakeet() {
        let mut config = voxkey_ipc::TranscriberConfig::default();

        EndpointKind::MistralBatch.set_insecure_http_allowed(&mut config, true);
        assert!(!config.parakeet.allow_insecure_http);

        EndpointKind::ParakeetHttp.set_insecure_http_allowed(&mut config, true);
        assert!(EndpointKind::ParakeetHttp.insecure_http_allowed(&config));
    }

    #[test]
    fn initial_injection_config_does_not_depend_on_transcriber_json() {
        let (_, injection) =
            parse_initial_config_sections("not valid JSON", r#"{"typing_delay_ms":7}"#);

        assert_eq!(injection.unwrap().typing_delay_ms, 7);
    }

    #[test]
    fn preview_combo_indices_cover_every_shared_enum_value() {
        for mode in [
            voxkey_ipc::PreviewMode::Auto,
            voxkey_ipc::PreviewMode::Always,
            voxkey_ipc::PreviewMode::Never,
        ] {
            assert_eq!(
                preview_mode_from_selected(selected_for_preview_mode(mode)),
                mode
            );
        }
        for strategy in [
            voxkey_ipc::PreviewStrategy::Whole,
            voxkey_ipc::PreviewStrategy::Segmented,
        ] {
            assert_eq!(
                preview_strategy_from_selected(selected_for_preview_strategy(strategy)),
                strategy
            );
        }
    }

    #[test]
    fn preview_presets_round_trip_and_preserve_custom_configs() {
        for preset in [
            PreviewPreset::Automatic,
            PreviewPreset::AlwaysLive,
            PreviewPreset::FinalOnly,
        ] {
            let config = preset.config().expect("named preset has a config");
            assert_eq!(PreviewPreset::from_config(&config), preset);
            assert_eq!(PreviewPreset::from_selected(preset.selected()), preset);
        }

        let custom = voxkey_ipc::PreviewConfig {
            interval_ms: 750,
            ..Default::default()
        };
        assert_eq!(PreviewPreset::from_config(&custom), PreviewPreset::Custom);
        assert!(PreviewPreset::Custom.config().is_none());
    }

    #[test]
    fn always_live_preset_warns_when_it_repeats_network_requests() {
        let transcriber = voxkey_ipc::TranscriberConfig {
            provider: voxkey_ipc::TranscriberProvider::Mistral,
            ..Default::default()
        };

        assert!(
            preview_preset_subtitle(PreviewPreset::AlwaysLive, &transcriber)
                .contains("Repeatedly sends")
        );
        assert!(
            preview_preset_subtitle(PreviewPreset::Automatic, &transcriber)
                .contains("avoids repeated network requests")
        );
    }

    #[test]
    fn daemon_states_are_presented_as_plain_language_actions() {
        let starting = dictation_status_presentation("StartingService", false);
        assert_eq!(starting.title, "Starting Voxkey…");
        assert!(starting.busy);

        let idle = dictation_status_presentation("Idle", false);
        assert_eq!(idle.title, "Ready to dictate");
        assert!(!idle.busy);

        let retry = dictation_status_presentation("Idle", true);
        assert_eq!(retry.title, "Ready to try again");
        assert!(retry.subtitle.contains("issue below"));
        assert_eq!(retry.style, "accent");

        let recording = dictation_status_presentation("Recording", true);
        assert_eq!(recording.title, "Listening…");
        assert!(recording.subtitle.contains("shortcut again"));
        assert!(recording.busy);

        let recovering = dictation_status_presentation("RecoveringSession", true);
        assert_eq!(recovering.style, "warning");
        assert!(!recovering.title.contains("Session"));
    }

    #[test]
    fn desktop_access_action_tracks_request_reset_and_busy_states() {
        let idle = permission_page_presentation(false, true, "Idle");
        assert_eq!(idle.title, "Allow Voxkey to type for you");
        assert_eq!(idle.icon, "preferences-system-privacy-symbolic");
        assert_eq!(idle.action, PermissionPageAction::Request);
        assert!(idle.action_enabled);

        let recovering = permission_page_presentation(false, true, "RecoveringSession");
        assert_eq!(recovering.title, "Restoring desktop access");
        assert!(recovering.description.contains("automatically"));
        assert_eq!(
            recovering.status_subtitle,
            "Restoring desktop access automatically"
        );
        assert_eq!(recovering.icon, "view-refresh-symbolic");
        assert_eq!(recovering.action, PermissionPageAction::None);
        assert!(!recovering.action_enabled);

        let recording = permission_page_presentation(false, true, "Recording");
        assert_eq!(recording.title, "Finish the current dictation first");
        assert_eq!(recording.action, PermissionPageAction::Request);
        assert!(!recording.action_enabled);

        let unavailable = permission_page_presentation(false, false, "Unavailable");
        assert_eq!(unavailable.title, "Start Voxkey first");
        assert_eq!(unavailable.action, PermissionPageAction::None);

        let ready = permission_page_presentation(true, true, "Idle");
        assert_eq!(ready.icon, "object-select-symbolic");
        assert_eq!(ready.action, PermissionPageAction::Reset);
        assert!(ready.action_enabled);

        let busy_ready = permission_page_presentation(true, true, "Recording");
        assert_eq!(busy_ready.action, PermissionPageAction::Reset);
        assert!(!busy_ready.action_enabled);
    }

    #[test]
    fn model_status_offers_only_the_relevant_action() {
        assert_eq!(
            parakeet_model_action_label("parakeet-tdt-0.6b-v2"),
            "Download"
        );
        assert_eq!(
            parakeet_model_action_label("parakeet-tdt-0.6b-v3"),
            "Download"
        );
        assert!(parakeet_model_can_download("parakeet-tdt-0.6b-v2"));
        assert!(parakeet_model_can_download("parakeet-tdt-0.6b-v3"));
        assert!(!parakeet_model_can_download("my-custom-model"));
        assert_eq!(
            parakeet_model_action_label("my-custom-model"),
            "Open model folder"
        );

        let mut config = voxkey_ipc::TranscriberConfig {
            provider: voxkey_ipc::TranscriberProvider::Parakeet,
            ..Default::default()
        };
        assert_eq!(
            local_parakeet_model_name(&config),
            Some(voxkey_ipc::ParakeetConfig::DEFAULT_MODEL)
        );
        config.parakeet.backend = voxkey_ipc::ParakeetBackend::Http;
        assert_eq!(local_parakeet_model_name(&config), None);

        let missing = model_status_presentation("not_downloaded", "parakeet-tdt-0.6b-v3");
        assert_eq!(missing.action, ModelStatusAction::Download);
        assert!(!missing.show_delete);
        assert_eq!(missing.subtitle, "Not downloaded · 670 MB download");

        let custom_missing = model_status_presentation("not_downloaded", "my-custom-model");
        assert_eq!(custom_missing.action, ModelStatusAction::OpenFolder);
        assert_eq!(custom_missing.subtitle, "Custom model files not found");
        assert!(!parakeet_model_folder_icon_visible(
            true,
            custom_missing.action,
        ));
        assert!(parakeet_model_folder_icon_visible(true, missing.action,));
        assert!(!parakeet_model_folder_icon_visible(false, missing.action,));

        let downloading = model_status_presentation("downloading", "parakeet-tdt-0.6b-v3");
        assert!(downloading.show_download_progress);
        assert_eq!(downloading.action, ModelStatusAction::CancelDownload);
        assert!(!downloading.show_delete);

        let cancelling = model_status_presentation("cancelling", "parakeet-tdt-0.6b-v3");
        assert_eq!(cancelling.subtitle, "Cancelling download…");
        assert_eq!(cancelling.action, ModelStatusAction::None);
        assert!(!cancelling.show_download_progress);

        let deleting = model_status_presentation("deleting", "parakeet-tdt-0.6b-v3");
        assert_eq!(deleting.subtitle, "Deleting model…");
        assert!(!deleting.show_download_progress);
        assert_eq!(deleting.action, ModelStatusAction::None);
        assert!(!deleting.show_delete);

        let available = model_status_presentation("available", "parakeet-tdt-0.6b-v3");
        assert!(!available.show_download_progress);
        assert_eq!(available.action, ModelStatusAction::None);
        assert!(available.show_delete);
        assert!(available.subtitle.contains("this computer"));
    }

    #[test]
    fn model_download_progress_is_bounded_to_the_complete_range() {
        assert_eq!(model_download_fraction(0), 0.0);
        assert!((model_download_fraction(37) - 0.37).abs() < f64::EPSILON);
        assert_eq!(model_download_fraction(100), 1.0);
        assert_eq!(model_download_fraction(255), 1.0);
    }

    #[test]
    fn standard_parakeet_models_have_readable_names() {
        assert_eq!(
            parakeet_model_display_name("parakeet-tdt-0.6b-v2"),
            "Parakeet v2"
        );
        assert_eq!(
            parakeet_model_display_name("parakeet-tdt-0.6b-v3"),
            "Parakeet v3"
        );
        assert_eq!(
            parakeet_model_display_name("my-custom-model"),
            "my-custom-model"
        );
        assert_eq!(
            parakeet_model_status_title("parakeet-tdt-0.6b-v2"),
            "Parakeet v2 model"
        );
        assert_eq!(
            parakeet_model_status_title("my-custom-model"),
            "Custom model: my-custom-model"
        );
    }

    #[test]
    fn custom_parakeet_models_are_not_presented_as_version_three() {
        let mut config = voxkey_ipc::TranscriberConfig {
            provider: voxkey_ipc::TranscriberProvider::Parakeet,
            ..Default::default()
        };
        assert_eq!(
            transcriber_choice_presentation(&config),
            TranscriberChoicePresentation {
                selected: choice_for_local_model(voxkey_ipc::ParakeetConfig::DEFAULT_MODEL)
                    .unwrap(),
                show_custom_parakeet: false,
            }
        );

        config.parakeet.model = "company-parakeet-server".to_string();
        assert_eq!(
            transcriber_choice_presentation(&config),
            TranscriberChoicePresentation {
                selected: CUSTOM_PARAKEET_CHOICE,
                show_custom_parakeet: true,
            }
        );
    }

    #[test]
    fn parakeet_http_preview_requires_always_mode() {
        let mut transcriber = voxkey_ipc::TranscriberConfig {
            provider: voxkey_ipc::TranscriberProvider::Parakeet,
            ..Default::default()
        };
        transcriber.parakeet.backend = voxkey_ipc::ParakeetBackend::Http;
        let mut preview = voxkey_ipc::PreviewConfig::default();

        assert!(!preview_controls_are_active(&preview, &transcriber));
        assert!(preview_mode_subtitle(preview.mode, &transcriber).contains("network model"));

        preview.mode = voxkey_ipc::PreviewMode::Always;
        assert!(preview_controls_are_active(&preview, &transcriber));
        assert!(preview_mode_subtitle(preview.mode, &transcriber).contains("active server"));
    }

    #[test]
    fn realtime_provider_explains_that_batch_preview_controls_do_not_apply() {
        let transcriber = voxkey_ipc::TranscriberConfig {
            provider: voxkey_ipc::TranscriberProvider::MistralRealtime,
            ..Default::default()
        };
        let preview = voxkey_ipc::PreviewConfig {
            mode: voxkey_ipc::PreviewMode::Always,
            ..Default::default()
        };

        assert!(!preview_controls_are_active(&preview, &transcriber));
        assert!(preview_mode_subtitle(preview.mode, &transcriber).contains("own live text"));
    }

    #[test]
    fn parakeet_http_backend_has_model_specific_subtitle() {
        let mut config = voxkey_ipc::TranscriberConfig {
            provider: voxkey_ipc::TranscriberProvider::Parakeet,
            ..Default::default()
        };
        config.parakeet.backend = voxkey_ipc::ParakeetBackend::Http;

        assert_eq!(
            transcriber_location_subtitle(&config),
            "Sends each finished recording to your transcription server"
        );
    }

    #[test]
    fn expert_mode_reveals_optional_transcription_controls() {
        let config = voxkey_ipc::TranscriberConfig {
            provider: voxkey_ipc::TranscriberProvider::Mistral,
            ..Default::default()
        };

        let simple = transcriber_visibility(&config, false);
        assert!(simple.api_key);
        assert!(!simple.model_name);
        assert!(!simple.batch_endpoint);

        let expert = transcriber_visibility(&config, true);
        assert!(expert.api_key);
        assert!(expert.model_name);
        assert!(expert.batch_endpoint);

        let whisper = voxkey_ipc::TranscriberConfig::default();
        let simple_whisper = transcriber_visibility(&whisper, false);
        assert!(simple_whisper.whisper_command);
        assert!(simple_whisper.whisper_model);
        assert!(!simple_whisper.whisper_arguments);
        assert!(transcriber_visibility(&whisper, true).whisper_arguments);
    }

    #[test]
    fn simple_whisper_setup_separates_the_model_from_advanced_arguments() {
        let mut config = voxkey_ipc::TranscriberConfig::default();
        config.whisper_cpp.args = vec![
            "--model".to_string(),
            "/models/ggml base.bin".to_string(),
            "{audio_file}".to_string(),
        ];

        assert_eq!(
            whisper_model_path(&config.whisper_cpp.args),
            Some("/models/ggml base.bin")
        );
        assert!(!transcriber_visibility(&config, false).whisper_arguments);

        config
            .whisper_cpp
            .args
            .extend(["--language".to_string(), "en".to_string()]);
        assert!(transcriber_visibility(&config, false).whisper_arguments);
    }

    #[test]
    fn whisper_model_setup_explains_what_the_missing_file_enables() {
        let mut config = voxkey_ipc::TranscriberConfig::default();
        assert_eq!(
            whisper_model_subtitle(&config),
            "Choose a model file to use Whisper"
        );
        assert_eq!(
            whisper_model_subtitle_markup(&config),
            "Choose a model file to use Whisper"
        );

        config.whisper_cpp.args = vec![
            "--model".to_string(),
            "/a-voxkey-<model>-that-does-not-exist-anywhere.bin".to_string(),
        ];
        assert!(whisper_model_subtitle(&config).starts_with("Model file not found:"));
        let markup = whisper_model_subtitle_markup(&config);
        assert!(markup.starts_with("Model file not found: <span font_family=\"monospace\">"));
        assert!(markup.contains("&lt;model&gt;"));

        config.whisper_cpp.args.clear();
        config.whisper_cpp.command = "/opt/custom-transcriber".to_string();
        assert!(whisper_model_subtitle(&config).contains("Whisper program"));
    }

    #[test]
    fn choosing_a_whisper_model_preserves_other_arguments() {
        let args = vec![
            "-m".to_string(),
            "/models/old.bin".to_string(),
            "--language".to_string(),
            "es".to_string(),
        ];
        assert_eq!(
            whisper_args_with_model(&args, "/models/new model.bin"),
            ["-m", "/models/new model.bin", "--language", "es"]
        );
        assert_eq!(
            whisper_args_with_model(&["--model=old.bin".to_string()], "new.bin"),
            ["--model=new.bin"]
        );
        assert_eq!(
            whisper_args_with_model(&[], "new.bin"),
            ["--model", "new.bin"]
        );
    }

    #[test]
    fn whisper_picker_rejects_non_executable_selections() {
        let current_executable = std::env::current_exe().expect("test executable has a path");
        assert_eq!(whisper_executable_error(&current_executable), None);
        assert_eq!(
            whisper_executable_error(Path::new("/")),
            Some("Choose a file, not a folder")
        );
        assert_eq!(
            whisper_executable_error(Path::new("/a-voxkey-file-that-does-not-exist-anywhere")),
            Some("The selected file could not be opened")
        );
        assert_eq!(whisper_model_file_error(&current_executable), None);
        assert_eq!(
            whisper_model_file_error(Path::new("/")),
            Some("Choose a model file, not a folder")
        );
    }

    #[test]
    fn whisper_command_readiness_uses_the_executable_search_path() {
        assert!(!whisper_command_available(""));
        assert!(!whisper_command_available(" /bin/sh "));
        assert!(!whisper_command_available(
            "voxkey-command-that-does-not-exist"
        ));

        let current_executable = std::env::current_exe().unwrap();
        assert!(whisper_command_available(
            current_executable.to_str().unwrap()
        ));
    }

    #[test]
    fn customized_transcription_controls_remain_visible_outside_expert_mode() {
        let mut mistral = voxkey_ipc::TranscriberConfig {
            provider: voxkey_ipc::TranscriberProvider::Mistral,
            ..Default::default()
        };
        mistral.mistral.model = "custom-model".to_string();
        mistral.mistral.endpoint = "https://speech.example.test/v1".to_string();
        let mistral_visibility = transcriber_visibility(&mistral, false);
        assert!(mistral_visibility.model_name);
        assert!(mistral_visibility.batch_endpoint);

        let mut whisper = voxkey_ipc::TranscriberConfig::default();
        whisper.whisper_cpp.args = vec!["--language".to_string(), "en".to_string()];
        assert!(transcriber_visibility(&whisper, false).whisper_arguments);

        let mut parakeet = voxkey_ipc::TranscriberConfig {
            provider: voxkey_ipc::TranscriberProvider::Parakeet,
            ..Default::default()
        };
        parakeet.parakeet.backend = voxkey_ipc::ParakeetBackend::Http;
        let server_visibility = transcriber_visibility(&parakeet, false);
        assert!(server_visibility.parakeet_backend);
        assert!(server_visibility.parakeet_endpoint);
        assert!(server_visibility.api_key);
        assert!(!server_visibility.execution_provider);
        assert!(!server_visibility.model_status);
    }

    #[test]
    fn api_key_status_matches_the_active_provider() {
        let realtime = voxkey_ipc::TranscriberConfig {
            provider: voxkey_ipc::TranscriberProvider::MistralRealtime,
            ..Default::default()
        };
        let mut server = voxkey_ipc::TranscriberConfig {
            provider: voxkey_ipc::TranscriberProvider::Parakeet,
            ..Default::default()
        };
        server.parakeet.backend = voxkey_ipc::ParakeetBackend::Http;
        let local_model = voxkey_ipc::TranscriberConfig {
            provider: voxkey_ipc::TranscriberProvider::Parakeet,
            ..Default::default()
        };
        assert_eq!(
            api_key_provider_name(&voxkey_ipc::TranscriberProvider::MistralRealtime),
            Some("Mistral Realtime")
        );
        assert_eq!(
            api_key_status_for_provider(
                voxkey_ipc::API_KEY_SERVICE_MISTRAL_REALTIME,
                true,
                &realtime,
            ),
            Some(true)
        );
        assert_eq!(
            api_key_status_for_provider(voxkey_ipc::API_KEY_SERVICE_MISTRAL, true, &realtime,),
            None
        );
        assert_eq!(
            api_key_status_for_provider(voxkey_ipc::API_KEY_SERVICE_MODEL_SERVER, true, &server,),
            Some(true)
        );
        assert_eq!(
            api_key_status_for_provider(
                voxkey_ipc::API_KEY_SERVICE_MODEL_SERVER,
                false,
                &local_model,
            ),
            None
        );
    }

    #[test]
    fn api_key_editor_names_replacement_only_when_a_key_exists() {
        assert_eq!(api_key_entry_title(None), "API key");
        assert_eq!(api_key_entry_title(Some(false)), "API key");
        assert_eq!(api_key_entry_title(Some(true)), "Replace API key");
    }

    #[test]
    fn api_key_save_confirmation_names_the_active_service() {
        assert_eq!(
            api_key_saved_message(&voxkey_ipc::TranscriberProvider::Mistral),
            Some("Mistral API key saved".to_string())
        );
        assert_eq!(
            api_key_saved_message(&voxkey_ipc::TranscriberProvider::MistralRealtime),
            Some("Mistral Realtime API key saved".to_string())
        );
        assert_eq!(
            api_key_saved_message(&voxkey_ipc::TranscriberProvider::Parakeet),
            Some("Model server API key saved".to_string())
        );
    }

    #[test]
    fn api_key_operations_ignore_provider_switches_and_later_requests() {
        let mistral = voxkey_ipc::TranscriberConfig {
            provider: voxkey_ipc::TranscriberProvider::Mistral,
            ..Default::default()
        };
        let realtime = voxkey_ipc::TranscriberConfig {
            provider: voxkey_ipc::TranscriberProvider::MistralRealtime,
            ..Default::default()
        };
        assert!(api_key_operation_is_current(
            voxkey_ipc::API_KEY_SERVICE_MISTRAL,
            &mistral,
            7,
            7,
        ));
        assert!(!api_key_operation_is_current(
            voxkey_ipc::API_KEY_SERVICE_MISTRAL,
            &realtime,
            7,
            7,
        ));
        assert!(!api_key_operation_is_current(
            voxkey_ipc::API_KEY_SERVICE_MISTRAL,
            &mistral,
            7,
            8,
        ));
    }

    #[test]
    fn api_key_status_reply_ignores_stale_requests() {
        if !initialize_adwaita() {
            return;
        }
        let status_row = adw::ActionRow::builder().title("API key").build();
        let entry_row = adw::PasswordEntryRow::builder()
            .title("New API key")
            .build();
        let remove_button = gtk4::Button::new();
        let state = Rc::new(RefCell::new(voxkey_ipc::TranscriberConfig {
            provider: voxkey_ipc::TranscriberProvider::MistralRealtime,
            ..Default::default()
        }));
        let stored_state = Rc::new(Cell::new(None));

        apply_api_key_status(
            voxkey_ipc::API_KEY_SERVICE_MISTRAL_REALTIME,
            true,
            1,
            2,
            &status_row,
            &entry_row,
            &remove_button,
            &stored_state,
            &state,
        );
        assert_eq!(status_row.subtitle().as_deref(), Some(""));
        assert_eq!(stored_state.get(), None);

        apply_api_key_status(
            voxkey_ipc::API_KEY_SERVICE_MISTRAL_REALTIME,
            true,
            2,
            2,
            &status_row,
            &entry_row,
            &remove_button,
            &stored_state,
            &state,
        );
        assert_eq!(
            status_row.subtitle().as_deref(),
            Some("Key stored. Enter a new key below to replace it.")
        );
        assert_eq!(stored_state.get(), Some(true));
        assert!(remove_button.is_visible());
    }

    #[test]
    fn missing_api_key_hides_removal_and_leaves_the_entry_empty() {
        if !initialize_adwaita() {
            return;
        }
        let status_row = adw::ActionRow::builder().title("API key").build();
        let entry_row = adw::PasswordEntryRow::builder()
            .title("New API key")
            .build();
        let remove_button = gtk4::Button::new();

        update_api_key_row_state(&status_row, &entry_row, &remove_button, Some(false));

        assert_eq!(
            status_row.subtitle().as_deref(),
            Some("No key stored. Enter a key below to use cloud transcription.")
        );
        assert!(!remove_button.is_visible());
        assert!(entry_row.is_sensitive());
        assert!(entry_row.text().is_empty());
    }

    #[test]
    fn daemon_echo_only_replaces_an_entry_that_is_still_clean() {
        assert!(should_replace_entry(true, "saved value", "saved value"));
        assert!(!should_replace_entry(
            true,
            "user is still typing",
            "saved value"
        ));
        assert!(should_replace_entry(
            false,
            "empty startup widget",
            "saved value"
        ));
    }

    #[test]
    fn whitespace_only_api_key_input_requests_a_clear() {
        assert!(normalized_api_key_input("  \t\n").is_empty());
        assert_eq!(normalized_api_key_input("  sk-live-key \n"), "sk-live-key");
    }

    #[test]
    fn whisper_arguments_with_spaces_round_trip_through_the_editor() {
        let args = vec![
            "-m".to_string(),
            "/tmp/model files/model.bin".to_string(),
            "--prompt".to_string(),
            "say \"Voxkey\"".to_string(),
            String::new(),
            "can't".to_string(),
        ];

        let displayed = format_whisper_args(&args);
        assert_eq!(parse_whisper_args(&displayed).unwrap(), args);
    }

    #[test]
    fn quoted_whisper_argument_preserves_backslashes_before_ordinary_characters() {
        assert_eq!(
            parse_whisper_args(r#""C:\models\ggml.bin""#).unwrap(),
            vec![r"C:\models\ggml.bin"]
        );
    }

    #[test]
    fn shortcut_capture_preserves_meta_and_hyper_modifiers() {
        assert_eq!(
            key_to_trigger(gdk::Key::d, gdk::ModifierType::META_MASK),
            Some("<Meta>d".to_string())
        );
        assert_eq!(
            key_to_trigger(gdk::Key::d, gdk::ModifierType::HYPER_MASK),
            Some("<Hyper>d".to_string())
        );
    }

    #[test]
    fn shortcut_capture_dialog_has_room_for_its_status_page() {
        const {
            assert!(SHORTCUT_DIALOG_DEFAULT_WIDTH >= 420);
            assert!(SHORTCUT_DIALOG_DEFAULT_HEIGHT >= 400);
        }
    }

    #[test]
    fn shell_extension_restart_notice_explains_the_disruptive_step() {
        assert!(SHELL_EXTENSION_RESTART_NOTICE.contains("Save your work"));
        assert!(SHELL_EXTENSION_RESTART_NOTICE.contains("log out and back in"));
        assert!(SHELL_EXTENSION_RESTART_NOTICE.contains("GNOME Shell"));
        assert_eq!(SHELL_EXTENSION_RESTART_ACTION, "Log Out…");
    }

    #[test]
    fn command_failure_toasts_strip_dbus_implementation_prefixes() {
        assert_eq!(
            sanitize_command_failure(
                "GDBus.Error:org.freedesktop.DBus.Error.Failed: Microphone is busy"
            ),
            "Microphone is busy"
        );
        assert_eq!(
            sanitize_command_failure("org.example.Failed: Choose a model file"),
            "Choose a model file"
        );
        assert_eq!(
            sanitize_command_failure("   "),
            "Try again, or check General for details."
        );
    }

    #[test]
    fn shortcut_subtitle_prefers_the_effective_desktop_binding() {
        assert_eq!(shortcut_subtitle("  F13  ", true), "F13");
        assert_eq!(
            shortcut_subtitle("Press <Alt><Super>d", true),
            "Press Alt + Super + D"
        );
        assert_eq!(
            shortcut_subtitle("<Primary><Shift>space", true),
            "Ctrl + Shift + Space"
        );
        assert_eq!(
            shortcut_subtitle("", true),
            "Choose the shortcut you want to use for dictation"
        );
        assert_eq!(
            shortcut_subtitle("", false),
            "Available after Voxkey starts"
        );
    }

    #[test]
    fn shortcut_capture_defers_single_key_safety_to_shared_validation() {
        assert_eq!(
            key_to_trigger(gdk::Key::F13, gdk::ModifierType::empty()),
            Some("F13".to_string())
        );
        assert!(voxkey_ipc::validate_shortcut_trigger("F13").is_ok());
        assert_eq!(
            key_to_trigger(gdk::Key::Dictate, gdk::ModifierType::empty()),
            Some("XF86Dictate".to_string())
        );
        assert!(voxkey_ipc::validate_shortcut_trigger("XF86Dictate").is_ok());

        for key in [gdk::Key::d, gdk::Key::space, gdk::Key::Return, gdk::Key::_1] {
            let trigger = key_to_trigger(key, gdk::ModifierType::empty()).unwrap();
            assert!(voxkey_ipc::validate_shortcut_trigger(&trigger).is_err());
        }
    }

    #[test]
    fn shortcut_capture_uses_daemon_validation_for_gnome_conflicts() {
        let trigger = key_to_trigger(gdk::Key::space, gdk::ModifierType::SUPER_MASK).unwrap();
        assert_eq!(trigger, "<Super>space");
        assert!(voxkey_ipc::validate_shortcut_trigger(&trigger).is_err());
    }

    #[test]
    fn modified_escape_is_available_as_a_shortcut() {
        assert!(should_cancel_shortcut_capture(
            gdk::Key::Escape,
            gdk::ModifierType::empty()
        ));
        assert!(!should_cancel_shortcut_capture(
            gdk::Key::Escape,
            gdk::ModifierType::SUPER_MASK
        ));
        assert_eq!(
            key_to_trigger(gdk::Key::Escape, gdk::ModifierType::SUPER_MASK),
            Some("<Super>Escape".to_string())
        );
    }

    #[test]
    fn invalid_shortcut_guidance_keeps_the_capture_dialog_actionable() {
        let description = shortcut_validation_description("That shortcut is already in use.");
        assert!(description.contains("already in use"));
        assert!(description.contains("Press another shortcut"));
        assert!(description.contains("Escape"));
        assert!(!description.contains(".."));

        let unpunctuated = shortcut_validation_description(
            "Only function and dedicated hardware keys can be used alone",
        );
        assert!(unpunctuated.contains("used alone. Press another shortcut"));
    }

    #[test]
    fn desktop_shortcut_rejections_keep_the_useful_reason() {
        let description = shortcut_save_failure_description(
            "org.example.Failed: Desktop rejected shortcut: That key is already reserved.",
        );
        assert!(description.starts_with("That key is already reserved."));
        assert!(description.contains("Press another shortcut"));

        let generic = shortcut_save_failure_description("D-Bus connection closed");
        assert!(generic.starts_with("Voxkey could not save that shortcut."));
    }
}
