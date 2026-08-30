// ABOUTME: Builds the Dictionary settings page with replacement and vocabulary views.
// ABOUTME: Pushes every change to the daemon over D-Bus, which persists it.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;

use adw::prelude::*;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;

use voxkey_ipc::{DictionaryConfig, WordReplacement};

use crate::daemon_client::{DaemonCommand, DaemonHandle};
use crate::gui_settings;

/// A rebuild closure that can call itself again, e.g. after a row is deleted.
type RebuildSlot = Rc<RefCell<Option<Rc<dyn Fn()>>>>;
const DICTIONARY_SEARCH_THRESHOLD: usize = 6;
const ACTION_CONTEXT_LIMIT: usize = 48;
const MAX_DICTIONARY_IMPORT_BYTES: usize = 1_048_576;
const REPLACEMENT_ADD_DESCRIPTION: &str = "Separate multiple heard phrases with commas";
const VOCABULARY_ADD_DESCRIPTION: &str = "Help Voxkey recognize a name or technical term";

pub struct DictionaryPage {
    pub widget: gtk4::Widget,
    pub apply_json: Rc<dyn Fn(&str)>,
    pub switcher: adw::ViewSwitcher,
    pub switcher_bar: adw::ViewSwitcherBar,
    pub focus_search: Rc<dyn Fn()>,
}

fn has_replacement_variant(original: &str) -> bool {
    original
        .split(',')
        .any(|variant| !variant.trim().is_empty())
}

fn replacement_input_error(original: &str, replacement: &str) -> Option<&'static str> {
    if !has_replacement_variant(original) {
        Some("Enter at least one original phrase")
    } else if replacement.trim().is_empty() {
        Some("Enter replacement text")
    } else {
        None
    }
}

fn replacement_is_duplicate(
    config: &DictionaryConfig,
    original: &str,
    replacement: &str,
    except_index: Option<usize>,
) -> bool {
    let original = original.trim();
    let replacement = replacement.trim();
    config.replacements.iter().enumerate().any(|(index, rule)| {
        Some(index) != except_index && rule.original == original && rule.replacement == replacement
    })
}

fn replacement_validation_error(
    config: &DictionaryConfig,
    original: &str,
    replacement: &str,
    except_index: Option<usize>,
) -> Option<&'static str> {
    replacement_input_error(original, replacement).or_else(|| {
        replacement_is_duplicate(config, original, replacement, except_index)
            .then_some("That replacement already exists")
    })
}

fn replacement_can_add(config: &DictionaryConfig, original: &str, replacement: &str) -> bool {
    replacement_validation_error(config, original, replacement, None).is_none()
}

fn replacement_add_description(
    config: &DictionaryConfig,
    original: &str,
    replacement: &str,
) -> &'static str {
    if original.trim().is_empty() && replacement.trim().is_empty() {
        REPLACEMENT_ADD_DESCRIPTION
    } else {
        replacement_validation_error(config, original, replacement, None)
            .unwrap_or("Ready to add this replacement")
    }
}

fn vocabulary_entries_match(left: &str, right: &str) -> bool {
    left.trim().to_lowercase() == right.trim().to_lowercase()
}

fn vocabulary_validation_error(
    config: &DictionaryConfig,
    word: &str,
    except_index: Option<usize>,
) -> Option<&'static str> {
    let word = word.trim();
    if word.is_empty() {
        Some("Enter a word or name")
    } else if config
        .vocabulary
        .iter()
        .enumerate()
        .any(|(index, existing)| {
            Some(index) != except_index && vocabulary_entries_match(existing, word)
        })
    {
        Some("That vocabulary entry already exists")
    } else {
        None
    }
}

fn vocabulary_input_error(config: &DictionaryConfig, word: &str) -> Option<&'static str> {
    vocabulary_validation_error(config, word, None)
}

fn vocabulary_can_add(config: &DictionaryConfig, word: &str) -> bool {
    vocabulary_input_error(config, word).is_none()
}

fn vocabulary_add_description(config: &DictionaryConfig, word: &str) -> &'static str {
    if word.trim().is_empty() {
        VOCABULARY_ADD_DESCRIPTION
    } else {
        vocabulary_input_error(config, word).unwrap_or("Ready to add this vocabulary entry")
    }
}

fn replacement_row_subtitle(replacement: &str, enabled: bool) -> String {
    let action = format!("Replace with “{replacement}”");
    if enabled {
        action
    } else {
        format!("Paused · {action}")
    }
}

fn replacements_description(count: usize) -> String {
    let noun = if count == 1 {
        "replacement"
    } else {
        "replacements"
    };
    format!("{count} {noun} · Applied before text is typed")
}

fn vocabulary_description(count: usize) -> String {
    let noun = if count == 1 { "entry" } else { "entries" };
    format!("{count} {noun} · Used to improve word recognition")
}

fn replacement_matches_query(rule: &WordReplacement, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || rule.original.to_lowercase().contains(&query)
        || rule.replacement.to_lowercase().contains(&query)
}

fn replacement_search_description(total: usize, visible: usize, searching: bool) -> String {
    if searching {
        format!("{visible} of {total} replacements")
    } else {
        replacements_description(total)
    }
}

fn vocabulary_matches_query(word: &str, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty() || word.to_lowercase().contains(&query)
}

fn vocabulary_search_description(total: usize, visible: usize, searching: bool) -> String {
    if searching {
        format!("{visible} of {total} entries")
    } else {
        vocabulary_description(total)
    }
}

fn dictionary_action_label(action: &str, item: &str) -> String {
    let item = item.trim();
    if item.is_empty() {
        return action.to_string();
    }
    let context = if item.chars().count() > ACTION_CONTEXT_LIMIT {
        let mut shortened = item
            .chars()
            .take(ACTION_CONTEXT_LIMIT - 1)
            .collect::<String>();
        shortened.push('…');
        shortened
    } else {
        item.to_string()
    };
    format!("{action}: {context}")
}

fn replacement_toggle_action_label(original: &str, enabled: bool) -> String {
    let action = if enabled {
        "Pause replacement"
    } else {
        "Enable replacement"
    };
    dictionary_action_label(action, original)
}

fn dictionary_search_should_clear(key: gtk4::gdk::Key, query: &str) -> bool {
    key == gtk4::gdk::Key::Escape && !query.is_empty()
}

fn compare_dictionary_text(left: &str, right: &str) -> Ordering {
    left.to_lowercase()
        .cmp(&right.to_lowercase())
        .then_with(|| left.cmp(right))
}

