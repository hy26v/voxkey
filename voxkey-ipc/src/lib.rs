// ABOUTME: Shared D-Bus interface and config types between the voxkey daemon and settings GUI.
// ABOUTME: Defines bus name, object path, state types, transcriber config, and proxy trait for IPC.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Well-known bus name the daemon registers on the session bus.
pub const BUS_NAME: &str = "io.github.hy26v.Voxkey.Daemon";

/// Object path the daemon interface is served at.
pub const OBJECT_PATH: &str = "/io/github/hy26v/Voxkey/Daemon";

/// Daemon state as exposed over D-Bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DaemonState {
    Idle,
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
pub struct WhisperCppConfig {
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MistralConfig {
    pub api_key: String,
    pub model: String,
    #[serde(default)]
    pub endpoint: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MistralRealtimeConfig {
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParakeetConfig {
    pub model: String,
    #[serde(default)]
    pub execution_provider: ExecutionProviderChoice,
}

impl Default for ParakeetConfig {
    fn default() -> Self {
        Self {
            model: "parakeet-tdt-0.6b-v3".to_string(),
            execution_provider: ExecutionProviderChoice::Auto,
        }
    }
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

impl Default for TranscriberConfig {
    fn default() -> Self {
        Self {
            provider: TranscriberProvider::default(),
            whisper_cpp: WhisperCppConfig::default(),
            mistral: MistralConfig::default(),
            mistral_realtime: MistralRealtimeConfig::default(),
            parakeet: ParakeetConfig::default(),
        }
    }
}

/// Configuration for text injection behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InjectionConfig {
    #[serde(default = "default_typing_delay_ms")]
    pub typing_delay_ms: u32,
}

fn default_typing_delay_ms() -> u32 {
    5
}

impl Default for InjectionConfig {
    fn default() -> Self {
        Self {
            typing_delay_ms: default_typing_delay_ms(),
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

    /// Transcriber configuration as serialized JSON.
    #[zbus(property)]
    fn transcriber_config(&self) -> zbus::Result<String>;

    /// Injection configuration as serialized JSON.
    #[zbus(property)]
    fn injection_config(&self) -> zbus::Result<String>;

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

    /// Most recent error message, empty when no error.
    #[zbus(property)]
    fn last_error(&self) -> zbus::Result<String>;

    /// Update the shortcut trigger. Takes effect on next session recovery.
    fn set_shortcut(&self, trigger: &str) -> zbus::Result<()>;

    /// Update the transcriber configuration from JSON.
    fn set_transcriber_config(&self, config_json: &str) -> zbus::Result<()>;

    /// Update the injection configuration from JSON.
    fn set_injection_config(&self, config_json: &str) -> zbus::Result<()>;

    /// Update audio settings. Takes effect on next recording.
    fn set_audio(&self, sample_rate: u32, channels: u16) -> zbus::Result<()>;

    /// Re-read config.toml from disk.
    fn reload_config(&self) -> zbus::Result<()>;

    /// Delete the stored portal restore token, forcing a fresh session.
    fn clear_restore_token(&self) -> zbus::Result<()>;

    /// Shut down the daemon process.
    fn quit(&self) -> zbus::Result<()>;

    /// Start downloading a Parakeet model by name.
    fn download_model(&self, model_name: &str) -> zbus::Result<()>;

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
        assert_eq!("Streaming".parse::<DaemonState>().unwrap(), DaemonState::Streaming);
    }

    #[test]
    fn existing_config_without_mistral_realtime_gets_defaults() {
        let json = r#"{"provider":"whisper-cpp","whisper_cpp":{"command":"whisper-cpp","args":[]},"mistral":{"api_key":"","model":"voxtral-mini-2602"}}"#;
        let parsed: TranscriberConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.mistral_realtime.model, "voxtral-mini-transcribe-realtime-2602");
        assert!(parsed.mistral_realtime.api_key.is_empty());
    }

    #[test]
    fn parakeet_config_default_values() {
        let config = ParakeetConfig::default();
        assert_eq!(config.model, "parakeet-tdt-0.6b-v3");
        assert_eq!(config.execution_provider, ExecutionProviderChoice::Auto);
    }

    #[test]
    fn provider_serializes_parakeet_as_kebab_case() {
        let json = serde_json::to_string(&TranscriberProvider::Parakeet).unwrap();
        assert_eq!(json, "\"parakeet\"");
    }

    #[test]
    fn execution_provider_choice_serializes_as_kebab_case() {
        assert_eq!(serde_json::to_string(&ExecutionProviderChoice::Auto).unwrap(), "\"auto\"");
        assert_eq!(serde_json::to_string(&ExecutionProviderChoice::Cpu).unwrap(), "\"cpu\"");
        assert_eq!(serde_json::to_string(&ExecutionProviderChoice::Cuda).unwrap(), "\"cuda\"");
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
                execution_provider: ExecutionProviderChoice::Cuda,
            },
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: TranscriberConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn transcriber_config_toml_round_trip_parakeet() {
        let config = TranscriberConfig {
            provider: TranscriberProvider::Parakeet,
            whisper_cpp: WhisperCppConfig::default(),
            mistral: MistralConfig::default(),
            mistral_realtime: MistralRealtimeConfig::default(),
            parakeet: ParakeetConfig::default(),
        };
        let toml_str = toml::to_string(&config).unwrap();
        let parsed: TranscriberConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn existing_config_without_parakeet_gets_defaults() {
        let json = r#"{"provider":"whisper-cpp","whisper_cpp":{"command":"whisper-cpp","args":[]},"mistral":{"api_key":"","model":"voxtral-mini-2602"},"mistral_realtime":{"api_key":"","model":"voxtral-mini-transcribe-realtime-2602"}}"#;
        let parsed: TranscriberConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.parakeet.model, "parakeet-tdt-0.6b-v3");
        assert_eq!(parsed.parakeet.execution_provider, ExecutionProviderChoice::Auto);
    }
}
