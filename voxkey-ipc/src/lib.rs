// ABOUTME: Shared D-Bus interface and config types between the voxkey daemon and settings GUI.
// ABOUTME: Defines bus name, object path, state types, transcriber config, and proxy trait for IPC.

use std::fmt;

use serde::{Deserialize, Serialize};
use xkbcommon::xkb;

pub mod model_library;

/// Well-known bus name the daemon registers on the session bus.
pub const BUS_NAME: &str = "io.github.hy26v.Voxkey.Daemon";

/// Well-known bus name owned by the GTK settings application while it is alive.
pub const SETTINGS_BUS_NAME: &str = "io.github.hy26v.Voxkey";

/// Object path the daemon interface is served at.
pub const OBJECT_PATH: &str = "/io/github/hy26v/Voxkey/Daemon";

/// Service name used with the API-key keyring methods for the Mistral batch API.
pub const API_KEY_SERVICE_MISTRAL: &str = "mistral";

/// Service name used with the API-key keyring methods for the Mistral Realtime API.
pub const API_KEY_SERVICE_MISTRAL_REALTIME: &str = "mistral-realtime";

/// Service name used for an optional bearer token on a self-hosted model server.
pub const API_KEY_SERVICE_MODEL_SERVER: &str = "model-server";

/// Default global shortcut offered by Voxkey on GNOME.
pub const DEFAULT_SHORTCUT_TRIGGER: &str = "<Super><Alt>d";

/// Return whether a shortcut collides with GNOME's input-source switcher.
///
/// This lives in the shared IPC crate so the settings UI and daemon apply the
/// same rule before either side presents a shortcut as accepted.
pub fn conflicts_with_gnome_input_source(trigger: &str) -> bool {
    let normalized: String = trigger
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    let mut accelerator = normalized.strip_prefix("press").unwrap_or(&normalized);
    let mut has_super = false;
    let mut has_other_modifier = false;

    loop {
        if let Some(rest) = accelerator.strip_prefix("super") {
            has_super = true;
            accelerator = rest;
        } else if let Some(rest) = accelerator.strip_prefix("mod4") {
            has_super = true;
            accelerator = rest;
        } else if let Some(rest) = accelerator.strip_prefix("shift") {
            accelerator = rest;
        } else if let Some(rest) = ["control", "primary", "ctrl", "alt", "mod1", "meta", "hyper"]
            .iter()
            .find_map(|modifier| accelerator.strip_prefix(modifier))
        {
            has_other_modifier = true;
            accelerator = rest;
        } else {
            break;
        }
    }

    has_super && !has_other_modifier && accelerator == "space"
}

fn has_non_shift_shortcut_modifier(trigger: &str) -> bool {
    const MODIFIERS: [&str; 9] = [
        "control", "primary", "ctrl", "alt", "mod1", "super", "meta", "hyper", "mod4",
    ];
    let mut remainder = trigger.trim_start();

    while let Some(after_open) = remainder.strip_prefix('<') {
        let Some(end) = after_open.find('>') else {
            return false;
        };
        let modifier = after_open[..end].to_ascii_lowercase();
        if MODIFIERS.contains(&modifier.as_str()) {
            return true;
        }
        remainder = after_open[end + 1..].trim_start();
    }

    false
}

fn shortcut_key(trigger: &str) -> Option<&str> {
    let mut remainder = trigger.trim_start();
    while let Some(after_open) = remainder.strip_prefix('<') {
        let end = after_open.find('>')?;
        remainder = after_open[end + 1..].trim_start();
    }
    let key = remainder.trim();
    (!key.is_empty()).then_some(key)
}

fn shortcut_keysym(key: &str) -> xkb::Keysym {
    if key.contains('\0') {
        return xkb::Keysym::new(xkb::keysyms::KEY_NoSymbol);
    }

    let keysym = xkb::keysym_from_name(key, xkb::KEYSYM_NO_FLAGS);
    if keysym.raw() != xkb::keysyms::KEY_NoSymbol || key.starts_with("XF86") {
        return keysym;
    }

    // GDK names traditional XF86 keysyms without their XF86 prefix (for
    // example, `AudioRecord`). The shortcuts specification and xkbcommon use
    // the prefixed spelling, so accept either form here.
    xkb::keysym_from_name(&format!("XF86{key}"), xkb::KEYSYM_NO_FLAGS)
}