fn enable_escape_to_clear(search: &gtk4::SearchEntry) {
    let keys = gtk4::EventControllerKey::new();
    let search_for_keys = search.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if dictionary_search_should_clear(key, &search_for_keys.text()) {
            search_for_keys.set_text("");
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    search.add_controller(keys);
}

/// Add a replacement rule to the config. Returns false (no change) when
/// either side is blank or the original contains only separators.
pub fn add_replacement(config: &mut DictionaryConfig, original: &str, replacement: &str) -> bool {
    let original = original.trim();
    let replacement = replacement.trim();
    if replacement_validation_error(config, original, replacement, None).is_some() {
        return false;
    }
    config.replacements.push(WordReplacement {
        original: original.to_string(),
        replacement: replacement.to_string(),
        enabled: true,
    });
    true
}

/// Add a vocabulary word. Returns false for blanks and duplicates.
pub fn add_vocabulary_word(config: &mut DictionaryConfig, word: &str) -> bool {
    let word = word.trim();
    if vocabulary_input_error(config, word).is_some() {
        return false;
    }
    config.vocabulary.push(word.to_string());
    true
}

fn edit_replacement_if_current(
    config: &mut DictionaryConfig,
    index: usize,
    expected: &WordReplacement,
    original: String,
    replacement: String,
) -> bool {
    if replacement_validation_error(config, &original, &replacement, Some(index)).is_some() {
        return false;
    }
    let Some(rule) = config.replacements.get_mut(index) else {
        return false;
    };
    if rule != expected {
        return false;
    }
    rule.original = original;
    rule.replacement = replacement;
    true
}

fn delete_replacement_if_current(
    config: &mut DictionaryConfig,
    index: usize,
    expected: &WordReplacement,
) -> bool {
    if config.replacements.get(index) != Some(expected) {
        return false;
    }
    config.replacements.remove(index);
    true
}

fn restore_replacement_if_missing(
    config: &mut DictionaryConfig,
    index: usize,
    replacement: &WordReplacement,
    original_occurrences: usize,
) -> bool {
    let current_occurrences = config
        .replacements
        .iter()
        .filter(|existing| *existing == replacement)
        .count();
    if current_occurrences >= original_occurrences {
        return false;
    }
    config
        .replacements
        .insert(index.min(config.replacements.len()), replacement.clone());
    true
}

fn set_replacement_enabled_if_current(
    config: &mut DictionaryConfig,
    index: usize,
    expected: &mut WordReplacement,
    enabled: bool,
) -> bool {
    let Some(rule) = config.replacements.get_mut(index) else {
        return false;
    };
    if rule != expected {
        return false;
    }
    rule.enabled = enabled;
    expected.enabled = enabled;
    true
}

fn edit_vocabulary_word_if_current(
    config: &mut DictionaryConfig,
    index: usize,
    expected: &str,
    word: String,
) -> bool {
    let word = word.trim().to_string();
    if vocabulary_validation_error(config, &word, Some(index)).is_some() {
        return false;
    }
    let Some(current) = config.vocabulary.get_mut(index) else {
        return false;
    };
    if current != expected {
        return false;
    }
    *current = word;
    true
}

fn delete_vocabulary_word_if_current(
    config: &mut DictionaryConfig,
    index: usize,
    expected: &str,
) -> bool {
    if config.vocabulary.get(index).map(String::as_str) != Some(expected) {
        return false;
    }
    config.vocabulary.remove(index);
    true
}

fn restore_vocabulary_word_if_missing(
    config: &mut DictionaryConfig,
    index: usize,
    word: &str,
) -> bool {
    if config
        .vocabulary
        .iter()
        .any(|existing| vocabulary_entries_match(existing, word))
    {
        return false;
    }
    config
        .vocabulary
        .insert(index.min(config.vocabulary.len()), word.to_string());
    true
}

fn send_config(config: &Rc<RefCell<DictionaryConfig>>, handle: &DaemonHandle) {
    if let Ok(json) = serde_json::to_string(&*config.borrow()) {
        handle.send(DaemonCommand::SetDictionaryConfig(json));
    }
}

fn dictionary_export_json(config: &DictionaryConfig) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(config).map(|mut json| {
        json.push('\n');
        json
    })
}

fn dictionary_import_json(contents: &[u8]) -> Result<DictionaryConfig, &'static str> {
    if contents.len() > MAX_DICTIONARY_IMPORT_BYTES {
        return Err("That dictionary backup is larger than 1 MB");
    }
    let config: DictionaryConfig = serde_json::from_slice(contents)
        .map_err(|_| "Choose a Voxkey dictionary backup in JSON format")?;
    if config
        .replacements
        .iter()
        .any(|rule| replacement_input_error(&rule.original, &rule.replacement).is_some())
        || config.vocabulary.iter().any(|word| word.trim().is_empty())
    {
        return Err("That backup contains an empty dictionary entry");
    }
    Ok(config)
}

fn dictionary_import_description(config: &DictionaryConfig, replacing: bool) -> String {
    let replacements = match config.replacements.len() {
        1 => "1 replacement".to_string(),
        count => format!("{count} replacements"),
    };
    let vocabulary = match config.vocabulary.len() {
        1 => "1 vocabulary entry".to_string(),
        count => format!("{count} vocabulary entries"),
    };
    let consequence = if replacing {
        " Importing it replaces your current dictionary."
    } else {
        ""
    };
    format!("This backup contains {replacements} and {vocabulary}.{consequence}")
}

