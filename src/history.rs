// ABOUTME: Persists transcription history and recoverable failed recordings.
// ABOUTME: Keeps bounded, private user data in the Voxkey XDG state directory.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use voxkey_ipc::{HistoryEntry, TranscriberConfig, TranscriberProvider, TranscriptOutcome};

const MAX_HISTORY_ENTRIES: usize = 500;

#[derive(Debug)]
pub struct PreserveFailedRecordingError {
    error: std::io::Error,
    /// A durable copy may exist even when publishing its History entry failed.
    pub saved_path: Option<PathBuf>,
}

impl std::fmt::Display for PreserveFailedRecordingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.saved_path {
            Some(path) => write!(
                formatter,
                "the recording was kept at {}, but History could not be updated: {}",
                path.display(),
                self.error
            ),
            None => write!(
                formatter,
                "the recording could not be preserved: {}",
                self.error
            ),
        }
    }
}

impl std::error::Error for PreserveFailedRecordingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

pub fn load() -> Vec<HistoryEntry> {
    load_from(&history_path())
}

pub fn append(
    entries: &mut Vec<HistoryEntry>,
    text: String,
    config: &TranscriberConfig,
    outcome: TranscriptOutcome,
    pending_insertion: Option<String>,
) -> std::io::Result<u64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let timestamp_id = now.as_nanos().min(u64::MAX as u128) as u64;
    let entry = HistoryEntry {
        id: next_entry_id(entries, timestamp_id),
        recorded_at_unix_ms: now.as_millis().min(i64::MAX as u128) as i64,
        text,
        provider: provider_label(config),
        outcome,
        pending_insertion,
        audio_path: None,
        error: None,
    };

    let id = entry.id;
    let mut updated = entries.clone();
    let removed = push_entry(&mut updated, entry);
    persist(&updated)?;
    *entries = updated;
    remove_managed_recordings(&removed, &history_path());
    Ok(id)
}

pub fn append_failed_recording(
    entries: &mut Vec<HistoryEntry>,
    source_audio: &Path,
    config: &TranscriberConfig,
    error: String,
) -> Result<u64, PreserveFailedRecordingError> {
    append_failed_recording_to(entries, source_audio, config, error, &history_path())
}

fn append_failed_recording_to(
    entries: &mut Vec<HistoryEntry>,
    source_audio: &Path,
    config: &TranscriberConfig,
    error: String,
    history_path: &Path,
) -> Result<u64, PreserveFailedRecordingError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let timestamp_id = now.as_nanos().min(u64::MAX as u128) as u64;
    let id = next_recording_id(entries, timestamp_id, history_path);
    let saved_audio = managed_recording_path(history_path, id);

    crate::persistence::copy_private(source_audio, &saved_audio).map_err(|error| {
        PreserveFailedRecordingError {
            error,
            saved_path: None,
        }
    })?;

    let entry = HistoryEntry {
        id,
        recorded_at_unix_ms: now.as_millis().min(i64::MAX as u128) as i64,
        text: String::new(),
        provider: provider_label(config),
        outcome: TranscriptOutcome::Failed,
        pending_insertion: None,
        audio_path: Some(saved_audio.to_string_lossy().into_owned()),
        error: Some(error),
    };
    let mut updated = entries.clone();
    let removed = push_entry(&mut updated, entry);
    if let Err(error) = persist_to(history_path, &updated) {
        return Err(PreserveFailedRecordingError {
            error,
            saved_path: Some(saved_audio),
        });
    }
    *entries = updated;
    remove_managed_recordings(&removed, history_path);
    Ok(id)
}

fn next_entry_id(entries: &[HistoryEntry], timestamp_id: u64) -> u64 {
    let mut candidate = timestamp_id;
    while entries.iter().any(|entry| entry.id == candidate) {
        candidate = candidate.wrapping_add(1);
    }
    candidate
}

fn next_recording_id(entries: &[HistoryEntry], timestamp_id: u64, history_path: &Path) -> u64 {
    let mut candidate = next_entry_id(entries, timestamp_id);
    while managed_recording_path(history_path, candidate).exists() {
        candidate = next_entry_id(entries, candidate.wrapping_add(1));
    }
    candidate
}