/// Return whether a key is safe for GNOME to reserve without a modifier.
///
/// Normal typing, editing, navigation, lock, and modifier keys deliberately
/// stay unavailable on their own. Function keys and dedicated hardware keys
/// do not produce text, so taking one globally does not make ordinary input
/// impossible. The portal remains responsible for detecting desktop-level
/// conflicts such as an already-bound media key.
fn is_safe_unmodified_shortcut_key(key: &str) -> bool {
    const MODERN_DEDICATED_KEYS: [&str; 8] = [
        "assistant",
        "dictate",
        "macrorecordstart",
        "macrorecordstop",
        "pauserecord",
        "stoprecord",
        "voicecommand",
        "voicemail",
    ];

    let normalized = key.strip_prefix("XF86").unwrap_or(key).to_ascii_lowercase();
    if MODERN_DEDICATED_KEYS.contains(&normalized.as_str()) {
        return true;
    }

    let keysym = shortcut_keysym(key).raw();
    let function_key = (xkb::keysyms::KEY_F1..=xkb::keysyms::KEY_F35).contains(&keysym)
        || (xkb::keysyms::KEY_KP_F1..=xkb::keysyms::KEY_KP_F4).contains(&keysym);
    let traditional_xf86_key = (0x1008_ff00..=0x1008_ffff).contains(&keysym);
    let evdev_xf86_key = (0x1008_1000..=0x1008_12ff).contains(&keysym);

    function_key
        || traditional_xf86_key
        || evdev_xf86_key
        || matches!(keysym, xkb::keysyms::KEY_Pause | xkb::keysyms::KEY_Print)
}

/// Validate a portal shortcut trigger accepted by both daemon and settings.
pub fn validate_shortcut_trigger(trigger: &str) -> Result<(), String> {
    if trigger.trim().is_empty() {
        return Err("Shortcut trigger must not be blank".to_string());
    }
    let Some(key) = shortcut_key(trigger) else {
        return Err("A dictation shortcut must include a non-modifier key".to_string());
    };
    if conflicts_with_gnome_input_source(trigger) {
        return Err("This shortcut is reserved by GNOME for switching input sources".to_string());
    }
    if !has_non_shift_shortcut_modifier(trigger) && !is_safe_unmodified_shortcut_key(key) {
        return Err(
            "Use Control, Alt, Super, Meta, or Hyper with typing and navigation keys; only function and dedicated hardware keys can be used alone"
                .to_string(),
        );
    }
    Ok(())
}

/// Daemon state as exposed over D-Bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DaemonState {
    Idle,
    Connecting,
    Recording,
    Streaming,
    Transcribing,
    Injecting,
    RecoveringSession,
}

impl fmt::Display for DaemonState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DaemonState::Idle => write!(f, "Idle"),
            DaemonState::Connecting => write!(f, "Connecting"),
            DaemonState::Recording => write!(f, "Recording"),
            DaemonState::Streaming => write!(f, "Streaming"),
            DaemonState::Transcribing => write!(f, "Transcribing"),
            DaemonState::Injecting => write!(f, "Injecting"),
            DaemonState::RecoveringSession => write!(f, "RecoveringSession"),
        }
    }
}

impl std::str::FromStr for DaemonState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Idle" => Ok(DaemonState::Idle),
            "Connecting" => Ok(DaemonState::Connecting),
            "Recording" => Ok(DaemonState::Recording),
            "Streaming" => Ok(DaemonState::Streaming),
            "Transcribing" => Ok(DaemonState::Transcribing),
            "Injecting" => Ok(DaemonState::Injecting),
            "RecoveringSession" => Ok(DaemonState::RecoveringSession),
            other => Err(format!("Unknown daemon state: {other}")),
        }
    }
}