/// Build the embedded Dictionary page used by the main settings window.
pub fn build_dictionary_page(
    config: Rc<RefCell<DictionaryConfig>>,
    handle: DaemonHandle,
    toast_overlay: &adw::ToastOverlay,
) -> DictionaryPage {
    let stack = adw::ViewStack::new();

    let (replacements_page, rebuild_replacements, replacements_search) =
        build_replacements_view(&config, &handle, toast_overlay);
    stack.add_titled_with_icon(
        &replacements_page,
        Some("replacements"),
        "Replacements",
        "edit-find-replace-symbolic",
    );

    let (vocabulary_page, rebuild_vocabulary, vocabulary_search) =
        build_vocabulary_view(&config, &handle, toast_overlay);
    stack.add_titled_with_icon(
        &vocabulary_page,
        Some("vocabulary"),
        "Vocabulary",
        "accessories-dictionary-symbolic",
    );
    stack.set_visible_child_name(&gui_settings::load_dictionary_tab());
    stack.connect_visible_child_name_notify(|stack| {
        if let Some(tab) = stack.visible_child_name() {
            gui_settings::save_dictionary_tab(&tab);
        }
    });

    let switcher = adw::ViewSwitcher::builder()
        .stack(&stack)
        .policy(adw::ViewSwitcherPolicy::Wide)
        .hexpand(true)
        .build();

    let dictionary_menu = gtk4::gio::Menu::new();
    dictionary_menu.append(Some("Import dictionary…"), Some("dictionary.import"));
    dictionary_menu.append(Some("Export dictionary…"), Some("dictionary.export"));
    let dictionary_menu_button = gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&dictionary_menu)
        .tooltip_text("Dictionary menu")
        .valign(gtk4::Align::Center)
        .build();
    dictionary_menu_button.add_css_class("flat");
    dictionary_menu_button.update_property(&[gtk4::accessible::Property::Label("Dictionary menu")]);
    let dictionary_actions = gtk4::gio::SimpleActionGroup::new();
    let import_action = gtk4::gio::SimpleAction::new("import", None);
    dictionary_actions.add_action(&import_action);
    let export_action = gtk4::gio::SimpleAction::new("export", None);
    dictionary_actions.add_action(&export_action);
    dictionary_menu_button.insert_action_group("dictionary", Some(&dictionary_actions));

    let dictionary_header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    dictionary_header.set_margin_top(18);
    dictionary_header.set_margin_bottom(6);
    dictionary_header.set_margin_start(24);
    dictionary_header.set_margin_end(18);
    dictionary_header.append(&switcher);
    dictionary_header.append(&dictionary_menu_button);

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    content.append(&dictionary_header);
    stack.set_vexpand(true);
    content.append(&stack);
    let switcher_bar = adw::ViewSwitcherBar::builder()
        .stack(&stack)
        .reveal(false)
        .build();
    content.append(&switcher_bar);

    let focus_search: Rc<dyn Fn()> = {
        let stack = stack.clone();
        Rc::new(move || {
            let search = if stack.visible_child_name().as_deref() == Some("vocabulary") {
                &vocabulary_search
            } else {
                &replacements_search
            };
            search.set_visible(true);
            search.grab_focus();
        })
    };

    let apply_json: Rc<dyn Fn(&str)> = {
        let config = config.clone();
        let rebuild_replacements = rebuild_replacements.clone();
        let rebuild_vocabulary = rebuild_vocabulary.clone();
        Rc::new(move |json| {
            if let Ok(parsed) = serde_json::from_str::<DictionaryConfig>(json) {
                *config.borrow_mut() = parsed;
                rebuild_replacements();
                rebuild_vocabulary();
            }
        })
    };

    {
        let config = config.clone();
        let menu_button = dictionary_menu_button.clone();
        let toast_overlay = toast_overlay.clone();
        export_action.connect_activate(move |_, _| {
            let json = match dictionary_export_json(&config.borrow()) {
                Ok(json) => json,
                Err(error) => {
                    tracing::warn!("Could not prepare dictionary export: {error}");
                    toast_overlay
                        .add_toast(adw::Toast::new("Could not prepare the dictionary backup"));
                    return;
                }
            };
            let Some(parent) = menu_button
                .root()
                .and_then(|root| root.downcast::<gtk4::Window>().ok())
            else {
                return;
            };
            let filter = gtk4::FileFilter::new();
            filter.set_name(Some("JSON dictionary backups"));
            filter.add_pattern("*.json");
            filter.add_mime_type("application/json");
            let dialog = gtk4::FileDialog::builder()
                .title("Export dictionary")
                .accept_label("Export")
                .initial_name("voxkey-dictionary.json")
                .default_filter(&filter)
                .modal(true)
                .build();
            let toast_overlay = toast_overlay.clone();
            glib::spawn_future_local(async move {
                let file = match dialog.save_future(Some(&parent)).await {
                    Ok(file) => file,
                    Err(error)
                        if error.matches(gtk4::DialogError::Cancelled)
                            || error.matches(gtk4::DialogError::Dismissed)
                            || error.matches(gtk4::gio::IOErrorEnum::Cancelled) =>
                    {
                        return;
                    }
                    Err(error) => {
                        tracing::warn!("Could not open the dictionary export chooser: {error}");
                        toast_overlay.add_toast(adw::Toast::new(
                            "Could not open the file chooser. Try again.",
                        ));
                        return;
                    }
                };
                match file
                    .replace_contents_future(
                        json.into_bytes(),
                        None,
                        false,
                        gtk4::gio::FileCreateFlags::REPLACE_DESTINATION,
                    )
                    .await
                {
                    Ok(_) => toast_overlay.add_toast(adw::Toast::new("Dictionary exported")),
                    Err((_, error)) => {
                        tracing::warn!("Could not write dictionary export: {error}");
                        toast_overlay.add_toast(adw::Toast::new(
                            "Could not save the dictionary backup. Try another folder.",
                        ));
                    }
                }
            });
        });
    }
    {
        let config = config.clone();
        let handle = handle.clone();
        let rebuild_replacements = rebuild_replacements.clone();
        let rebuild_vocabulary = rebuild_vocabulary.clone();
        let menu_button = dictionary_menu_button.clone();
        let toast_overlay = toast_overlay.clone();
        import_action.connect_activate(move |_, _| {
            let Some(parent) = menu_button
                .root()
                .and_then(|root| root.downcast::<gtk4::Window>().ok())
            else {
                return;
            };
            let filter = gtk4::FileFilter::new();
            filter.set_name(Some("JSON dictionary backups"));
            filter.add_pattern("*.json");
            filter.add_mime_type("application/json");
            let dialog = gtk4::FileDialog::builder()
                .title("Import dictionary")
                .accept_label("Open")
                .default_filter(&filter)
                .modal(true)
                .build();
            let config = config.clone();
            let handle = handle.clone();
            let rebuild_replacements = rebuild_replacements.clone();
            let rebuild_vocabulary = rebuild_vocabulary.clone();
            let toast_overlay = toast_overlay.clone();
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
                        tracing::warn!("Could not open the dictionary import chooser: {error}");
                        toast_overlay.add_toast(adw::Toast::new(
                            "Could not open the file chooser. Try again.",
                        ));
                        return;
                    }
                };
                let contents = match file.load_contents_future().await {
                    Ok((contents, _)) => contents,
                    Err(error) => {
                        tracing::warn!("Could not read dictionary import: {error}");
                        toast_overlay
                            .add_toast(adw::Toast::new("Could not read that dictionary backup"));
                        return;
                    }
                };
                let imported = match dictionary_import_json(&contents) {
                    Ok(imported) => imported,
                    Err(message) => {
                        toast_overlay.add_toast(adw::Toast::new(message));
                        return;
                    }
                };
                let replacing = {
                    let current = config.borrow();
                    !current.replacements.is_empty() || !current.vocabulary.is_empty()
                };
                let confirmation = adw::AlertDialog::builder()
                    .heading(if replacing {
                        "Replace current dictionary?"
                    } else {
                        "Import this dictionary?"
                    })
                    .body(dictionary_import_description(&imported, replacing))
                    .build();
                confirmation.add_response("cancel", "Cancel");
                confirmation.add_response("import", "Import");
                if replacing {
                    confirmation
                        .set_response_appearance("import", adw::ResponseAppearance::Destructive);
                    confirmation.set_default_response(Some("cancel"));
                } else {
                    confirmation
                        .set_response_appearance("import", adw::ResponseAppearance::Suggested);
                    confirmation.set_default_response(Some("import"));
                }
                confirmation.set_close_response("cancel");

                let config = config.clone();
                let handle = handle.clone();
                let rebuild_replacements = rebuild_replacements.clone();
                let rebuild_vocabulary = rebuild_vocabulary.clone();
                let toast_overlay = toast_overlay.clone();
                confirmation.connect_response(Some("import"), move |_, _| {
                    *config.borrow_mut() = imported.clone();
                    send_config(&config, &handle);
                    rebuild_replacements();
                    rebuild_vocabulary();
                    toast_overlay.add_toast(adw::Toast::new("Dictionary imported"));
                });
                confirmation.present(Some(&parent));
            });
        });
    }

    DictionaryPage {
        widget: content.upcast(),
        apply_json,
        switcher,
        switcher_bar,
        focus_search,
    }
}

fn build_replacements_view(
    config: &Rc<RefCell<DictionaryConfig>>,
    handle: &DaemonHandle,
    toast_overlay: &adw::ToastOverlay,
) -> (gtk4::Widget, Rc<dyn Fn()>, gtk4::SearchEntry) {
    let clamp = adw::Clamp::builder()
        .maximum_size(600)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(12)
        .margin_end(12)
        .build();
    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 12);

    let add_group = adw::PreferencesGroup::builder()
        .title("Add replacement")
        .description(REPLACEMENT_ADD_DESCRIPTION)
        .build();
    let original_entry = adw::EntryRow::builder().title("When Voxkey hears").build();
    let replacement_entry = adw::EntryRow::builder().title("Type this instead").build();
    let add_button = gtk4::Button::with_label("Add");
    add_button.add_css_class("suggested-action");
    add_button.set_sensitive(false);
    add_button.set_tooltip_text(Some("Add replacement"));
    add_button.update_property(&[gtk4::accessible::Property::Label("Add replacement")]);
    add_button.set_valign(gtk4::Align::Center);
    replacement_entry.add_suffix(&add_button);
    add_group.add(&original_entry);
    add_group.add(&replacement_entry);
    vbox.append(&add_group);

    for entry in [&original_entry, &replacement_entry] {
        let config = config.clone();
        let original_entry = original_entry.clone();
        let replacement_entry = replacement_entry.clone();
        let add_button = add_button.clone();
        let add_group = add_group.clone();
        entry.connect_changed(move |_| {
            add_button.set_sensitive(replacement_can_add(
                &config.borrow(),
                &original_entry.text(),
                &replacement_entry.text(),
            ));
            add_group.set_description(Some(replacement_add_description(
                &config.borrow(),
                &original_entry.text(),
                &replacement_entry.text(),
            )));
        });
    }

    let search = gtk4::SearchEntry::builder()
        .placeholder_text("Search replacements")
        .visible(false)
        .build();
    enable_escape_to_clear(&search);
    vbox.append(&search);

    let list_group = adw::PreferencesGroup::builder()
        .title("Replacements")
        .description("Applied to every transcription before the text is typed")
        .build();
    vbox.append(&list_group);

    let rebuild = make_replacements_rebuild(config, handle, &list_group, &search, toast_overlay);
    rebuild();
    {
        let rebuild = rebuild.clone();
        search.connect_search_changed(move |_| rebuild());
    }

    let do_add = {
        let config = config.clone();
        let handle = handle.clone();
        let original_entry = original_entry.clone();
        let replacement_entry = replacement_entry.clone();
        let search = search.clone();
        let rebuild = rebuild.clone();
        let toast_overlay = toast_overlay.clone();
        move || {
            let added = add_replacement(
                &mut config.borrow_mut(),
                &original_entry.text(),
                &replacement_entry.text(),
            );
            if added {
                send_config(&config, &handle);
                original_entry.set_text("");
                replacement_entry.set_text("");
                search.set_text("");
                rebuild();
                original_entry.grab_focus();
            } else if let Some(error) = replacement_validation_error(
                &config.borrow(),
                &original_entry.text(),
                &replacement_entry.text(),
                None,
            ) {
                toast_overlay.add_toast(adw::Toast::new(error));
            }
        }
    };
    {
        let do_add = do_add.clone();
        add_button.connect_clicked(move |_| do_add());
    }
    {
        let replacement_entry = replacement_entry.clone();
        original_entry.connect_activate(move |_| {
            replacement_entry.grab_focus();
        });
    }
    replacement_entry.connect_activate(move |_| do_add());

    clamp.set_child(Some(&vbox));
    (scroll_dictionary_content(&clamp), rebuild, search)
}

