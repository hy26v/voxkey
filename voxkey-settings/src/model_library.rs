// ABOUTME: Renders the installable local speech-model shelf in Voxkey settings.
// ABOUTME: Owns per-model install, progress, deletion, source, and license controls.

use std::collections::HashMap;

use adw::prelude::*;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;

use crate::daemon_client::{DaemonCommand, DaemonHandle};

#[derive(Clone)]
struct ModelRow {
    selected: gtk4::Image,
    status: adw::ActionRow,
    status_icon: gtk4::Image,
    progress: gtk4::ProgressBar,
    action: gtk4::Button,
}

impl ModelRow {
    fn set_status(&self, status: &str) {
        self.progress.set_visible(false);
        self.action.remove_css_class("destructive-action");
        self.action.remove_css_class("suggested-action");
        match status {
            "available" => {
                self.status.set_title("Ready on this computer");
                self.status
                    .set_subtitle("Verified and available for local dictation");
                self.status_icon
                    .set_icon_name(Some("object-select-symbolic"));
                self.status_icon.add_css_class("success");
                self.action.set_label("Delete");
                self.action.add_css_class("destructive-action");
                self.action.set_sensitive(true);
            }
            "downloading" => {
                self.status.set_title("Downloading…");
                self.status
                    .set_subtitle("Voxkey will verify every file before using it");
                self.status_icon
                    .set_icon_name(Some("folder-download-symbolic"));
                self.status_icon.remove_css_class("success");
                self.progress.set_fraction(0.0);
                self.progress.set_visible(true);
                self.action.set_label("Downloading");
                self.action.set_sensitive(false);
            }
            "deleting" => {
                self.status.set_title("Deleting model…");
                self.status.set_subtitle("Removing downloaded files");
                self.status_icon.set_icon_name(Some("user-trash-symbolic"));
                self.status_icon.remove_css_class("success");
                self.action.set_label("Deleting");
                self.action.set_sensitive(false);
            }
            "checking" => {
                self.status.set_title("Checking installation…");
                self.status
                    .set_subtitle("Verifying the downloaded model files");
                self.status_icon
                    .set_icon_name(Some("content-loading-symbolic"));
                self.status_icon.remove_css_class("success");
                self.action.set_label("Checking");
                self.action.set_sensitive(false);
            }
            _ => {
                self.status.set_title("Not downloaded");
                self.status
                    .set_subtitle("Download once, then dictate without a network connection");
                self.status_icon
                    .set_icon_name(Some("folder-download-symbolic"));
                self.status_icon.remove_css_class("success");
                self.action.set_label("Download");
                self.action.add_css_class("suggested-action");
                self.action.set_sensitive(true);
            }
        }
    }

    fn set_progress(&self, percent: u8) {
        let percent = percent.min(100);
        self.set_status(if percent == 100 {
            "available"
        } else {
            "downloading"
        });
        if percent < 100 {
            self.progress.set_fraction(f64::from(percent) / 100.0);
            self.progress
                .set_tooltip_text(Some(&format!("Model download: {percent}%")));
        }
    }
}

pub struct ModelLibrary {
    pub group: adw::PreferencesGroup,
    rows: HashMap<&'static str, ModelRow>,
}

