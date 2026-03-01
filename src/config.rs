// ABOUTME: Loads TOML configuration and manages restore token persistence.
// ABOUTME: Provides defaults for shortcut, transcriber, audio, and persistence settings.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub use voxkey_ipc::{InjectionConfig, TranscriberConfig};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub shortcut: ShortcutConfig,
    #[serde(default)]
    pub transcriber: TranscriberConfig,
    #[serde(default)]
    pub injection: InjectionConfig,
    #[serde(default)]
    pub persistence: PersistenceConfig,
    #[serde(default)]
    pub audio: AudioConfig,
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
}

fn default_shortcut_id() -> String {
    "dictate_hold".to_string()
}

fn default_shortcut_description() -> String {
    "Dictate".to_string()
}

fn default_shortcut_trigger() -> String {
    "<Super>space".to_string()
}

fn default_token_path() -> String {
    let xdg_config = std::env::var("XDG_CONFIG_HOME")
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "~".to_string());
            format!("{home}/.config")
        });
    format!("{xdg_config}/voxkey/restore_token")
}

fn default_sample_rate() -> u32 {
    16000
}

fn default_channels() -> u16 {
    1
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
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            shortcut: ShortcutConfig::default(),
            transcriber: TranscriberConfig::default(),
            injection: InjectionConfig::default(),
            persistence: PersistenceConfig::default(),
            audio: AudioConfig::default(),
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
        if !config_path.exists() {
            return Ok(Config::default());
        }
        let contents = std::fs::read_to_string(&config_path)?;
        Self::load_from_str(&contents)
    }

    fn load_from_str(contents: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // The new TranscriberConfig silently ignores unknown fields like
        // bare `command`/`args`, so this always succeeds — but loses custom
        // whisper-cpp settings from old configs. We detect and migrate them.
        let mut config: Config = toml::from_str(contents)?;

        // Check for legacy bare command/args under [transcriber]
        if let Ok(legacy) = toml::from_str::<LegacyConfig>(contents) {
            if let Some(legacy_t) = legacy.transcriber {
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
        }

        Ok(config)
    }

    fn config_file_path() -> PathBuf {
        let xdg_config = std::env::var("XDG_CONFIG_HOME")
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| "~".to_string());
                format!("{home}/.config")
            });
        Path::new(&xdg_config).join("voxkey").join("config.toml")
    }

    /// Save the current configuration to the standard config file location.
    pub fn save(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let config_path = Self::config_file_path();
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)?;
        std::fs::write(&config_path, contents)?;
        tracing::info!("Configuration saved to {}", config_path.display());
        Ok(())
    }

    /// Resolve the token path, respecting VOXKEY_RESTORE_TOKEN_PATH env var override.
    pub fn token_path(&self) -> PathBuf {
        if let Ok(override_path) = std::env::var("VOXKEY_RESTORE_TOKEN_PATH") {
            return PathBuf::from(override_path);
        }
        PathBuf::from(&self.persistence.token_path)
    }
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
        assert_eq!(config.transcriber.whisper_cpp.command, "/usr/local/bin/my-whisper");
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
    fn load_empty_toml_gives_defaults() {
        let config = Config::load_from_str("").unwrap();
        assert_eq!(config.transcriber.provider, TranscriberProvider::WhisperCpp);
        assert_eq!(config.transcriber.whisper_cpp.command, "whisper-cpp");
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
}