/// Build a closure that clears and repopulates the replacements list from
/// the current config. Rows get an enable switch and a delete button.
fn make_replacements_rebuild(
    config: &Rc<RefCell<DictionaryConfig>>,
    handle: &DaemonHandle,
    group: &adw::PreferencesGroup,
    search: &gtk4::SearchEntry,
    toast_overlay: &adw::ToastOverlay,
) -> Rc<dyn Fn()> {
    let config = config.clone();
    let handle = handle.clone();
    let group = group.clone();
    let search = search.clone();
    let toast_overlay = toast_overlay.clone();
    let rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));

    let rebuild: RebuildSlot = Rc::new(RefCell::new(None));
    let rebuild_impl: Rc<dyn Fn()> = {
        let rebuild = rebuild.clone();
        Rc::new(move || {
            for row in rows.borrow_mut().drain(..) {
                group.remove(&row);
            }
            let snapshot = config.borrow().replacements.clone();
            let query = search.text().trim().to_lowercase();
            search.set_visible(snapshot.len() >= DICTIONARY_SEARCH_THRESHOLD || !query.is_empty());
            let mut filtered: Vec<_> = snapshot
                .iter()
                .enumerate()
                .filter(|(_, rule)| replacement_matches_query(rule, &query))
                .collect();
            filtered.sort_by(|(_, left), (_, right)| {
                compare_dictionary_text(&left.original, &right.original)
                    .then_with(|| compare_dictionary_text(&left.replacement, &right.replacement))
            });
            group.set_description(Some(&replacement_search_description(
                snapshot.len(),
                filtered.len(),
                !query.is_empty(),
            )));
            if snapshot.is_empty() {
                let row = adw::ActionRow::builder()
                    .title("No replacements yet")
                    .subtitle("Example: replace “vox key” with “Voxkey”")
                    .subtitle_lines(2)
                    .sensitive(false)
                    .build();
                let icon = gtk4::Image::from_icon_name("edit-find-replace-symbolic");
                icon.add_css_class("dim-label");
                row.add_prefix(&icon);
                group.add(&row);
                rows.borrow_mut().push(row);
                return;
            }
            if filtered.is_empty() {
                let row = adw::ActionRow::builder()
                    .title("No matching replacements")
                    .subtitle("Try a different search term")
                    .sensitive(false)
                    .build();
                let icon = gtk4::Image::from_icon_name("edit-find-symbolic");
                icon.add_css_class("dim-label");
                row.add_prefix(&icon);
                group.add(&row);
                rows.borrow_mut().push(row);
                return;
            }
            for (index, rule) in filtered {
                let row = adw::ActionRow::builder()
                    .title(&rule.original)
                    .subtitle(replacement_row_subtitle(&rule.replacement, rule.enabled))
                    .title_lines(2)
                    .subtitle_lines(2)
                    .use_markup(false)
                    .build();

                let toggle = gtk4::Switch::builder()
                    .active(rule.enabled)
                    .valign(gtk4::Align::Center)
                    .build();
                let toggle_label = replacement_toggle_action_label(&rule.original, rule.enabled);
                toggle.set_tooltip_text(Some(if rule.enabled {
                    "Pause replacement"
                } else {
                    "Enable replacement"
                }));
                toggle.update_property(&[gtk4::accessible::Property::Label(&toggle_label)]);
                {
                    let config = config.clone();
                    let handle = handle.clone();
                    let expected_rule = Rc::new(RefCell::new(rule.clone()));
                    let original = rule.original.clone();
                    toggle.connect_state_set(move |toggle, state| {
                        if !set_replacement_enabled_if_current(
                            &mut config.borrow_mut(),
                            index,
                            &mut expected_rule.borrow_mut(),
                            state,
                        ) {
                            return glib::Propagation::Proceed;
                        }
                        send_config(&config, &handle);
                        let label = replacement_toggle_action_label(&original, state);
                        toggle.set_tooltip_text(Some(if state {
                            "Pause replacement"
                        } else {
                            "Enable replacement"
                        }));
                        toggle.update_property(&[gtk4::accessible::Property::Label(&label)]);
                        glib::Propagation::Proceed
                    });
                }
                row.add_suffix(&toggle);

                row.set_activatable(true);
                row.set_tooltip_text(Some("Edit replacement"));
                row.update_property(&[gtk4::accessible::Property::Description(
                    "Activate to edit this replacement",
                )]);
                {
                    let config = config.clone();
                    let handle = handle.clone();
                    let rebuild = rebuild.clone();
                    let rule = rule.clone();
                    let toast_overlay = toast_overlay.clone();
                    row.connect_activated(move |row| {
                        let original_entry = adw::EntryRow::builder()
                            .title("When Voxkey hears")
                            .text(&rule.original)
                            .build();
                        let replacement_entry = adw::EntryRow::builder()
                            .title("Type this instead")
                            .text(&rule.replacement)
                            .build();
                        let fields = adw::PreferencesGroup::builder()
                            .description("Separate multiple heard phrases with commas")
                            .build();
                        fields.add(&original_entry);
                        fields.add(&replacement_entry);
                        let form = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
                        form.append(&fields);
                        let validation_label = gtk4::Label::builder()
                            .xalign(0.0)
                            .wrap(true)
                            .visible(false)
                            .build();
                        validation_label.add_css_class("error");
                        form.append(&validation_label);

                        let dialog = adw::AlertDialog::builder()
                            .heading("Edit replacement")
                            .extra_child(&form)
                            .build();
                        dialog.add_response("cancel", "Cancel");
                        dialog.add_response("save", "Save");
                        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
                        dialog.set_default_response(Some("save"));
                        dialog.set_close_response("cancel");

                        {
                            let replacement_entry = replacement_entry.clone();
                            original_entry.connect_activate(move |_| {
                                replacement_entry.grab_focus();
                                replacement_entry.select_region(0, -1);
                            });
                        }
                        {
                            let dialog = dialog.downgrade();
                            replacement_entry.connect_activate(move |_| {
                                let Some(dialog) = dialog.upgrade() else {
                                    return;
                                };
                                if dialog.is_response_enabled("save") {
                                    dialog.emit_by_name::<()>("response", &[&"save"]);
                                }
                            });
                        }

                        for entry in [&original_entry, &replacement_entry] {
                            let dialog = dialog.downgrade();
                            let config = config.clone();
                            let original_entry = original_entry.clone();
                            let replacement_entry = replacement_entry.clone();
                            let validation_label = validation_label.clone();
                            entry.connect_changed(move |_| {
                                let Some(dialog) = dialog.upgrade() else {
                                    return;
                                };
                                let error = replacement_validation_error(
                                    &config.borrow(),
                                    &original_entry.text(),
                                    &replacement_entry.text(),
                                    Some(index),
                                );
                                dialog.set_response_enabled("save", error.is_none());
                                validation_label.set_label(error.unwrap_or_default());
                                validation_label.set_visible(error.is_some());
                            });
                        }

                        let initial_focus = original_entry.clone();
                        let config = config.clone();
                        let handle = handle.clone();
                        let rebuild = rebuild.clone();
                        let expected_rule = rule.clone();
                        let toast_overlay = toast_overlay.clone();
                        dialog.connect_response(None, move |_, response| {
                            if response != "save" {
                                return;
                            }
                            let new_original = original_entry.text().trim().to_string();
                            let new_replacement = replacement_entry.text().trim().to_string();
                            if new_original.is_empty() || new_replacement.is_empty() {
                                return;
                            }
                            if !edit_replacement_if_current(
                                &mut config.borrow_mut(),
                                index,
                                &expected_rule,
                                new_original,
                                new_replacement,
                            ) {
                                toast_overlay.add_toast(adw::Toast::new(
                                    "That replacement changed; reopen it and try again",
                                ));
                                return;
                            }
                            send_config(&config, &handle);
                            if let Some(r) = rebuild.borrow().clone() {
                                r();
                            }
                            toast_overlay.add_toast(adw::Toast::new("Replacement updated"));
                        });
                        dialog.present(Some(&row.root().unwrap()));
                        initial_focus.grab_focus();
                        initial_focus.select_region(0, -1);
                    });
                }

                let delete = gtk4::Button::from_icon_name("user-trash-symbolic");
                delete.add_css_class("flat");
                delete.add_css_class("destructive-action");
                delete.set_valign(gtk4::Align::Center);
                delete.set_tooltip_text(Some("Delete replacement"));
                let delete_label = dictionary_action_label("Delete replacement", &rule.original);
                delete.update_property(&[gtk4::accessible::Property::Label(&delete_label)]);
                {
                    let config = config.clone();
                    let handle = handle.clone();
                    let rebuild = rebuild.clone();
                    let expected_rule = rule.clone();
                    let toast_overlay = toast_overlay.clone();
                    delete.connect_clicked(move |_| {
                        let original_occurrences = config
                            .borrow()
                            .replacements
                            .iter()
                            .filter(|existing| *existing == &expected_rule)
                            .count();
                        if !delete_replacement_if_current(
                            &mut config.borrow_mut(),
                            index,
                            &expected_rule,
                        ) {
                            return;
                        }
                        send_config(&config, &handle);
                        if let Some(r) = rebuild.borrow().clone() {
                            r();
                        }

                        let toast = adw::Toast::builder()
                            .title("Replacement deleted")
                            .button_label("Undo")
                            .timeout(5)
                            .build();
                        let config = config.clone();
                        let handle = handle.clone();
                        let rebuild = rebuild.clone();
                        let expected_rule = expected_rule.clone();
                        toast.connect_button_clicked(move |_| {
                            if !restore_replacement_if_missing(
                                &mut config.borrow_mut(),
                                index,
                                &expected_rule,
                                original_occurrences,
                            ) {
                                return;
                            }
                            send_config(&config, &handle);
                            if let Some(r) = rebuild.borrow().clone() {
                                r();
                            }
                        });
                        toast_overlay.add_toast(toast);
                    });
                }
                row.add_suffix(&delete);
                let details_icon = gtk4::Image::from_icon_name("go-next-symbolic");
                details_icon.add_css_class("dim-label");
                details_icon.set_accessible_role(gtk4::AccessibleRole::Presentation);
                row.add_suffix(&details_icon);

                group.add(&row);
                rows.borrow_mut().push(row);
            }
        })
    };
    *rebuild.borrow_mut() = Some(rebuild_impl.clone());
    rebuild_impl
}