fn push_entry(entries: &mut Vec<HistoryEntry>, entry: HistoryEntry) -> Vec<HistoryEntry> {
    entries.insert(0, entry);
    entries.split_off(entries.len().min(MAX_HISTORY_ENTRIES))
}

pub fn set_pending_insertion(
    entries: &mut Vec<HistoryEntry>,
    id: u64,
    pending_insertion: Option<String>,
) -> std::io::Result<bool> {
    let mut updated = entries.clone();
    let Some(entry) = updated.iter_mut().find(|entry| entry.id == id) else {
        return Ok(false);
    };
    entry.pending_insertion = pending_insertion;
    persist(&updated)?;
    *entries = updated;
    Ok(true)
}

pub fn delete(entries: &mut Vec<HistoryEntry>, id: u64) -> std::io::Result<bool> {
    let removed = entries.iter().find(|entry| entry.id == id).cloned();
    let changed = delete_with(entries, id, persist)?;
    if changed && let Some(entry) = removed {
        remove_managed_recordings(&[entry], &history_path());
    }
    Ok(changed)
}

fn delete_with<F>(entries: &mut Vec<HistoryEntry>, id: u64, persist: F) -> std::io::Result<bool>
where
    F: FnOnce(&[HistoryEntry]) -> std::io::Result<()>,
{
    let mut updated = entries.clone();
    if !remove_entry(&mut updated, id) {
        return Ok(false);
    }
    persist(&updated)?;
    *entries = updated;
    Ok(true)
}

fn remove_entry(entries: &mut Vec<HistoryEntry>, id: u64) -> bool {
    let old_len = entries.len();
    entries.retain(|entry| entry.id != id);
    old_len != entries.len()
}

pub fn clear(entries: &mut Vec<HistoryEntry>) -> std::io::Result<()> {
    let removed = entries.clone();
    clear_with(entries, persist)?;
    remove_managed_recordings(&removed, &history_path());
    Ok(())
}

fn clear_with<F>(entries: &mut Vec<HistoryEntry>, persist: F) -> std::io::Result<()>
where
    F: FnOnce(&[HistoryEntry]) -> std::io::Result<()>,
{
    persist(&[])?;
    entries.clear();
    Ok(())
}

fn provider_label(config: &TranscriberConfig) -> String {
    match config.provider {
        TranscriberProvider::WhisperCpp => "Whisper.cpp".to_string(),
        TranscriberProvider::Mistral => "Mistral".to_string(),
        TranscriberProvider::MistralRealtime => "Mistral Realtime".to_string(),
        TranscriberProvider::Parakeet => {
            let model = voxkey_ipc::model_library::local_model(&config.parakeet.model)
                .map(|model| model.name)
                .unwrap_or(&config.parakeet.model);
            match config.parakeet.backend {
                voxkey_ipc::ParakeetBackend::Local => model.to_string(),
                voxkey_ipc::ParakeetBackend::Http => format!("{model} Server"),
            }
        }
    }
}

pub fn recording_path(entries: &[HistoryEntry], id: u64) -> Result<PathBuf, String> {
    recording_path_from(entries, id, &history_path())
}

fn recording_path_from(
    entries: &[HistoryEntry],
    id: u64,
    history_path: &Path,
) -> Result<PathBuf, String> {
    let entry = entries
        .iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| "The failed dictation is no longer in History".to_string())?;
    if entry.outcome != TranscriptOutcome::Failed {
        return Err("Only failed dictations have a recording to retry".to_string());
    }
    let expected = managed_recording_path(history_path, id);
    let Some(saved) = entry.audio_path.as_deref() else {
        return Err("This failed dictation has no saved recording".to_string());
    };
    if Path::new(saved) != expected {
        return Err("The saved recording path is not managed by Voxkey".to_string());
    }
    if !expected.is_file() {
        return Err("The saved recording is no longer available".to_string());
    }
    Ok(expected)
}

fn managed_recording_path(history_path: &Path, id: u64) -> PathBuf {
    history_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join("recordings")
        .join(format!("{id}.wav"))
}

