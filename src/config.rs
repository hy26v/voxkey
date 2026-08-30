// ABOUTME: Loads TOML configuration and manages restore token persistence.
// ABOUTME: Provides defaults for shortcut, transcriber, audio, and persistence settings.

use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};

pub use voxkey_ipc::{
    DictionaryConfig, InjectionConfig, PreviewConfig, PreviewStrategy, TranscriberConfig,
    validate_shortcut_trigger,
};
#[cfg(test)]
use voxkey_ipc::{PreviewMode, conflicts_with_gnome_input_source};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub shortcut: ShortcutConfig,
    #[serde(default)]
    pub transcriber: TranscriberConfig,
    #[serde(default)]
    pub injection: InjectionConfig,
    #[serde(default)]
    pub dictionary: DictionaryConfig,
    #[serde(default)]
    pub persistence: PersistenceConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub preview: PreviewConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShortcutConfig {
    #[serde(default = "default_shortcut_id")]
    pub id: String,
    #[serde(default = "default_shortcut_description")]
    pub description: String,
    #[serde(default = "default_shortcut_trigger")]
    pub trigger: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistenceConfig {
    #[serde(default = "default_token_path")]
    pub token_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_channels")]
    pub channels: u16,
    #[serde(default = "default_tail_capture_ms")]
    pub tail_capture_ms: u32,
    /// Hard capture limit. This bounds both recording duration and the WAV
    /// space consumed if a key-release event is lost.
    #[serde(default = "default_max_recording_seconds")]
    pub max_recording_seconds: u32,
    /// Exact CPAL device name to use, or an empty string to follow the
    /// desktop's current default input device.
    #[serde(default)]
    pub input_device: String,
    /// Opt-in environmental noise control. Voxkey restores the sink only when
    /// it changed an originally-unmuted sink itself.
    #[serde(default)]
    pub mute_output_while_recording: bool,
}

fn default_shortcut_id() -> String {
    shortcut_id_for_trigger(&default_shortcut_trigger())
}

fn default_shortcut_description() -> String {
    "Dictate".to_string()
}

fn default_shortcut_trigger() -> String {
    voxkey_ipc::DEFAULT_SHORTCUT_TRIGGER.to_string()
}

/// Give each requested trigger a distinct portal action identity.
///
/// GlobalShortcuts backends persist a user's binding by application and
/// shortcut ID. Once an ID has been seen, `preferred_trigger` is only a hint
/// for that first registration; reusing the ID with a different hint returns
/// the old binding. A deterministic ID keeps normal restarts stable while a
/// user-requested trigger change is presented to the desktop as a new action.
pub(crate) fn shortcut_id_for_trigger(trigger: &str) -> String {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET_BASIS;
    for byte in trigger.trim().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }

    format!("dictate_toggle_{hash:016x}")
}

fn config_home() -> PathBuf {
    config_home_from(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

fn config_home_from(
    xdg_config_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> PathBuf {
    xdg_config_home
        .filter(|path| !path.is_empty() && Path::new(path).is_absolute())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            home.map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("~"))
                .join(".config")
        })
}

fn default_token_path() -> String {
    config_home()
        .join("voxkey")
        .join("restore_token")
        .to_string_lossy()
        .into_owned()
}

fn default_sample_rate() -> u32 {
    16000
}

fn default_channels() -> u16 {
    1
}

fn default_tail_capture_ms() -> u32 {
    1000
}

fn default_max_recording_seconds() -> u32 {
    600
}

pub(crate) fn normalize_transcriber_config(config: &mut TranscriberConfig) {
    normalize_model(
        &mut config.mistral.model,
        voxkey_ipc::MistralConfig::DEFAULT_MODEL,
    );
    normalize_model(
        &mut config.mistral_realtime.model,
        voxkey_ipc::MistralRealtimeConfig::DEFAULT_MODEL,
    );
    normalize_model(
        &mut config.parakeet.model,
        voxkey_ipc::ParakeetConfig::DEFAULT_MODEL,
    );
}

pub(crate) fn normalize_injection_config(config: &mut InjectionConfig) {
    config.typing_delay_ms = config
        .typing_delay_ms
        .min(InjectionConfig::MAX_TYPING_DELAY_MS);
}

pub(crate) fn normalize_preview_config(config: &mut PreviewConfig) {
    config.max_audio_seconds = config
        .max_audio_seconds
        .min(PreviewConfig::MAX_AUDIO_SECONDS);
}

fn normalize_model(model: &mut String, default: &str) {
    let normalized = model.trim();
    *model = if normalized.is_empty() {
        default.to_string()
    } else {
        normalized.to_string()
    };
}

