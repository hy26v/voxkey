// ABOUTME: Persists transcription history and recoverable failed recordings.
// ABOUTME: Keeps bounded, private user data in the Voxkey XDG state directory.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use voxkey_ipc::{
    HistoryEntry, HistoryMetrics, HistoryRetentionConfig, TranscriberConfig, TranscriberProvider,
    TranscriptOutcome,
};

const MILLIS_PER_DAY: i64 = 86_400_000;
const MAX_HISTORY_TEXT_BYTES: usize = 1024 * 1024;

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

/// Persist a candidate history by ownership. Shared state already made the one
/// isolation copy needed to keep its mutex available during disk I/O, so this
/// path avoids cloning every entry a second time.
pub(crate) fn append_owned_with_policy(
    mut entries: Vec<HistoryEntry>,
    text: String,
    config: &TranscriberConfig,
    outcome: TranscriptOutcome,
    pending_insertion: Option<String>,
    metrics: HistoryMetrics,
    retention: &HistoryRetentionConfig,
) -> std::io::Result<(Vec<HistoryEntry>, u64)> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let now_ms = now.as_millis().min(i64::MAX as u128) as i64;
    let timestamp_id = now.as_nanos().min(u64::MAX as u128) as u64;
    let entry = HistoryEntry {
        id: next_entry_id(&entries, timestamp_id),
        recorded_at_unix_ms: now_ms,
        text,
        provider: provider_label(config),
        outcome,
        pending_insertion,
        audio_path: None,
        error: None,
        pinned: false,
        edited_at_unix_ms: None,
        audio_duration_ms: metrics.audio_duration_ms,
        processing_duration_ms: metrics.processing_duration_ms,
    };

    let id = entry.id;
    let removed = push_entry(&mut entries, entry, retention, now_ms);
    persist(&entries)?;
    remove_managed_recordings(&removed, &history_path());
    Ok((entries, id))
}

#[cfg(test)]
fn append_failed_recording_to(
    entries: &mut Vec<HistoryEntry>,
    source_audio: &Path,
    config: &TranscriberConfig,
    error: String,
    metrics: HistoryMetrics,
    retention: &HistoryRetentionConfig,
    history_path: &Path,
) -> Result<u64, PreserveFailedRecordingError> {
    let (updated, id) = append_failed_recording_owned_to(
        entries.clone(),
        source_audio,
        config,
        error,
        metrics,
        retention,
        history_path,
    )?;
    *entries = updated;
    Ok(id)
}

pub(crate) fn append_failed_recording_owned_with_policy(
    entries: Vec<HistoryEntry>,
    source_audio: &Path,
    config: &TranscriberConfig,
    error: String,
    metrics: HistoryMetrics,
    retention: &HistoryRetentionConfig,
) -> Result<(Vec<HistoryEntry>, u64), PreserveFailedRecordingError> {
    append_failed_recording_owned_to(
        entries,
        source_audio,
        config,
        error,
        metrics,
        retention,
        &history_path(),
    )
}

fn append_failed_recording_owned_to(
    mut entries: Vec<HistoryEntry>,
    source_audio: &Path,
    config: &TranscriberConfig,
    error: String,
    metrics: HistoryMetrics,
    retention: &HistoryRetentionConfig,
    history_path: &Path,
) -> Result<(Vec<HistoryEntry>, u64), PreserveFailedRecordingError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let timestamp_id = now.as_nanos().min(u64::MAX as u128) as u64;
    let now_ms = now.as_millis().min(i64::MAX as u128) as i64;
    let id = next_recording_id(&entries, timestamp_id, history_path);
    let saved_audio = managed_recording_path(history_path, id);

    crate::persistence::copy_private(source_audio, &saved_audio).map_err(|error| {
        PreserveFailedRecordingError {
            error,
            saved_path: None,
        }
    })?;

    let entry = HistoryEntry {
        id,
        recorded_at_unix_ms: now_ms,
        text: String::new(),
        provider: provider_label(config),
        outcome: TranscriptOutcome::Failed,
        pending_insertion: None,
        audio_path: Some(saved_audio.to_string_lossy().into_owned()),
        error: Some(error),
        pinned: false,
        edited_at_unix_ms: None,
        audio_duration_ms: metrics.audio_duration_ms,
        processing_duration_ms: metrics.processing_duration_ms,
    };
    let removed = push_entry(&mut entries, entry, retention, now_ms);
    if let Err(error) = persist_to(history_path, &entries) {
        return Err(PreserveFailedRecordingError {
            error,
            saved_path: Some(saved_audio),
        });
    }
    remove_managed_recordings(&removed, history_path);
    Ok((entries, id))
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