/// Which transcription backend to use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TranscriberProvider {
    #[default]
    WhisperCpp,
    Mistral,
    MistralRealtime,
    Parakeet,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WhisperCppConfig {
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MistralConfig {
    /// Legacy plaintext API key. Auto-migrated to the system keyring on first daemon start
    /// after upgrade, then left empty. New code should call `set_api_key` over D-Bus instead.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    pub model: String,
    #[serde(default)]
    pub endpoint: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MistralRealtimeConfig {
    /// Legacy plaintext API key. Auto-migrated to the system keyring on first daemon start
    /// after upgrade, then left empty. New code should call `set_api_key` over D-Bus instead.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    pub model: String,
    #[serde(default)]
    pub endpoint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionProviderChoice {
    #[default]
    Auto,
    Cpu,
    Cuda,
}

/// How a Parakeet model is executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ParakeetBackend {
    /// Run the downloaded model in-process with sherpa-onnx.
    #[default]
    Local,
    /// Send recorded audio to an OpenAI-compatible HTTP transcription server.
    Http,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ParakeetConfig {
    pub model: String,
    #[serde(default)]
    pub backend: ParakeetBackend,
    /// Endpoint used only when `backend` is `http`.
    #[serde(default)]
    pub endpoint: String,
    /// Legacy plaintext bearer token. New values are stored in the system
    /// keyring and injected only into the daemon's runtime configuration.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    /// Permit plaintext HTTP to literal private-network IP addresses. This is
    /// intentionally opt-in because recorded audio and transcripts are not
    /// encrypted in transit.
    #[serde(default)]
    pub allow_insecure_http: bool,
    #[serde(default)]
    pub execution_provider: ExecutionProviderChoice,
}

impl Default for ParakeetConfig {
    fn default() -> Self {
        Self {
            model: Self::DEFAULT_MODEL.to_string(),
            backend: ParakeetBackend::Local,
            endpoint: String::new(),
            api_key: String::new(),
            allow_insecure_http: false,
            execution_provider: ExecutionProviderChoice::Auto,
        }
    }
}

impl ParakeetConfig {
    pub const DEFAULT_MODEL: &str = "parakeet-tdt-0.6b-v3";
}

impl MistralConfig {
    pub const DEFAULT_MODEL: &str = "voxtral-mini-2602";
    pub const DEFAULT_ENDPOINT: &str = "https://api.mistral.ai/v1/audio/transcriptions";
}

impl MistralRealtimeConfig {
    pub const DEFAULT_MODEL: &str = "voxtral-mini-transcribe-realtime-2602";
    pub const DEFAULT_ENDPOINT: &str = "wss://api.mistral.ai/v1/audio/transcriptions/realtime";
}

/// Provider-based transcription configuration.
/// Holds settings for all providers; `provider` selects which one is active.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TranscriberConfig {
    #[serde(default)]
    pub provider: TranscriberProvider,
    #[serde(default)]
    pub whisper_cpp: WhisperCppConfig,
    #[serde(default)]
    pub mistral: MistralConfig,
    #[serde(default)]
    pub mistral_realtime: MistralRealtimeConfig,
    #[serde(default)]
    pub parakeet: ParakeetConfig,
}

/// Result of checking a custom transcription endpoint without sending audio
/// or credentials. `Reachable` means the network route answered the probe;
/// credentials and model access are intentionally verified during dictation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointCheckResult {
    pub status: EndpointCheckStatus,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EndpointCheckStatus {
    Reachable,
    Failed,
}

impl EndpointCheckResult {
    pub fn reachable(message: impl Into<String>) -> Self {
        Self {
            status: EndpointCheckStatus::Reachable,
            message: message.into(),
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            status: EndpointCheckStatus::Failed,
            message: message.into(),
        }
    }
}

impl Default for WhisperCppConfig {
    fn default() -> Self {
        Self {
            command: "whisper-cpp".to_string(),
            args: Vec::new(),
        }
    }
}

impl Default for MistralConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: Self::DEFAULT_MODEL.to_string(),
            endpoint: String::new(),
        }
    }
}

impl Default for MistralRealtimeConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: Self::DEFAULT_MODEL.to_string(),
            endpoint: String::new(),
        }
    }
}

/// Configuration for text injection behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InjectionConfig {
    #[serde(default = "default_typing_delay_ms")]
    pub typing_delay_ms: u32,
}

impl InjectionConfig {
    pub const MAX_TYPING_DELAY_MS: u32 = 50;
}

fn default_typing_delay_ms() -> u32 {
    0
}

impl Default for InjectionConfig {
    fn default() -> Self {
        Self {
            typing_delay_ms: default_typing_delay_ms(),
        }
    }
}

/// When Voxkey may spend transcription work on replaceable live previews.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PreviewMode {
    /// Preview only with providers that run on this machine. Network-backed
    /// providers must opt in because each refresh sends another request.
    #[default]
    Auto,
    /// Preview with every batch provider, including network-backed ones.
    Always,
    /// Never produce previews for batch providers.
    Never,
}

/// How Voxkey divides a recording between live-preview transcriptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PreviewStrategy {
    /// Keep a stable prefix and repeatedly decode only the uncertain tail with
    /// enough preceding audio to preserve context.
    #[default]
    Whole,
    /// Commit silence-delimited phrases and preview only the open phrase.
    Segmented,
}

/// Configuration for replaceable transcription previews shown while recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewConfig {
    #[serde(default)]
    pub mode: PreviewMode,
    #[serde(default)]
    pub strategy: PreviewStrategy,
    /// How often a new preview transcription may be requested.
    #[serde(default = "default_preview_interval_ms")]
    pub interval_ms: u32,
    /// Maximum length of the unconfirmed decode window. Zero is unlimited.
    #[serde(default = "default_preview_max_audio_seconds")]
    pub max_audio_seconds: u32,
}

impl PreviewConfig {
    pub const MIN_INTERVAL_MS: u32 = 250;
    pub const MAX_AUDIO_SECONDS: u32 = 10 * 60;

    /// Whether previews should run for a transcriber with the given locality.
    pub fn allows(&self, runs_locally: bool) -> bool {
        match self.mode {
            PreviewMode::Auto => runs_locally,
            PreviewMode::Always => true,
            PreviewMode::Never => false,
        }
    }

    pub fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.interval_ms.max(Self::MIN_INTERVAL_MS) as u64)
    }
}

fn default_preview_interval_ms() -> u32 {
    1000
}

fn default_preview_max_audio_seconds() -> u32 {
    0
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            mode: PreviewMode::default(),
            strategy: PreviewStrategy::default(),
            interval_ms: default_preview_interval_ms(),
            max_audio_seconds: default_preview_max_audio_seconds(),
        }
    }
}

