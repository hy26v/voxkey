// ABOUTME: D-Bus interface exposing daemon state and configuration to the settings GUI.
// ABOUTME: Registered on the session bus so the GUI can read properties and call methods.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot, watch};

use crate::config::Config;
use crate::model_download::DownloadStatus;
use crate::state::State;

const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MODEL_DOWNLOAD_CANCEL_TIMEOUT: Duration = Duration::from_secs(5);

/// User actions accepted by the daemon's serialized session event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictationAction {
    Start,
    Stop,
    Cancel,
    InsertLastTranscript,
    RetryHistoryEntry(u64),
}

/// One acknowledged request from a D-Bus client to the active session.
pub struct DictationRequest {
    pub action: DictationAction,
    pub session_generation: u64,
    deadline: tokio::time::Instant,
    response: oneshot::Sender<Result<(), String>>,
}

impl DictationRequest {
    pub fn expired(&self) -> bool {
        self.response.is_closed() || tokio::time::Instant::now() >= self.deadline
    }

    pub fn respond(self, result: Result<(), String>) {
        let _ = self.response.send(result);
    }
}

fn remove_restore_token(path: &std::path::Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    std::fs::remove_file(path)?;
    Ok(true)
}

/// Shared daemon state readable by the D-Bus interface and writable by the event loop.
#[derive(Clone)]
pub struct SharedState {
    inner: Arc<Mutex<SharedStateInner>>,
    persistence_lock: Arc<Mutex<()>>,
    model_status_scan_lock: Arc<tokio::sync::Mutex<()>>,
    restart_signal: Arc<tokio::sync::Notify>,
    shutdown_signal: Arc<tokio::sync::Notify>,
}

struct ConfigurationChangeGuard {
    shared: SharedState,
}