impl Default for ShortcutConfig {
    fn default() -> Self {
        Self {
            id: default_shortcut_id(),
            description: default_shortcut_description(),
            trigger: default_shortcut_trigger(),
        }
    }
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            token_path: default_token_path(),
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: default_sample_rate(),
            channels: default_channels(),
            tail_capture_ms: default_tail_capture_ms(),
            max_recording_seconds: default_max_recording_seconds(),
            input_device: String::new(),
            mute_output_while_recording: false,
        }
    }
}

/// Old-format transcriber section with bare command/args fields.
#[derive(Deserialize)]
struct LegacyTranscriberFields {
    command: Option<String>,
    args: Option<Vec<String>>,
}

/// Mirror of Config that captures old-format legacy fields for migration.
#[derive(Deserialize)]
struct LegacyConfig {
    #[serde(default)]
    transcriber: Option<LegacyTranscriberFields>,
}

impl Config {
    /// Load configuration from the standard config file location.
    /// Falls back to defaults if the file doesn't exist.
    /// Migrates old-format `[transcriber]` (bare `command`/`args`) to the
    /// provider-based structure.
    pub fn load() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let config_path = Self::config_file_path();
        Self::load_from_path(&config_path)
    }

    fn load_from_path(
        config_path: &Path,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let fd = match rustix::fs::open(
            config_path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(Config::default()),
            Err(error) => return Err(std::io::Error::from(error).into()),
        };
        let metadata = rustix::fs::fstat(&fd)?;
        if !rustix::fs::FileType::from_raw_mode(metadata.st_mode).is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "configuration path is not a regular file: {}",
                    config_path.display()
                ),
            )
            .into());
        }
        if metadata.st_uid != rustix::process::geteuid().as_raw() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "configuration file is not owned by the current user: {}",
                    config_path.display()
                ),
            )
            .into());
        }

        // Secure the exact inode opened above before parsing anything from it.
        // O_NOFOLLOW plus fd-based metadata/chmod avoids a symlink-swap window.
        rustix::fs::fchmod(&fd, rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR)?;
        let mut file: std::fs::File = fd.into();
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        Self::load_from_str(&contents)
    }

    fn load_from_str(contents: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // The new TranscriberConfig silently ignores unknown fields like
        // bare `command`/`args`, so this always succeeds — but loses custom
        // whisper-cpp settings from old configs. We detect and migrate them.
        let mut config: Config = toml::from_str(contents)?;

        // Check for legacy bare command/args under [transcriber]
        if let Ok(legacy) = toml::from_str::<LegacyConfig>(contents)
            && let Some(legacy_t) = legacy.transcriber
        {
            let has_legacy = legacy_t.command.is_some() || legacy_t.args.is_some();
            if has_legacy {
                if let Some(cmd) = legacy_t.command {
                    config.transcriber.whisper_cpp.command = cmd;
                }
                if let Some(args) = legacy_t.args {
                    config.transcriber.whisper_cpp.args = args;
                }
                tracing::info!("Migrated legacy transcriber config format");
            }
        }

        normalize_transcriber_config(&mut config.transcriber);

        // Older releases represented every OpenAI-compatible batch server as
        // "mistral", even when that server actually hosted a Parakeet model.
        // Preserve those installations while separating model family from
        // transport in the current schema.
        if config.transcriber.provider == voxkey_ipc::TranscriberProvider::Mistral
            && config.transcriber.mistral.model.starts_with("parakeet-")
            && !config.transcriber.mistral.endpoint.trim().is_empty()
        {
            config.transcriber.provider = voxkey_ipc::TranscriberProvider::Parakeet;
            config.transcriber.parakeet.model = config.transcriber.mistral.model.clone();
            config.transcriber.parakeet.backend = voxkey_ipc::ParakeetBackend::Http;
            config.transcriber.parakeet.endpoint = config.transcriber.mistral.endpoint.clone();
            config.transcriber.mistral.model = voxkey_ipc::MistralConfig::DEFAULT_MODEL.to_string();
            config.transcriber.mistral.endpoint.clear();
            tracing::info!("Migrated legacy Parakeet HTTP server configuration");
        }

        // Voxkey previously defaulted to Super+Space, which GNOME reserves
        // for switching to the next input source. Do not keep requesting that
        // collision from existing configs created with the old default.
        if let Err(error) = validate_shortcut_trigger(&config.shortcut.trigger) {
            config.shortcut.trigger = default_shortcut_trigger();
            tracing::warn!("Migrated invalid shortcut to Super+Alt+D: {error}");
        }
        config.shortcut.id = shortcut_id_for_trigger(&config.shortcut.trigger);
        if config.shortcut.description == "Hold to dictate" {
            config.shortcut.description = default_shortcut_description();
        }

        // D-Bus rejects zero-valued formats, but users can also edit the TOML
        // file directly. Keep those persisted values from reaching CPAL and
        // duration/sample-count calculations that require a real format.
        if config.audio.sample_rate == 0 {
            tracing::warn!("Ignoring zero audio sample rate from config; using the default");
            config.audio.sample_rate = default_sample_rate();
        }
        if config.audio.channels == 0 {
            tracing::warn!("Ignoring zero audio channel count from config; using the default");
            config.audio.channels = default_channels();
        }
        if config.audio.max_recording_seconds == 0 {
            tracing::warn!("Ignoring zero recording limit from config; using the default");
            config.audio.max_recording_seconds = default_max_recording_seconds();
        }
        if config.audio.max_recording_seconds > 3_600 {
            tracing::warn!("Recording limit exceeds 3600s; clamping it");
            config.audio.max_recording_seconds = 3_600;
        }
        normalize_injection_config(&mut config.injection);
        // 120 seconds was the old built-in default, written into some config
        // files before agreement-based seeking bounded normal decode work.
        // Treat that legacy value as unlimited so long dictations regain live
        // previews without requiring users to edit generated configuration.
        if config.preview.max_audio_seconds == 120 {
            tracing::info!("Migrated the legacy 120s preview limit to unlimited");
            config.preview.max_audio_seconds = 0;
        }
        if config.preview.max_audio_seconds > PreviewConfig::MAX_AUDIO_SECONDS {
            tracing::warn!(
                "Preview audio limit exceeds {}s; clamping it",
                PreviewConfig::MAX_AUDIO_SECONDS
            );
            config.preview.max_audio_seconds = PreviewConfig::MAX_AUDIO_SECONDS;
        }

        Ok(config)
    }

    fn config_file_path() -> PathBuf {
        config_home().join("voxkey").join("config.toml")
    }

    /// Persist only fields changed from `previous`, merging them into the
    /// latest on-disk document. Comments, unknown keys, and unrelated manual
    /// edits made while the daemon is running remain untouched.
    pub fn save_delta(
        previous: &Config,
        current: &Config,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let config_path = Self::config_file_path();
        Self::save_delta_to(&config_path, previous, current)?;
        tracing::info!("Configuration saved to {}", config_path.display());
        Ok(())
    }

    fn save_delta_to(
        path: &Path,
        previous: &Config,
        current: &Config,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut document = match std::fs::read_to_string(path) {
            Ok(contents) => contents.parse::<toml_edit::DocumentMut>()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                toml_edit::DocumentMut::new()
            }
            Err(error) => return Err(error.into()),
        };
        let previous = toml_edit::ser::to_document(previous)?;
        let current = toml_edit::ser::to_document(current)?;
        apply_table_delta(
            document.as_table_mut(),
            Some(previous.as_table()),
            current.as_table(),
        );
        crate::persistence::write_private(path, document.to_string().as_bytes())?;
        Ok(())
    }

    /// Write the config through a private temporary file and rename it into
    /// place. The config holds the user's dictionary, and a plaintext API key
    /// whenever the keyring was unavailable, so it stays readable only by its
    /// owner. Renaming also means an interrupted save leaves the previous
    /// configuration intact instead of a truncated file the daemon
    /// cannot parse on its next start.
    #[cfg(test)]
    fn save_to(
        path: &Path,
        config: &Config,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let contents = toml::to_string_pretty(config)?;
        crate::persistence::write_private(path, contents.as_bytes())?;
        Ok(())
    }

    /// Resolve the token path, respecting VOXKEY_RESTORE_TOKEN_PATH env var override.
    pub fn token_path(&self) -> Result<PathBuf, String> {
        token_path_from(
            std::env::var_os("VOXKEY_RESTORE_TOKEN_PATH").as_deref(),
            &self.persistence.token_path,
            &default_token_path(),
            std::env::var_os("HOME").as_deref(),
        )
    }
}