/// A single "wrong -> right" replacement rule. `original` may contain
/// comma-separated variants that all map to the same replacement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WordReplacement {
    pub original: String,
    pub replacement: String,
    #[serde(default = "default_replacement_enabled")]
    pub enabled: bool,
}

fn default_replacement_enabled() -> bool {
    true
}

/// User dictionary: replacement rules applied to transcription output and
/// vocabulary words passed as hints to backends that accept context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DictionaryConfig {
    #[serde(default)]
    pub replacements: Vec<WordReplacement>,
    #[serde(default)]
    pub vocabulary: Vec<String>,
}

/// Why a realtime provider stopped producing a saved transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptOutcome {
    /// The provider sent its explicit completion event.
    #[default]
    Completed,
    /// The provider reported an error after producing some text.
    PartialProviderError,
    /// The transport ended before the provider's completion event.
    PartialTransportClose,
    /// The user cancelled after some provider text had already arrived.
    Cancelled,
    /// Capture, protocol, or another local operation failed after partial text.
    PartialFailure,
    /// A finalized batch recording could not be transcribed. The associated
    /// history entry may carry the preserved WAV and provider error.
    Failed,
}

/// A transcription saved by the daemon for the History screen.
///
/// The history is exchanged as JSON over D-Bus so the schema can evolve
/// without exposing a complex D-Bus container type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Unique-enough identifier derived from the Unix timestamp in
    /// nanoseconds. It is used for row actions such as deletion.
    pub id: u64,
    /// Wall-clock completion time in Unix milliseconds.
    pub recorded_at_unix_ms: i64,
    pub text: String,
    /// Human-readable provider name captured at transcription time.
    pub provider: String,
    /// Whether the provider completed normally or this is preserved partial
    /// output. Missing values from older history files mean `Completed`.
    #[serde(default)]
    pub outcome: TranscriptOutcome,
    /// Exact output still safe to insert after a partial injection. `None`
    /// means the prior insertion completed and "insert last" intentionally
    /// repeats the full transcript; `Some("")` means no retry is outstanding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_insertion: Option<String>,
    /// Finalized WAV retained after a batch provider failure. Older entries
    /// and successful transcriptions have no associated audio file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_path: Option<String>,
    /// User-facing failure reported by the provider for a retained recording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl HistoryEntry {
    /// Text for the next explicit "insert last" request.
    pub fn text_for_insertion(&self) -> Option<&str> {
        match self.pending_insertion.as_deref() {
            Some("") => None,
            Some(remaining) => Some(remaining),
            None if self.text.is_empty() => None,
            None => Some(&self.text),
        }
    }
}

/// D-Bus proxy for the GUI to communicate with the daemon.
///
/// The daemon implements the server side of this interface using
/// `zbus::interface` on a struct that holds daemon state.
#[zbus::proxy(
    interface = "io.github.hy26v.Voxkey.Daemon1",
    default_service = "io.github.hy26v.Voxkey.Daemon",
    default_path = "/io/github/hy26v/Voxkey/Daemon"
)]
pub trait Daemon {
    /// Current daemon state as a string.
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;

    /// Current shortcut trigger string.
    #[zbus(property)]
    fn shortcut_trigger(&self) -> zbus::Result<String>;

    /// User-facing description of the shortcut actually bound by the portal.
    #[zbus(property)]
    fn shortcut_description(&self) -> zbus::Result<String>;

    /// Current normalized microphone level, from 0.0 (silence) to 1.0.
    #[zbus(property)]
    fn audio_level(&self) -> zbus::Result<f64>;

    /// Transcriber configuration as serialized JSON.
    #[zbus(property)]
    fn transcriber_config(&self) -> zbus::Result<String>;

    /// Injection configuration as serialized JSON.
    #[zbus(property)]
    fn injection_config(&self) -> zbus::Result<String>;

    /// Dictionary configuration as serialized JSON.
    #[zbus(property)]
    fn dictionary_config(&self) -> zbus::Result<String>;

    /// Live-preview configuration as serialized JSON.
    #[zbus(property)]
    fn preview_config(&self) -> zbus::Result<String>;

    /// Audio sample rate in Hz.
    #[zbus(property)]
    fn sample_rate(&self) -> zbus::Result<u32>;

    /// Audio channel count.
    #[zbus(property)]
    fn channels(&self) -> zbus::Result<u16>;

    /// Whether portal sessions are connected.
    #[zbus(property)]
    fn portal_connected(&self) -> zbus::Result<bool>;

    /// Most recent transcription result.
    #[zbus(property)]
    fn last_transcript(&self) -> zbus::Result<String>;

    /// Replaceable transcription hypothesis for the recording in progress.
    #[zbus(property)]
    fn live_transcript(&self) -> zbus::Result<String>;

    /// Persisted transcription history as serialized JSON.
    #[zbus(property)]
    fn transcription_history(&self) -> zbus::Result<String>;

    /// Configured audio input device name. Empty means the system default.
    #[zbus(property)]
    fn audio_input_device(&self) -> zbus::Result<String>;

