// ABOUTME: Builds searchable History for transcripts and recoverable recordings.
// ABOUTME: Offers copy, open, retry, delete, and clear actions for saved entries.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk4::prelude::*;
use libadwaita as adw;

use crate::{
    daemon_client::{DaemonCommand, DaemonHandle},
    gui_settings,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HistoryOutcomePresentation {
    icon: &'static str,
    style: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryFilter {
    All,
    Completed,
    NeedsAttention,
    Cancelled,
}

impl HistoryFilter {
    fn key(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Completed => "completed",
            Self::NeedsAttention => "needs-attention",
            Self::Cancelled => "cancelled",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "All dictations",
            Self::Completed => "Completed",
            Self::NeedsAttention => "Needs attention",
            Self::Cancelled => "Cancelled",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        match key {
            "all" => Some(Self::All),
            "completed" => Some(Self::Completed),
            "needs-attention" => Some(Self::NeedsAttention),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

fn update_history_filter_button(button: &gtk4::MenuButton, filter: HistoryFilter) {
    let accessible_label = if filter == HistoryFilter::All {
        "Filter dictations".to_string()
    } else {
        format!("Filter dictations: {}", filter.label())
    };
    button.set_tooltip_text(Some(&accessible_label));
    button.update_property(&[gtk4::accessible::Property::Label(&accessible_label)]);
    if filter == HistoryFilter::All {
        button.remove_css_class("accent");
    } else {
        button.add_css_class("accent");
    }
}

pub struct HistoryPage {
    pub widget: gtk4::Widget,
    pub apply_json: Rc<dyn Fn(&str)>,
    pub empty_action: gtk4::Button,
    pub search_entry: gtk4::SearchEntry,
    pub copy_latest_action: gtk4::gio::SimpleAction,
    pub set_actions_available: Rc<dyn Fn(bool)>,
}

pub fn build_history_page(handle: &DaemonHandle, toast_overlay: &adw::ToastOverlay) -> HistoryPage {
    let entries = Rc::new(RefCell::new(Vec::<voxkey_ipc::HistoryEntry>::new()));
    let rows = Rc::new(RefCell::new(Vec::<adw::ActionRow>::new()));
    let retry_buttons = Rc::new(RefCell::new(Vec::<gtk4::Button>::new()));
    let actions_available = Rc::new(Cell::new(false));
    let initial_filter =
        HistoryFilter::from_key(&gui_settings::load_history_filter()).unwrap_or(HistoryFilter::All);
    let active_filter = Rc::new(Cell::new(initial_filter));
    let latest_copy_text = Rc::new(RefCell::new(None::<String>));
    let copy_latest_action = gtk4::gio::SimpleAction::new("copy-latest", None);
    copy_latest_action.set_enabled(false);
    {
        let latest_copy_text = latest_copy_text.clone();
        let toast_overlay = toast_overlay.clone();
        copy_latest_action.connect_activate(move |_, _| {
            let Some(text) = latest_copy_text.borrow().clone() else {
                return;
            };
            if let Some(display) = gtk4::gdk::Display::default() {
                display.clipboard().set_text(&text);
                toast_overlay.add_toast(adw::Toast::new("Latest transcription copied"));
            }
        });
    }

    let page = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

    let search_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    search_bar.set_margin_top(18);
    search_bar.set_margin_bottom(12);
    search_bar.set_margin_start(24);
    search_bar.set_margin_end(24);

    let search = gtk4::SearchEntry::builder()
        .placeholder_text("Search dictations")
        .hexpand(true)
        .build();
    search.set_key_capture_widget(Some(&page));
    let search_keys = gtk4::EventControllerKey::new();
    {
        let search = search.clone();
        search_keys.connect_key_pressed(move |_, key, _, _| {
            if history_search_should_clear(key, &search.text()) {
                search.set_text("");
                return gtk4::glib::Propagation::Stop;
            }
            gtk4::glib::Propagation::Proceed
        });
    }
    search.add_controller(search_keys);
    search_bar.append(&search);

    let filter_menu = gtk4::gio::Menu::new();
    for filter in [
        HistoryFilter::All,
        HistoryFilter::Completed,
        HistoryFilter::NeedsAttention,
        HistoryFilter::Cancelled,
    ] {
        let item = gtk4::gio::MenuItem::new(Some(filter.label()), None);
        item.set_action_and_target_value(Some("history.filter"), Some(&filter.key().to_variant()));
        filter_menu.append_item(&item);
    }
    let filter_button = gtk4::MenuButton::builder()
        .icon_name("view-filter-symbolic")
        .menu_model(&filter_menu)
        .tooltip_text("Filter dictations")
        .valign(gtk4::Align::Center)
        .build();
    filter_button.add_css_class("flat");
    update_history_filter_button(&filter_button, initial_filter);
    let filter_action = gtk4::gio::SimpleAction::new_stateful(
        "filter",
        Some(gtk4::glib::VariantTy::STRING),
        &initial_filter.key().to_variant(),
    );
    let filter_actions = gtk4::gio::SimpleActionGroup::new();
    filter_actions.add_action(&filter_action);
    filter_button.insert_action_group("history", Some(&filter_actions));
    search_bar.append(&filter_button);

    let clear_button = gtk4::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Delete all saved transcripts and failed recordings")
        .valign(gtk4::Align::Center)
        .build();
    clear_button.add_css_class("flat");
    clear_button.add_css_class("destructive-action");
    clear_button.update_property(&[gtk4::accessible::Property::Label("Clear history")]);
    search_bar.append(&clear_button);
    page.append(&search_bar);

    let view_stack = gtk4::Stack::builder()
        .transition_type(gtk4::StackTransitionType::Crossfade)
        .vexpand(true)
        .build();

    let empty_action = gtk4::Button::with_label("Open General");
    empty_action.add_css_class("suggested-action");
    empty_action.set_halign(gtk4::Align::Center);
    let clear_search_action = gtk4::Button::with_label("Clear search");
    clear_search_action.set_halign(gtk4::Align::Center);
    clear_search_action.set_visible(false);
    {
        let search = search.clone();
        let filter_action = filter_action.clone();
        clear_search_action.connect_clicked(move |_| {
            search.set_text("");
            filter_action.activate(Some(&HistoryFilter::All.key().to_variant()));
            search.grab_focus();
        });
    }
    let empty_actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    empty_actions.set_halign(gtk4::Align::Center);
    empty_actions.append(&empty_action);
    empty_actions.append(&clear_search_action);
    let empty = adw::StatusPage::builder()
        .icon_name("document-open-recent-symbolic")
        .title("No saved dictations")
        .description(
            "Use your keyboard shortcut to dictate in any app. Completed transcripts and recoverable failures will appear here.",
        )
        .child(&empty_actions)
        .build();
    view_stack.add_named(&empty, Some("empty"));

    let scrolled = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .build();
    let clamp = adw::Clamp::builder()
        .maximum_size(760)
        .margin_top(6)
        .margin_bottom(24)
        .margin_start(18)
        .margin_end(18)
        .build();
    let group = adw::PreferencesGroup::builder()
        .title("Recent dictations")
        .description("Saved locally on this computer")
        .build();
    clamp.set_child(Some(&group));
    scrolled.set_child(Some(&clamp));
    view_stack.add_named(&scrolled, Some("list"));
    page.append(&view_stack);

    let rebuild: Rc<dyn Fn()> = {
        let entries = entries.clone();
        let rows = rows.clone();
        let retry_buttons = retry_buttons.clone();
        let actions_available = actions_available.clone();
        let active_filter = active_filter.clone();
        let latest_copy_text = latest_copy_text.clone();
        let copy_latest_action = copy_latest_action.clone();
        let group = group.clone();
        let search = search.clone();
        let search_bar = search_bar.clone();
        let view_stack = view_stack.clone();
        let empty = empty.clone();
        let empty_action = empty_action.clone();
        let clear_search_action = clear_search_action.clone();
        let clear_button = clear_button.clone();
        let handle = handle.clone();
        let toast_overlay = toast_overlay.clone();
        Rc::new(move || {
            for row in rows.borrow_mut().drain(..) {
                group.remove(&row);
            }
            retry_buttons.borrow_mut().clear();

            let query = search.text().trim().to_lowercase();
            let filter = active_filter.get();
            let filtered: Vec<_> = entries
                .borrow()
                .iter()
                .filter(|entry| {
                    history_matches_filter(entry, filter) && history_matches_query(entry, &query)
                })
                .cloned()
                .collect();

            let total = entries.borrow().len();
            let latest_text = latest_history_text(&entries.borrow());
            copy_latest_action.set_enabled(latest_text.is_some());
            *latest_copy_text.borrow_mut() = latest_text;
            let clear_label = clear_history_action_label(total);
            clear_button.set_tooltip_text(Some(&clear_label));
            clear_button.update_property(&[gtk4::accessible::Property::Label(&clear_label)]);
            let filtering = !query.is_empty() || filter != HistoryFilter::All;
            search_bar.set_visible(total > 0);
            group.set_title(history_group_title(&query, filter));
            group.set_description(Some(&history_count_description(
                total,
                filtered.len(),
                filtering,
            )));

            clear_button.set_sensitive(total > 0 && actions_available.get());
            if filtered.is_empty() {
                if entries.borrow().is_empty() {
                    empty.set_title("No saved dictations");
                    empty.set_description(Some(
                        "Use your keyboard shortcut to dictate in any app. Completed transcripts and recoverable failures will appear here.",
                    ));
                    empty_action.set_visible(true);
                    clear_search_action.set_visible(false);
                } else {
                    empty.set_title("No matches");
                    empty.set_description(Some("Try a different search or filter"));
                    empty_action.set_visible(false);
                    clear_search_action.set_label("Clear filters");
                    clear_search_action.set_visible(true);
                }
                view_stack.set_visible_child_name("empty");
                return;
            }

            view_stack.set_visible_child_name("list");
            for entry in filtered {
                let row = adw::ActionRow::builder()
                    .title(history_title(&entry))
                    .subtitle(format_history_subtitle(
                        entry.recorded_at_unix_ms,
                        &entry.provider,
                        entry.outcome,
                        entry.pending_insertion.as_deref(),
                        entry.error.as_deref(),
                    ))
                    .title_lines(2)
                    .subtitle_lines(2)
                    .activatable(true)
                    .build();
                row.set_tooltip_text(Some("View dictation details"));
                row.update_property(&[gtk4::accessible::Property::Description(
                    "Activate to view the full dictation details",
                )]);
                let outcome = history_outcome_presentation(
                    entry.outcome,
                    entry
                        .pending_insertion
                        .as_deref()
                        .is_some_and(|pending| !pending.is_empty()),
                );
                let outcome_icon = gtk4::Image::from_icon_name(outcome.icon);
                outcome_icon.add_css_class(outcome.style);
                outcome_icon.set_accessible_role(gtk4::AccessibleRole::Presentation);
                row.add_prefix(&outcome_icon);

                {
                    let entry = entry.clone();
                    let toast_overlay = toast_overlay.clone();
                    let handle = handle.clone();
                    let actions_available = actions_available.clone();
                    row.connect_activated(move |row| {
                        let dialog = build_history_details_dialog(
                            &entry,
                            &toast_overlay,
                            &handle,
                            actions_available.get(),
                        );
                        if let Some(root) = row.root() {
                            dialog.present(Some(&root));
                        }
                    });
                }

                if history_can_retry(&entry) {
                    let retry = gtk4::Button::with_label("Retry");
                    retry.add_css_class("flat");
                    retry.set_valign(gtk4::Align::Center);
                    retry.set_tooltip_text(Some("Retry transcription"));
                    let accessible_label = history_action_label("Retry transcription", &entry);
                    retry.update_property(&[gtk4::accessible::Property::Label(&accessible_label)]);
                    retry.set_sensitive(actions_available.get());
                    {
                        let handle = handle.clone();
                        let toast_overlay = toast_overlay.clone();
                        let actions_available = actions_available.clone();
                        retry.connect_clicked(move |button| {
                            button.set_label("Retrying…");
                            button.set_tooltip_text(Some("Retrying transcription"));
                            button.set_sensitive(false);
                            let completion =
                                handle.send(DaemonCommand::RetryHistoryEntry(entry.id));
                            let toast_overlay = toast_overlay.clone();
                            let button = button.clone();
                            let actions_available = actions_available.clone();
                            gtk4::glib::spawn_future_local(async move {
                                if completion.wait().await.is_ok() {
                                    toast_overlay
                                        .add_toast(adw::Toast::new("Retrying saved recording…"));
                                } else {
                                    button.set_label("Retry");
                                    button.set_tooltip_text(Some("Retry transcription"));
                                    button.set_sensitive(actions_available.get());
                                }
                            });
                        });
                    }
                    row.add_suffix(&retry);
                    retry_buttons.borrow_mut().push(retry);
                }

                if !entry.text.is_empty() {
                    let copy = gtk4::Button::from_icon_name("edit-copy-symbolic");
                    copy.add_css_class("flat");
                    copy.set_valign(gtk4::Align::Center);
                    copy.set_tooltip_text(Some("Copy transcription"));
                    let accessible_label = history_action_label("Copy transcription", &entry);
                    copy.update_property(&[gtk4::accessible::Property::Label(&accessible_label)]);
                    let text = entry.text.clone();
                    let toast_overlay = toast_overlay.clone();
                    copy.connect_clicked(move |_| {
                        if let Some(display) = gtk4::gdk::Display::default() {
                            display.clipboard().set_text(&text);
                            toast_overlay.add_toast(adw::Toast::new("Transcription copied"));
                        }
                    });
                    row.add_suffix(&copy);
                }

                let details_icon = gtk4::Image::from_icon_name("go-next-symbolic");
                details_icon.add_css_class("dim-label");
                details_icon.set_accessible_role(gtk4::AccessibleRole::Presentation);
                row.add_suffix(&details_icon);

                group.add(&row);
                rows.borrow_mut().push(row);
            }
        })
    };
    {
        let active_filter = active_filter.clone();
        let filter_button = filter_button.clone();
        let rebuild = rebuild.clone();
        filter_action.connect_activate(move |action, parameter| {
            let Some(filter) = parameter
                .and_then(gtk4::glib::Variant::str)
                .and_then(HistoryFilter::from_key)
            else {
                return;
            };
            active_filter.set(filter);
            action.set_state(&filter.key().to_variant());
            gui_settings::save_history_filter(filter.key());
            update_history_filter_button(&filter_button, filter);
            rebuild();
        });
    }
    {
        let rebuild = rebuild.clone();
        search.connect_search_changed(move |_| rebuild());
    }

    {
        let entries = entries.clone();
        let handle = handle.clone();
        let actions_available = actions_available.clone();
        clear_button.connect_clicked(move |button| {
            if !actions_available.get() {
                return;
            }
            let count = entries.borrow().len();
            if count == 0 {
                return;
            }
            let dialog = adw::AlertDialog::builder()
                .heading(clear_history_heading(count))
                .body(
                    "This permanently removes every saved transcript and failed recording from this computer.",
                )
                .build();
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("clear", "Clear history");
            dialog.set_response_appearance("clear", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            let handle = handle.clone();
            dialog.connect_response(Some("clear"), move |_, _| {
                handle.send(DaemonCommand::ClearHistory);
            });
            if let Some(root) = button.root() {
                dialog.present(Some(&root));
            }
        });
    }

    let apply_json: Rc<dyn Fn(&str)> = {
        let entries = entries.clone();
        let rebuild = rebuild.clone();
        Rc::new(move |json| {
            if let Ok(parsed) = serde_json::from_str::<Vec<voxkey_ipc::HistoryEntry>>(json) {
                *entries.borrow_mut() = parsed;
                rebuild();
            }
        })
    };
    rebuild();

    let set_actions_available: Rc<dyn Fn(bool)> = {
        let actions_available = actions_available.clone();
        let entries = entries.clone();
        let clear_button = clear_button.clone();
        let retry_buttons = retry_buttons.clone();
        Rc::new(move |available| {
            actions_available.set(available);
            clear_button.set_sensitive(available && !entries.borrow().is_empty());
            for button in retry_buttons.borrow().iter() {
                if available {
                    button.set_label("Retry");
                    button.set_tooltip_text(Some("Retry transcription"));
                }
                button.set_sensitive(available);
            }
        })
    };

    HistoryPage {
        widget: page.upcast(),
        apply_json,
        empty_action,
        search_entry: search,
        copy_latest_action,
        set_actions_available,
    }
}

fn history_title(entry: &voxkey_ipc::HistoryEntry) -> String {
    if entry.text.is_empty() && entry.outcome == voxkey_ipc::TranscriptOutcome::Failed {
        "Transcription failed".to_string()
    } else if entry.text.is_empty() {
        "No transcription text".to_string()
    } else {
        gtk4::glib::markup_escape_text(&entry.text).to_string()
    }
}

fn latest_history_text(entries: &[voxkey_ipc::HistoryEntry]) -> Option<String> {
    entries
        .iter()
        .filter(|entry| !entry.text.trim().is_empty())
        .max_by_key(|entry| entry.recorded_at_unix_ms)
        .map(|entry| entry.text.clone())
}

fn history_search_should_clear(key: gtk4::gdk::Key, query: &str) -> bool {
    key == gtk4::gdk::Key::Escape && !query.is_empty()
}

fn history_matches_filter(entry: &voxkey_ipc::HistoryEntry, filter: HistoryFilter) -> bool {
    let has_pending_text = entry
        .pending_insertion
        .as_deref()
        .is_some_and(|pending| !pending.is_empty());
    match filter {
        HistoryFilter::All => true,
        HistoryFilter::Completed => {
            entry.outcome == voxkey_ipc::TranscriptOutcome::Completed && !has_pending_text
        }
        HistoryFilter::NeedsAttention => {
            has_pending_text
                || matches!(
                    entry.outcome,
                    voxkey_ipc::TranscriptOutcome::PartialProviderError
                        | voxkey_ipc::TranscriptOutcome::PartialTransportClose
                        | voxkey_ipc::TranscriptOutcome::PartialFailure
                        | voxkey_ipc::TranscriptOutcome::Failed
                )
        }
        HistoryFilter::Cancelled => entry.outcome == voxkey_ipc::TranscriptOutcome::Cancelled,
    }
}

fn history_group_title(query: &str, filter: HistoryFilter) -> &'static str {
    if !query.is_empty() {
        return "Search results";
    }
    match filter {
        HistoryFilter::All => "Recent dictations",
        HistoryFilter::Completed => "Completed dictations",
        HistoryFilter::NeedsAttention => "Needs attention",
        HistoryFilter::Cancelled => "Cancelled dictations",
    }
}

fn history_matches_query(entry: &voxkey_ipc::HistoryEntry, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    [
        entry.text.as_str(),
        entry.provider.as_str(),
        entry.pending_insertion.as_deref().unwrap_or_default(),
        entry.error.as_deref().unwrap_or_default(),
        history_outcome_label(entry.outcome).unwrap_or_default(),
    ]
    .into_iter()
    .any(|value| value.to_lowercase().contains(&query))
        || history_provider_name(&entry.provider)
            .to_lowercase()
            .contains(&query)
        || format_timestamp(entry.recorded_at_unix_ms)
            .to_lowercase()
            .contains(&query)
        || (entry.text.is_empty() && history_title(entry).to_lowercase().contains(&query))
}

fn history_action_label(action: &str, entry: &voxkey_ipc::HistoryEntry) -> String {
    let text = entry.text.trim();
    let context = if text.is_empty() {
        if entry.outcome == voxkey_ipc::TranscriptOutcome::Failed {
            "failed dictation".to_string()
        } else {
            "dictation without text".to_string()
        }
    } else {
        let mut chars = text.chars();
        let preview: String = chars.by_ref().take(48).collect();
        if chars.next().is_some() {
            format!("{preview}…")
        } else {
            preview
        }
    };
    format!("{action}: {context}")
}

fn history_can_retry(entry: &voxkey_ipc::HistoryEntry) -> bool {
    entry.audio_path.is_some() && entry.outcome == voxkey_ipc::TranscriptOutcome::Failed
}

fn history_count_description(total: usize, visible: usize, searching: bool) -> String {
    let noun = if total == 1 {
        "dictation"
    } else {
        "dictations"
    };
    if searching {
        format!("{visible} of {total} {noun}")
    } else {
        format!("{total} {noun} saved locally on this computer")
    }
}

fn history_details_text(entry: &voxkey_ipc::HistoryEntry) -> String {
    let mut sections = Vec::new();
    if let Some(error) = entry.error.as_deref().filter(|error| !error.is_empty()) {
        sections.push(format!("What went wrong\n{error}"));
    }
    if entry.text.is_empty() {
        sections.push("No transcription text was produced.".to_string());
    } else {
        sections.push(entry.text.clone());
    }
    if let Some(pending) = entry
        .pending_insertion
        .as_deref()
        .filter(|pending| !pending.is_empty())
    {
        sections.push(format!("Not typed yet\n{pending}"));
    }
    sections.join("\n\n")
}

fn history_copy_content(
    entry: &voxkey_ipc::HistoryEntry,
) -> Option<(&'static str, String, &'static str)> {
    let has_recovery_details = entry
        .error
        .as_deref()
        .is_some_and(|error| !error.is_empty())
        || entry
            .pending_insertion
            .as_deref()
            .is_some_and(|pending| !pending.is_empty());
    if has_recovery_details {
        Some((
            "Copy details",
            history_details_text(entry),
            "Dictation details copied",
        ))
    } else if !entry.text.is_empty() {
        Some((
            "Copy transcription",
            entry.text.clone(),
            "Transcription copied",
        ))
    } else {
        None
    }
}

fn build_history_details_dialog(
    entry: &voxkey_ipc::HistoryEntry,
    toast_overlay: &adw::ToastOverlay,
    handle: &DaemonHandle,
    daemon_actions_available: bool,
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
    text_view.buffer().set_text(&history_details_text(entry));

    let scrolled = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .min_content_height(140)
        .max_content_height(320)
        .propagate_natural_height(true)
        .child(&text_view)
        .build();
    scrolled.add_css_class("card");

    let heading = if entry.outcome == voxkey_ipc::TranscriptOutcome::Failed {
        "Failed dictation"
    } else {
        "Dictation details"
    };
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(format!(
            "{}  •  {}",
            format_timestamp(entry.recorded_at_unix_ms),
            history_provider_name(&entry.provider)
        ))
        .body_use_markup(false)
        .extra_child(&scrolled)
        .build();
    dialog.add_response("close", "Close");
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");

    if let Some(audio_path) = entry.audio_path.clone() {
        dialog.add_response("open-folder", "Open recording folder");
        dialog.set_response_enabled("open-folder", daemon_actions_available);
        let handle = handle.clone();
        dialog.connect_response(Some("open-folder"), move |_, _| {
            handle.send(DaemonCommand::OpenRecordingFolder(audio_path.clone()));
        });
    }

    if history_can_retry(entry) {
        dialog.add_response("retry", "Retry transcription");
        dialog.set_response_appearance("retry", adw::ResponseAppearance::Suggested);
        dialog.set_response_enabled("retry", daemon_actions_available);
        let handle = handle.clone();
        let toast_overlay = toast_overlay.clone();
        let entry_id = entry.id;
        dialog.connect_response(Some("retry"), move |_, _| {
            let completion = handle.send(DaemonCommand::RetryHistoryEntry(entry_id));
            let toast_overlay = toast_overlay.clone();
            gtk4::glib::spawn_future_local(async move {
                if completion.wait().await.is_ok() {
                    toast_overlay.add_toast(adw::Toast::new("Retrying saved recording…"));
                }
            });
        });
    }

    if let Some((label, text, confirmation)) = history_copy_content(entry) {
        dialog.add_response("copy", label);
        let toast_overlay = toast_overlay.clone();
        dialog.connect_response(Some("copy"), move |_, _| {
            if let Some(display) = gtk4::gdk::Display::default() {
                display.clipboard().set_text(&text);
                toast_overlay.add_toast(adw::Toast::new(confirmation));
            }
        });
    }

    dialog.add_response("delete", "Delete…");
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_response_enabled("delete", daemon_actions_available);
    let entry_id = entry.id;
    let deletes_recording = entry.audio_path.is_some();
    let handle = handle.clone();
    let toast_overlay = toast_overlay.clone();
    dialog.connect_response(Some("delete"), move |_, _| {
        let confirmation = adw::AlertDialog::builder()
            .heading("Delete this dictation?")
            .body(history_delete_description(deletes_recording))
            .build();
        confirmation.add_response("cancel", "Cancel");
        confirmation.add_response("delete", "Delete");
        confirmation.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        confirmation.set_default_response(Some("cancel"));
        confirmation.set_close_response("cancel");

        let handle = handle.clone();
        confirmation.connect_response(Some("delete"), move |_, _| {
            handle.send(DaemonCommand::DeleteHistoryEntry(entry_id));
        });
        if let Some(root) = toast_overlay.root() {
            confirmation.present(Some(&root));
        }
    });

    dialog
}

fn unix_seconds(unix_ms: i64) -> i64 {
    unix_ms.div_euclid(1000)
}

fn history_delete_description(deletes_recording: bool) -> &'static str {
    if deletes_recording {
        "This permanently deletes the saved recording and its history entry."
    } else {
        "This permanently deletes the saved transcription."
    }
}

fn clear_history_heading(count: usize) -> String {
    if count == 1 {
        "Clear 1 dictation?".to_string()
    } else {
        format!("Clear {count} dictations?")
    }
}

fn clear_history_action_label(count: usize) -> String {
    match count {
        0 => "Clear history".to_string(),
        1 => "Delete 1 saved dictation".to_string(),
        count => format!("Delete {count} saved dictations"),
    }
}

fn format_timestamp(unix_ms: i64) -> String {
    let Ok(now) = gtk4::glib::DateTime::now_local() else {
        return "Unknown time".to_string();
    };
    format_timestamp_at(unix_ms, &now, uses_12_hour_clock())
}

fn uses_12_hour_clock() -> bool {
    let Some(source) = gtk4::gio::SettingsSchemaSource::default() else {
        return false;
    };
    let Some(schema) = source.lookup("org.gnome.desktop.interface", true) else {
        return false;
    };
    let settings = gtk4::gio::Settings::new_full(&schema, gtk4::gio::SettingsBackend::NONE, None);
    settings.string("clock-format") == "12h"
}

fn format_clock(date: &gtk4::glib::DateTime, use_12_hour_clock: bool) -> Option<String> {
    let clock = date
        .format(if use_12_hour_clock {
            "%I:%M %p"
        } else {
            "%H:%M"
        })
        .ok()?
        .to_string();
    Some(if use_12_hour_clock {
        clock.strip_prefix('0').unwrap_or(&clock).to_string()
    } else {
        clock
    })
}

fn format_timestamp_at(
    unix_ms: i64,
    now: &gtk4::glib::DateTime,
    use_12_hour_clock: bool,
) -> String {
    let Ok(date) = gtk4::glib::DateTime::from_unix_local(unix_seconds(unix_ms)) else {
        return "Unknown time".to_string();
    };
    let Some(clock) = format_clock(&date, use_12_hour_clock) else {
        return "Unknown time".to_string();
    };

    if date.ymd() == now.ymd() {
        return format!("Today, {clock}");
    }
    if now
        .add_days(-1)
        .is_ok_and(|yesterday| date.ymd() == yesterday.ymd())
    {
        return format!("Yesterday, {clock}");
    }

    date.format("%x")
        .map(|day| format!("{day}, {clock}"))
        .unwrap_or_else(|_| "Unknown time".to_string())
}

fn format_history_subtitle(
    recorded_at_unix_ms: i64,
    provider: &str,
    outcome: voxkey_ipc::TranscriptOutcome,
    pending_insertion: Option<&str>,
    error: Option<&str>,
) -> String {
    let mut parts = vec![
        format_timestamp(recorded_at_unix_ms),
        gtk4::glib::markup_escape_text(&history_provider_name(provider)).to_string(),
    ];
    if let Some(outcome) = history_outcome_label(outcome) {
        parts.push(outcome.to_string());
    }
    if pending_insertion.is_some_and(|pending| !pending.is_empty()) {
        parts.push("Typing incomplete".to_string());
    }
    if let Some(error) = error {
        let summary = error.chars().take(240).collect::<String>();
        parts.push(gtk4::glib::markup_escape_text(&summary).to_string());
    }
    parts.join("  •  ")
}

fn history_outcome_label(outcome: voxkey_ipc::TranscriptOutcome) -> Option<&'static str> {
    match outcome {
        voxkey_ipc::TranscriptOutcome::Completed => None,
        voxkey_ipc::TranscriptOutcome::PartialProviderError => Some("Partial — engine error"),
        voxkey_ipc::TranscriptOutcome::PartialTransportClose => Some("Partial — connection lost"),
        voxkey_ipc::TranscriptOutcome::Cancelled => Some("Cancelled"),
        voxkey_ipc::TranscriptOutcome::PartialFailure => Some("Partial — dictation interrupted"),
        voxkey_ipc::TranscriptOutcome::Failed => Some("Recording saved"),
    }
}

fn history_outcome_presentation(
    outcome: voxkey_ipc::TranscriptOutcome,
    typing_incomplete: bool,
) -> HistoryOutcomePresentation {
    if typing_incomplete {
        return HistoryOutcomePresentation {
            icon: "dialog-warning-symbolic",
            style: "warning",
        };
    }
    match outcome {
        voxkey_ipc::TranscriptOutcome::Completed => HistoryOutcomePresentation {
            icon: "emblem-ok-symbolic",
            style: "success",
        },
        voxkey_ipc::TranscriptOutcome::Cancelled => HistoryOutcomePresentation {
            icon: "process-stop-symbolic",
            style: "dim-label",
        },
        voxkey_ipc::TranscriptOutcome::Failed => HistoryOutcomePresentation {
            icon: "dialog-error-symbolic",
            style: "error",
        },
        voxkey_ipc::TranscriptOutcome::PartialProviderError
        | voxkey_ipc::TranscriptOutcome::PartialTransportClose
        | voxkey_ipc::TranscriptOutcome::PartialFailure => HistoryOutcomePresentation {
            icon: "dialog-warning-symbolic",
            style: "warning",
        },
    }
}

fn history_provider_name(provider: &str) -> String {
    if provider == "whisper.cpp" {
        "Whisper.cpp".to_string()
    } else if let Some(model) = provider.strip_suffix(" (HTTP Server)") {
        format!("{model} Server")
    } else {
        provider.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_format_has_a_time_component() {
        assert!(format_timestamp(1_720_000_000_000).contains(':'));
    }

    #[test]
    fn recent_timestamps_use_scannable_day_names() {
        let now = gtk4::glib::DateTime::from_local(2026, 8, 25, 18, 0, 0.0).unwrap();
        let today = gtk4::glib::DateTime::from_local(2026, 8, 25, 13, 42, 0.0).unwrap();
        let yesterday = gtk4::glib::DateTime::from_local(2026, 8, 24, 23, 7, 0.0).unwrap();

        assert_eq!(
            format_timestamp_at(today.to_unix() * 1000, &now, false),
            "Today, 13:42"
        );
        assert_eq!(
            format_timestamp_at(yesterday.to_unix() * 1000, &now, false),
            "Yesterday, 23:07"
        );
        assert_eq!(
            format_timestamp_at(today.to_unix() * 1000, &now, true),
            "Today, 1:42 PM"
        );
        assert_eq!(
            format_timestamp_at(yesterday.to_unix() * 1000, &now, true),
            "Yesterday, 11:07 PM"
        );
    }

    #[test]
    fn milliseconds_before_the_epoch_round_down_to_the_previous_second() {
        assert_eq!(unix_seconds(-1), -1);
        assert_eq!(unix_seconds(-999), -1);
    }

    #[test]
    fn delete_warning_names_a_saved_recording_when_one_will_be_removed() {
        assert!(history_delete_description(true).contains("saved recording"));
        assert!(!history_delete_description(false).contains("recording"));
    }

    #[test]
    fn only_failed_entries_with_recordings_can_retry() {
        let mut entry = voxkey_ipc::HistoryEntry {
            id: 1,
            recorded_at_unix_ms: 0,
            text: String::new(),
            provider: "Test".to_string(),
            outcome: voxkey_ipc::TranscriptOutcome::Failed,
            pending_insertion: None,
            audio_path: Some("/tmp/recording.wav".to_string()),
            error: None,
        };
        assert!(history_can_retry(&entry));

        entry.audio_path = None;
        assert!(!history_can_retry(&entry));
        entry.audio_path = Some("/tmp/recording.wav".to_string());
        entry.outcome = voxkey_ipc::TranscriptOutcome::Completed;
        assert!(!history_can_retry(&entry));
    }

    #[test]
    fn clear_history_warning_names_the_number_of_dictations() {
        assert_eq!(clear_history_heading(1), "Clear 1 dictation?");
        assert_eq!(clear_history_heading(12), "Clear 12 dictations?");
        assert_eq!(clear_history_action_label(0), "Clear history");
        assert_eq!(clear_history_action_label(1), "Delete 1 saved dictation");
        assert_eq!(clear_history_action_label(12), "Delete 12 saved dictations");
    }

    #[test]
    fn row_action_labels_name_their_dictation_without_becoming_unwieldy() {
        let mut entry = voxkey_ipc::HistoryEntry {
            id: 1,
            recorded_at_unix_ms: 0,
            text: String::new(),
            provider: "Test".to_string(),
            outcome: voxkey_ipc::TranscriptOutcome::Failed,
            pending_insertion: None,
            audio_path: None,
            error: None,
        };

        assert_eq!(
            history_action_label("Retry transcription", &entry),
            "Retry transcription: failed dictation"
        );

        entry.text = "a".repeat(49);
        assert_eq!(
            history_action_label("Copy transcription", &entry),
            format!("Copy transcription: {}…", "a".repeat(48))
        );
    }

    #[test]
    fn history_details_include_recovery_information_without_truncation() {
        let mut entry = voxkey_ipc::HistoryEntry {
            id: 1,
            recorded_at_unix_ms: 0,
            text: "A complete transcript".to_string(),
            provider: "Test".to_string(),
            outcome: voxkey_ipc::TranscriptOutcome::PartialFailure,
            pending_insertion: Some("remaining words".to_string()),
            audio_path: None,
            error: Some("full provider error".to_string()),
        };

        let details = history_details_text(&entry);
        assert!(details.contains("A complete transcript"));
        assert!(details.contains("remaining words"));
        assert!(details.contains("full provider error"));
        let error_position = details.find("full provider error").unwrap();
        let transcript_position = details.find("A complete transcript").unwrap();
        assert!(error_position < transcript_position);

        let (label, copied, confirmation) = history_copy_content(&entry).unwrap();
        assert_eq!(label, "Copy details");
        assert!(copied.contains("full provider error"));
        assert_eq!(confirmation, "Dictation details copied");

        entry.error = None;
        entry.pending_insertion = None;
        let (label, copied, confirmation) = history_copy_content(&entry).unwrap();
        assert_eq!(label, "Copy transcription");
        assert_eq!(copied, "A complete transcript");
        assert_eq!(confirmation, "Transcription copied");
    }

    #[test]
    fn copy_latest_uses_the_newest_non_empty_transcription() {
        let entry = |id, recorded_at_unix_ms, text: &str| voxkey_ipc::HistoryEntry {
            id,
            recorded_at_unix_ms,
            text: text.to_string(),
            provider: "Test".to_string(),
            outcome: voxkey_ipc::TranscriptOutcome::Completed,
            pending_insertion: None,
            audio_path: None,
            error: None,
        };
        let entries = vec![
            entry(1, 100, "Older transcription"),
            entry(2, 300, "   "),
            entry(3, 200, "Latest transcription"),
        ];

        assert_eq!(
            latest_history_text(&entries).as_deref(),
            Some("Latest transcription")
        );
        assert_eq!(latest_history_text(&[entry(4, 400, "")]), None);
    }

    #[test]
    fn history_count_explains_full_and_filtered_lists() {
        assert_eq!(
            history_count_description(1, 1, false),
            "1 dictation saved locally on this computer"
        );
        assert_eq!(history_count_description(12, 3, true), "3 of 12 dictations");
        assert_eq!(history_count_description(1, 1, true), "1 of 1 dictation");
    }

    #[test]
    fn history_search_matches_the_metadata_people_see() {
        let entry = voxkey_ipc::HistoryEntry {
            id: 1,
            recorded_at_unix_ms: 1_720_000_000_000,
            text: String::new(),
            provider: "Parakeet v3 (HTTP Server)".to_string(),
            outcome: voxkey_ipc::TranscriptOutcome::Failed,
            pending_insertion: Some("remaining words".to_string()),
            audio_path: None,
            error: Some("unprocessable response".to_string()),
        };

        assert!(history_matches_query(&entry, "parakeet v3 server"));
        assert!(history_matches_query(&entry, "recording saved"));
        assert!(history_matches_query(&entry, "transcription failed"));
        assert!(history_matches_query(&entry, "remaining words"));
        assert!(history_matches_query(&entry, "unprocessable"));
        assert!(!history_matches_query(&entry, "whisper"));
    }

    #[test]
    fn history_filters_separate_completed_cancelled_and_attention_items() {
        let mut entry = voxkey_ipc::HistoryEntry {
            id: 1,
            recorded_at_unix_ms: 0,
            text: "Transcript".to_string(),
            provider: "Test".to_string(),
            outcome: voxkey_ipc::TranscriptOutcome::Completed,
            pending_insertion: None,
            audio_path: None,
            error: None,
        };

        assert!(history_matches_filter(&entry, HistoryFilter::All));
        assert!(history_matches_filter(&entry, HistoryFilter::Completed));
        assert!(!history_matches_filter(
            &entry,
            HistoryFilter::NeedsAttention
        ));

        entry.pending_insertion = Some("not typed".to_string());
        assert!(!history_matches_filter(&entry, HistoryFilter::Completed));
        assert!(history_matches_filter(
            &entry,
            HistoryFilter::NeedsAttention
        ));

        entry.pending_insertion = None;
        entry.outcome = voxkey_ipc::TranscriptOutcome::Cancelled;
        assert!(history_matches_filter(&entry, HistoryFilter::Cancelled));
        assert!(!history_matches_filter(
            &entry,
            HistoryFilter::NeedsAttention
        ));

        entry.outcome = voxkey_ipc::TranscriptOutcome::Failed;
        assert!(history_matches_filter(
            &entry,
            HistoryFilter::NeedsAttention
        ));
    }

    #[test]
    fn history_filter_keys_and_headings_are_stable() {
        for filter in [
            HistoryFilter::All,
            HistoryFilter::Completed,
            HistoryFilter::NeedsAttention,
            HistoryFilter::Cancelled,
        ] {
            assert_eq!(HistoryFilter::from_key(filter.key()), Some(filter));
        }
        assert_eq!(
            history_group_title("", HistoryFilter::All),
            "Recent dictations"
        );
        assert_eq!(
            history_group_title("", HistoryFilter::NeedsAttention),
            "Needs attention"
        );
        assert_eq!(
            history_group_title("server", HistoryFilter::Completed),
            "Search results"
        );
    }

    #[test]
    fn escape_clears_only_an_active_history_search() {
        assert!(history_search_should_clear(gtk4::gdk::Key::Escape, "vox"));
        assert!(!history_search_should_clear(gtk4::gdk::Key::Escape, ""));
        assert!(!history_search_should_clear(gtk4::gdk::Key::Return, "vox"));
    }

    #[test]
    fn history_subtitle_escapes_provider_markup() {
        let subtitle = format_history_subtitle(
            1_720_000_000_000,
            "<b>remote & custom</b>",
            voxkey_ipc::TranscriptOutcome::Completed,
            None,
            None,
        );

        assert!(
            subtitle.contains("&lt;b&gt;remote &amp; custom&lt;/b&gt;"),
            "{subtitle}"
        );
        assert!(!subtitle.contains("<b>"), "{subtitle}");
    }

    #[test]
    fn legacy_provider_names_match_the_current_interface() {
        assert_eq!(history_provider_name("whisper.cpp"), "Whisper.cpp");
        assert_eq!(
            history_provider_name("Parakeet v3 (HTTP Server)"),
            "Parakeet v3 Server"
        );
        assert_eq!(
            history_provider_name("custom-model (HTTP Server)"),
            "custom-model Server"
        );
        assert_eq!(history_provider_name("Mistral"), "Mistral");
    }

    #[test]
    fn history_subtitle_marks_partial_output_and_uninserted_text() {
        let subtitle = format_history_subtitle(
            1_720_000_000_000,
            "Mistral Realtime",
            voxkey_ipc::TranscriptOutcome::PartialTransportClose,
            Some("remaining words"),
            None,
        );

        assert!(subtitle.contains("Partial — connection lost"), "{subtitle}");
        assert!(subtitle.contains("Typing incomplete"), "{subtitle}");
    }

    #[test]
    fn partial_history_outcomes_use_user_facing_causes() {
        for (outcome, expected) in [
            (
                voxkey_ipc::TranscriptOutcome::PartialProviderError,
                "Partial — engine error",
            ),
            (
                voxkey_ipc::TranscriptOutcome::PartialFailure,
                "Partial — dictation interrupted",
            ),
        ] {
            let subtitle =
                format_history_subtitle(1_720_000_000_000, "Mistral Realtime", outcome, None, None);
            assert!(subtitle.contains(expected), "{subtitle}");
        }
    }

    #[test]
    fn history_icons_make_outcomes_scannable() {
        assert_eq!(
            history_outcome_presentation(voxkey_ipc::TranscriptOutcome::Completed, false),
            HistoryOutcomePresentation {
                icon: "emblem-ok-symbolic",
                style: "success",
            }
        );
        assert_eq!(
            history_outcome_presentation(voxkey_ipc::TranscriptOutcome::Failed, false).style,
            "error"
        );
        assert_eq!(
            history_outcome_presentation(
                voxkey_ipc::TranscriptOutcome::PartialTransportClose,
                false,
            )
            .style,
            "warning"
        );
        assert_eq!(
            history_outcome_presentation(voxkey_ipc::TranscriptOutcome::Completed, true).style,
            "warning"
        );
    }

    #[test]
    fn failed_recording_subtitle_escapes_the_provider_error() {
        let subtitle = format_history_subtitle(
            1_720_000_000_000,
            "Parakeet HTTP",
            voxkey_ipc::TranscriptOutcome::Failed,
            None,
            Some("422 <unprocessable> & rejected"),
        );

        assert!(subtitle.contains("Recording saved"), "{subtitle}");
        assert!(
            subtitle.contains("422 &lt;unprocessable&gt; &amp; rejected"),
            "{subtitle}"
        );
    }
}