fn apply_table_delta(
    target: &mut dyn toml_edit::TableLike,
    previous: Option<&dyn toml_edit::TableLike>,
    current: &dyn toml_edit::TableLike,
) {
    let mut keys = std::collections::BTreeSet::new();
    if let Some(previous) = previous {
        keys.extend(previous.iter().map(|(key, _)| key.to_string()));
    }
    keys.extend(current.iter().map(|(key, _)| key.to_string()));

    for key in keys {
        let previous_item = previous.and_then(|table| table.get(&key));
        let current_item = current.get(&key);
        if items_match(previous_item, current_item) {
            continue;
        }

        let Some(current_item) = current_item else {
            target.remove(&key);
            continue;
        };

        if let Some(current_table) = current_item.as_table_like() {
            let previous_table = previous_item.and_then(toml_edit::Item::as_table_like);
            if target
                .get(&key)
                .and_then(toml_edit::Item::as_table_like)
                .is_none()
            {
                target.insert(&key, toml_edit::Item::Table(toml_edit::Table::new()));
            }
            let target_table = target
                .get_mut(&key)
                .and_then(toml_edit::Item::as_table_like_mut)
                .expect("newly inserted TOML table must be table-like");
            apply_table_delta(target_table, previous_table, current_table);
            continue;
        }

        let mut replacement = current_item.clone();
        if let Some(existing) = target.get(&key) {
            preserve_item_decor(existing, &mut replacement);
        }
        target.insert(&key, replacement);
    }
}