fn build_vocabulary_view(
    config: &Rc<RefCell<DictionaryConfig>>,
    handle: &DaemonHandle,
    toast_overlay: &adw::ToastOverlay,
) -> (gtk4::Widget, Rc<dyn Fn()>, gtk4::SearchEntry) {
    let clamp = adw::Clamp::builder()
        .maximum_size(600)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(12)
        .margin_end(12)
        .build();
    let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 12);

    let add_group = adw::PreferencesGroup::builder()
        .title("Add vocabulary")
        .description(VOCABULARY_ADD_DESCRIPTION)
        .build();
    let word_entry = adw::EntryRow::builder().title("Word or name").build();
    let add_button = gtk4::Button::with_label("Add");
    add_button.add_css_class("suggested-action");
    add_button.set_sensitive(false);
    add_button.set_tooltip_text(Some("Add vocabulary entry"));
    add_button.update_property(&[gtk4::accessible::Property::Label("Add vocabulary entry")]);
    add_button.set_valign(gtk4::Align::Center);
    word_entry.add_suffix(&add_button);
    add_group.add(&word_entry);
    vbox.append(&add_group);

    {
        let add_button = add_button.clone();
        let config = config.clone();
        let add_group = add_group.clone();
        word_entry.connect_changed(move |entry| {
            add_button.set_sensitive(vocabulary_can_add(&config.borrow(), &entry.text()));
            add_group.set_description(Some(vocabulary_add_description(
                &config.borrow(),
                &entry.text(),
            )));
        });
    }

    let search = gtk4::SearchEntry::builder()
        .placeholder_text("Search vocabulary")
        .visible(false)
        .build();
    enable_escape_to_clear(&search);
    vbox.append(&search);

    let list_group = adw::PreferencesGroup::builder()
        .title("Vocabulary")
        .description("Applied to every dictation to improve recognition of these words")
        .build();
    vbox.append(&list_group);

    let rebuild = make_vocabulary_rebuild(config, handle, &list_group, &search, toast_overlay);
    rebuild();
    {
        let rebuild = rebuild.clone();
        search.connect_search_changed(move |_| rebuild());
    }

    let do_add = {
        let config = config.clone();
        let handle = handle.clone();
        let word_entry = word_entry.clone();
        let search = search.clone();
        let rebuild = rebuild.clone();
        let toast_overlay = toast_overlay.clone();
        move || {
            if add_vocabulary_word(&mut config.borrow_mut(), &word_entry.text()) {
                send_config(&config, &handle);
                word_entry.set_text("");
                search.set_text("");
                rebuild();
                word_entry.grab_focus();
            } else if let Some(error) = vocabulary_input_error(&config.borrow(), &word_entry.text())
            {
                toast_overlay.add_toast(adw::Toast::new(error));
            }
        }
    };
    {
        let do_add = do_add.clone();
        add_button.connect_clicked(move |_| do_add());
    }
    word_entry.connect_activate(move |_| do_add());

    clamp.set_child(Some(&vbox));
    (scroll_dictionary_content(&clamp), rebuild, search)
}

fn scroll_dictionary_content(content: &adw::Clamp) -> gtk4::Widget {
    gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(content)
        .build()
        .upcast()
}