fn remove_managed_recordings(entries: &[HistoryEntry], history_path: &Path) {
    for entry in entries {
        if entry.audio_path.is_none() {
            continue;
        }
        let path = managed_recording_path(history_path, entry.id);
        if let Err(error) = std::fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                "History was updated, but its recording at {} could not be removed: {error}",
                path.display()
            );
        }
    }
}

fn history_path() -> PathBuf {
    history_path_from(
        std::env::var_os("VOXKEY_HISTORY_PATH").as_deref(),
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

fn history_path_from(
    override_path: Option<&std::ffi::OsStr>,
    xdg_state_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> PathBuf {
    if let Some(path) = override_path.filter(|path| !path.is_empty()) {
        let path = PathBuf::from(path);
        if let Some(expanded) = expand_history_home_path(&path, home) {
            return expanded;
        }
        if !has_home_prefix(&path) {
            // A plain relative override is intentionally useful for isolated
            // test/portable invocations. Only shell-style home prefixes need
            // expansion because environment variable values are not expanded
            // by the shell.
            return path;
        }
        tracing::warn!(
            "VOXKEY_HISTORY_PATH uses a home prefix but HOME is not an absolute path; using the XDG state location"
        );
    }
    let state_home = xdg_state_home
        .filter(|path| !path.is_empty() && Path::new(path).is_absolute())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            home.map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("~"))
                .join(".local/state")
        });
    state_home.join("voxkey").join("history.json")
}

fn has_home_prefix(path: &Path) -> bool {
    path.to_str().is_some_and(|text| {
        matches!(text, "~" | "$HOME" | "${HOME}")
            || text.starts_with("~/")
            || text.starts_with("$HOME/")
            || text.starts_with("${HOME}/")
    })
}

fn expand_history_home_path(path: &Path, home: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    let text = path.to_str()?;
    let suffix = if matches!(text, "~" | "$HOME" | "${HOME}") {
        ""
    } else if let Some(suffix) = text.strip_prefix("~/") {
        suffix
    } else if let Some(suffix) = text.strip_prefix("$HOME/") {
        suffix
    } else {
        text.strip_prefix("${HOME}/")?
    };
    let home = home.map(Path::new).filter(|home| home.is_absolute())?;
    Some(home.join(suffix))
}

fn load_from(path: &Path) -> Vec<HistoryEntry> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!("Could not read transcription history: {error}");
            keep_unreadable_history_aside(path);
            return Vec::new();
        }
    };
    let Some(mut entries) = parse_entries(&contents) else {
        keep_unreadable_history_aside(path);
        return Vec::new();
    };
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.recorded_at_unix_ms));
    entries.truncate(MAX_HISTORY_ENTRIES);
    repair_duplicate_ids(&mut entries);
    entries
}

fn parse_entries(contents: &str) -> Option<Vec<HistoryEntry>> {
    if let Ok(entries) = serde_json::from_str::<Vec<HistoryEntry>>(contents) {
        return Some(entries);
    }

    let saved = serde_json::from_str::<Vec<serde_json::Value>>(contents).ok()?;
    let total = saved.len();
    let entries: Vec<HistoryEntry> = saved
        .into_iter()
        .filter_map(|entry| serde_json::from_value(entry).ok())
        .collect();
    tracing::warn!(
        "Kept {} of {total} saved transcriptions; the rest could not be read",
        entries.len()
    );
    if total > 0 && entries.is_empty() {
        return None;
    }
    Some(entries)
}

fn keep_unreadable_history_aside(path: &Path) {
    let kept = match next_unreadable_history_path(path) {
        Ok(kept) => kept,
        Err(error) => {
            tracing::warn!("Could not choose a path for unreadable history: {error}");
            return;
        }
    };
    match std::fs::rename(path, &kept) {
        Ok(()) => tracing::warn!(
            "Could not read transcription history; kept it at {}",
            kept.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            "Could not preserve unreadable history at {}: {error}",
            kept.display()
        ),
    }
}