fn items_match(previous: Option<&toml_edit::Item>, current: Option<&toml_edit::Item>) -> bool {
    match (previous, current) {
        (None, None) => true,
        (Some(previous), Some(current)) => previous.to_string() == current.to_string(),
        _ => false,
    }
}

fn preserve_item_decor(existing: &toml_edit::Item, replacement: &mut toml_edit::Item) {
    match (existing, replacement) {
        (toml_edit::Item::Value(existing), toml_edit::Item::Value(replacement)) => {
            *replacement.decor_mut() = existing.decor().clone();
        }
        (toml_edit::Item::Table(existing), toml_edit::Item::Table(replacement)) => {
            *replacement.decor_mut() = existing.decor().clone();
        }
        _ => {}
    }
}

fn token_path_from(
    override_path: Option<&std::ffi::OsStr>,
    configured_path: &str,
    default_path: &str,
    home: Option<&std::ffi::OsStr>,
) -> Result<PathBuf, String> {
    let selected = override_path
        .filter(|path| {
            !path.is_empty()
                && !path
                    .to_str()
                    .is_some_and(|utf8_path| utf8_path.trim().is_empty())
        })
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(if configured_path.trim().is_empty() {
                default_path
            } else {
                configured_path
            })
        });
    absolute_user_path(&selected, home).ok_or_else(|| {
        format!(
            "restore token path must be absolute (or start with ~/ when HOME is set): {}",
            selected.display()
        )
    })
}