fn push_entry(
    entries: &mut Vec<HistoryEntry>,
    entry: HistoryEntry,
    retention: &HistoryRetentionConfig,
    now_ms: i64,
) -> Vec<HistoryEntry> {
    entries.insert(0, entry);
    prune_entries_at(entries, retention, now_ms)
}

fn prune_entries_at(
    entries: &mut Vec<HistoryEntry>,
    retention: &HistoryRetentionConfig,
    now_ms: i64,
) -> Vec<HistoryEntry> {
    let retention = retention.clone().normalized();
    let cutoff = (retention.max_age_days != 0).then(|| {
        now_ms.saturating_sub(i64::from(retention.max_age_days).saturating_mul(MILLIS_PER_DAY))
    });
    let max_entries = retention.max_entries as usize;
    let mut kept = Vec::with_capacity(entries.len());
    let mut removed = Vec::new();
    let mut unpinned_kept = 0_usize;

    for entry in std::mem::take(entries) {
        let expired = cutoff.is_some_and(|cutoff| entry.recorded_at_unix_ms < cutoff);
        if entry.pinned {
            kept.push(entry);
        } else if expired || unpinned_kept >= max_entries {
            removed.push(entry);
        } else {
            unpinned_kept += 1;
            kept.push(entry);
        }
    }
    *entries = kept;
    removed
}

/// Apply retention immediately and delete any managed failed recordings whose
/// entries were removed. Pinned entries are always preserved.
pub fn enforce_retention(
    entries: &mut Vec<HistoryEntry>,
    retention: &HistoryRetentionConfig,
) -> std::io::Result<usize> {
    let (updated, removed) = enforce_retention_owned(entries.clone(), retention)?;
    *entries = updated;
    Ok(removed)
}

pub(crate) fn enforce_retention_owned(
    mut entries: Vec<HistoryEntry>,
    retention: &HistoryRetentionConfig,
) -> std::io::Result<(Vec<HistoryEntry>, usize)> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    let removed = prune_entries_at(&mut entries, retention, now_ms);
    if removed.is_empty() {
        return Ok((entries, 0));
    }
    persist(&entries)?;
    let removed_count = removed.len();
    remove_managed_recordings(&removed, &history_path());
    Ok((entries, removed_count))
}

pub(crate) fn set_pinned_owned(
    mut entries: Vec<HistoryEntry>,
    id: u64,
    pinned: bool,
    retention: &HistoryRetentionConfig,
) -> std::io::Result<(Vec<HistoryEntry>, bool)> {
    let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) else {
        return Ok((entries, false));
    };
    if entry.pinned == pinned {
        return Ok((entries, true));
    }
    entry.pinned = pinned;
    let removed = if pinned {
        Vec::new()
    } else {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(i64::MAX as u128) as i64;
        prune_entries_at(&mut entries, retention, now_ms)
    };
    persist(&entries)?;
    remove_managed_recordings(&removed, &history_path());
    Ok((entries, true))
}

pub(crate) fn update_text_owned(
    entries: Vec<HistoryEntry>,
    id: u64,
    text: &str,
) -> Result<(Vec<HistoryEntry>, bool), String> {
    let edited_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    update_text_owned_at_with(entries, id, text, edited_at_unix_ms, persist)
}