    /// Currently available audio input device names as serialized JSON.
    #[zbus(property)]
    fn audio_input_devices(&self) -> zbus::Result<String>;

    /// Most recent error message, empty when no error.
    #[zbus(property)]
    fn last_error(&self) -> zbus::Result<String>;

    /// Start a new dictation while the daemon is idle.
    fn start_dictation(&self) -> zbus::Result<()>;

    /// Finish the active recording and process its transcript.
    fn stop_dictation(&self) -> zbus::Result<()>;

    /// Discard the active recording or pending transcription.
    fn cancel_dictation(&self) -> zbus::Result<()>;

    /// Insert the most recently completed transcript again.
    fn insert_last_transcript(&self) -> zbus::Result<()>;

    /// Dismiss the daemon's most recent recoverable error.
    fn clear_last_error(&self) -> zbus::Result<()>;

    /// Update the shortcut trigger. Takes effect on next session recovery.
    fn set_shortcut(&self, trigger: &str) -> zbus::Result<()>;

    /// Update the transcriber configuration from JSON.
    fn set_transcriber_config(&self, config_json: &str) -> zbus::Result<()>;

    /// Check the selected network endpoint without persisting the candidate
    /// configuration or sending audio and credentials. Returns a serialized
    /// `EndpointCheckResult`.
    fn check_transcriber_endpoint(&self, config_json: &str) -> zbus::Result<String>;

    /// Update the injection configuration from JSON.
    fn set_injection_config(&self, config_json: &str) -> zbus::Result<()>;

    /// Update the dictionary configuration from JSON.
    fn set_dictionary_config(&self, config_json: &str) -> zbus::Result<()>;

    /// Update live-preview configuration from JSON.
    fn set_preview_config(&self, config_json: &str) -> zbus::Result<()>;

    /// Update audio settings. Takes effect on next recording.
    fn set_audio(&self, sample_rate: u32, channels: u16) -> zbus::Result<()>;

    /// Select an audio input device by name. Empty selects the system default.
    fn set_audio_input_device(&self, device_name: &str) -> zbus::Result<()>;

    /// Delete one entry from transcription history.
    fn delete_history_entry(&self, id: u64) -> zbus::Result<()>;

    /// Delete all saved transcription history.
    fn clear_transcription_history(&self) -> zbus::Result<()>;

    /// Transcribe a WAV retained by a failed history entry with the currently
    /// selected batch provider.
    fn retry_history_entry(&self, id: u64) -> zbus::Result<()>;

    /// Re-read config.toml from disk.
    fn reload_config(&self) -> zbus::Result<()>;

    /// Delete the stored portal restore token, forcing a fresh session.
    fn clear_restore_token(&self) -> zbus::Result<()>;

    /// Shut down the daemon process.
    fn quit(&self) -> zbus::Result<()>;

    /// Link daemon lifetime to the settings application. The daemon shuts down
    /// when the settings application's D-Bus name disappears.
    fn attach_settings(&self) -> zbus::Result<()>;

    /// Store an API key for a transcription provider in the system keyring.
    /// Service names: "mistral", "mistral-realtime". Triggers a session restart
    /// so the new key takes effect immediately.
    fn set_api_key(&self, service: &str, key: &str) -> zbus::Result<()>;

    /// Remove the stored API key for a transcription provider.
    fn clear_api_key(&self, service: &str) -> zbus::Result<()>;

    /// Return true if an API key is stored for the given provider. The key
    /// itself never leaves the daemon over D-Bus.
    fn has_api_key(&self, service: &str) -> zbus::Result<bool>;

    /// Start downloading a Parakeet model by name.
    fn download_model(&self, model_name: &str) -> zbus::Result<()>;

    /// Stop an active model download and remove its incomplete file.
    fn cancel_model_download(&self, model_name: &str) -> zbus::Result<()>;

    /// Delete a downloaded Parakeet model.
    fn delete_model(&self, model_name: &str) -> zbus::Result<()>;

    /// Check if a Parakeet model is available locally.
    /// Returns "available", "downloading", or "not_downloaded".
    fn model_status(&self, model_name: &str) -> zbus::Result<String>;

    /// Emitted when a transcription completes.
    #[zbus(signal)]
    fn transcription_complete(text: &str) -> zbus::Result<()>;

    /// Emitted on recoverable errors.
    #[zbus(signal)]
    fn error_occurred(message: &str) -> zbus::Result<()>;

    /// Emitted during model download with progress percentage.
    #[zbus(signal)]
    fn download_progress(model_name: &str, percent: u8) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcriber_config_default_is_whisper_cpp() {
        let config = TranscriberConfig::default();
        assert_eq!(config.provider, TranscriberProvider::WhisperCpp);
        assert_eq!(config.whisper_cpp.command, "whisper-cpp");
        assert!(config.whisper_cpp.args.is_empty());
        assert_eq!(config.mistral.model, "voxtral-mini-2602");
        assert!(config.mistral.api_key.is_empty());
    }