fn absolute_user_path(path: &Path, home: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    let home = home.map(Path::new).filter(|home| home.is_absolute())?;
    let text = path.to_str()?;
    let suffix = if text == "~" || text == "$HOME" || text == "${HOME}" {
        ""
    } else if let Some(suffix) = text.strip_prefix("~/") {
        suffix
    } else if let Some(suffix) = text.strip_prefix("$HOME/") {
        suffix
    } else {
        text.strip_prefix("${HOME}/")?
    };
    Some(home.join(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use voxkey_ipc::TranscriberProvider;

    #[test]
    fn load_old_format_migrates_command_and_args() {
        let toml = r#"
[transcriber]
command = "/usr/local/bin/my-whisper"
args = ["-m", "model.bin", "{audio_file}"]
"#;
        let config = Config::load_from_str(toml).unwrap();
        assert_eq!(config.transcriber.provider, TranscriberProvider::WhisperCpp);
        assert_eq!(
            config.transcriber.whisper_cpp.command,
            "/usr/local/bin/my-whisper"
        );
        assert_eq!(
            config.transcriber.whisper_cpp.args,
            vec!["-m", "model.bin", "{audio_file}"]
        );
    }

    #[test]
    fn load_new_format_preserves_provider() {
        let toml = r#"
[transcriber]
provider = "mistral"

[transcriber.whisper_cpp]
command = "whisper-cpp"
args = []

[transcriber.mistral]
api_key = "sk-test"
model = "voxtral-mini-2602"
"#;
        let config = Config::load_from_str(toml).unwrap();
        assert_eq!(config.transcriber.provider, TranscriberProvider::Mistral);
        assert_eq!(config.transcriber.mistral.api_key, "sk-test");
    }

    #[test]
    fn blank_mistral_models_fall_back_to_provider_defaults() {
        let toml = r#"
[transcriber.mistral]
model = "   "

[transcriber.mistral_realtime]
model = "\t"
"#;

        let config = Config::load_from_str(toml).unwrap();

        assert_eq!(
            config.transcriber.mistral.model,
            voxkey_ipc::MistralConfig::DEFAULT_MODEL
        );
        assert_eq!(
            config.transcriber.mistral_realtime.model,
            voxkey_ipc::MistralRealtimeConfig::DEFAULT_MODEL
        );
    }

    #[test]
    fn blank_parakeet_model_falls_back_to_the_provider_default() {
        let config = Config::load_from_str("[transcriber.parakeet]\nmodel = \"  \\t \"\n").unwrap();

        assert_eq!(
            config.transcriber.parakeet.model,
            voxkey_ipc::ParakeetConfig::DEFAULT_MODEL
        );
    }

    #[test]
    fn migrates_legacy_parakeet_http_server_out_of_mistral_config() {
        let toml = r#"
[transcriber]
provider = "mistral"

[transcriber.mistral]
model = "parakeet-tdt-0.6b-v3"
endpoint = "http://192.168.1.132:8000/v1/audio/transcriptions"
"#;
        let config = Config::load_from_str(toml).unwrap();

        assert_eq!(config.transcriber.provider, TranscriberProvider::Parakeet);
        assert_eq!(
            config.transcriber.parakeet.backend,
            voxkey_ipc::ParakeetBackend::Http
        );
        assert_eq!(config.transcriber.parakeet.model, "parakeet-tdt-0.6b-v3");
        assert_eq!(
            config.transcriber.parakeet.endpoint,
            "http://192.168.1.132:8000/v1/audio/transcriptions"
        );
        assert!(!config.transcriber.parakeet.allow_insecure_http);
        assert_eq!(
            config.transcriber.mistral.model,
            voxkey_ipc::MistralConfig::DEFAULT_MODEL
        );
        assert!(config.transcriber.mistral.endpoint.is_empty());
    }

    #[test]
    fn load_empty_toml_gives_defaults() {
        let config = Config::load_from_str("").unwrap();
        assert_eq!(config.transcriber.provider, TranscriberProvider::WhisperCpp);
        assert_eq!(config.transcriber.whisper_cpp.command, "whisper-cpp");
    }

    #[test]
    fn config_without_dictionary_section_gets_empty_default() {
        let config = Config::load_from_str("").unwrap();
        assert!(config.dictionary.replacements.is_empty());
        assert!(config.dictionary.vocabulary.is_empty());
    }

    #[test]
    fn dictionary_section_round_trips_through_toml() {
        let mut config = Config::default();
        config.dictionary.vocabulary = vec!["Voxkey".to_string()];
        config.dictionary.replacements = vec![voxkey_ipc::WordReplacement {
            original: "jon".to_string(),
            replacement: "John".to_string(),
            enabled: true,
        }];
        let serialized = toml::to_string_pretty(&config).unwrap();
        let parsed = Config::load_from_str(&serialized).unwrap();
        assert_eq!(parsed.dictionary.vocabulary, vec!["Voxkey"]);
        assert_eq!(parsed.dictionary.replacements.len(), 1);
    }

    #[test]
    fn load_old_format_preserves_other_sections() {
        let toml = r#"
[shortcut]
trigger = "<Control>d"

[transcriber]
command = "my-whisper"

[audio]
sample_rate = 48000
"#;
        let config = Config::load_from_str(toml).unwrap();
        assert_eq!(config.shortcut.trigger, "<Control>d");
        assert_eq!(config.transcriber.whisper_cpp.command, "my-whisper");
        assert_eq!(config.audio.sample_rate, 48000);
    }

    #[test]
    fn existing_audio_config_gets_default_tail_capture() {
        let config = Config::load_from_str("[audio]\nsample_rate = 16000\nchannels = 1\n").unwrap();
        assert_eq!(config.audio.tail_capture_ms, 1000);
        assert_eq!(
            config.audio.max_recording_seconds,
            default_max_recording_seconds()
        );
        assert!(config.audio.input_device.is_empty());
    }

    #[test]
    fn persisted_zero_audio_format_falls_back_to_safe_defaults() {
        let config = Config::load_from_str("[audio]\nsample_rate = 0\nchannels = 0\n").unwrap();

        assert_eq!(config.audio.sample_rate, default_sample_rate());
        assert_eq!(config.audio.channels, default_channels());
    }

    #[test]
    fn excessive_typing_delay_is_capped_at_the_settings_limit() {
        let config = Config::load_from_str("[injection]\ntyping_delay_ms = 4294967295\n").unwrap();

        assert_eq!(
            config.injection.typing_delay_ms,
            InjectionConfig::MAX_TYPING_DELAY_MS
        );
    }

    #[test]
    fn audio_input_device_round_trips_through_toml() {
        let mut config = Config::default();
        config.audio.input_device = "USB Microphone".to_string();
        let serialized = toml::to_string_pretty(&config).unwrap();
        let parsed = Config::load_from_str(&serialized).unwrap();
        assert_eq!(parsed.audio.input_device, "USB Microphone");
    }

    #[test]
    fn old_gnome_input_source_shortcut_is_migrated_in_memory() {
        let config = Config::load_from_str("[shortcut]\ntrigger = \"<Super>space\"\n").unwrap();
        assert_eq!(config.shortcut.trigger, "<Super><Alt>d");
    }

    #[test]
    fn portal_shortcut_identity_changes_with_the_requested_trigger() {
        let default_id = shortcut_id_for_trigger("<Super><Alt>d");
        let single_key_id = shortcut_id_for_trigger("F13");

        assert_eq!(default_id, shortcut_id_for_trigger(" <Super><Alt>d "));
        assert_ne!(default_id, single_key_id);
        assert!(default_id.starts_with("dictate_toggle_"));
    }

    #[test]
    fn legacy_shortcut_identity_is_migrated_to_the_configured_trigger() {
        let config = Config::load_from_str(
            "[shortcut]\nid = \"dictate_hold\"\ndescription = \"Hold to dictate\"\ntrigger = \"F13\"\n",
        )
        .unwrap();

        assert_eq!(config.shortcut.id, shortcut_id_for_trigger("F13"));
        assert_eq!(config.shortcut.description, "Dictate");
    }

    #[test]
    fn blank_shortcut_trigger_is_replaced_with_the_default() {
        for trigger in ["", "   \t"] {
            assert!(validate_shortcut_trigger(trigger).is_err());
            let source = format!("[shortcut]\ntrigger = {trigger:?}\n");
            let config = Config::load_from_str(&source).unwrap();
            assert_eq!(config.shortcut.trigger, "<Super><Alt>d");
        }
    }

    #[test]
    fn shortcut_validation_rejects_unsafe_keys_without_a_non_shift_modifier() {
        for trigger in [
            "d",
            "space",
            "Return",
            "Left",
            "Escape",
            "comma",
            "<Shift>d",
            "<Shift>space",
        ] {
            assert!(
                validate_shortcut_trigger(trigger).is_err(),
                "global shortcut {trigger:?} would make its key untypeable"
            );
        }

        for trigger in ["<Control>d", "<Alt>d", "<Super>d", "<Meta>d", "<Hyper>d"] {
            assert!(
                validate_shortcut_trigger(trigger).is_ok(),
                "shortcut {trigger:?} has a non-Shift modifier"
            );
        }
    }

    #[test]
    fn shortcut_validation_allows_safe_single_keys() {
        for trigger in [
            "F1",
            "F8",
            "F13",
            "F35",
            "<Shift>F8",
            "KP_F1",
            "Pause",
            "Print",
            "AudioRecord",
            "XF86AudioRecord",
            "AudioMicMute",
            "Dictate",
            "XF86Dictate",
        ] {
            assert!(
                validate_shortcut_trigger(trigger).is_ok(),
                "safe single-key shortcut {trigger:?} was rejected"
            );
        }
    }

    #[test]
    fn shortcut_validation_rejects_modifier_only_accelerators() {
        for trigger in ["<Control>", "<Super><Shift>", "<Alt> \t"] {
            assert!(
                validate_shortcut_trigger(trigger).is_err(),
                "modifier-only shortcut {trigger:?} cannot activate dictation"
            );
            let source = format!("[shortcut]\ntrigger = {trigger:?}\n");
            let config = Config::load_from_str(&source).unwrap();
            assert_eq!(config.shortcut.trigger, "<Super><Alt>d");
        }
    }

    #[test]
    fn every_persisted_gnome_input_source_shortcut_is_migrated() {
        for trigger in ["<Super><Shift>space", "Super+Space"] {
            let source = format!("[shortcut]\ntrigger = {trigger:?}\n");
            let config = Config::load_from_str(&source).unwrap();
            assert_eq!(
                config.shortcut.trigger, "<Super><Alt>d",
                "reserved trigger {trigger:?} survived config loading"
            );
        }
    }

    #[test]
    fn preview_defaults_keep_network_providers_off_the_hook() {
        let config = Config::load_from_str("").unwrap();
        assert_eq!(config.preview.mode, PreviewMode::Auto);
        assert_eq!(config.preview.strategy, PreviewStrategy::Whole);
        assert_eq!(config.preview.interval_ms, 1000);
        assert!(config.preview.allows(true));
        assert!(!config.preview.allows(false));
    }

    #[test]
    fn preview_strategy_parses_whole() {
        let config = Config::load_from_str("[preview]\nstrategy = \"whole\"\n").unwrap();
        assert_eq!(config.preview.strategy, PreviewStrategy::Whole);
    }

    #[test]
    fn preview_strategy_rejects_unknown_values() {
        assert!(Config::load_from_str("[preview]\nstrategy = \"sideways\"\n").is_err());
    }

    #[test]
    fn preview_mode_always_opts_network_providers_in() {
        let config = Config::load_from_str("[preview]\nmode = \"always\"\n").unwrap();
        assert!(config.preview.allows(false));
        assert!(config.preview.allows(true));
    }

    #[test]
    fn preview_mode_never_disables_every_provider() {
        let config = Config::load_from_str("[preview]\nmode = \"never\"\n").unwrap();
        assert!(!config.preview.allows(true));
        assert!(!config.preview.allows(false));
    }

    #[test]
    fn preview_interval_never_drops_below_a_sane_floor() {
        let config = Config::load_from_str("[preview]\ninterval_ms = 1\n").unwrap();
        assert_eq!(
            config.preview.interval(),
            std::time::Duration::from_millis(250)
        );
    }

    #[test]
    fn preview_audio_retention_is_bounded_after_config_load() {
        let config =
            Config::load_from_str(&format!("[preview]\nmax_audio_seconds = {}\n", u32::MAX))
                .unwrap();

        assert_eq!(
            config.preview.max_audio_seconds,
            PreviewConfig::MAX_AUDIO_SECONDS
        );
    }

    #[test]
    fn preview_section_round_trips_through_toml() {
        let mut config = Config::default();
        config.preview.mode = PreviewMode::Always;
        config.preview.interval_ms = 3000;
        config.preview.max_audio_seconds = 45;
        let parsed = Config::load_from_str(&toml::to_string_pretty(&config).unwrap()).unwrap();
        assert_eq!(parsed.preview.mode, PreviewMode::Always);
        assert_eq!(parsed.preview.interval_ms, 3000);
        assert_eq!(parsed.preview.max_audio_seconds, 45);
    }

    #[test]
    fn blank_xdg_config_home_uses_the_home_directory_default() {
        assert_eq!(
            config_home_from(
                Some(std::ffi::OsStr::new("")),
                Some(std::ffi::OsStr::new("/home/test")),
            ),
            PathBuf::from("/home/test/.config")
        );
        assert_eq!(
            config_home_from(
                Some(std::ffi::OsStr::new("relative-config")),
                Some(std::ffi::OsStr::new("/home/test")),
            ),
            PathBuf::from("/home/test/.config")
        );
    }

    #[test]
    fn blank_restore_token_override_uses_the_configured_path() {
        assert_eq!(
            token_path_from(
                Some(std::ffi::OsStr::new("")),
                "/config/restore_token",
                "/default/restore_token",
                Some(std::ffi::OsStr::new("/home/test")),
            )
            .unwrap(),
            PathBuf::from("/config/restore_token")
        );
    }

    #[test]
    fn whitespace_restore_token_override_uses_the_configured_path() {
        assert_eq!(
            token_path_from(
                Some(std::ffi::OsStr::new("  \t")),
                "/config/restore_token",
                "/default/restore_token",
                Some(std::ffi::OsStr::new("/home/test")),
            )
            .unwrap(),
            PathBuf::from("/config/restore_token")
        );
    }

    #[test]
    fn blank_configured_restore_token_path_uses_the_default() {
        for configured in ["", "  \t"] {
            assert_eq!(
                token_path_from(
                    None,
                    configured,
                    "/default/restore_token",
                    Some(std::ffi::OsStr::new("/home/test")),
                )
                .unwrap(),
                PathBuf::from("/default/restore_token")
            );
        }
    }

    #[test]
    fn restore_token_path_expands_common_home_prefixes() {
        for configured in ["~/token", "$HOME/token", "${HOME}/token"] {
            assert_eq!(
                token_path_from(
                    None,
                    configured,
                    "/default/restore_token",
                    Some(std::ffi::OsStr::new("/home/test")),
                )
                .unwrap(),
                PathBuf::from("/home/test/token")
            );
        }
    }

    #[test]
    fn relative_restore_token_path_is_rejected_instead_of_using_service_cwd() {
        let error = token_path_from(
            None,
            "relative/token",
            "/default/restore_token",
            Some(std::ffi::OsStr::new("/home/test")),
        )
        .unwrap_err();

        assert!(error.contains("must be absolute"));
    }

    /// The config can hold a plaintext API key whenever the keyring is
    /// unavailable, and always holds the user's dictionary. It must not be
    /// readable by other accounts on the machine.
    #[test]
    fn saved_config_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested/config.toml");
        Config::save_to(&path, &Config::default()).unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn saving_tightens_the_permissions_of_a_world_readable_config() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        Config::save_to(&path, &Config::default()).unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn loading_tightens_permissions_before_plaintext_secrets_are_used() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"[transcriber.mistral]
api_key = "plaintext-test-secret"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let loaded = Config::load_from_path(&path).unwrap();

        assert_eq!(loaded.transcriber.mistral.api_key, "plaintext-test-secret");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "a config containing a usable secret remained readable by other users"
        );
    }

    /// A save must replace the file in one step. A reader that already opened
    /// the config keeps seeing a complete document rather than a half-written
    /// one, which is exactly what a truncate-in-place write would expose.
    #[test]
    fn saving_replaces_the_config_in_one_step() {
        use std::io::Read;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");

        let mut first = Config::default();
        first.shortcut.trigger = "<Super>1".to_string();
        Config::save_to(&path, &first).unwrap();
        let original = std::fs::read_to_string(&path).unwrap();

        let mut reader = std::fs::File::open(&path).unwrap();

        let mut second = Config::default();
        second.shortcut.trigger = "<Super>2".to_string();
        Config::save_to(&path, &second).unwrap();

        let mut seen = String::new();
        reader.read_to_string(&mut seen).unwrap();
        assert_eq!(
            seen, original,
            "an open reader saw a partially written config"
        );
        assert_eq!(
            Config::load_from_str(&std::fs::read_to_string(&path).unwrap())
                .unwrap()
                .shortcut
                .trigger,
            "<Super>2"
        );
    }

    #[test]
    fn saving_leaves_no_scratch_files_behind() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        Config::save_to(&path, &Config::default()).unwrap();

        let entries: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, ["config.toml"]);
    }

    #[test]
    fn failed_config_publication_leaves_no_sensitive_scratch_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::create_dir(&path).unwrap();
        let mut config = Config::default();
        config.transcriber.mistral.api_key = "sk-private-fallback".to_string();

        assert!(Config::save_to(&path, &config).is_err());

        let entries = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, ["config.toml"]);
    }

    #[test]
    fn delta_save_preserves_comments_and_unknown_settings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let original = r#"# Keep this explanation.