#[cfg(test)]
fn update_text_at_with<F>(
    entries: &mut Vec<HistoryEntry>,
    id: u64,
    text: &str,
    edited_at_unix_ms: i64,
    persist: F,
) -> Result<bool, String>
where
    F: FnOnce(&[HistoryEntry]) -> std::io::Result<()>,
{
    let (updated, changed) =
        update_text_owned_at_with(entries.clone(), id, text, edited_at_unix_ms, persist)?;
    *entries = updated;
    Ok(changed)
}

fn update_text_owned_at_with<F>(
    mut entries: Vec<HistoryEntry>,
    id: u64,
    text: &str,
    edited_at_unix_ms: i64,
    persist: F,
) -> Result<(Vec<HistoryEntry>, bool), String>
where
    F: FnOnce(&[HistoryEntry]) -> std::io::Result<()>,
{
    let text = text.trim();
    if text.is_empty() {
        return Err("A saved transcription cannot be empty".to_string());
    }
    if text.len() > MAX_HISTORY_TEXT_BYTES {
        return Err(format!(
            "A saved transcription cannot exceed {MAX_HISTORY_TEXT_BYTES} bytes"
        ));
    }
    let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) else {
        return Ok((entries, false));
    };
    entry.text = text.to_string();
    entry.edited_at_unix_ms = Some(edited_at_unix_ms);
    // A correction becomes the authoritative content for the next explicit
    // insertion, replacing any stale suffix from the original text.
    entry.pending_insertion = None;
    persist(&entries).map_err(|error| error.to_string())?;
    Ok((entries, true))
}

pub(crate) fn set_pending_insertion_owned(
    mut entries: Vec<HistoryEntry>,
    id: u64,
    pending_insertion: Option<String>,
) -> std::io::Result<(Vec<HistoryEntry>, bool)> {
    let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) else {
        return Ok((entries, false));
    };
    entry.pending_insertion = pending_insertion;
    persist(&entries)?;
    Ok((entries, true))
}

pub(crate) fn delete_owned(
    entries: Vec<HistoryEntry>,
    id: u64,
) -> std::io::Result<(Vec<HistoryEntry>, bool)> {
    let removed = entries.iter().find(|entry| entry.id == id).cloned();
    let (updated, changed) = delete_owned_with(entries, id, persist)?;
    if changed && let Some(entry) = removed {
        remove_managed_recordings(&[entry], &history_path());
    }
    Ok((updated, changed))
}

#[cfg(test)]
fn delete_with<F>(entries: &mut Vec<HistoryEntry>, id: u64, persist: F) -> std::io::Result<bool>
where
    F: FnOnce(&[HistoryEntry]) -> std::io::Result<()>,
{
    let (updated, changed) = delete_owned_with(entries.clone(), id, persist)?;
    *entries = updated;
    Ok(changed)
}

fn delete_owned_with<F>(
    mut entries: Vec<HistoryEntry>,
    id: u64,
    persist: F,
) -> std::io::Result<(Vec<HistoryEntry>, bool)>
where
    F: FnOnce(&[HistoryEntry]) -> std::io::Result<()>,
{
    if !remove_entry(&mut entries, id) {
        return Ok((entries, false));
    }
    persist(&entries)?;
    Ok((entries, true))
}

fn remove_entry(entries: &mut Vec<HistoryEntry>, id: u64) -> bool {
    let old_len = entries.len();
    entries.retain(|entry| entry.id != id);
    old_len != entries.len()
}

pub(crate) fn clear_owned(entries: Vec<HistoryEntry>) -> std::io::Result<Vec<HistoryEntry>> {
    let (kept, removed): (Vec<_>, Vec<_>) = entries.into_iter().partition(|entry| entry.pinned);
    persist(&kept)?;
    remove_managed_recordings(&removed, &history_path());
    Ok(kept)
}