impl Drop for ConfigurationChangeGuard {
    fn drop(&mut self) {
        self.shared
            .inner
            .lock()
            .unwrap()
            .configuration_change_in_progress = false;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastInsertion {
    pub history_id: u64,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastErrorDismissal {
    Cleared,
    AlreadyClear,
    Replaced,
}

struct SharedStateInner {
    state: State,
    configuration_change_in_progress: bool,
    config: Config,
    portal_connected: bool,
    session_generation: u64,
    shortcut_description: String,
    audio_level: f64,
    live_transcript: String,
    live_transcript_generation: u64,
    last_transcript: String,
    last_transcript_entry_id: Option<u64>,
    transcription_history: Vec<voxkey_ipc::HistoryEntry>,
    last_error: String,
    model_downloads: HashMap<String, ActiveModelDownload>,
    model_generations: HashMap<String, u64>,
    settings_lifecycle_generation: u64,
    settings_lifecycle_attached: bool,
}

struct ActiveModelDownload {
    status: watch::Receiver<DownloadStatus>,
    handle: Option<crate::model_download::DownloadHandle>,
}

impl ActiveModelDownload {
    fn managed(handle: crate::model_download::DownloadHandle) -> Self {
        Self {
            status: handle.status(),
            handle: Some(handle),
        }
    }

    #[cfg(test)]
    fn unmanaged(status: watch::Receiver<DownloadStatus>) -> Self {
        Self {
            status,
            handle: None,
        }
    }
}

impl SharedState {
    pub fn new(config: Config) -> Self {
        let transcription_history = crate::history::load();
        let latest_transcript = transcription_history
            .iter()
            .find(|entry| !entry.text.is_empty());
        let last_transcript = latest_transcript
            .map(|entry| entry.text.clone())
            .unwrap_or_default();
        let last_transcript_entry_id = latest_transcript.map(|entry| entry.id);
        Self {
            inner: Arc::new(Mutex::new(SharedStateInner {
                state: State::Idle,
                configuration_change_in_progress: false,
                config,
                portal_connected: false,
                session_generation: 0,
                shortcut_description: String::new(),
                audio_level: 0.0,
                live_transcript: String::new(),
                live_transcript_generation: 0,
                last_transcript,
                last_transcript_entry_id,
                transcription_history,
                last_error: String::new(),
                model_downloads: HashMap::new(),
                model_generations: HashMap::new(),
                settings_lifecycle_generation: 0,
                settings_lifecycle_attached: false,
            })),
            persistence_lock: Arc::new(Mutex::new(())),
            model_status_scan_lock: Arc::new(tokio::sync::Mutex::new(())),
            restart_signal: Arc::new(tokio::sync::Notify::new()),
            shutdown_signal: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub fn state(&self) -> State {
        self.inner.lock().unwrap().state
    }

    pub fn set_state(&self, state: State) {
        self.inner.lock().unwrap().state = state;
    }

    /// Reserve the Idle session for one restart-producing configuration
    /// mutation. The reservation and the state check share one mutex with
    /// dictation admission, so a shortcut cannot race an async setter after
    /// it has checked the state but before it persists the new value.
    fn begin_configuration_change(&self) -> Result<ConfigurationChangeGuard, String> {
        let mut inner = self.inner.lock().unwrap();
        if inner.state != State::Idle {
            return Err(format!(
                "Cannot change settings while Voxkey is {}. Stop or cancel dictation first.",
                inner.state
            ));
        }
        if inner.configuration_change_in_progress {
            return Err("Another settings change is still in progress".to_string());
        }
        inner.configuration_change_in_progress = true;
        // Reject controls that were admitted before this reservation.
        inner.session_generation = inner.session_generation.wrapping_add(1).max(1);
        drop(inner);
        Ok(ConfigurationChangeGuard {
            shared: self.clone(),
        })
    }

    /// Atomically reserve the shared state for capture. Portal activations do
    /// not carry a session generation, so they must observe the same settings
    /// reservation as D-Bus controls.
    pub fn try_begin_dictation(&self, state: State) -> Result<(), String> {
        debug_assert!(matches!(
            state,
            State::Recording | State::Connecting | State::Streaming
        ));
        self.try_begin_idle_operation(state)
    }

    pub fn try_begin_transcription(&self) -> Result<(), String> {
        self.try_begin_idle_operation(State::Transcribing)
    }

    fn try_begin_idle_operation(&self, state: State) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        if inner.configuration_change_in_progress {
            return Err(
                "Cannot start dictation while a settings change is in progress".to_string(),
            );
        }
        if inner.state != State::Idle {
            return Err(format!(
                "Cannot start dictation while Voxkey is {}",
                inner.state
            ));
        }
        inner.state = state;
        Ok(())
    }

    pub fn set_portal_connected(&self, connected: bool) {
        let mut inner = self.inner.lock().unwrap();
        if inner.portal_connected != connected {
            inner.session_generation = inner.session_generation.wrapping_add(1).max(1);
        }
        inner.portal_connected = connected;
        if !connected {
            inner.shortcut_description.clear();
            inner.audio_level = 0.0;
        }
    }

    pub fn session_generation(&self) -> u64 {
        self.inner.lock().unwrap().session_generation
    }

    /// Publish a normalized microphone level. Returns whether D-Bus clients
    /// need a PropertiesChanged notification.
    pub fn set_audio_level(&self, level: f64) -> bool {
        let level = if level.is_finite() {
            level.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let mut inner = self.inner.lock().unwrap();
        if (inner.audio_level - level).abs() < 0.005 {
            return false;
        }
        inner.audio_level = level;
        true
    }

    pub fn set_shortcut_description(&self, description: String) {
        self.inner.lock().unwrap().shortcut_description = description;
    }

    /// Begin a replaceable preview for a new recording. The returned generation
    /// must accompany every update so a slow result from an older recording
    /// cannot overwrite the current one.
    pub fn begin_live_transcript(&self) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        inner.live_transcript_generation = inner.live_transcript_generation.wrapping_add(1).max(1);
        inner.live_transcript.clear();
        inner.live_transcript_generation
    }

    /// Replace the current preview if it still belongs to the active recording.
    /// Returns true only when the exposed D-Bus value actually changed.
    pub fn update_live_transcript(&self, generation: u64, text: String) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if generation != inner.live_transcript_generation || text == inner.live_transcript {
            return false;
        }
        inner.live_transcript = text;
        true
    }

    pub fn record_transcript(
        &self,
        text: String,
        completed_with: &voxkey_ipc::TranscriberConfig,
        outcome: voxkey_ipc::TranscriptOutcome,
        pending_insertion: Option<String>,
    ) -> std::io::Result<u64> {
        self.record_transcript_with(
            text,
            completed_with,
            outcome,
            pending_insertion,
            crate::history::append,
        )
    }

    pub fn record_failed_transcription(
        &self,
        audio_path: &std::path::Path,
        completed_with: &voxkey_ipc::TranscriberConfig,
        error: String,
    ) -> Result<u64, crate::history::PreserveFailedRecordingError> {
        let _serial = self.persistence_lock.lock().unwrap();
        let mut history = self.inner.lock().unwrap().transcription_history.clone();
        let id = crate::history::append_failed_recording(
            &mut history,
            audio_path,
            completed_with,
            error,
        )?;
        self.inner.lock().unwrap().transcription_history = history;
        Ok(id)
    }

    fn record_transcript_with<F>(
        &self,
        text: String,
        completed_with: &voxkey_ipc::TranscriberConfig,
        outcome: voxkey_ipc::TranscriptOutcome,
        pending_insertion: Option<String>,
        append: F,
    ) -> std::io::Result<u64>
    where
        F: FnOnce(
            &mut Vec<voxkey_ipc::HistoryEntry>,
            String,
            &voxkey_ipc::TranscriberConfig,
            voxkey_ipc::TranscriptOutcome,
            Option<String>,
        ) -> std::io::Result<u64>,
    {
        let _serial = self.persistence_lock.lock().unwrap();
        let mut history = self.inner.lock().unwrap().transcription_history.clone();
        let id = append(
            &mut history,
            text.clone(),
            completed_with,
            outcome,
            pending_insertion,
        )?;
        let mut inner = self.inner.lock().unwrap();
        inner.transcription_history = history;
        inner.last_transcript = text;
        inner.last_transcript_entry_id = Some(id);
        Ok(id)
    }

    pub fn set_last_error(&self, text: String) {
        let should_notify = {
            let mut inner = self.inner.lock().unwrap();
            let new = crate::notifications::should_notify_error(&inner.last_error, &text);
            inner.last_error = text.clone();
            new
        };
        if should_notify {
            crate::notifications::last_error(&text);
        }
    }

    fn clear_last_error(&self) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.last_error.is_empty() {
            return false;
        }
        inner.last_error.clear();
        true
    }

    /// Clear only the error whose details the caller actually reviewed. This
    /// keeps a delayed dialog or Shell menu action from erasing a newer failure.
    fn dismiss_last_error(&self, expected: &str) -> LastErrorDismissal {
        let mut inner = self.inner.lock().unwrap();
        if inner.last_error.is_empty() {
            return LastErrorDismissal::AlreadyClear;
        }
        if inner.last_error != expected {
            return LastErrorDismissal::Replaced;
        }
        inner.last_error.clear();
        LastErrorDismissal::Cleared
    }

    pub fn config(&self) -> Config {
        self.inner.lock().unwrap().config.clone()
    }

    pub fn update_config(&self, config: Config) {
        self.inner.lock().unwrap().config = config;
    }

    fn update_config_with<F, P, E>(&self, update: F, persist: P) -> Result<Config, E>
    where
        F: FnOnce(&mut Config),
        P: FnOnce(&Config, &Config) -> Result<(), E>,
    {
        let _serial = self.persistence_lock.lock().unwrap();
        let previous = self.inner.lock().unwrap().config.clone();
        let mut config = previous.clone();
        update(&mut config);
        persist(&previous, &config)?;
        let mut inner = self.inner.lock().unwrap();
        inner.config = config.clone();
        Ok(config)
    }

    fn update_transcriber_config_with<P, E>(
        &self,
        mut transcriber: voxkey_ipc::TranscriberConfig,
        persist: P,
    ) -> Result<Config, E>
    where
        P: FnOnce(&Config, &Config) -> Result<(), E>,
    {
        self.update_config_with(
            move |config| {
                transcriber.mistral.api_key = config.transcriber.mistral.api_key.clone();
                transcriber.mistral_realtime.api_key =
                    config.transcriber.mistral_realtime.api_key.clone();
                transcriber.parakeet.api_key = config.transcriber.parakeet.api_key.clone();
                config.transcriber = transcriber;
            },
            persist,
        )
    }

    fn clear_plaintext_api_key_with<P, E>(&self, service: &str, persist: P) -> Result<(), E>
    where
        P: FnOnce(&Config, &Config) -> Result<(), E>,
    {
        let _serial = self.persistence_lock.lock().unwrap();
        let previous = self.inner.lock().unwrap().config.clone();
        let mut config = previous.clone();
        if !clear_plaintext_api_key(&mut config, service) {
            return Ok(());
        }
        persist(&previous, &config)?;
        let mut inner = self.inner.lock().unwrap();
        inner.config = config;
        Ok(())
    }

    /// Apply a reloaded config and request a session restart so the recorder,
    /// transcriber, and injector are rebuilt from the new values.
    fn apply_reloaded_config(&self, config: Config) {
        self.update_config(config);
        self.request_session_restart();
    }

    fn portal_connected(&self) -> bool {
        self.inner.lock().unwrap().portal_connected
    }

    fn shortcut_description(&self) -> String {
        self.inner.lock().unwrap().shortcut_description.clone()
    }

    fn audio_level(&self) -> f64 {
        self.inner.lock().unwrap().audio_level
    }

    pub fn last_transcript(&self) -> String {
        self.inner.lock().unwrap().last_transcript.clone()
    }

    pub fn last_insertion(&self) -> Option<LastInsertion> {
        let inner = self.inner.lock().unwrap();
        let id = inner.last_transcript_entry_id?;
        let entry = inner
            .transcription_history
            .iter()
            .find(|entry| entry.id == id)?;
        Some(LastInsertion {
            history_id: id,
            text: entry.text_for_insertion()?.to_string(),
        })
    }

    pub fn set_pending_insertion(
        &self,
        history_id: u64,
        pending_insertion: Option<String>,
    ) -> std::io::Result<bool> {
        let _serial = self.persistence_lock.lock().unwrap();
        let mut history = self.inner.lock().unwrap().transcription_history.clone();
        let changed =
            crate::history::set_pending_insertion(&mut history, history_id, pending_insertion)?;
        if changed {
            self.inner.lock().unwrap().transcription_history = history;
        }
        Ok(changed)
    }

    fn live_transcript(&self) -> String {
        self.inner.lock().unwrap().live_transcript.clone()
    }

    fn transcription_history(&self) -> Vec<voxkey_ipc::HistoryEntry> {
        self.inner.lock().unwrap().transcription_history.clone()
    }

    pub fn failed_recording_path(&self, id: u64) -> Result<std::path::PathBuf, String> {
        crate::history::recording_path(&self.inner.lock().unwrap().transcription_history, id)
    }

    fn delete_history_entry(&self, id: u64) -> std::io::Result<bool> {
        self.delete_history_entry_with(id, crate::history::delete)
    }

    fn clear_transcription_history(&self) -> std::io::Result<()> {
        self.clear_transcription_history_with(crate::history::clear)
    }

    fn delete_history_entry_with<F>(&self, id: u64, delete: F) -> std::io::Result<bool>
    where
        F: FnOnce(&mut Vec<voxkey_ipc::HistoryEntry>, u64) -> std::io::Result<bool>,
    {
        let _serial = self.persistence_lock.lock().unwrap();
        let mut history = self.inner.lock().unwrap().transcription_history.clone();
        let changed = delete(&mut history, id)?;
        if changed {
            let latest = history
                .iter()
                .find(|entry| !entry.text.is_empty())
                .map(|entry| (entry.id, entry.text.clone()));
            let mut inner = self.inner.lock().unwrap();
            inner.transcription_history = history;
            inner.last_transcript = latest
                .as_ref()
                .map(|(_, text)| text.clone())
                .unwrap_or_default();
            inner.last_transcript_entry_id = latest.map(|(id, _)| id);
        }
        Ok(changed)
    }

    fn clear_transcription_history_with<F>(&self, clear: F) -> std::io::Result<()>
    where
        F: FnOnce(&mut Vec<voxkey_ipc::HistoryEntry>) -> std::io::Result<()>,
    {
        let _serial = self.persistence_lock.lock().unwrap();
        let mut history = self.inner.lock().unwrap().transcription_history.clone();
        clear(&mut history)?;
        let mut inner = self.inner.lock().unwrap();
        inner.transcription_history = history;
        inner.last_transcript.clear();
        inner.last_transcript_entry_id = None;
        Ok(())
    }

    fn last_error(&self) -> String {
        self.inner.lock().unwrap().last_error.clone()
    }

    pub fn request_session_restart(&self) {
        // Invalidate controls immediately, not only after the event loop has
        // begun portal teardown. A click racing a settings change must never
        // start work in the session built from the old configuration.
        let mut inner = self.inner.lock().unwrap();
        if inner.state != State::Idle {
            tracing::error!(
                state = %inner.state,
                "Refusing a session restart that would discard active dictation work"
            );
            return;
        }
        inner.session_generation = inner.session_generation.wrapping_add(1).max(1);
        drop(inner);
        self.restart_signal.notify_one();
    }

    pub async fn session_restart_requested(&self) {
        self.restart_signal.notified().await;
    }

    pub fn request_shutdown(&self) {
        self.shutdown_signal.notify_one();
    }

    pub async fn shutdown_requested(&self) {
        self.shutdown_signal.notified().await;
    }

    /// Claim responsibility for monitoring the settings application's D-Bus
    /// name. Only one monitor may be active at a time.
    fn begin_settings_lifecycle_monitor(&self) -> Option<u64> {
        let mut inner = self.inner.lock().unwrap();
        if inner.settings_lifecycle_attached {
            return None;
        }
        inner.settings_lifecycle_generation =
            inner.settings_lifecycle_generation.wrapping_add(1).max(1);
        inner.settings_lifecycle_attached = true;
        Some(inner.settings_lifecycle_generation)
    }

    /// Release the current lifecycle monitor. A generation prevents an older
    /// task from detaching a newer monitor after a reconnect race.
    fn finish_settings_lifecycle_monitor(&self, generation: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if !inner.settings_lifecycle_attached || inner.settings_lifecycle_generation != generation {
            return false;
        }
        inner.settings_lifecycle_attached = false;
        true
    }

    /// Begin downloading a model, or follow the download already running for
    /// it. Starting a second one would have both write the same files at once
    /// and leave a corrupt model on disk that still looks complete.
    /// The boolean is true only for the caller that owns progress monitoring.
    pub fn start_model_download(
        &self,
        model_name: String,
    ) -> (watch::Receiver<DownloadStatus>, bool) {
        self.start_model_download_with(model_name, |model_name| {
            ActiveModelDownload::managed(crate::model_download::start_download(model_name))
        })
    }

    fn start_model_download_with<F>(
        &self,
        model_name: String,
        start: F,
    ) -> (watch::Receiver<DownloadStatus>, bool)
    where
        F: FnOnce(String) -> ActiveModelDownload,
    {
        let mut inner = self.inner.lock().unwrap();
        if let Some(running) = inner.model_downloads.get(&model_name)
            && matches!(*running.status.borrow(), DownloadStatus::InProgress(_))
        {
            tracing::info!("Model {model_name} is already downloading; following that download");
            return (running.status.clone(), false);
        }

        let download = start(model_name.clone());
        let status = download.status.clone();
        Self::bump_model_generation(&mut inner, &model_name);
        inner.model_downloads.insert(model_name, download);
        (status, true)
    }

    fn bump_model_generation(inner: &mut SharedStateInner, model_name: &str) {
        let generation = inner
            .model_generations
            .entry(model_name.to_string())
            .or_default();
        *generation = generation.wrapping_add(1).max(1);
    }

    fn model_download_snapshot(&self, model_name: &str) -> (Option<DownloadStatus>, u64) {
        let inner = self.inner.lock().unwrap();
        let status = inner
            .model_downloads
            .get(model_name)
            .map(|download| download.status.borrow().clone());
        let generation = inner
            .model_generations
            .get(model_name)
            .copied()
            .unwrap_or_default();
        (status, generation)
    }

    fn cancel_model_download(
        &self,
        model_name: &str,
    ) -> Result<watch::Receiver<DownloadStatus>, String> {
        let inner = self.inner.lock().unwrap();
        let download = inner
            .model_downloads
            .get(model_name)
            .ok_or_else(|| format!("No download is running for model '{model_name}'"))?;
        if !matches!(*download.status.borrow(), DownloadStatus::InProgress(_)) {
            return Err(format!("No download is running for model '{model_name}'"));
        }
        let handle = download
            .handle
            .as_ref()
            .ok_or_else(|| format!("The download for model '{model_name}' cannot be cancelled"))?;
        handle.cancel();
        Ok(download.status.clone())
    }

    fn finish_model_download(&self, model_name: &str, finished: &watch::Receiver<DownloadStatus>) {
        let mut inner = self.inner.lock().unwrap();
        let is_current = inner
            .model_downloads
            .get(model_name)
            .is_some_and(|current| current.status.same_channel(finished));
        if is_current {
            inner.model_downloads.remove(model_name);
            Self::bump_model_generation(&mut inner, model_name);
        }
    }

    fn delete_model_with<F>(&self, model_name: &str, delete: F) -> Result<(), String>
    where
        F: FnOnce(&str) -> Result<(), std::io::Error>,
    {
        let mut inner = self.inner.lock().unwrap();
        if matches!(
            inner
                .model_downloads
                .get(model_name)
                .map(|download| download.status.borrow().clone()),
            Some(DownloadStatus::InProgress(_))
        ) {
            return Err(format!(
                "Cannot delete model '{model_name}' while it is downloading"
            ));
        }
        delete(model_name).map_err(|error| error.to_string())?;
        Self::bump_model_generation(&mut inner, model_name);
        Ok(())
    }
}

async fn await_model_download_cancellation(
    mut status: watch::Receiver<DownloadStatus>,
) -> Result<(), String> {
    loop {
        match status.borrow().clone() {
            DownloadStatus::InProgress(_) => {}
            DownloadStatus::Cancelled => return Ok(()),
            DownloadStatus::Complete => {
                return Err("The model finished downloading before it could be cancelled".into());
            }
            DownloadStatus::Failed(message) => {
                return Err(format!(
                    "The model download stopped with an error before it could be cancelled: {message}"
                ));
            }
        }

        tokio::time::timeout(MODEL_DOWNLOAD_CANCEL_TIMEOUT, status.changed())
            .await
            .map_err(|_| "Timed out while waiting for the model download to stop".to_string())?
            .map_err(|_| "The model download stopped without reporting its result".to_string())?;
    }
}

fn immediate_model_status(status: Option<&DownloadStatus>) -> Option<&'static str> {
    match status {
        Some(DownloadStatus::InProgress(_)) => Some("downloading"),
        Some(DownloadStatus::Complete) => Some("available"),
        Some(DownloadStatus::Cancelled | DownloadStatus::Failed(_)) | None => None,
    }
}

enum ModelStatusScan {
    Current(&'static str),
    Scanned { generation: u64, available: bool },
}

async fn resolve_model_status_with<F>(
    shared: SharedState,
    model_name: String,
    check_available: F,
) -> Result<String, String>
where
    F: Fn() -> bool + Send + Sync + 'static,
{
    let check_available = Arc::new(check_available);
    loop {
        let (status, _) = shared.model_download_snapshot(&model_name);
        if let Some(status) = immediate_model_status(status.as_ref()) {
            return Ok(status.to_string());
        }

        // Wait for the disk lane asynchronously so queued checks do not occupy
        // Tokio's limited blocking-worker threads. The owned guard then moves
        // into the blocking task, keeping scans serialized even if the D-Bus
        // caller disconnects and drops this future while hashing is underway.
        let scan_guard = shared.model_status_scan_lock.clone().lock_owned().await;
        let scan_shared = shared.clone();
        let scan_model_name = model_name.clone();
        let check_available = check_available.clone();
        let scan = tokio::task::spawn_blocking(move || {
            let _scan_guard = scan_guard;
            let (status, generation) = scan_shared.model_download_snapshot(&scan_model_name);
            if let Some(status) = immediate_model_status(status.as_ref()) {
                return ModelStatusScan::Current(status);
            }
            ModelStatusScan::Scanned {
                generation,
                available: check_available(),
            }
        })
        .await
        .map_err(|error| format!("Model integrity check stopped unexpectedly: {error}"))?;

        let (generation, available) = match scan {
            ModelStatusScan::Current(status) => return Ok(status.to_string()),
            ModelStatusScan::Scanned {
                generation,
                available,
            } => (generation, available),
        };

        let (latest_status, latest_generation) = shared.model_download_snapshot(&model_name);
        if let Some(status) = immediate_model_status(latest_status.as_ref()) {
            return Ok(status.to_string());
        }
        if latest_generation == generation {
            return Ok(if available {
                "available".to_string()
            } else {
                "not_downloaded".to_string()
            });
        }

        // A download completed, a retry started, or a deletion finished while
        // the scan was running. Repeat against the new filesystem generation
        // instead of publishing the stale result over the newer operation.
    }
}

/// D-Bus interface implementation served by the daemon.
pub struct DaemonInterface {
    shared: SharedState,
    control_tx: mpsc::Sender<DictationRequest>,
}

impl DaemonInterface {
    pub fn new(shared: SharedState, control_tx: mpsc::Sender<DictationRequest>) -> Self {
        Self { shared, control_tx }
    }