fn next_unreadable_history_path(path: &Path) -> std::io::Result<PathBuf> {
    let base = path.with_extension("json.unreadable");
    match std::fs::symlink_metadata(&base) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(base),
        Err(error) => return Err(error),
        Ok(_) => {}
    }

    for suffix in 1..=u64::MAX {
        let mut candidate = base.as_os_str().to_os_string();
        candidate.push(format!(".{suffix}"));
        let candidate = PathBuf::from(candidate);
        match std::fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Err(error) => return Err(error),
            Ok(_) => {}
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no unused unreadable-history path remains",
    ))
}

fn repair_duplicate_ids(entries: &mut [HistoryEntry]) {
    let mut unavailable: HashSet<u64> = entries.iter().map(|entry| entry.id).collect();
    let mut claimed = HashSet::new();

    for entry in entries {
        if claimed.insert(entry.id) {
            continue;
        }

        let mut candidate = entry.id.wrapping_add(1);
        while unavailable.contains(&candidate) {
            candidate = candidate.wrapping_add(1);
        }
        entry.id = candidate;
        unavailable.insert(candidate);
        claimed.insert(candidate);
    }
}

fn persist(entries: &[HistoryEntry]) -> std::io::Result<()> {
    persist_to(&history_path(), entries)
}

#[cfg(test)]
fn persistence_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn persist_to(path: &Path, entries: &[HistoryEntry]) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(entries).map_err(std::io::Error::other)?;
    crate::persistence::write_private(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u64, timestamp: i64) -> HistoryEntry {
        HistoryEntry {
            id,
            recorded_at_unix_ms: timestamp,
            text: format!("Transcript {id}"),
            provider: "whisper.cpp".to_string(),
            outcome: TranscriptOutcome::Completed,
            pending_insertion: None,
            audio_path: None,
            error: None,
        }
    }

    #[test]
    fn persistence_round_trip_is_newest_first_and_private() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested/history.json");
        persist_to(&path, &[entry(1, 100), entry(2, 200)]).unwrap();

        let loaded = load_from(&path);
        assert_eq!(
            loaded.iter().map(|item| item.id).collect::<Vec<_>>(),
            [2, 1]
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn failed_recording_is_copied_privately_and_published_in_history() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("capture.wav");
        let history_path = temp.path().join("state/history.json");
        std::fs::write(&source, b"private captured audio").unwrap();
        let mut entries = Vec::new();

        let id = append_failed_recording_to(
            &mut entries,
            &source,
            &TranscriberConfig::default(),
            "provider rejected the request".to_string(),
            &history_path,
        )
        .unwrap();

        assert_eq!(entries.len(), 1);
        let failed = &entries[0];
        assert_eq!(failed.id, id);
        assert_eq!(failed.outcome, TranscriptOutcome::Failed);
        assert_eq!(
            failed.error.as_deref(),
            Some("provider rejected the request")
        );
        let saved = PathBuf::from(failed.audio_path.as_deref().unwrap());
        assert_eq!(std::fs::read(&saved).unwrap(), b"private captured audio");
        assert!(
            source.exists(),
            "archival must not consume caller ownership"
        );
        assert_eq!(load_from(&history_path), entries);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(saved).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn failed_history_write_reports_the_durable_recording_path() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("capture.wav");
        let history_path = temp.path().join("history.json");
        std::fs::write(&source, b"private captured audio").unwrap();
        std::fs::create_dir(&history_path).unwrap();
        let mut entries = Vec::new();

        let error = append_failed_recording_to(
            &mut entries,
            &source,
            &TranscriberConfig::default(),
            "provider failure".to_string(),
            &history_path,
        )
        .unwrap_err();

        let saved = error
            .saved_path
            .clone()
            .expect("the successful audio copy must remain discoverable");
        assert!(saved.is_file());
        assert!(
            entries.is_empty(),
            "unpublished History state leaked in memory"
        );
        assert!(error.to_string().contains(&saved.display().to_string()));
    }

    #[test]
    fn retry_accepts_only_the_managed_recording_for_the_history_id() {
        let temp = tempfile::tempdir().unwrap();
        let history_path = temp.path().join("history.json");
        let expected = managed_recording_path(&history_path, 7);
        std::fs::create_dir_all(expected.parent().unwrap()).unwrap();
        std::fs::write(&expected, b"audio").unwrap();
        let mut failed = entry(7, 100);
        failed.text.clear();
        failed.outcome = TranscriptOutcome::Failed;
        failed.audio_path = Some(expected.to_string_lossy().into_owned());

        assert_eq!(
            recording_path_from(&[failed.clone()], 7, &history_path).unwrap(),
            expected
        );

        failed.audio_path = Some(
            temp.path()
                .join("unmanaged.wav")
                .to_string_lossy()
                .into_owned(),
        );
        let error = recording_path_from(&[failed], 7, &history_path).unwrap_err();
        assert!(error.contains("not managed"), "{error}");
    }

    #[test]
    fn failed_history_publication_leaves_no_transcript_scratch_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.json");
        std::fs::create_dir(&path).unwrap();

        assert!(persist_to(&path, &[entry(1, 100)]).is_err());

        let entries = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, ["history.json"]);
    }

    #[test]
    fn invalid_history_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(load_from(&path).is_empty());
    }

    #[test]
    fn a_damaged_entry_does_not_discard_intact_history() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.json");
        std::fs::write(
            &path,
            r#"[
              {"id":1,"recorded_at_unix_ms":100,"text":"first","provider":"whisper.cpp"},
              {"id":2,"recorded_at_unix_ms":200,"text":"damaged"},
              {"id":3,"recorded_at_unix_ms":300,"text":"third","provider":"Mistral"}
            ]"#,
        )
        .unwrap();

        let loaded = load_from(&path);

        assert_eq!(
            loaded.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            [3, 1]
        );
    }

    #[test]
    fn history_with_no_readable_entries_is_preserved_as_unreadable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.json");
        let damaged = r#"[{"id":1,"recorded_at_unix_ms":100,"text":"missing provider"}]"#;
        std::fs::write(&path, damaged).unwrap();

        assert!(load_from(&path).is_empty());

        let kept = path.with_extension("json.unreadable");
        assert!(
            !path.exists(),
            "unreadable history remained in the overwrite path"
        );
        assert_eq!(std::fs::read_to_string(kept).unwrap(), damaged);
    }

    #[test]
    fn structurally_unreadable_history_is_moved_out_of_overwrite_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.json");
        let damaged = "[{\"id\":1,\"text\":\"half a fi";
        std::fs::write(&path, damaged).unwrap();

        assert!(load_from(&path).is_empty());

        let kept = path.with_extension("json.unreadable");
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(kept).unwrap(), damaged);
    }

    #[test]
    fn preserving_a_second_damaged_history_does_not_overwrite_the_first() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.json");
        let first_kept = path.with_extension("json.unreadable");
        let second_kept = path.with_extension("json.unreadable.1");
        std::fs::write(&first_kept, "first damaged history").unwrap();
        std::fs::write(&path, "second damaged history").unwrap();

        assert!(load_from(&path).is_empty());

        assert_eq!(
            std::fs::read_to_string(first_kept).unwrap(),
            "first damaged history"
        );
        assert_eq!(
            std::fs::read_to_string(second_kept).unwrap(),
            "second damaged history"
        );
        assert!(!path.exists());
    }

    #[test]
    fn blank_xdg_state_home_uses_the_home_directory_default() {
        assert_eq!(
            history_path_from(
                None,
                Some(std::ffi::OsStr::new("")),
                Some(std::ffi::OsStr::new("/home/test")),
            ),
            PathBuf::from("/home/test/.local/state/voxkey/history.json")
        );
        assert_eq!(
            history_path_from(
                None,
                Some(std::ffi::OsStr::new("relative-state")),
                Some(std::ffi::OsStr::new("/home/test")),
            ),
            PathBuf::from("/home/test/.local/state/voxkey/history.json")
        );
    }

    #[test]
    fn blank_history_override_uses_the_xdg_history_path() {
        assert_eq!(
            history_path_from(
                Some(std::ffi::OsStr::new("")),
                Some(std::ffi::OsStr::new("/state")),
                Some(std::ffi::OsStr::new("/home/test")),
            ),
            PathBuf::from("/state/voxkey/history.json")
        );
    }

    #[test]
    fn history_override_expands_shell_style_home_prefixes() {
        for path in [
            "~/history.json",
            "$HOME/history.json",
            "${HOME}/history.json",
        ] {
            assert_eq!(
                history_path_from(
                    Some(std::ffi::OsStr::new(path)),
                    Some(std::ffi::OsStr::new("/state")),
                    Some(std::ffi::OsStr::new("/home/test")),
                ),
                PathBuf::from("/home/test/history.json")
            );
        }
    }

    #[test]
    fn unexpandable_history_home_prefix_uses_the_xdg_location() {
        assert_eq!(
            history_path_from(
                Some(std::ffi::OsStr::new("~/history.json")),
                Some(std::ffi::OsStr::new("/state")),
                None,
            ),
            PathBuf::from("/state/voxkey/history.json")
        );
    }

    #[test]
    fn bare_history_filename_is_persisted_in_the_current_directory() {
        assert_eq!(
            persistence_parent(Path::new("history.json")),
            Path::new(".")
        );
    }

    #[test]
    fn delete_only_reports_existing_entries() {
        let mut entries = vec![entry(1, 100), entry(2, 200)];
        assert!(!remove_entry(&mut entries, 3));
        assert!(remove_entry(&mut entries, 1));
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn failed_destructive_persistence_keeps_history_in_memory() {
        let original = vec![entry(1, 100), entry(2, 200)];

        let mut after_delete = original.clone();
        assert!(
            delete_with(&mut after_delete, 1, |_| Err(std::io::Error::other(
                "disk full"
            )))
            .is_err()
        );
        assert_eq!(after_delete, original);

        let mut after_clear = original.clone();
        assert!(
            clear_with(&mut after_clear, |_| Err(std::io::Error::other(
                "disk full"
            )))
            .is_err()
        );
        assert_eq!(after_clear, original);
    }

    #[test]
    fn entries_completed_in_the_same_clock_tick_get_distinct_ids() {
        let entries = vec![entry(42, 100)];
        assert_ne!(next_entry_id(&entries, 42), 42);
    }

    #[test]
    fn loading_legacy_history_repairs_duplicate_ids() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.json");
        let legacy = vec![entry(42, 200), entry(42, 100)];
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let loaded = load_from(&path);
        assert_eq!(loaded.len(), 2);
        assert_ne!(loaded[0].id, loaded[1].id);

        let mut after_delete = loaded.clone();
        assert!(remove_entry(&mut after_delete, loaded[0].id));
        assert_eq!(after_delete.len(), 1);
    }

    #[test]
    fn new_entries_are_first_and_history_is_bounded() {
        let mut entries = (0..MAX_HISTORY_ENTRIES as u64)
            .map(|id| entry(id, id as i64))
            .collect::<Vec<_>>();
        let removed = push_entry(&mut entries, entry(9999, 9999));
        assert_eq!(entries.len(), MAX_HISTORY_ENTRIES);
        assert_eq!(entries[0].id, 9999);
        assert_eq!(removed.len(), 1);
    }

    #[test]
    fn provider_label_preserves_the_selected_model() {
        let config = TranscriberConfig {
            provider: TranscriberProvider::Parakeet,
            parakeet: voxkey_ipc::ParakeetConfig {
                model: "parakeet-tdt-0.6b-v3".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(provider_label(&config), "Parakeet v3");

        let newest = TranscriberConfig {
            provider: TranscriberProvider::Parakeet,
            parakeet: voxkey_ipc::ParakeetConfig {
                model: "nemotron-3.5-asr-streaming-0.6b".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(provider_label(&newest), "Nemotron 3.5");
    }

    #[test]
    fn provider_label_uses_the_name_shown_in_settings() {
        assert_eq!(provider_label(&TranscriberConfig::default()), "Whisper.cpp");
    }

    #[test]
    fn provider_label_distinguishes_parakeet_http_server() {
        let config = TranscriberConfig {
            provider: TranscriberProvider::Parakeet,
            parakeet: voxkey_ipc::ParakeetConfig {
                model: "parakeet-tdt-0.6b-v3".to_string(),
                backend: voxkey_ipc::ParakeetBackend::Http,
                endpoint: "http://server.test/v1/audio/transcriptions".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(provider_label(&config), "Parakeet v3 Server");
    }
}