    #[test]
    fn transcriber_config_json_round_trip() {
        let config = TranscriberConfig {
            provider: TranscriberProvider::Mistral,
            whisper_cpp: WhisperCppConfig::default(),
            mistral: MistralConfig {
                api_key: "sk-test-123".to_string(),
                model: "voxtral-mini-2602".to_string(),
                endpoint: String::new(),
            },
            mistral_realtime: MistralRealtimeConfig::default(),
            parakeet: ParakeetConfig::default(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: TranscriberConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn endpoint_check_result_json_round_trip() {
        let result = EndpointCheckResult::reachable("Server responded in 24 ms.");
        let json = serde_json::to_string(&result).unwrap();

        assert!(json.contains(r#""status":"reachable""#));
        assert_eq!(
            serde_json::from_str::<EndpointCheckResult>(&json).unwrap(),
            result
        );
    }

    #[test]
    fn transcriber_config_toml_round_trip() {
        let config = TranscriberConfig {
            provider: TranscriberProvider::WhisperCpp,
            whisper_cpp: WhisperCppConfig {
                command: "/usr/bin/whisper".to_string(),
                args: vec!["-m".to_string(), "model.bin".to_string()],
            },
            mistral: MistralConfig::default(),
            mistral_realtime: MistralRealtimeConfig::default(),
            parakeet: ParakeetConfig::default(),
        };
        let toml_str = toml::to_string(&config).unwrap();
        let parsed: TranscriberConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn partial_whisper_config_uses_default_arguments() {
        let parsed: TranscriberConfig = toml::from_str(
            r#"
[whisper_cpp]
command = "/opt/whisper-custom"
"#,
        )
        .unwrap();

        assert_eq!(parsed.whisper_cpp.command, "/opt/whisper-custom");
        assert!(parsed.whisper_cpp.args.is_empty());
    }

    #[test]
    fn partial_mistral_config_uses_the_default_model() {
        let parsed: TranscriberConfig = toml::from_str(
            r#"
[mistral]
endpoint = "https://transcribe.example.test/v1"
"#,
        )
        .unwrap();

        assert_eq!(parsed.mistral.model, MistralConfig::DEFAULT_MODEL);
        assert_eq!(
            parsed.mistral.endpoint,
            "https://transcribe.example.test/v1"
        );
    }

    #[test]
    fn partial_realtime_config_uses_the_default_model() {
        let parsed: TranscriberConfig = toml::from_str(
            r#"
[mistral_realtime]
endpoint = "wss://realtime.example.test/v1"
"#,
        )
        .unwrap();

        assert_eq!(
            parsed.mistral_realtime.model,
            MistralRealtimeConfig::DEFAULT_MODEL
        );
        assert_eq!(
            parsed.mistral_realtime.endpoint,
            "wss://realtime.example.test/v1"
        );
    }

    #[test]
    fn partial_parakeet_config_uses_the_default_model() {
        let parsed: TranscriberConfig = toml::from_str(
            r#"
[parakeet]
backend = "http"
endpoint = "http://parakeet.example.test/v1"
"#,
        )
        .unwrap();

        assert_eq!(parsed.parakeet.model, "parakeet-tdt-0.6b-v3");
        assert_eq!(parsed.parakeet.backend, ParakeetBackend::Http);
        assert_eq!(parsed.parakeet.endpoint, "http://parakeet.example.test/v1");
    }

    #[test]
    fn history_entry_json_round_trip() {
        let entry = HistoryEntry {
            id: 42,
            recorded_at_unix_ms: 1_720_000_000_000,
            text: "Hello from Voxkey".to_string(),
            provider: "Parakeet v3".to_string(),
            outcome: TranscriptOutcome::Completed,
            pending_insertion: Some("Voxkey".to_string()),
            audio_path: None,
            error: None,
        };

        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(serde_json::from_str::<HistoryEntry>(&json).unwrap(), entry);
    }

    #[test]
    fn legacy_history_defaults_to_a_completed_full_insertion() {
        let entry: HistoryEntry = serde_json::from_str(
            r#"{"id":1,"recorded_at_unix_ms":2,"text":"legacy","provider":"whisper.cpp"}"#,
        )
        .unwrap();

        assert_eq!(entry.outcome, TranscriptOutcome::Completed);
        assert_eq!(entry.pending_insertion, None);
        assert_eq!(entry.audio_path, None);
        assert_eq!(entry.error, None);
        assert_eq!(entry.text_for_insertion(), Some("legacy"));
    }

    #[test]
    fn failed_history_entry_round_trips_its_recoverable_audio() {
        let entry = HistoryEntry {
            id: 43,
            recorded_at_unix_ms: 1_720_000_000_001,
            text: String::new(),
            provider: "Parakeet v3 (HTTP Server)".to_string(),
            outcome: TranscriptOutcome::Failed,
            pending_insertion: None,
            audio_path: Some("/state/voxkey/recordings/43.wav".to_string()),
            error: Some("422 Unprocessable Entity".to_string()),
        };

        let json = serde_json::to_string(&entry).unwrap();

        assert_eq!(serde_json::from_str::<HistoryEntry>(&json).unwrap(), entry);
        assert_eq!(entry.text_for_insertion(), None);
    }

    #[test]
    fn provider_serializes_as_kebab_case() {
        let json = serde_json::to_string(&TranscriberProvider::WhisperCpp).unwrap();
        assert_eq!(json, "\"whisper-cpp\"");
        let json = serde_json::to_string(&TranscriberProvider::Mistral).unwrap();
        assert_eq!(json, "\"mistral\"");
        let json = serde_json::to_string(&TranscriberProvider::MistralRealtime).unwrap();
        assert_eq!(json, "\"mistral-realtime\"");
    }

    #[test]
    fn mistral_realtime_config_default_model() {
        let config = MistralRealtimeConfig::default();
        assert_eq!(config.model, "voxtral-mini-transcribe-realtime-2602");
        assert!(config.api_key.is_empty());
    }

    #[test]
    fn transcriber_config_json_round_trip_mistral_realtime() {
        let config = TranscriberConfig {
            provider: TranscriberProvider::MistralRealtime,
            whisper_cpp: WhisperCppConfig::default(),
            mistral: MistralConfig::default(),
            mistral_realtime: MistralRealtimeConfig {
                api_key: "sk-rt-test".to_string(),
                model: "voxtral-mini-transcribe-realtime-2602".to_string(),
                endpoint: String::new(),
            },
            parakeet: ParakeetConfig::default(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: TranscriberConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn transcriber_config_toml_round_trip_mistral_realtime() {
        let config = TranscriberConfig {
            provider: TranscriberProvider::MistralRealtime,
            whisper_cpp: WhisperCppConfig::default(),
            mistral: MistralConfig::default(),
            mistral_realtime: MistralRealtimeConfig {
                api_key: "sk-rt-test".to_string(),
                model: "voxtral-mini-transcribe-realtime-2602".to_string(),
                endpoint: String::new(),
            },
            parakeet: ParakeetConfig::default(),
        };
        let toml_str = toml::to_string(&config).unwrap();
        let parsed: TranscriberConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn daemon_state_streaming_display_and_parse() {
        let state = DaemonState::Streaming;
        assert_eq!(state.to_string(), "Streaming");
        assert_eq!(
            "Streaming".parse::<DaemonState>().unwrap(),
            DaemonState::Streaming
        );
    }

    #[test]
    fn daemon_state_connecting_display_and_parse() {
        let state = DaemonState::Connecting;
        assert_eq!(state.to_string(), "Connecting");
        assert_eq!(
            "Connecting".parse::<DaemonState>().unwrap(),
            DaemonState::Connecting
        );
    }

    #[test]
    fn existing_config_without_mistral_realtime_gets_defaults() {
        let json = r#"{"provider":"whisper-cpp","whisper_cpp":{"command":"whisper-cpp","args":[]},"mistral":{"api_key":"","model":"voxtral-mini-2602"}}"#;
        let parsed: TranscriberConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed.mistral_realtime.model,
            "voxtral-mini-transcribe-realtime-2602"
        );
        assert!(parsed.mistral_realtime.api_key.is_empty());
    }

    #[test]
    fn parakeet_config_default_values() {
        let config = ParakeetConfig::default();
        assert_eq!(config.model, "parakeet-tdt-0.6b-v3");
        assert_eq!(config.backend, ParakeetBackend::Local);
        assert!(config.endpoint.is_empty());
        assert!(!config.allow_insecure_http);
        assert_eq!(config.execution_provider, ExecutionProviderChoice::Auto);
    }

    #[test]
    fn existing_parakeet_config_without_backend_remains_local() {
        let json = r#"{"model":"parakeet-tdt-0.6b-v3","execution_provider":"cuda"}"#;
        let parsed: ParakeetConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.backend, ParakeetBackend::Local);
        assert!(parsed.endpoint.is_empty());
        assert!(!parsed.allow_insecure_http);
        assert_eq!(parsed.execution_provider, ExecutionProviderChoice::Cuda);
    }

    #[test]
    fn provider_serializes_parakeet_as_kebab_case() {
        let json = serde_json::to_string(&TranscriberProvider::Parakeet).unwrap();
        assert_eq!(json, "\"parakeet\"");
    }

    #[test]
    fn execution_provider_choice_serializes_as_kebab_case() {
        assert_eq!(
            serde_json::to_string(&ExecutionProviderChoice::Auto).unwrap(),
            "\"auto\""
        );
        assert_eq!(
            serde_json::to_string(&ExecutionProviderChoice::Cpu).unwrap(),
            "\"cpu\""
        );
        assert_eq!(
            serde_json::to_string(&ExecutionProviderChoice::Cuda).unwrap(),
            "\"cuda\""
        );
    }

    #[test]
    fn transcriber_config_json_round_trip_parakeet() {
        let config = TranscriberConfig {
            provider: TranscriberProvider::Parakeet,
            whisper_cpp: WhisperCppConfig::default(),
            mistral: MistralConfig::default(),
            mistral_realtime: MistralRealtimeConfig::default(),
            parakeet: ParakeetConfig {
                model: "parakeet-tdt-0.6b-v2".to_string(),
                backend: ParakeetBackend::Http,
                endpoint: "http://192.168.1.132:8000/v1/audio/transcriptions".to_string(),
                api_key: "server-secret".to_string(),
                allow_insecure_http: true,
                execution_provider: ExecutionProviderChoice::Cuda,
            },
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: TranscriberConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn parakeet_backend_serializes_as_kebab_case() {
        assert_eq!(
            serde_json::to_string(&ParakeetBackend::Local).unwrap(),
            "\"local\""
        );
        assert_eq!(
            serde_json::to_string(&ParakeetBackend::Http).unwrap(),
            "\"http\""
        );
    }

    #[test]
    fn transcriber_config_toml_round_trip_parakeet() {
        let mut config = TranscriberConfig {
            provider: TranscriberProvider::Parakeet,
            whisper_cpp: WhisperCppConfig::default(),
            mistral: MistralConfig::default(),
            mistral_realtime: MistralRealtimeConfig::default(),
            parakeet: ParakeetConfig::default(),
        };
        config.parakeet.backend = ParakeetBackend::Http;
        config.parakeet.endpoint = "http://192.168.1.132:8000/v1/audio/transcriptions".to_string();
        config.parakeet.allow_insecure_http = true;
        let toml_str = toml::to_string(&config).unwrap();
        let parsed: TranscriberConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn existing_config_without_parakeet_gets_defaults() {
        let json = r#"{"provider":"whisper-cpp","whisper_cpp":{"command":"whisper-cpp","args":[]},"mistral":{"api_key":"","model":"voxtral-mini-2602"},"mistral_realtime":{"api_key":"","model":"voxtral-mini-transcribe-realtime-2602"}}"#;
        let parsed: TranscriberConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.parakeet.model, "parakeet-tdt-0.6b-v3");
        assert!(!parsed.parakeet.allow_insecure_http);
        assert_eq!(
            parsed.parakeet.execution_provider,
            ExecutionProviderChoice::Auto
        );
    }

    #[test]
    fn injection_config_default_typing_delay_is_zero() {
        assert_eq!(InjectionConfig::default().typing_delay_ms, 0);
    }

    #[test]
    fn injection_config_preserves_explicit_nonzero_delay() {
        let config: InjectionConfig = serde_json::from_str(r#"{"typing_delay_ms":5}"#).unwrap();
        assert_eq!(config.typing_delay_ms, 5);
    }

    #[test]
    fn preview_config_defaults_match_the_settings_ui() {
        let config = PreviewConfig::default();

        assert_eq!(config.mode, PreviewMode::Auto);
        assert_eq!(config.strategy, PreviewStrategy::Whole);
        assert_eq!(config.interval_ms, 1000);
        assert_eq!(config.max_audio_seconds, 0);
        assert!(config.allows(true));
        assert!(!config.allows(false));
    }

    #[test]
    fn preview_config_json_round_trip_preserves_every_control() {
        let config = PreviewConfig {
            mode: PreviewMode::Always,
            strategy: PreviewStrategy::Segmented,
            interval_ms: 2750,
            max_audio_seconds: 45,
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: PreviewConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed, config);
    }

    #[test]
    fn preview_config_missing_fields_remain_backward_compatible() {
        let parsed: PreviewConfig = serde_json::from_str(r#"{"mode":"never"}"#).unwrap();

        assert_eq!(parsed.mode, PreviewMode::Never);
        assert_eq!(parsed.strategy, PreviewStrategy::Whole);
        assert_eq!(parsed.interval_ms, 1000);
        assert_eq!(parsed.max_audio_seconds, 0);
    }

    #[test]
    fn dictionary_config_default_is_empty() {
        let config = DictionaryConfig::default();
        assert!(config.replacements.is_empty());
        assert!(config.vocabulary.is_empty());
    }

    #[test]
    fn dictionary_config_json_round_trip() {
        let config = DictionaryConfig {
            replacements: vec![WordReplacement {
                original: "vox key, box key".to_string(),
                replacement: "Voxkey".to_string(),
                enabled: true,
            }],
            vocabulary: vec!["Voxkey".to_string(), "Barduhn".to_string()],
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: DictionaryConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn dictionary_config_toml_round_trip() {
        let config = DictionaryConfig {
            replacements: vec![WordReplacement {
                original: "jon".to_string(),
                replacement: "John".to_string(),
                enabled: false,
            }],
            vocabulary: vec![],
        };
        let toml_str = toml::to_string(&config).unwrap();
        let parsed: DictionaryConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn word_replacement_enabled_defaults_to_true() {
        let parsed: WordReplacement =
            serde_json::from_str(r#"{"original":"a","replacement":"b"}"#).unwrap();
        assert!(parsed.enabled);
    }
}