    fn reserve_idle_configuration_change(&self) -> zbus::fdo::Result<ConfigurationChangeGuard> {
        self.shared
            .begin_configuration_change()
            .map_err(zbus::fdo::Error::Failed)
    }

    async fn request_dictation_action(&self, action: DictationAction) -> zbus::fdo::Result<()> {
        if !self.shared.portal_connected() {
            return Err(zbus::fdo::Error::Failed(
                "Voxkey is reconnecting to the desktop portal".to_string(),
            ));
        }

        let session_generation = self.shared.session_generation();
        let (response, reply) = oneshot::channel();
        let request = DictationRequest {
            action,
            session_generation,
            deadline: tokio::time::Instant::now() + CONTROL_REQUEST_TIMEOUT,
            response,
        };
        let outcome = tokio::time::timeout(CONTROL_REQUEST_TIMEOUT, async {
            self.control_tx
                .send(request)
                .await
                .map_err(|_| "The dictation controller is unavailable".to_string())?;
            reply
                .await
                .map_err(|_| "The dictation controller stopped before replying".to_string())?
        })
        .await
        .map_err(|_| {
            zbus::fdo::Error::Failed("The dictation request timed out safely".to_string())
        })?;

        outcome.map_err(zbus::fdo::Error::Failed)
    }

    pub async fn notify_state(connection: &zbus::Connection) {
        let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        else {
            return;
        };
        let _ = iface_ref
            .get()
            .await
            .state_changed(iface_ref.signal_emitter())
            .await;
    }

    pub async fn notify_portal_connected(connection: &zbus::Connection) {
        let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        else {
            return;
        };
        let iface = iface_ref.get().await;
        let emitter = iface_ref.signal_emitter();
        let _ = iface.portal_connected_changed(emitter).await;
        let _ = iface.shortcut_description_changed(emitter).await;
    }

    pub async fn notify_shortcut_description(connection: &zbus::Connection) {
        let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        else {
            return;
        };
        let _ = iface_ref
            .get()
            .await
            .shortcut_description_changed(iface_ref.signal_emitter())
            .await;
    }

    pub async fn notify_last_error(connection: &zbus::Connection) {
        let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        else {
            return;
        };
        let iface = iface_ref.get().await;
        let emitter = iface_ref.signal_emitter();
        let message = iface.shared.last_error();
        let _ = iface.last_error_changed(emitter).await;
        if !message.is_empty() {
            let _ = Self::error_occurred(emitter, &message).await;
        }
    }

    pub async fn notify_last_transcript(connection: &zbus::Connection) {
        let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        else {
            return;
        };
        let iface = iface_ref.get().await;
        let emitter = iface_ref.signal_emitter();
        let _ = iface.last_transcript_changed(emitter).await;
        let _ = iface.transcription_history_changed(emitter).await;
    }

    pub async fn notify_transcription_complete(connection: &zbus::Connection, text: &str) {
        let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        else {
            return;
        };
        let iface = iface_ref.get().await;
        let emitter = iface_ref.signal_emitter();
        let _ = iface.last_transcript_changed(emitter).await;
        let _ = iface.transcription_history_changed(emitter).await;
        let _ = Self::transcription_complete(emitter, text).await;
    }

    pub async fn notify_live_transcript(connection: &zbus::Connection) {
        let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        else {
            return;
        };
        let _ = iface_ref
            .get()
            .await
            .live_transcript_changed(iface_ref.signal_emitter())
            .await;
    }