future_root_setting = "untouched" # unknown root key

[shortcut]
trigger = "<Super><Alt>d" # user shortcut note
future_shortcut_setting = 17

[future_plugin]
enabled = true # unknown table
"#;
        std::fs::write(&path, original).unwrap();

        let previous = Config::load_from_str(original).unwrap();
        let mut current = previous.clone();
        current.shortcut.trigger = "<Control><Alt>d".to_string();
        Config::save_delta_to(&path, &previous, &current).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# Keep this explanation."));
        assert!(saved.contains("future_root_setting = \"untouched\" # unknown root key"));
        assert!(saved.contains("trigger = \"<Control><Alt>d\" # user shortcut note"));
        assert!(saved.contains("future_shortcut_setting = 17"));
        assert!(saved.contains("[future_plugin]"));
        assert!(saved.contains("enabled = true # unknown table"));
    }

    #[test]
    fn delta_save_does_not_clobber_an_unrelated_manual_edit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let loaded = r#"[shortcut]
trigger = "<Super><Alt>d"

[audio]
sample_rate = 16000
channels = 1
"#;
        let manually_edited = loaded.replace("sample_rate = 16000", "sample_rate = 48000");
        std::fs::write(&path, manually_edited).unwrap();

        let previous = Config::load_from_str(loaded).unwrap();
        let mut current = previous.clone();
        current.shortcut.trigger = "<Control><Alt>d".to_string();
        Config::save_delta_to(&path, &previous, &current).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("sample_rate = 48000"));
        assert!(saved.contains("trigger = \"<Control><Alt>d\""));
    }

    #[test]
    fn invalid_latest_toml_is_never_overwritten_by_a_delta_save() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        let invalid = b"[shortcut\ntrigger = broken\n";
        std::fs::write(&path, invalid).unwrap();

        let previous = Config::default();
        let mut current = previous.clone();
        current.shortcut.trigger = "<Control><Alt>d".to_string();
        assert!(Config::save_delta_to(&path, &previous, &current).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), invalid);
    }

    #[test]
    fn gnome_input_source_shortcuts_are_recognized() {
        assert!(conflicts_with_gnome_input_source("<Super>space"));
        assert!(conflicts_with_gnome_input_source("<Super><Shift>space"));
        assert!(conflicts_with_gnome_input_source("<Shift><Super>space"));
        assert!(conflicts_with_gnome_input_source("Super+Space"));
        assert!(conflicts_with_gnome_input_source("Press <Super>space"));
        assert!(!conflicts_with_gnome_input_source("<Super><Alt>d"));
    }

    #[test]
    fn duplicate_modifiers_cannot_disguise_a_gnome_input_source_shortcut() {
        for trigger in [
            "<Super><Super>space",
            "<Shift><Super><Shift>space",
            "<Super><Shift><Super>space",
        ] {
            assert!(
                conflicts_with_gnome_input_source(trigger),
                "duplicate modifiers disguised reserved trigger {trigger:?}"
            );
            assert!(validate_shortcut_trigger(trigger).is_err());
        }
    }

    #[test]
    fn extra_modifiers_do_not_become_reserved_based_on_their_order() {
        for trigger in ["<Control><Super>space", "<Super><Control>space"] {
            assert!(
                !conflicts_with_gnome_input_source(trigger),
                "an extra Control modifier makes {trigger:?} a different chord"
            );
            assert!(
                validate_shortcut_trigger(trigger).is_ok(),
                "validity must not depend on modifier ordering for {trigger:?}"
            );

            let source = format!("[shortcut]\ntrigger = {trigger:?}\n");
            let config = Config::load_from_str(&source).unwrap();
            assert_eq!(
                config.shortcut.trigger, trigger,
                "loading must preserve the valid custom shortcut"
            );
        }
    }
}