fn make_vocabulary_rebuild(
    config: &Rc<RefCell<DictionaryConfig>>,
    handle: &DaemonHandle,
    group: &adw::PreferencesGroup,
    search: &gtk4::SearchEntry,
    toast_overlay: &adw::ToastOverlay,
) -> Rc<dyn Fn()> {
    let config = config.clone();
    let handle = handle.clone();
    let group = group.clone();
    let search = search.clone();
    let toast_overlay = toast_overlay.clone();
    let rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));

    let rebuild: RebuildSlot = Rc::new(RefCell::new(None));
    let rebuild_impl: Rc<dyn Fn()> = {
        let rebuild = rebuild.clone();
        Rc::new(move || {
            for row in rows.borrow_mut().drain(..) {
                group.remove(&row);
            }
            let snapshot = config.borrow().vocabulary.clone();
            let query = search.text().trim().to_lowercase();
            search.set_visible(snapshot.len() >= DICTIONARY_SEARCH_THRESHOLD || !query.is_empty());
            let mut filtered: Vec<_> = snapshot
                .iter()
                .enumerate()
                .filter(|(_, word)| vocabulary_matches_query(word, &query))
                .collect();
            filtered.sort_by(|(_, left), (_, right)| compare_dictionary_text(left, right));
            group.set_description(Some(&vocabulary_search_description(
                snapshot.len(),
                filtered.len(),
                !query.is_empty(),
            )));
            if snapshot.is_empty() {
                let row = adw::ActionRow::builder()
                    .title("No vocabulary yet")
                    .subtitle("Add a name or term above to help Voxkey recognize it")
                    .subtitle_lines(2)
                    .sensitive(false)
                    .build();
                let icon = gtk4::Image::from_icon_name("accessories-dictionary-symbolic");
                icon.add_css_class("dim-label");
                row.add_prefix(&icon);
                group.add(&row);
                rows.borrow_mut().push(row);
                return;
            }
            if filtered.is_empty() {
                let row = adw::ActionRow::builder()
                    .title("No matching vocabulary")
                    .subtitle("Try a different search term")
                    .sensitive(false)
                    .build();
                let icon = gtk4::Image::from_icon_name("edit-find-symbolic");
                icon.add_css_class("dim-label");
                row.add_prefix(&icon);
                group.add(&row);
                rows.borrow_mut().push(row);
                return;
            }
            for (index, word) in filtered {
                let row = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(word))
                    .title_lines(2)
                    .build();
                row.set_activatable(true);
                row.set_tooltip_text(Some("Edit vocabulary entry"));
                row.update_property(&[gtk4::accessible::Property::Description(
                    "Activate to edit this vocabulary entry",
                )]);
                {
                    let config = config.clone();
                    let handle = handle.clone();
                    let rebuild = rebuild.clone();
                    let expected_word = word.clone();
                    let toast_overlay = toast_overlay.clone();
                    row.connect_activated(move |row| {
                        let word_entry = adw::EntryRow::builder()
                            .title("Word or name")
                            .text(&expected_word)
                            .build();
                        let fields = adw::PreferencesGroup::new();
                        fields.add(&word_entry);
                        let form = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
                        form.append(&fields);
                        let validation_label = gtk4::Label::builder()
                            .xalign(0.0)
                            .wrap(true)
                            .visible(false)
                            .build();
                        validation_label.add_css_class("error");
                        form.append(&validation_label);

                        let dialog = adw::AlertDialog::builder()
                            .heading("Edit vocabulary entry")
                            .extra_child(&form)
                            .build();
                        dialog.add_response("cancel", "Cancel");
                        dialog.add_response("save", "Save");
                        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
                        dialog.set_default_response(Some("save"));
                        dialog.set_close_response("cancel");

                        {
                            let dialog = dialog.downgrade();
                            word_entry.connect_activate(move |_| {
                                let Some(dialog) = dialog.upgrade() else {
                                    return;
                                };
                                if dialog.is_response_enabled("save") {
                                    dialog.emit_by_name::<()>("response", &[&"save"]);
                                }
                            });
                        }
                        {
                            let dialog = dialog.downgrade();
                            let config = config.clone();
                            let validation_label = validation_label.clone();
                            word_entry.connect_changed(move |entry| {
                                let Some(dialog) = dialog.upgrade() else {
                                    return;
                                };
                                let error = vocabulary_validation_error(
                                    &config.borrow(),
                                    &entry.text(),
                                    Some(index),
                                );
                                dialog.set_response_enabled("save", error.is_none());
                                validation_label.set_label(error.unwrap_or_default());
                                validation_label.set_visible(error.is_some());
                            });
                        }

                        let initial_focus = word_entry.clone();
                        let config = config.clone();
                        let handle = handle.clone();
                        let rebuild = rebuild.clone();
                        let expected_word = expected_word.clone();
                        let toast_overlay = toast_overlay.clone();
                        dialog.connect_response(Some("save"), move |_, _| {
                            if !edit_vocabulary_word_if_current(
                                &mut config.borrow_mut(),
                                index,
                                &expected_word,
                                word_entry.text().to_string(),
                            ) {
                                toast_overlay.add_toast(adw::Toast::new(
                                    "That vocabulary entry changed; reopen it and try again",
                                ));
                                return;
                            }
                            send_config(&config, &handle);
                            if let Some(rebuild) = rebuild.borrow().clone() {
                                rebuild();
                            }
                            toast_overlay.add_toast(adw::Toast::new("Vocabulary entry updated"));
                        });
                        dialog.present(Some(&row.root().expect("row must belong to the window")));
                        initial_focus.grab_focus();
                        initial_focus.select_region(0, -1);
                    });
                }
                let delete = gtk4::Button::from_icon_name("user-trash-symbolic");
                delete.add_css_class("flat");
                delete.add_css_class("destructive-action");
                delete.set_valign(gtk4::Align::Center);
                delete.set_tooltip_text(Some("Delete vocabulary entry"));
                let delete_label = dictionary_action_label("Delete vocabulary entry", word);
                delete.update_property(&[gtk4::accessible::Property::Label(&delete_label)]);
                {
                    let config = config.clone();
                    let handle = handle.clone();
                    let rebuild = rebuild.clone();
                    let expected_word = word.clone();
                    let toast_overlay = toast_overlay.clone();
                    delete.connect_clicked(move |_| {
                        if !delete_vocabulary_word_if_current(
                            &mut config.borrow_mut(),
                            index,
                            &expected_word,
                        ) {
                            return;
                        }
                        send_config(&config, &handle);
                        if let Some(r) = rebuild.borrow().clone() {
                            r();
                        }

                        let toast = adw::Toast::builder()
                            .title("Vocabulary entry deleted")
                            .button_label("Undo")
                            .timeout(5)
                            .build();
                        let config = config.clone();
                        let handle = handle.clone();
                        let rebuild = rebuild.clone();
                        let expected_word = expected_word.clone();
                        toast.connect_button_clicked(move |_| {
                            if !restore_vocabulary_word_if_missing(
                                &mut config.borrow_mut(),
                                index,
                                &expected_word,
                            ) {
                                return;
                            }
                            send_config(&config, &handle);
                            if let Some(r) = rebuild.borrow().clone() {
                                r();
                            }
                        });
                        toast_overlay.add_toast(toast);
                    });
                }
                row.add_suffix(&delete);
                let details_icon = gtk4::Image::from_icon_name("go-next-symbolic");
                details_icon.add_css_class("dim-label");
                details_icon.set_accessible_role(gtk4::AccessibleRole::Presentation);
                row.add_suffix(&details_icon);
                group.add(&row);
                rows.borrow_mut().push(row);
            }
        })
    };
    *rebuild.borrow_mut() = Some(rebuild_impl.clone());
    rebuild_impl
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_replacement_rejects_blank_sides() {
        let mut config = DictionaryConfig::default();
        assert!(!add_replacement(&mut config, "", "John"));
        assert!(!add_replacement(&mut config, "jon", "  "));
        assert!(config.replacements.is_empty());
    }

    #[test]
    fn dictionary_export_is_readable_pretty_json() {
        let config = DictionaryConfig {
            replacements: vec![WordReplacement {
                original: "vox key".to_string(),
                replacement: "Voxkey".to_string(),
                enabled: true,
            }],
            vocabulary: vec!["PipeWire".to_string()],
        };

        let exported = dictionary_export_json(&config).unwrap();
        assert!(exported.ends_with('\n'));
        assert!(exported.contains("\n  \"replacements\""));
        assert_eq!(
            serde_json::from_str::<DictionaryConfig>(&exported).unwrap(),
            config
        );
    }

    #[test]
    fn dictionary_import_rejects_unknown_empty_and_oversized_backups() {
        assert_eq!(
            dictionary_import_json(b"not json").unwrap_err(),
            "Choose a Voxkey dictionary backup in JSON format"
        );
        assert_eq!(
            dictionary_import_json(br#"{"replacements":[],"vocabulary":[""]}"#,).unwrap_err(),
            "That backup contains an empty dictionary entry"
        );
        assert_eq!(
            dictionary_import_json(&vec![b' '; MAX_DICTIONARY_IMPORT_BYTES + 1]).unwrap_err(),
            "That dictionary backup is larger than 1 MB"
        );
    }

    #[test]
    fn dictionary_import_confirmation_summarizes_what_will_change() {
        let config = DictionaryConfig {
            replacements: vec![WordReplacement {
                original: "vox key".to_string(),
                replacement: "Voxkey".to_string(),
                enabled: true,
            }],
            vocabulary: vec!["PipeWire".to_string(), "Wayland".to_string()],
        };

        assert_eq!(
            dictionary_import_description(&config, false),
            "This backup contains 1 replacement and 2 vocabulary entries."
        );
        assert!(
            dictionary_import_description(&config, true)
                .contains("Importing it replaces your current dictionary")
        );
    }

    #[test]
    fn add_replacement_rejects_an_original_without_any_variants() {
        let mut config = DictionaryConfig::default();

        assert!(!add_replacement(&mut config, " , ,\t, ", "John"));
        assert!(config.replacements.is_empty());
    }

    #[test]
    fn replacement_add_action_requires_a_real_phrase_on_both_sides() {
        let config = DictionaryConfig::default();
        assert!(!replacement_can_add(&config, "", "Voxkey"));
        assert!(!replacement_can_add(&config, " , , ", "Voxkey"));
        assert!(!replacement_can_add(&config, "vox key", "  "));
        assert!(replacement_can_add(&config, "vox key", "Voxkey"));
    }

    #[test]
    fn replacement_add_guidance_explains_each_disabled_state() {
        let mut config = DictionaryConfig::default();
        add_replacement(&mut config, "vox key", "Voxkey");

        assert_eq!(
            replacement_add_description(&config, "", ""),
            REPLACEMENT_ADD_DESCRIPTION
        );
        assert_eq!(
            replacement_add_description(&config, "vox key", ""),
            "Enter replacement text"
        );
        assert_eq!(
            replacement_add_description(&config, "vox key", "Voxkey"),
            "That replacement already exists"
        );
        assert_eq!(
            replacement_add_description(&config, "box key", "Voxkey"),
            "Ready to add this replacement"
        );
    }

    #[test]
    fn add_replacement_trims_and_appends_enabled_rule() {
        let mut config = DictionaryConfig::default();
        assert!(add_replacement(&mut config, " jon ", " John "));
        assert_eq!(config.replacements[0].original, "jon");
        assert_eq!(config.replacements[0].replacement, "John");
        assert!(config.replacements[0].enabled);
    }

    #[test]
    fn exact_duplicate_replacements_are_rejected() {
        let mut config = DictionaryConfig::default();
        assert!(add_replacement(&mut config, "vox key", "Voxkey"));
        assert!(!replacement_can_add(&config, " vox key ", " Voxkey "));
        assert!(!add_replacement(&mut config, "vox key", "Voxkey"));
        assert!(add_replacement(&mut config, "box key", "Voxkey"));

        let second = config.replacements[1].clone();
        assert!(!edit_replacement_if_current(
            &mut config,
            1,
            &second,
            "vox key".to_string(),
            "Voxkey".to_string(),
        ));
        assert_eq!(config.replacements.len(), 2);
    }

    #[test]
    fn replacement_rows_make_the_direction_explicit() {
        assert_eq!(
            replacement_row_subtitle("Voxkey", true),
            "Replace with “Voxkey”"
        );
        assert_eq!(
            replacement_row_subtitle("Voxkey", false),
            "Paused · Replace with “Voxkey”"
        );
    }

    #[test]
    fn dictionary_counts_use_readable_singular_and_plural_labels() {
        assert_eq!(
            replacements_description(1),
            "1 replacement · Applied before text is typed"
        );
        assert_eq!(
            replacements_description(3),
            "3 replacements · Applied before text is typed"
        );
        assert_eq!(
            vocabulary_description(1),
            "1 entry · Used to improve word recognition"
        );
        assert_eq!(
            vocabulary_description(3),
            "3 entries · Used to improve word recognition"
        );
    }

    #[test]
    fn dictionary_row_actions_name_the_item_they_change() {
        assert_eq!(
            replacement_toggle_action_label("vox key", true),
            "Pause replacement: vox key"
        );
        assert_eq!(
            replacement_toggle_action_label("vox key", false),
            "Enable replacement: vox key"
        );
        assert_eq!(
            dictionary_action_label("Delete vocabulary entry", "PipeWire"),
            "Delete vocabulary entry: PipeWire"
        );

        let long_item = "a".repeat(80);
        let label = dictionary_action_label("Edit replacement", &long_item);
        assert!(label.ends_with('…'));
        assert_eq!(label.chars().count(), "Edit replacement: ".len() + 48);
    }

    #[test]
    fn replacement_search_matches_heard_and_output_phrases() {
        let rule = WordReplacement {
            original: "vox key, box key".to_string(),
            replacement: "Voxkey".to_string(),
            enabled: true,
        };

        assert!(replacement_matches_query(&rule, "BOX"));
        assert!(replacement_matches_query(&rule, "voxkey"));
        assert!(replacement_matches_query(&rule, "  "));
        assert!(!replacement_matches_query(&rule, "wayland"));
        assert_eq!(
            replacement_search_description(12, 2, true),
            "2 of 12 replacements"
        );
    }

    #[test]
    fn vocabulary_search_is_case_insensitive_and_reports_filtered_counts() {
        assert!(vocabulary_matches_query("PipeWire", "pipe"));
        assert!(vocabulary_matches_query("PipeWire", " WIRE "));
        assert!(!vocabulary_matches_query("PipeWire", "wayland"));
        assert_eq!(vocabulary_search_description(8, 1, true), "1 of 8 entries");
    }

    #[test]
    fn dictionary_rows_sort_case_insensitively_with_stable_ties() {
        let mut words = vec!["zebra", "Wayland", "alpha", "Alpha"];
        words.sort_by(|left, right| compare_dictionary_text(left, right));

        assert_eq!(words, vec!["Alpha", "alpha", "Wayland", "zebra"]);
    }

    #[test]
    fn escape_clears_only_an_active_dictionary_search() {
        assert!(dictionary_search_should_clear(
            gtk4::gdk::Key::Escape,
            "vox"
        ));
        assert!(!dictionary_search_should_clear(gtk4::gdk::Key::Escape, ""));
        assert!(!dictionary_search_should_clear(
            gtk4::gdk::Key::Return,
            "vox"
        ));
    }

    #[test]
    fn add_vocabulary_word_rejects_blank_and_duplicate() {
        let mut config = DictionaryConfig::default();
        assert!(add_vocabulary_word(&mut config, "Voxkey"));
        assert!(!add_vocabulary_word(&mut config, "Voxkey"));
        assert!(!add_vocabulary_word(&mut config, " voxKEY "));
        assert!(!add_vocabulary_word(&mut config, "  "));
        assert_eq!(config.vocabulary, vec!["Voxkey"]);
    }

    #[test]
    fn vocabulary_add_action_disables_blank_and_duplicate_entries() {
        let config = DictionaryConfig {
            replacements: Vec::new(),
            vocabulary: vec!["Voxkey".to_string()],
        };

        assert!(!vocabulary_can_add(&config, "  "));
        assert!(!vocabulary_can_add(&config, "Voxkey"));
        assert!(!vocabulary_can_add(&config, "voxkey"));
        assert!(vocabulary_can_add(&config, "Wayland"));
    }

    #[test]
    fn vocabulary_add_guidance_explains_duplicates() {
        let config = DictionaryConfig {
            replacements: Vec::new(),
            vocabulary: vec!["Voxkey".to_string()],
        };

        assert_eq!(
            vocabulary_add_description(&config, ""),
            VOCABULARY_ADD_DESCRIPTION
        );
        assert_eq!(
            vocabulary_add_description(&config, "voxkey"),
            "That vocabulary entry already exists"
        );
        assert_eq!(
            vocabulary_add_description(&config, "Wayland"),
            "Ready to add this vocabulary entry"
        );
    }

    #[test]
    fn stale_edit_dialog_does_not_overwrite_a_rebuilt_dictionary_row() {
        let opened_rule = WordReplacement {
            original: "old name".to_string(),
            replacement: "Old Name".to_string(),
            enabled: true,
        };
        let replacement_from_daemon = WordReplacement {
            original: "new term".to_string(),
            replacement: "New Term".to_string(),
            enabled: true,
        };
        let mut config = DictionaryConfig {
            replacements: vec![replacement_from_daemon.clone()],
            vocabulary: Vec::new(),
        };

        let changed = edit_replacement_if_current(
            &mut config,
            0,
            &opened_rule,
            "edited old name".to_string(),
            "Edited Old Name".to_string(),
        );

        assert!(!changed);
        assert_eq!(config.replacements, vec![replacement_from_daemon]);
    }

    #[test]
    fn edit_replacement_rejects_an_original_without_any_variants() {
        let expected = WordReplacement {
            original: "jon".to_string(),
            replacement: "John".to_string(),
            enabled: true,
        };
        let mut config = DictionaryConfig {
            replacements: vec![expected.clone()],
            vocabulary: Vec::new(),
        };

        assert!(!edit_replacement_if_current(
            &mut config,
            0,
            &expected,
            ", ,".to_string(),
            "Jane".to_string(),
        ));
        assert_eq!(config.replacements, vec![expected]);
    }

    #[test]
    fn stale_delete_button_does_not_remove_a_rebuilt_replacement_row() {
        let old_rule = WordReplacement {
            original: "old".to_string(),
            replacement: "Old".to_string(),
            enabled: true,
        };
        let current_rule = WordReplacement {
            original: "current".to_string(),
            replacement: "Current".to_string(),
            enabled: true,
        };
        let mut config = DictionaryConfig {
            replacements: vec![current_rule.clone()],
            vocabulary: Vec::new(),
        };

        assert!(!delete_replacement_if_current(&mut config, 0, &old_rule));
        assert_eq!(config.replacements, vec![current_rule]);
    }

    #[test]
    fn deleted_replacement_can_be_restored_to_its_previous_position() {
        let first = WordReplacement {
            original: "vox key".to_string(),
            replacement: "Voxkey".to_string(),
            enabled: false,
        };
        let second = WordReplacement {
            original: "rust".to_string(),
            replacement: "Rust".to_string(),
            enabled: true,
        };
        let mut config = DictionaryConfig {
            replacements: vec![first.clone(), second.clone()],
            vocabulary: Vec::new(),
        };

        assert!(delete_replacement_if_current(&mut config, 0, &first));
        assert!(restore_replacement_if_missing(&mut config, 0, &first, 1));
        assert_eq!(config.replacements, vec![first, second]);
    }

    #[test]
    fn replacement_undo_restores_only_the_deleted_duplicate() {
        let rule = WordReplacement {
            original: "vox key".to_string(),
            replacement: "Voxkey".to_string(),
            enabled: true,
        };
        let mut config = DictionaryConfig {
            replacements: vec![rule.clone(), rule.clone()],
            vocabulary: Vec::new(),
        };

        assert!(delete_replacement_if_current(&mut config, 0, &rule));
        assert!(restore_replacement_if_missing(&mut config, 0, &rule, 2));
        assert!(!restore_replacement_if_missing(&mut config, 0, &rule, 2));
        assert_eq!(config.replacements, vec![rule.clone(), rule]);
    }

    #[test]
    fn stale_toggle_does_not_change_a_rebuilt_replacement_row() {
        let mut old_rule = WordReplacement {
            original: "old".to_string(),
            replacement: "Old".to_string(),
            enabled: true,
        };
        let current_rule = WordReplacement {
            original: "current".to_string(),
            replacement: "Current".to_string(),
            enabled: true,
        };
        let mut config = DictionaryConfig {
            replacements: vec![current_rule.clone()],
            vocabulary: Vec::new(),
        };

        assert!(!set_replacement_enabled_if_current(
            &mut config,
            0,
            &mut old_rule,
            false,
        ));
        assert_eq!(config.replacements, vec![current_rule]);
    }

    #[test]
    fn stale_delete_button_does_not_remove_a_rebuilt_vocabulary_row() {
        let mut config = DictionaryConfig {
            replacements: Vec::new(),
            vocabulary: vec!["current".to_string()],
        };

        assert!(!delete_vocabulary_word_if_current(&mut config, 0, "old"));
        assert_eq!(config.vocabulary, vec!["current"]);
    }

    #[test]
    fn vocabulary_entries_can_be_renamed_without_creating_duplicates() {
        let mut config = DictionaryConfig {
            replacements: Vec::new(),
            vocabulary: vec!["Voxkey".to_string(), "Wayland".to_string()],
        };

        assert!(edit_vocabulary_word_if_current(
            &mut config,
            0,
            "Voxkey",
            "  PipeWire  ".to_string(),
        ));
        assert_eq!(config.vocabulary[0], "PipeWire");

        assert!(!edit_vocabulary_word_if_current(
            &mut config,
            0,
            "PipeWire",
            "wayland".to_string(),
        ));
        assert_eq!(config.vocabulary, vec!["PipeWire", "Wayland"]);
    }

    #[test]
    fn stale_vocabulary_editor_does_not_overwrite_a_rebuilt_row() {
        let mut config = DictionaryConfig {
            replacements: Vec::new(),
            vocabulary: vec!["Current".to_string()],
        };

        assert!(!edit_vocabulary_word_if_current(
            &mut config,
            0,
            "Old",
            "Edited".to_string(),
        ));
        assert_eq!(config.vocabulary, vec!["Current"]);
    }

    #[test]
    fn deleted_vocabulary_can_be_restored_without_duplicates() {
        let mut config = DictionaryConfig {
            replacements: Vec::new(),
            vocabulary: vec!["Voxkey".to_string(), "Wayland".to_string()],
        };

        assert!(delete_vocabulary_word_if_current(&mut config, 0, "Voxkey"));
        assert!(restore_vocabulary_word_if_missing(&mut config, 0, "Voxkey"));
        assert!(!restore_vocabulary_word_if_missing(
            &mut config,
            0,
            "Voxkey"
        ));
        assert_eq!(config.vocabulary, vec!["Voxkey", "Wayland"]);
    }

    #[test]
    fn vocabulary_restore_respects_case_insensitive_duplicates() {
        let mut config = DictionaryConfig {
            replacements: Vec::new(),
            vocabulary: vec!["voxkey".to_string()],
        };

        assert!(!restore_vocabulary_word_if_missing(
            &mut config,
            0,
            "Voxkey"
        ));
        assert_eq!(config.vocabulary, vec!["voxkey"]);
    }
}