    pub async fn notify_audio_level(connection: &zbus::Connection) {
        let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        else {
            return;
        };
        let _ = iface_ref
            .get()
            .await
            .audio_level_changed(iface_ref.signal_emitter())
            .await;
    }
}

/// Attach daemon lifetime to the GTK application's well-known D-Bus name.
///
/// The monitor lives in the daemon, so it still runs when the GUI is killed
/// with SIGKILL. Hiding the window keeps the application name owned and leaves
/// the daemon running.
pub(crate) async fn attach_settings_lifecycle(
    connection: &zbus::Connection,
    shared: SharedState,
) -> Result<bool, String> {
    let connection = connection.clone();
    let (setup_tx, setup_rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let dbus = match zbus::fdo::DBusProxy::new(&connection).await {
            Ok(proxy) => proxy,
            Err(error) => {
                let _ = setup_tx.send(Err(format!(
                    "Could not connect to the session bus manager: {error}"
                )));
                return;
            }
        };
        // Subscribe before checking ownership so an exit cannot slip between
        // the query and signal registration. Filtering new_owner to empty means
        // a direct replacement keeps the daemon linked to the replacement UI.
        let mut owner_changes = match dbus
            .receive_name_owner_changed_with_args(&[(0, voxkey_ipc::SETTINGS_BUS_NAME), (2, "")])
            .await
        {
            Ok(stream) => stream,
            Err(error) => {
                let _ = setup_tx.send(Err(format!(
                    "Could not monitor the settings application: {error}"
                )));
                return;
            }
        };
        let settings_name = match voxkey_ipc::SETTINGS_BUS_NAME.try_into() {
            Ok(name) => name,
            Err(error) => {
                let _ = setup_tx.send(Err(format!("Invalid settings D-Bus name: {error}")));
                return;
            }
        };
        let is_running = match dbus.name_has_owner(settings_name).await {
            Ok(is_running) => is_running,
            Err(error) => {
                let _ = setup_tx.send(Err(format!(
                    "Could not inspect the settings application: {error}"
                )));
                return;
            }
        };
        if !is_running {
            let _ = setup_tx.send(Ok(false));
            return;
        }

        let Some(generation) = shared.begin_settings_lifecycle_monitor() else {
            let _ = setup_tx.send(Ok(true));
            return;
        };
        let _ = setup_tx.send(Ok(true));

        let reason = if owner_changes.next().await.is_some() {
            "settings application exited"
        } else {
            "settings application monitor ended"
        };
        if shared.finish_settings_lifecycle_monitor(generation) {
            tracing::info!("{reason}; requesting graceful daemon shutdown");
            shared.request_shutdown();
        }
    });

    setup_rx
        .await
        .map_err(|_| "Settings lifecycle monitor stopped during setup".to_string())?
}

fn validate_audio_format(sample_rate: u32, channels: u16) -> zbus::fdo::Result<()> {
    if sample_rate == 0 {
        return Err(zbus::fdo::Error::InvalidArgs(
            "Audio sample rate must be greater than zero".to_string(),
        ));
    }
    if channels == 0 {
        return Err(zbus::fdo::Error::InvalidArgs(
            "Audio channel count must be greater than zero".to_string(),
        ));
    }
    Ok(())
}

fn validate_api_key_service(service: &str) -> zbus::fdo::Result<()> {
    match service {
        voxkey_ipc::API_KEY_SERVICE_MISTRAL
        | voxkey_ipc::API_KEY_SERVICE_MISTRAL_REALTIME
        | voxkey_ipc::API_KEY_SERVICE_MODEL_SERVER => Ok(()),
        _ => Err(zbus::fdo::Error::InvalidArgs(format!(
            "Unknown API key service: {service}"
        ))),
    }
}

fn validate_api_key_value(key: &str) -> zbus::fdo::Result<&str> {
    let key = key.trim();
    if key.is_empty() {
        return Err(zbus::fdo::Error::InvalidArgs(
            "API key must not be blank; use ClearApiKey to remove it".to_string(),
        ));
    }
    Ok(key)
}

fn clear_plaintext_api_key(config: &mut Config, service: &str) -> bool {
    let key = match service {
        voxkey_ipc::API_KEY_SERVICE_MISTRAL => &mut config.transcriber.mistral.api_key,
        voxkey_ipc::API_KEY_SERVICE_MISTRAL_REALTIME => {
            &mut config.transcriber.mistral_realtime.api_key
        }
        voxkey_ipc::API_KEY_SERVICE_MODEL_SERVER => &mut config.transcriber.parakeet.api_key,
        _ => return false,
    };
    let changed = !key.is_empty();
    key.clear();
    changed
}

fn public_transcriber_config_json(config: &voxkey_ipc::TranscriberConfig) -> String {
    let mut public = config.clone();
    public.mistral.api_key.clear();
    public.mistral_realtime.api_key.clear();
    public.parakeet.api_key.clear();
    serde_json::to_string(&public).unwrap_or_default()
}

#[zbus::interface(name = "io.github.hy26v.Voxkey.Daemon1")]
impl DaemonInterface {
    #[zbus(property)]
    fn state(&self) -> String {
        self.shared.state().to_string()
    }

    #[zbus(property)]
    fn shortcut_trigger(&self) -> String {
        self.shared.config().shortcut.trigger.clone()
    }

    #[zbus(property)]
    fn shortcut_description(&self) -> String {
        self.shared.shortcut_description()
    }

    /// Current normalized microphone level, from 0.0 (silence) to 1.0.
    #[zbus(property)]
    fn audio_level(&self) -> f64 {
        self.shared.audio_level()
    }

    #[zbus(property)]
    fn transcriber_config(&self) -> String {
        public_transcriber_config_json(&self.shared.config().transcriber)
    }

    #[zbus(property)]
    fn injection_config(&self) -> String {
        serde_json::to_string(&self.shared.config().injection).unwrap_or_default()
    }

    #[zbus(property)]
    fn dictionary_config(&self) -> String {
        serde_json::to_string(&self.shared.config().dictionary).unwrap_or_default()
    }

    #[zbus(property)]
    fn preview_config(&self) -> String {
        serde_json::to_string(&self.shared.config().preview).unwrap_or_default()
    }

    #[zbus(property)]
    fn sample_rate(&self) -> u32 {
        self.shared.config().audio.sample_rate
    }

    #[zbus(property)]
    fn channels(&self) -> u16 {
        self.shared.config().audio.channels
    }

    #[zbus(property)]
    fn portal_connected(&self) -> bool {
        self.shared.portal_connected()
    }

    #[zbus(property)]
    fn last_transcript(&self) -> String {
        self.shared.last_transcript()
    }

    /// Replaceable transcription hypothesis for the recording in progress.
    #[zbus(property)]
    fn live_transcript(&self) -> String {
        self.shared.live_transcript()
    }

    #[zbus(property)]
    fn transcription_history(&self) -> String {
        serde_json::to_string(&self.shared.transcription_history()).unwrap_or_default()
    }

    #[zbus(property)]
    fn audio_input_device(&self) -> String {
        self.shared.config().audio.input_device
    }

    #[zbus(property)]
    fn audio_input_devices(&self) -> String {
        serde_json::to_string(&crate::recorder::available_input_devices()).unwrap_or_default()
    }

    #[zbus(property)]
    fn last_error(&self) -> String {
        self.shared.last_error()
    }

    async fn start_dictation(&self) -> zbus::fdo::Result<()> {
        self.request_dictation_action(DictationAction::Start).await
    }

    async fn stop_dictation(&self) -> zbus::fdo::Result<()> {
        self.request_dictation_action(DictationAction::Stop).await
    }

    async fn cancel_dictation(&self) -> zbus::fdo::Result<()> {
        self.request_dictation_action(DictationAction::Cancel).await
    }

    async fn insert_last_transcript(&self) -> zbus::fdo::Result<()> {
        self.request_dictation_action(DictationAction::InsertLastTranscript)
            .await
    }

    async fn clear_last_error(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        if self.shared.clear_last_error() {
            Self::notify_last_error(connection).await;
        }
        Ok(())
    }

    /// Dismiss the error the caller displayed without clearing a newer one
    /// that may have arrived while its confirmation UI was open.
    async fn dismiss_last_error(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        expected: &str,
    ) -> zbus::fdo::Result<()> {
        match self.shared.dismiss_last_error(expected) {
            LastErrorDismissal::Cleared => Self::notify_last_error(connection).await,
            LastErrorDismissal::AlreadyClear => {}
            LastErrorDismissal::Replaced => {
                return Err(zbus::fdo::Error::Failed(
                    "A newer error is available. Review it before dismissing.".to_string(),
                ));
            }
        }
        Ok(())
    }

    async fn set_shortcut(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        trigger: &str,
    ) -> zbus::fdo::Result<()> {
        crate::config::validate_shortcut_trigger(trigger).map_err(zbus::fdo::Error::InvalidArgs)?;
        let _change = self.reserve_idle_configuration_change()?;
        let mut candidate = self.shared.config().shortcut;
        if candidate.trigger == trigger {
            return Ok(());
        }
        candidate.trigger = trigger.to_string();
        candidate.id = crate::config::shortcut_id_for_trigger(trigger);

        // Do not persist a shortcut until the desktop portal has accepted and
        // returned the requested binding. Otherwise one transient rejection
        // can make every subsequent daemon start fail on the saved value.
        crate::shortcuts::ShortcutController::validate_binding(connection.clone(), &candidate)
            .await
            .map_err(|error| {
                zbus::fdo::Error::Failed(format!("Desktop rejected shortcut: {error}"))
            })?;
        let shortcut_id = candidate.id.clone();
        self.shared
            .update_config_with(
                |config| {
                    config.shortcut.id = shortcut_id;
                    config.shortcut.trigger = trigger.to_string();
                },
                Config::save_delta,
            )
            .map_err(|e| zbus::fdo::Error::Failed(format!("Failed to save config: {e}")))?;

        if let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        {
            let _ = iface_ref
                .get()
                .await
                .shortcut_trigger_changed(iface_ref.signal_emitter())
                .await;
        }

        self.shared.request_session_restart();

        Ok(())
    }

    async fn set_transcriber_config(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        config_json: &str,
    ) -> zbus::fdo::Result<()> {
        let mut transcriber_config: voxkey_ipc::TranscriberConfig =
            serde_json::from_str(config_json).map_err(|e| {
                zbus::fdo::Error::InvalidArgs(format!("Invalid transcriber config JSON: {e}"))
            })?;
        crate::config::normalize_transcriber_config(&mut transcriber_config);
        let _change = self.reserve_idle_configuration_change()?;
        self.shared
            .update_transcriber_config_with(transcriber_config, Config::save_delta)
            .map_err(|e| zbus::fdo::Error::Failed(format!("Failed to save config: {e}")))?;

        if let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        {
            let _ = iface_ref
                .get()
                .await
                .transcriber_config_changed(iface_ref.signal_emitter())
                .await;
        }

        self.shared.request_session_restart();

        Ok(())
    }