impl ModelLibrary {
    pub fn new(handle: &DaemonHandle, toast_overlay: &adw::ToastOverlay) -> Self {
        let group = adw::PreferencesGroup::builder()
            .title("Model library")
            .description(
                "Downloadable speech models Voxkey verifies and runs without sending audio away",
            )
            .build();
        let mut rows = HashMap::new();

        for model in voxkey_ipc::model_library::LOCAL_MODELS {
            let row = adw::ExpanderRow::builder()
                .title(model.name)
                .subtitle(model.facts())
                .subtitle_lines(2)
                .build();
            let family_icon = gtk4::Image::from_icon_name(match model.runtime {
                voxkey_ipc::model_library::LocalModelRuntime::OnlineTransducer => {
                    "audio-input-microphone-symbolic"
                }
                voxkey_ipc::model_library::LocalModelRuntime::OfflineTransducer => {
                    "document-open-recent-symbolic"
                }
            });
            family_icon.add_css_class("accent");
            row.add_prefix(&family_icon);

            let selected = gtk4::Image::from_icon_name("object-select-symbolic");
            selected.set_tooltip_text(Some("Selected transcription model"));
            selected.add_css_class("success");
            selected.set_visible(false);
            row.add_suffix(&selected);

            if let Some(badge) = model.badge {
                let badge = gtk4::Label::new(Some(badge));
                badge.add_css_class("caption");
                badge.add_css_class("accent");
                badge.set_valign(gtk4::Align::Center);
                row.add_suffix(&badge);
            }

            let details = adw::ActionRow::builder()
                .title(model.description)
                .subtitle(format!(
                    "{} · Released {} · {}",
                    model.family,
                    model.released,
                    model.runtime.label()
                ))
                .title_lines(3)
                .subtitle_lines(2)
                .build();
            let source = gtk4::LinkButton::builder()
                .label("Model card")
                .uri(model.source_url)
                .valign(gtk4::Align::Center)
                .build();
            source.add_css_class("flat");
            details.add_suffix(&source);
            let license = gtk4::LinkButton::builder()
                .label("License")
                .uri(model.license_url)
                .valign(gtk4::Align::Center)
                .build();
            license.add_css_class("flat");
            details.add_suffix(&license);
            row.add_row(&details);

            let status = adw::ActionRow::builder()
                .title("Checking installation…")
                .subtitle("Verifying the downloaded model files")
                .subtitle_lines(2)
                .build();
            let status_icon = gtk4::Image::from_icon_name("content-loading-symbolic");
            status.add_prefix(&status_icon);
            let progress = gtk4::ProgressBar::builder()
                .width_request(120)
                .valign(gtk4::Align::Center)
                .visible(false)
                .build();
            status.add_suffix(&progress);
            let action = gtk4::Button::with_label("Checking");
            action.set_valign(gtk4::Align::Center);
            action.set_sensitive(false);
            status.add_suffix(&action);
            row.add_row(&status);
            group.add(&row);

            let widgets = ModelRow {
                selected,
                status,
                status_icon,
                progress,
                action: action.clone(),
            };
            let action_widgets = widgets.clone();
            let handle = handle.clone();
            let toast_overlay = toast_overlay.clone();
            action.connect_clicked(move |button| {
                if button.label().as_deref() == Some("Delete") {
                    let dialog = adw::AlertDialog::builder()
                        .heading(format!("Delete {}?", model.name))
                        .heading_use_markup(false)
                        .body("The model will need to be downloaded again before local transcription can use it.")
                        .build();
                    dialog.add_response("cancel", "Cancel");
                    dialog.add_response("delete", "Delete");
                    dialog.set_response_appearance(
                        "delete",
                        adw::ResponseAppearance::Destructive,
                    );
                    dialog.set_default_response(Some("cancel"));
                    dialog.set_close_response("cancel");
                    let handle = handle.clone();
                    let widgets = action_widgets.clone();
                    let toast_overlay = toast_overlay.clone();
                    dialog.connect_response(None, move |_, response| {
                        if response != "delete" {
                            return;
                        }
                        widgets.set_status("deleting");
                        let completion = handle.send(DaemonCommand::DeleteModel(model.id.to_string()));
                        let widgets = widgets.clone();
                        let toast_overlay = toast_overlay.clone();
                        glib::spawn_future_local(async move {
                            if completion.wait().await.is_ok() {
                                widgets.set_status("not_downloaded");
                                toast_overlay.add_toast(adw::Toast::new(&format!(
                                    "{} deleted",
                                    model.name
                                )));
                            } else {
                                widgets.set_status("available");
                            }
                        });
                    });
                    dialog.present(Some(&button.root().expect("model action must belong to the window")));
                } else {
                    action_widgets.set_status("downloading");
                    handle.send(DaemonCommand::DownloadModel(model.id.to_string()));
                }
            });

            rows.insert(model.id, widgets);
        }

        Self { group, rows }
    }

    pub fn request_statuses(&self, handle: &DaemonHandle) {
        for model in voxkey_ipc::model_library::LOCAL_MODELS {
            self.set_status(model.id, "checking");
            handle.send(DaemonCommand::ModelStatus(model.id.to_string()));
        }
    }

    pub fn set_status(&self, model_id: &str, status: &str) {
        if let Some(row) = self.rows.get(model_id) {
            row.set_status(status);
        }
    }

    pub fn set_progress(&self, model_id: &str, percent: u8) {
        if let Some(row) = self.rows.get(model_id) {
            row.set_progress(percent);
        }
    }

    pub fn set_selected(&self, model_id: Option<&str>) {
        for (id, row) in &self.rows {
            row.selected.set_visible(model_id == Some(*id));
        }
    }
}