#[cfg(test)]
fn clear_with<F>(entries: &mut Vec<HistoryEntry>, persist: F) -> std::io::Result<()>
where
    F: FnOnce(&[HistoryEntry]) -> std::io::Result<()>,
{
    let kept = clear_owned_with(entries.clone(), persist)?;
    *entries = kept;
    Ok(())
}

#[cfg(test)]
fn clear_owned_with<F>(entries: Vec<HistoryEntry>, persist: F) -> std::io::Result<Vec<HistoryEntry>>
where
    F: FnOnce(&[HistoryEntry]) -> std::io::Result<()>,
{
    let kept = entries
        .iter()
        .filter(|entry| entry.pinned)
        .cloned()
        .collect::<Vec<_>>();
    persist(&kept)?;
    Ok(kept)
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
            ..Default::default()
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
            HistoryMetrics::default(),
            &HistoryRetentionConfig::default(),
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
            HistoryMetrics::default(),
            &HistoryRetentionConfig::default(),
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
        let retention = HistoryRetentionConfig::default();
        let limit = retention.max_entries as usize;
        let mut entries = (0..limit as u64)
            .map(|id| entry(id, id as i64))
            .collect::<Vec<_>>();
        let removed = push_entry(&mut entries, entry(9999, 9999), &retention, 9999);
        assert_eq!(entries.len(), limit);
        assert_eq!(entries[0].id, 9999);
        assert_eq!(removed.len(), 1);
    }

    #[test]
    fn retention_preserves_pins_while_applying_age_and_count_limits() {
        let now = 100 * MILLIS_PER_DAY;
        let mut pinned_old = entry(1, now - 90 * MILLIS_PER_DAY);
        pinned_old.pinned = true;
        let mut entries = (0..27)
            .map(|offset| entry(100 - offset, now - offset as i64 * MILLIS_PER_DAY))
            .collect::<Vec<_>>();
        entries.push(pinned_old);
        entries.push(entry(5, now - 60 * MILLIS_PER_DAY));
        let removed = prune_entries_at(
            &mut entries,
            &HistoryRetentionConfig {
                max_entries: HistoryRetentionConfig::MIN_ENTRIES,
                max_age_days: 30,
            },
            now,
        );

        assert_eq!(
            entries.len(),
            HistoryRetentionConfig::MIN_ENTRIES as usize + 1
        );
        assert_eq!(entries.first().map(|entry| entry.id), Some(100));
        assert_eq!(
            entries[HistoryRetentionConfig::MIN_ENTRIES as usize - 1].id,
            76
        );
        assert!(entries.iter().any(|entry| entry.id == 1 && entry.pinned));
        assert_eq!(removed.len(), 3);
        assert!(removed.iter().any(|entry| entry.id == 75));
        assert!(removed.iter().any(|entry| entry.id == 74));
        assert!(removed.iter().any(|entry| entry.id == 5));
    }

    #[test]
    fn bulk_clear_keeps_pinned_entries() {
        let mut pinned = entry(2, 200);
        pinned.pinned = true;
        let mut entries = vec![entry(3, 300), pinned.clone(), entry(1, 100)];
        let mut persisted = Vec::new();

        clear_with(&mut entries, |updated| {
            persisted = updated.to_vec();
            Ok(())
        })
        .unwrap();

        assert_eq!(entries, vec![pinned.clone()]);
        assert_eq!(persisted, vec![pinned]);
    }

    #[test]
    fn correcting_text_replaces_stale_insertion_content() {
        let mut original = entry(7, 100);
        original.pending_insertion = Some("old suffix".to_string());
        let mut entries = vec![original];

        assert!(
            update_text_at_with(&mut entries, 7, "  corrected text  ", 900, |_| Ok(())).unwrap()
        );
        assert_eq!(entries[0].text, "corrected text");
        assert_eq!(entries[0].edited_at_unix_ms, Some(900));
        assert_eq!(entries[0].pending_insertion, None);
        assert_eq!(entries[0].text_for_insertion(), Some("corrected text"));
        assert!(update_text_at_with(&mut entries, 7, "  ", 901, |_| Ok(())).is_err());
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