    async fn check_transcriber_endpoint(&self, config_json: &str) -> zbus::fdo::Result<String> {
        let mut candidate: voxkey_ipc::TranscriberConfig = serde_json::from_str(config_json)
            .map_err(|error| {
                zbus::fdo::Error::InvalidArgs(format!(
                    "Invalid endpoint-check configuration: {error}"
                ))
            })?;
        crate::config::normalize_transcriber_config(&mut candidate);
        let result = crate::endpoint_check::check(&candidate).await;
        serde_json::to_string(&result).map_err(|error| {
            zbus::fdo::Error::Failed(format!("Could not encode endpoint-check result: {error}"))
        })
    }

    async fn set_injection_config(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        config_json: &str,
    ) -> zbus::fdo::Result<()> {
        let mut injection_config: voxkey_ipc::InjectionConfig = serde_json::from_str(config_json)
            .map_err(|e| {
            zbus::fdo::Error::InvalidArgs(format!("Invalid injection config JSON: {e}"))
        })?;
        crate::config::normalize_injection_config(&mut injection_config);
        let _change = self.reserve_idle_configuration_change()?;
        self.shared
            .update_config_with(
                move |config| config.injection = injection_config,
                Config::save_delta,
            )
            .map_err(|e| zbus::fdo::Error::Failed(format!("Failed to save config: {e}")))?;

        if let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        {
            let _ = iface_ref
                .get()
                .await
                .injection_config_changed(iface_ref.signal_emitter())
                .await;
        }

        self.shared.request_session_restart();

        Ok(())
    }

    async fn set_dictionary_config(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        config_json: &str,
    ) -> zbus::fdo::Result<()> {
        let dictionary_config: voxkey_ipc::DictionaryConfig = serde_json::from_str(config_json)
            .map_err(|e| {
                zbus::fdo::Error::InvalidArgs(format!("Invalid dictionary config JSON: {e}"))
            })?;
        let _change = self.reserve_idle_configuration_change()?;
        self.shared
            .update_config_with(
                move |config| config.dictionary = dictionary_config,
                Config::save_delta,
            )
            .map_err(|e| zbus::fdo::Error::Failed(format!("Failed to save config: {e}")))?;
        crate::transcriber::remove_legacy_hotwords_file();

        if let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        {
            let _ = iface_ref
                .get()
                .await
                .dictionary_config_changed(iface_ref.signal_emitter())
                .await;
        }

        self.shared.request_session_restart();

        Ok(())
    }

    async fn set_preview_config(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        config_json: &str,
    ) -> zbus::fdo::Result<()> {
        let mut preview_config: crate::config::PreviewConfig = serde_json::from_str(config_json)
            .map_err(|error| {
                zbus::fdo::Error::InvalidArgs(format!("Invalid preview config JSON: {error}"))
            })?;
        crate::config::normalize_preview_config(&mut preview_config);
        let _change = self.reserve_idle_configuration_change()?;
        self.shared
            .update_config_with(
                move |config| config.preview = preview_config,
                Config::save_delta,
            )
            .map_err(|error| zbus::fdo::Error::Failed(format!("Failed to save config: {error}")))?;

        if let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        {
            let _ = iface_ref
                .get()
                .await
                .preview_config_changed(iface_ref.signal_emitter())
                .await;
        }

        self.shared.request_session_restart();
        Ok(())
    }

    async fn set_audio(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        sample_rate: u32,
        channels: u16,
    ) -> zbus::fdo::Result<()> {
        validate_audio_format(sample_rate, channels)?;
        let _change = self.reserve_idle_configuration_change()?;
        self.shared
            .update_config_with(
                |config| {
                    config.audio.sample_rate = sample_rate;
                    config.audio.channels = channels;
                },
                Config::save_delta,
            )
            .map_err(|e| zbus::fdo::Error::Failed(format!("Failed to save config: {e}")))?;

        if let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        {
            let iface = iface_ref.get().await;
            let emitter = iface_ref.signal_emitter();
            let _ = iface.sample_rate_changed(emitter).await;
            let _ = iface.channels_changed(emitter).await;
        }

        self.shared.request_session_restart();
        Ok(())
    }

    async fn set_audio_input_device(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        device_name: &str,
    ) -> zbus::fdo::Result<()> {
        if !device_name.is_empty()
            && !crate::recorder::available_input_devices()
                .iter()
                .any(|available| available == device_name)
        {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "Audio input device is not available: {device_name}"
            )));
        }
        let _change = self.reserve_idle_configuration_change()?;
        self.shared
            .update_config_with(
                |config| config.audio.input_device = device_name.to_string(),
                Config::save_delta,
            )
            .map_err(|e| zbus::fdo::Error::Failed(format!("Failed to save config: {e}")))?;

        if let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        {
            let _ = iface_ref
                .get()
                .await
                .audio_input_device_changed(iface_ref.signal_emitter())
                .await;
        }
        self.shared.request_session_restart();
        Ok(())
    }

    async fn delete_history_entry(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        id: u64,
    ) -> zbus::fdo::Result<()> {
        if self.shared.delete_history_entry(id).map_err(|error| {
            zbus::fdo::Error::Failed(format!("Failed to delete history entry: {error}"))
        })? {
            Self::notify_last_transcript(connection).await;
        }
        Ok(())
    }

    async fn clear_transcription_history(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        self.shared.clear_transcription_history().map_err(|error| {
            zbus::fdo::Error::Failed(format!("Failed to clear transcription history: {error}"))
        })?;
        Self::notify_last_transcript(connection).await;
        Ok(())
    }

    async fn retry_history_entry(&self, id: u64) -> zbus::fdo::Result<()> {
        self.request_dictation_action(DictationAction::RetryHistoryEntry(id))
            .await
    }

    async fn reload_config(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        let config = Config::load()
            .map_err(|e| zbus::fdo::Error::Failed(format!("Failed to reload config: {e}")))?;
        let _change = self.reserve_idle_configuration_change()?;
        crate::transcriber::remove_legacy_hotwords_file();
        self.shared.apply_reloaded_config(config);
        tracing::info!("Configuration reloaded via D-Bus");

        if let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        {
            let iface = iface_ref.get().await;
            let emitter = iface_ref.signal_emitter();
            let _ = iface.shortcut_trigger_changed(emitter).await;
            let _ = iface.transcriber_config_changed(emitter).await;
            let _ = iface.injection_config_changed(emitter).await;
            let _ = iface.dictionary_config_changed(emitter).await;
            let _ = iface.preview_config_changed(emitter).await;
            let _ = iface.sample_rate_changed(emitter).await;
            let _ = iface.channels_changed(emitter).await;
            let _ = iface.audio_input_device_changed(emitter).await;
        }

        Ok(())
    }

    async fn quit(&self) -> zbus::fdo::Result<()> {
        tracing::info!("Quit requested via D-Bus");
        self.shared.request_shutdown();
        Ok(())
    }

    async fn attach_settings(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<()> {
        match attach_settings_lifecycle(connection, self.shared.clone()).await {
            Ok(true) => {
                tracing::info!("Daemon lifetime linked to settings application");
                Ok(())
            }
            Ok(false) => Err(zbus::fdo::Error::Failed(
                "The Voxkey settings application is not running".to_string(),
            )),
            Err(error) => Err(zbus::fdo::Error::Failed(error)),
        }
    }

    async fn clear_restore_token(&self) -> zbus::fdo::Result<()> {
        let _change = self.reserve_idle_configuration_change()?;
        let token_path = self
            .shared
            .config()
            .token_path()
            .map_err(zbus::fdo::Error::InvalidArgs)?;
        if remove_restore_token(&token_path)
            .map_err(|e| zbus::fdo::Error::Failed(format!("Failed to remove token: {e}")))?
        {
            tracing::info!("Restore token cleared via D-Bus");
        }
        self.shared.request_session_restart();
        Ok(())
    }

    async fn set_api_key(&self, service: &str, key: &str) -> zbus::fdo::Result<()> {
        validate_api_key_service(service)?;
        let key = validate_api_key_value(key)?;
        let _change = self.reserve_idle_configuration_change()?;
        crate::secret_store::set(service, key).await.map_err(|e| {
            zbus::fdo::Error::Failed(format!("Could not store API key in keyring: {e}"))
        })?;
        self.shared
            .clear_plaintext_api_key_with(service, Config::save_delta)
            .map_err(|e| {
                zbus::fdo::Error::Failed(format!(
                    "Stored API key but could not remove its plaintext fallback: {e}"
                ))
            })?;
        tracing::info!("Stored API key for service '{service}' in keyring");
        self.shared.request_session_restart();
        Ok(())
    }

    async fn clear_api_key(&self, service: &str) -> zbus::fdo::Result<()> {
        validate_api_key_service(service)?;
        let _change = self.reserve_idle_configuration_change()?;
        crate::secret_store::delete(service).await.map_err(|e| {
            zbus::fdo::Error::Failed(format!("Could not delete API key from keyring: {e}"))
        })?;
        self.shared
            .clear_plaintext_api_key_with(service, Config::save_delta)
            .map_err(|e| {
                zbus::fdo::Error::Failed(format!(
                    "Deleted keyring item but could not remove its plaintext fallback: {e}"
                ))
            })?;
        tracing::info!("Cleared API key for service '{service}' from keyring");
        self.shared.request_session_restart();
        Ok(())
    }

    async fn has_api_key(&self, service: &str) -> zbus::fdo::Result<bool> {
        validate_api_key_service(service)?;
        Ok(crate::secret_store::has(service).await)
    }

    async fn download_model(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        model_name: &str,
    ) -> zbus::fdo::Result<()> {
        crate::model_download::validate_model_name(model_name)
            .map_err(zbus::fdo::Error::InvalidArgs)?;
        let model_name = model_name.to_string();
        let (mut rx, starts_monitor) = self.shared.start_model_download(model_name.clone());
        if !starts_monitor {
            return Ok(());
        }
        let connection = connection.clone();
        let shared = self.shared.clone();

        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let status = rx.borrow().clone();
                if let Some(percent) = status.reported_percent()
                    && let Ok(iface_ref) = connection
                        .object_server()
                        .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
                        .await
                {
                    let _ = DaemonInterface::download_progress(
                        iface_ref.signal_emitter(),
                        &model_name,
                        percent,
                    )
                    .await;
                }

                let Some((outcome, message)) = status
                    .terminal_outcome()
                    .map(|(outcome, message)| (outcome, message.to_string()))
                else {
                    continue;
                };
                match &status {
                    DownloadStatus::Complete => {
                        tracing::info!("Model download complete: {model_name}");
                        crate::notifications::info(
                            "Voxkey",
                            &format!("Model {model_name} is ready"),
                        );
                    }
                    DownloadStatus::Cancelled => {
                        tracing::info!("Model download cancelled: {model_name}");
                    }
                    DownloadStatus::Failed(msg) => {
                        tracing::error!("Model download failed: {msg}");
                        shared.set_last_error(format!("Download failed: {msg}"));
                        DaemonInterface::notify_last_error(&connection).await;
                    }
                    DownloadStatus::InProgress(_) => unreachable!("terminal status was checked"),
                }
                if let Ok(iface_ref) = connection
                    .object_server()
                    .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
                    .await
                    && let Err(error) = DaemonInterface::model_download_finished(
                        iface_ref.signal_emitter(),
                        &model_name,
                        outcome.as_wire_value(),
                        &message,
                    )
                    .await
                {
                    tracing::warn!(
                        "Failed to publish the terminal model download result for {model_name}: {error}"
                    );
                }
                break;
            }
            shared.finish_model_download(&model_name, &rx);
        });

        Ok(())
    }

    async fn cancel_model_download(&self, model_name: &str) -> zbus::fdo::Result<()> {
        crate::model_download::validate_model_name(model_name)
            .map_err(zbus::fdo::Error::InvalidArgs)?;
        let status = self
            .shared
            .cancel_model_download(model_name)
            .map_err(zbus::fdo::Error::Failed)?;
        await_model_download_cancellation(status)
            .await
            .map_err(zbus::fdo::Error::Failed)
    }

    async fn delete_model(&self, model_name: &str) -> zbus::fdo::Result<()> {
        let config = self.shared.config();
        let active_model = config.transcriber.provider == voxkey_ipc::TranscriberProvider::Parakeet
            && config.transcriber.parakeet.backend == voxkey_ipc::ParakeetBackend::Local
            && config.transcriber.parakeet.model == model_name;
        let _change = if active_model {
            Some(self.reserve_idle_configuration_change()?)
        } else {
            None
        };

        self.shared
            .delete_model_with(model_name, crate::model_download::delete_model)
            .map_err(|error| {
                zbus::fdo::Error::Failed(format!("Failed to delete model: {error}"))
            })?;

        if active_model {
            self.shared.request_session_restart();
        }
        Ok(())
    }

    async fn model_status(&self, model_name: &str) -> zbus::fdo::Result<String> {
        let model_name = model_name.to_string();
        let checked_model = model_name.clone();
        resolve_model_status_with(self.shared.clone(), model_name, move || {
            crate::models::is_model_available(&checked_model)
        })
        .await
        .map_err(zbus::fdo::Error::Failed)
    }

    #[zbus(signal)]
    async fn transcription_complete(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
        text: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn error_occurred(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
        message: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn download_progress(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
        model_name: &str,
        percent: u8,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn model_download_finished(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
        model_name: &str,
        outcome: &str,
        message: &str,
    ) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_current_settings_lifecycle_monitor_can_finish() {
        let state = SharedState::new(Config::default());

        let first = state.begin_settings_lifecycle_monitor().unwrap();
        assert!(state.begin_settings_lifecycle_monitor().is_none());
        assert!(!state.finish_settings_lifecycle_monitor(first.wrapping_add(1)));
        assert!(state.begin_settings_lifecycle_monitor().is_none());
        assert!(state.finish_settings_lifecycle_monitor(first));

        let second = state.begin_settings_lifecycle_monitor().unwrap();
        assert_ne!(first, second);
        assert!(state.finish_settings_lifecycle_monitor(second));
    }
    use crate::config::Config;

    #[test]
    fn conditional_error_dismissal_preserves_a_newer_failure() {
        let state = SharedState::new(Config::default());
        state.inner.lock().unwrap().last_error = "new failure".to_string();

        assert_eq!(
            state.dismiss_last_error("older failure"),
            LastErrorDismissal::Replaced
        );
        assert_eq!(state.last_error(), "new failure");
        assert_eq!(
            state.dismiss_last_error("new failure"),
            LastErrorDismissal::Cleared
        );
        assert!(state.last_error().is_empty());
        assert_eq!(
            state.dismiss_last_error("new failure"),
            LastErrorDismissal::AlreadyClear
        );
    }

    #[test]
    fn legacy_error_clear_is_idempotent() {
        let state = SharedState::new(Config::default());
        assert!(!state.clear_last_error());

        state.inner.lock().unwrap().last_error = "failure".to_string();
        assert!(state.clear_last_error());
        assert!(!state.clear_last_error());
        assert!(state.last_error().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn clearing_a_restore_token_removes_a_dangling_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let token_path = temp.path().join("restore_token");
        symlink(temp.path().join("missing-target"), &token_path).unwrap();

        assert!(remove_restore_token(&token_path).unwrap());
        assert!(matches!(
            std::fs::symlink_metadata(&token_path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ));
    }

    /// `ReloadConfig` must not merely update `SharedState`: the running
    /// session was built from the prior config, so applying a reload must
    /// also request a session restart to pick up the new values.
    #[test]
    fn apply_reloaded_config_updates_state_and_requests_restart() {
        use futures_util::FutureExt;

        let state = SharedState::new(Config::default());
        let mut changed = Config::default();
        changed.shortcut.trigger = "<Super>x".to_string();

        state.apply_reloaded_config(changed);

        assert_eq!(state.config().shortcut.trigger, "<Super>x");
        assert!(state.session_restart_requested().now_or_never().is_some());
    }

    #[test]
    fn losing_the_portal_clears_the_effective_shortcut_description() {
        let state = SharedState::new(Config::default());
        state.set_shortcut_description("F13".to_string());
        state.set_portal_connected(true);

        assert_eq!(state.shortcut_description(), "F13");

        state.set_portal_connected(false);

        assert_eq!(state.shortcut_description(), "");
    }

    #[test]
    fn portal_replacement_invalidates_queued_control_requests() {
        let state = SharedState::new(Config::default());
        state.set_portal_connected(true);
        let first_session = state.session_generation();

        state.request_session_restart();
        assert_ne!(state.session_generation(), first_session);
        let restart_generation = state.session_generation();

        state.set_portal_connected(false);
        state.set_portal_connected(true);

        assert_ne!(state.session_generation(), restart_generation);
    }

    #[test]
    fn restart_producing_settings_are_rejected_in_every_busy_state() {
        for busy in [
            State::Connecting,
            State::Recording,
            State::Streaming,
            State::Transcribing,
            State::Injecting,
            State::RecoveringSession,
        ] {
            let state = SharedState::new(Config::default());
            state.set_state(busy);

            let error = match state.begin_configuration_change() {
                Ok(_) => panic!("settings mutation was admitted while {busy}"),
                Err(error) => error,
            };

            assert!(error.contains(&busy.to_string()), "{error}");
            assert_eq!(state.state(), busy);
        }
    }

    #[test]
    fn settings_reservation_and_dictation_admission_cannot_race() {
        let state = SharedState::new(Config::default());
        let generation = state.session_generation();
        let change = state.begin_configuration_change().unwrap();

        assert_ne!(state.session_generation(), generation);
        assert!(state.try_begin_dictation(State::Recording).is_err());
        assert_eq!(state.state(), State::Idle);

        drop(change);
        state.try_begin_dictation(State::Recording).unwrap();
        assert_eq!(state.state(), State::Recording);
    }

    #[test]
    fn session_restart_itself_refuses_to_discard_active_work() {
        use futures_util::FutureExt;

        let state = SharedState::new(Config::default());
        state.set_state(State::Transcribing);
        let generation = state.session_generation();

        state.request_session_restart();

        assert_eq!(state.session_generation(), generation);
        assert!(state.session_restart_requested().now_or_never().is_none());
    }

    #[test]
    fn microphone_level_is_clamped_and_only_reports_material_changes() {
        let state = SharedState::new(Config::default());
        assert!(state.set_audio_level(2.0));
        assert_eq!(state.audio_level(), 1.0);
        assert!(!state.set_audio_level(0.999));
        assert!(state.set_audio_level(-1.0));
        assert_eq!(state.audio_level(), 0.0);
        assert!(!state.set_audio_level(f64::NAN));
        assert_eq!(state.audio_level(), 0.0);
    }

    #[test]
    fn a_failed_config_save_does_not_change_live_state() {
        let state = SharedState::new(Config::default());
        let original = state.config().shortcut.trigger;

        let result = state.update_config_with(
            |config| config.shortcut.trigger = "<Super>x".to_string(),
            |_, _| Err::<(), _>("disk is read-only"),
        );

        assert!(result.is_err());
        assert_eq!(state.config().shortcut.trigger, original);
    }

    #[test]
    fn slow_history_persistence_does_not_hold_the_shared_state_mutex() {
        let state = SharedState::new(Config::default());
        let writer_state = state.clone();
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let writer_gate = gate.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();

        let writer = std::thread::spawn(move || {
            writer_state.record_transcript_with(
                "durable text".to_string(),
                &voxkey_ipc::TranscriberConfig::default(),
                voxkey_ipc::TranscriptOutcome::Completed,
                None,
                |entries, text, config, outcome, pending| {
                    entered_tx.send(()).unwrap();
                    let (open, wake) = &*writer_gate;
                    let mut open = open.lock().unwrap();
                    while !*open {
                        open = wake.wait(open).unwrap();
                    }
                    entries.insert(
                        0,
                        voxkey_ipc::HistoryEntry {
                            id: 7,
                            recorded_at_unix_ms: 0,
                            text,
                            provider: format!("{:?}", config.provider),
                            outcome,
                            pending_insertion: pending,
                            audio_path: None,
                            error: None,
                        },
                    );
                    Ok(7)
                },
            )
        });
        entered_rx.recv().unwrap();

        let reader_state = state.clone();
        let (read_tx, read_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || read_tx.send(reader_state.state()).unwrap());
        let observed = read_rx.recv_timeout(Duration::from_millis(100));

        let (open, wake) = &*gate;
        *open.lock().unwrap() = true;
        wake.notify_all();
        writer.join().unwrap().unwrap();

        assert_eq!(observed.unwrap(), State::Idle);
    }

    /// Two downloads of one model write the same files at the same time and
    /// leave a corrupt model behind, so a repeat request must join the
    /// download already running.
    ///
    /// Deliberately not a `#[tokio::test]`: starting a second download calls
    /// `tokio::spawn`, which panics outside a runtime. Passing here is proof
    /// that no second download was started.
    #[test]
    fn a_repeated_download_request_joins_the_one_already_running() {
        let state = SharedState::new(Config::default());
        let (progress, watcher) = watch::channel(DownloadStatus::InProgress(42));
        state.inner.lock().unwrap().model_downloads.insert(
            "parakeet-tdt-0.6b-v3".to_string(),
            ActiveModelDownload::unmanaged(watcher),
        );

        let (joined, _) = state.start_model_download("parakeet-tdt-0.6b-v3".to_string());

        assert!(matches!(*joined.borrow(), DownloadStatus::InProgress(42)));
        drop(progress);
    }

    #[test]
    fn a_joined_download_does_not_start_a_second_notification_monitor() {
        let state = SharedState::new(Config::default());
        let (_progress, watcher) = watch::channel(DownloadStatus::InProgress(42));
        state.inner.lock().unwrap().model_downloads.insert(
            "parakeet-tdt-0.6b-v3".to_string(),
            ActiveModelDownload::unmanaged(watcher),
        );

        let (_joined, starts_monitor) =
            state.start_model_download("parakeet-tdt-0.6b-v3".to_string());

        assert!(!starts_monitor);
    }

    #[test]
    fn cancelling_requires_a_running_download() {
        let state = SharedState::new(Config::default());

        let error = state
            .cancel_model_download("parakeet-tdt-0.6b-v3")
            .expect_err("an idle cancellation request must be rejected");

        assert!(error.contains("No download is running"), "{error}");
    }

    #[tokio::test]
    async fn cancellation_waits_for_the_transfer_terminal_state() {
        let (progress, status) = watch::channel(DownloadStatus::InProgress(73));
        let waiter = tokio::spawn(await_model_download_cancellation(status));

        tokio::task::yield_now().await;
        progress.send(DownloadStatus::Cancelled).unwrap();

        assert_eq!(waiter.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn a_completed_transfer_is_not_reported_as_cancelled() {
        let (_progress, status) = watch::channel(DownloadStatus::Complete);

        let error = await_model_download_cancellation(status)
            .await
            .expect_err("completion won the cancellation race");

        assert!(error.contains("finished downloading"), "{error}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slow_model_status_scan_keeps_async_work_responsive_and_yields_to_a_download() {
        let state = SharedState::new(Config::default());
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let scan_gate = gate.clone();
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let scan_state = state.clone();
        let scan = tokio::spawn(resolve_model_status_with(
            scan_state,
            "model".to_string(),
            move || {
                let _ = entered_tx.send(());
                let (open, wake) = &*scan_gate;
                let mut open = open.lock().unwrap();
                while !*open {
                    let (next, timeout) = wake.wait_timeout(open, Duration::from_secs(1)).unwrap();
                    open = next;
                    if timeout.timed_out() {
                        break;
                    }
                }
                false
            },
        ));

        tokio::time::timeout(Duration::from_millis(200), entered_rx.recv())
            .await
            .expect("filesystem scan blocked the single-threaded async runtime")
            .expect("filesystem scan stopped before it began");
        let (_progress, watcher) = watch::channel(DownloadStatus::InProgress(12));
        state.start_model_download_with("model".to_string(), |_| {
            ActiveModelDownload::unmanaged(watcher)
        });

        let (open, wake) = &*gate;
        *open.lock().unwrap() = true;
        wake.notify_all();

        assert_eq!(scan.await.unwrap(), Ok("downloading".to_string()));
    }

    #[tokio::test]
    async fn a_finished_download_during_a_scan_forces_a_fresh_filesystem_result() {
        let state = SharedState::new(Config::default());
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let scan_gate = gate.clone();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let scan_calls = calls.clone();
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let scan = tokio::spawn(resolve_model_status_with(
            state.clone(),
            "model".to_string(),
            move || {
                let call = scan_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call == 0 {
                    let _ = entered_tx.send(());
                    let (open, wake) = &*scan_gate;
                    let mut open = open.lock().unwrap();
                    while !*open {
                        let (next, timeout) =
                            wake.wait_timeout(open, Duration::from_secs(1)).unwrap();
                        open = next;
                        if timeout.timed_out() {
                            break;
                        }
                    }
                    false
                } else {
                    true
                }
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), entered_rx.recv())
            .await
            .expect("model integrity scan did not start")
            .expect("model integrity scan stopped before it began");

        let (progress, watcher) = watch::channel(DownloadStatus::InProgress(99));
        let (status, _) = state.start_model_download_with("model".to_string(), |_| {
            ActiveModelDownload::unmanaged(watcher)
        });
        progress.send(DownloadStatus::Complete).unwrap();
        state.finish_model_download("model", &status);
        let (open, wake) = &*gate;
        *open.lock().unwrap() = true;
        wake.notify_all();

        assert_eq!(scan.await.unwrap(), Ok("available".to_string()));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cancelling_a_status_caller_does_not_release_its_running_disk_scan() {
        let state = SharedState::new(Config::default());
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let first_gate = gate.clone();
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let first = tokio::spawn(resolve_model_status_with(
            state.clone(),
            "first".to_string(),
            move || {
                let _ = entered_tx.send(());
                let (open, wake) = &*first_gate;
                let mut open = open.lock().unwrap();
                while !*open {
                    let (next, timeout) = wake.wait_timeout(open, Duration::from_secs(1)).unwrap();
                    open = next;
                    if timeout.timed_out() {
                        break;
                    }
                }
                false
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), entered_rx.recv())
            .await
            .expect("first model integrity scan did not start")
            .expect("first model integrity scan stopped before it began");
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());

        let second_entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let second_flag = second_entered.clone();
        let second = tokio::spawn(resolve_model_status_with(
            state,
            "second".to_string(),
            move || {
                second_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                false
            },
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !second_entered.load(std::sync::atomic::Ordering::SeqCst),
            "two large model files were scanned at the same time"
        );

        let (open, wake) = &*gate;
        *open.lock().unwrap() = true;
        wake.notify_all();
        assert_eq!(second.await.unwrap(), Ok("not_downloaded".to_string()));
        assert!(second_entered.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// A download that already finished must not block a later attempt, so a
    /// failed download can be retried from the settings window.
    #[tokio::test]
    async fn a_finished_download_can_be_started_again() {
        let state = SharedState::new(Config::default());
        let (progress, watcher) = watch::channel(DownloadStatus::Failed("earlier".to_string()));
        state
            .inner
            .lock()
            .unwrap()
            .model_downloads
            // Unknown to the downloader, so the retry fails immediately
            // instead of reaching the network.
            .insert(
                "voxkey-unknown-model".to_string(),
                ActiveModelDownload::unmanaged(watcher),
            );

        let (restarted, _) = state.start_model_download("voxkey-unknown-model".to_string());

        let status = restarted.borrow().clone();
        assert!(
            !matches!(&status, DownloadStatus::Failed(message) if message == "earlier"),
            "the retry reported the previous failure instead of starting over"
        );
        drop(progress);
    }

    #[test]
    fn an_old_download_cannot_remove_a_new_retry_watcher() {
        let state = SharedState::new(Config::default());
        let (_old_tx, old_rx) = watch::channel(DownloadStatus::Complete);
        let (_new_tx, new_rx) = watch::channel(DownloadStatus::InProgress(0));
        state.inner.lock().unwrap().model_downloads.insert(
            "model".to_string(),
            ActiveModelDownload::unmanaged(new_rx.clone()),
        );

        state.finish_model_download("model", &old_rx);

        let current = state
            .inner
            .lock()
            .unwrap()
            .model_downloads
            .get("model")
            .map(|download| download.status.clone());
        assert!(current.is_some_and(|status| status.same_channel(&new_rx)));
    }

    #[test]
    fn simultaneous_download_requests_start_only_one_transfer() {
        let state = SharedState::new(Config::default());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

        let spawn_request = |state: SharedState| {
            let entered_tx = entered_tx.clone();
            let gate = gate.clone();
            std::thread::spawn(move || {
                state.start_model_download_with("same-model".to_string(), move |_| {
                    entered_tx.send(()).unwrap();
                    let (open, wake) = &*gate;
                    let mut open = open.lock().unwrap();
                    while !*open {
                        open = wake.wait(open).unwrap();
                    }
                    let (_progress, watcher) = watch::channel(DownloadStatus::InProgress(0));
                    ActiveModelDownload::unmanaged(watcher)
                })
            })
        };

        let first = spawn_request(state.clone());
        entered_rx.recv().unwrap();
        let second = spawn_request(state);
        let second_started = entered_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .is_ok();

        let (open, wake) = &*gate;
        *open.lock().unwrap() = true;
        wake.notify_all();

        let (first, _) = first.join().unwrap();
        let (second, _) = second.join().unwrap();
        assert!(!second_started, "both requests started their own transfer");
        assert!(first.same_channel(&second));
    }

    #[test]
    fn live_transcript_rejects_results_from_an_older_recording() {
        let state = SharedState::new(Config::default());
        let first = state.begin_live_transcript();
        assert!(state.update_live_transcript(first, "first preview".to_string()));

        let second = state.begin_live_transcript();
        assert_eq!(state.live_transcript(), "");
        assert!(!state.update_live_transcript(first, "stale preview".to_string()));
        assert_eq!(state.live_transcript(), "");

        assert!(state.update_live_transcript(second, "current preview".to_string()));
        assert_eq!(state.live_transcript(), "current preview");
        assert!(!state.update_live_transcript(second, "current preview".to_string()));
    }

    #[test]
    fn clearing_history_also_clears_the_exposed_last_transcript() {
        let state = SharedState::new(Config::default());
        state.inner.lock().unwrap().last_transcript = "private words".to_string();

        state
            .clear_transcription_history_with(|entries| {
                entries.clear();
                Ok(())
            })
            .unwrap();

        assert_eq!(state.last_transcript(), "");
    }

    #[test]
    fn deleting_the_latest_history_entry_exposes_the_next_one() {
        let state = SharedState::new(Config::default());
        {
            let mut inner = state.inner.lock().unwrap();
            inner.transcription_history = vec![
                voxkey_ipc::HistoryEntry {
                    id: 2,
                    recorded_at_unix_ms: 2,
                    text: "newest".to_string(),
                    provider: "test".to_string(),
                    outcome: voxkey_ipc::TranscriptOutcome::Completed,
                    pending_insertion: None,
                    audio_path: None,
                    error: None,
                },
                voxkey_ipc::HistoryEntry {
                    id: 1,
                    recorded_at_unix_ms: 1,
                    text: "previous".to_string(),
                    provider: "test".to_string(),
                    outcome: voxkey_ipc::TranscriptOutcome::Completed,
                    pending_insertion: None,
                    audio_path: None,
                    error: None,
                },
            ];
            inner.last_transcript = "newest".to_string();
            inner.last_transcript_entry_id = Some(2);
        }

        state
            .delete_history_entry_with(2, |entries, id| {
                entries.retain(|entry| entry.id != id);
                Ok(true)
            })
            .unwrap();

        assert_eq!(state.last_transcript(), "previous");
    }

    #[test]
    fn completed_transcript_uses_the_provider_that_produced_it() {
        let mut current = Config::default();
        current.transcriber.provider = voxkey_ipc::TranscriberProvider::Mistral;
        let state = SharedState::new(current);
        let completed_with = voxkey_ipc::TranscriberConfig {
            provider: voxkey_ipc::TranscriberProvider::WhisperCpp,
            ..Default::default()
        };

        state
            .record_transcript_with(
                "finished before settings changed".to_string(),
                &completed_with,
                voxkey_ipc::TranscriptOutcome::Completed,
                None,
                |_, _, recorded_config, _, _| {
                    assert_eq!(recorded_config.provider, completed_with.provider);
                    Ok(1)
                },
            )
            .unwrap();
    }

    #[test]
    fn audio_format_rejects_zero_rate_and_channels() {
        assert!(validate_audio_format(0, 1).is_err());
        assert!(validate_audio_format(16_000, 0).is_err());
        assert!(validate_audio_format(16_000, 1).is_ok());
    }

    #[test]
    fn a_download_cannot_start_in_the_middle_of_model_deletion() {
        let state = SharedState::new(Config::default());
        let (deleting_tx, deleting_rx) = std::sync::mpsc::channel();
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

        let deleting_state = state.clone();
        let deleting_gate = gate.clone();
        let deletion = std::thread::spawn(move || {
            deleting_state.delete_model_with("model", move |_| {
                deleting_tx.send(()).unwrap();
                let (open, wake) = &*deleting_gate;
                let mut open = open.lock().unwrap();
                while !*open {
                    open = wake.wait(open).unwrap();
                }
                Ok(())
            })
        });
        deleting_rx.recv().unwrap();

        let (download_tx, download_rx) = std::sync::mpsc::channel();
        let downloading_state = state.clone();
        let download = std::thread::spawn(move || {
            downloading_state.start_model_download_with("model".to_string(), move |_| {
                download_tx.send(()).unwrap();
                let (_progress, watcher) = watch::channel(DownloadStatus::InProgress(0));
                ActiveModelDownload::unmanaged(watcher)
            })
        });

        let started_during_deletion = download_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_ok();
        let (open, wake) = &*gate;
        *open.lock().unwrap() = true;
        wake.notify_all();

        deletion.join().unwrap().unwrap();
        download.join().unwrap();
        assert!(
            !started_during_deletion,
            "download started while its model directory was being deleted"
        );
    }

    #[test]
    fn api_key_methods_reject_unknown_services() {
        assert!(validate_api_key_service(voxkey_ipc::API_KEY_SERVICE_MISTRAL).is_ok());
        assert!(validate_api_key_service(voxkey_ipc::API_KEY_SERVICE_MISTRAL_REALTIME).is_ok());
        assert!(validate_api_key_service(voxkey_ipc::API_KEY_SERVICE_MODEL_SERVER).is_ok());
        assert!(validate_api_key_service("unrelated-secret").is_err());
    }

    #[test]
    fn public_transcriber_config_does_not_expose_plaintext_api_keys() {
        let batch_secret = "sk-batch-never-publish";
        let realtime_secret = "sk-realtime-never-publish";
        let server_secret = "sk-server-never-publish";
        let mut config = Config::default();
        config.transcriber.mistral.api_key = batch_secret.to_string();
        config.transcriber.mistral_realtime.api_key = realtime_secret.to_string();
        config.transcriber.parakeet.api_key = server_secret.to_string();
        let (control_tx, _control_rx) = mpsc::channel(1);
        let interface = DaemonInterface::new(SharedState::new(config), control_tx);

        let published = interface.transcriber_config();

        assert!(!published.contains(batch_secret));
        assert!(!published.contains(realtime_secret));
        assert!(!published.contains(server_secret));
        let published: voxkey_ipc::TranscriberConfig = serde_json::from_str(&published).unwrap();
        assert!(published.mistral.api_key.is_empty());
        assert!(published.mistral_realtime.api_key.is_empty());
        assert!(published.parakeet.api_key.is_empty());
    }

    #[test]
    fn preview_property_uses_the_shared_ipc_contract() {
        let config = Config {
            preview: voxkey_ipc::PreviewConfig {
                mode: voxkey_ipc::PreviewMode::Always,
                strategy: voxkey_ipc::PreviewStrategy::Segmented,
                interval_ms: 2500,
                max_audio_seconds: 45,
            },
            ..Default::default()
        };
        let (control_tx, _control_rx) = mpsc::channel(1);
        let interface = DaemonInterface::new(SharedState::new(config), control_tx);

        let published: voxkey_ipc::PreviewConfig =
            serde_json::from_str(&interface.preview_config()).unwrap();

        assert_eq!(published.mode, voxkey_ipc::PreviewMode::Always);
        assert_eq!(published.strategy, voxkey_ipc::PreviewStrategy::Segmented);
        assert_eq!(published.interval_ms, 2500);
        assert_eq!(published.max_audio_seconds, 45);
    }

    #[test]
    fn public_transcriber_updates_cannot_replace_private_api_keys() {
        let mut config = Config::default();
        config.transcriber.mistral.api_key = "private-batch-key".to_string();
        config.transcriber.mistral_realtime.api_key = "private-realtime-key".to_string();
        config.transcriber.parakeet.api_key = "private-server-key".to_string();
        let state = SharedState::new(config);
        let mut public_update = state.config().transcriber;
        public_update.mistral.model = "updated-model".to_string();
        public_update.mistral.api_key = "attacker-batch-key".to_string();
        public_update.mistral_realtime.api_key = "attacker-realtime-key".to_string();
        public_update.parakeet.api_key = "attacker-server-key".to_string();

        let updated = state
            .update_transcriber_config_with(public_update, |_, _| Ok::<_, ()>(()))
            .unwrap();

        assert_eq!(updated.transcriber.mistral.model, "updated-model");
        assert_eq!(updated.transcriber.mistral.api_key, "private-batch-key");
        assert_eq!(
            updated.transcriber.mistral_realtime.api_key,
            "private-realtime-key"
        );
        assert_eq!(updated.transcriber.parakeet.api_key, "private-server-key");
    }

    #[test]
    fn keyring_changes_remove_the_matching_plaintext_fallback() {
        let mut config = Config::default();
        config.transcriber.mistral.api_key = "old-batch-key".to_string();
        config.transcriber.mistral_realtime.api_key = "old-realtime-key".to_string();
        config.transcriber.parakeet.api_key = "old-server-key".to_string();

        assert!(clear_plaintext_api_key(
            &mut config,
            voxkey_ipc::API_KEY_SERVICE_MISTRAL
        ));
        assert!(config.transcriber.mistral.api_key.is_empty());
        assert_eq!(
            config.transcriber.mistral_realtime.api_key,
            "old-realtime-key"
        );

        assert!(clear_plaintext_api_key(
            &mut config,
            voxkey_ipc::API_KEY_SERVICE_MISTRAL_REALTIME,
        ));
        assert!(config.transcriber.mistral_realtime.api_key.is_empty());
        assert_eq!(config.transcriber.parakeet.api_key, "old-server-key");

        assert!(clear_plaintext_api_key(
            &mut config,
            voxkey_ipc::API_KEY_SERVICE_MODEL_SERVER,
        ));
        assert!(config.transcriber.parakeet.api_key.is_empty());

        let state = SharedState::new(config);
        state
            .clear_plaintext_api_key_with(voxkey_ipc::API_KEY_SERVICE_MISTRAL, |_, _| {
                Err::<(), _>("an unchanged config must not be written")
            })
            .unwrap();
    }

    #[test]
    fn api_key_storage_rejects_blank_values() {
        assert_eq!(
            validate_api_key_value("  sk-real-key \n").unwrap(),
            "sk-real-key"
        );
        assert!(validate_api_key_value("").is_err());
        assert!(validate_api_key_value(" \t\n").is_err());
    }
}
