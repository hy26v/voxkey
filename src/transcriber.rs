// ABOUTME: Dispatches transcription to the configured provider (whisper-cpp subprocess or Mistral HTTP API).
// ABOUTME: Captures transcript text from either stdout or JSON response.

use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

use voxkey_ipc::{TranscriberConfig, TranscriberProvider};

/// Transcription backend selected by provider configuration.
pub enum Transcriber {
    WhisperCpp {
        command: String,
        args: Vec<String>,
    },
    Mistral {
        client: reqwest::Client,
        api_key: String,
        model: String,
        endpoint: String,
    },
    MistralRealtime {
        api_key: String,
        model: String,
    },
}

impl Transcriber {
    /// Whether this transcriber uses the streaming (real-time) flow rather than batch.
    pub fn is_streaming(&self) -> bool {
        matches!(self, Self::MistralRealtime { .. })
    }

    pub fn from_config(config: &TranscriberConfig) -> Self {
        match config.provider {
            TranscriberProvider::WhisperCpp => Self::WhisperCpp {
                command: config.whisper_cpp.command.clone(),
                args: config.whisper_cpp.args.clone(),
            },
            TranscriberProvider::Mistral => Self::Mistral {
                client: reqwest::Client::new(),
                api_key: config.mistral.api_key.clone(),
                model: config.mistral.model.clone(),
                endpoint: config.mistral.endpoint.clone(),
            },
            TranscriberProvider::MistralRealtime => Self::MistralRealtime {
                api_key: config.mistral_realtime.api_key.clone(),
                model: config.mistral_realtime.model.clone(),
            },
        }
    }

    /// Run transcription on the given audio file.
    /// Returns the transcript text, trimmed.
    pub async fn transcribe(
        &self,
        audio_path: &Path,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let result = match self {
            Self::WhisperCpp { command, args } => {
                transcribe_whisper_cpp(command, args, audio_path).await
            }
            Self::Mistral {
                client,
                api_key,
                model,
                endpoint,
            } => transcribe_mistral(client, api_key, model, endpoint, audio_path).await,
            Self::MistralRealtime { .. } => {
                unreachable!("streaming transcriber uses run_streaming_session, not transcribe()")
            }
        };

        // Clean up the temp audio file regardless of outcome
        if let Err(e) = tokio::fs::remove_file(audio_path).await {
            tracing::warn!("Failed to remove temp audio file: {e}");
        }

        result
    }
}

async fn transcribe_whisper_cpp(
    command: &str,
    args: &[String],
    audio_path: &Path,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let audio_str = audio_path.to_string_lossy();

    // Replace {audio_file} placeholder in args
    let resolved_args: Vec<String> = args
        .iter()
        .map(|arg| arg.replace("{audio_file}", &audio_str))
        .collect();

    tracing::info!(
        "Running transcription: {} {}",
        command,
        resolved_args.join(" ")
    );

    let output = Command::new(command)
        .args(&resolved_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?
        .wait_with_output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "Transcription command failed (exit {}): {}",
            output.status, stderr
        )
        .into());
    }

    let transcript = String::from_utf8_lossy(&output.stdout).trim().to_string();
    tracing::info!("Transcription complete ({} chars)", transcript.len());
    Ok(transcript)
}

/// Mistral audio transcription API response.
#[derive(serde::Deserialize)]
struct MistralTranscriptionResponse {
    text: String,
}

async fn transcribe_mistral(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    endpoint: &str,
    audio_path: &Path,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let url = if endpoint.is_empty() {
        voxkey_ipc::MistralConfig::DEFAULT_ENDPOINT
    } else {
        endpoint
    };
    tracing::info!("Sending audio to Mistral API (model: {model}, endpoint: {url})");

    let file_bytes = tokio::fs::read(audio_path).await?;
    let file_name = audio_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "audio.wav".to_string());

    let file_part = reqwest::multipart::Part::bytes(file_bytes)
        .file_name(file_name)
        .mime_str("audio/wav")?;

    let form = reqwest::multipart::Form::new()
        .text("model", model.to_string())
        .part("file", file_part);

    let response = client
        .post(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .multipart(form)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Mistral API error ({status}): {body}").into());
    }

    let parsed: MistralTranscriptionResponse = response.json().await?;
    let transcript = parsed.text.trim().to_string();
    tracing::info!("Transcription complete ({} chars)", transcript.len());
    Ok(transcript)
}

#[cfg(test)]
fn parse_mistral_response(
    json: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let parsed: MistralTranscriptionResponse = serde_json::from_str(json)?;
    Ok(parsed.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use voxkey_ipc::{MistralConfig, MistralRealtimeConfig, WhisperCppConfig};

    #[test]
    fn from_config_creates_whisper_cpp_variant() {
        let config = TranscriberConfig {
            provider: TranscriberProvider::WhisperCpp,
            whisper_cpp: WhisperCppConfig {
                command: "/usr/bin/whisper".to_string(),
                args: vec!["-m".to_string(), "model.bin".to_string()],
            },
            mistral: MistralConfig::default(),
            mistral_realtime: MistralRealtimeConfig::default(),
        };
        let t = Transcriber::from_config(&config);
        match t {
            Transcriber::WhisperCpp { command, args } => {
                assert_eq!(command, "/usr/bin/whisper");
                assert_eq!(args, vec!["-m", "model.bin"]);
            }
            _ => panic!("Expected WhisperCpp variant"),
        }
    }

    #[test]
    fn from_config_creates_mistral_variant() {
        let config = TranscriberConfig {
            provider: TranscriberProvider::Mistral,
            whisper_cpp: WhisperCppConfig::default(),
            mistral: MistralConfig {
                api_key: "sk-test".to_string(),
                model: "voxtral-mini-2602".to_string(),
                endpoint: String::new(),
            },
            mistral_realtime: MistralRealtimeConfig::default(),
        };
        let t = Transcriber::from_config(&config);
        match t {
            Transcriber::Mistral {
                api_key, model, endpoint: _, ..
            } => {
                assert_eq!(api_key, "sk-test");
                assert_eq!(model, "voxtral-mini-2602");
            }
            _ => panic!("Expected Mistral variant"),
        }
    }

    #[test]
    fn from_config_creates_mistral_realtime_variant() {
        let config = TranscriberConfig {
            provider: TranscriberProvider::MistralRealtime,
            whisper_cpp: WhisperCppConfig::default(),
            mistral: MistralConfig::default(),
            mistral_realtime: MistralRealtimeConfig {
                api_key: "sk-rt".to_string(),
                model: "voxtral-mini-transcribe-realtime-2602".to_string(),
                endpoint: String::new(),
            },
        };
        let t = Transcriber::from_config(&config);
        match t {
            Transcriber::MistralRealtime { api_key, model } => {
                assert_eq!(api_key, "sk-rt");
                assert_eq!(model, "voxtral-mini-transcribe-realtime-2602");
            }
            _ => panic!("Expected MistralRealtime variant"),
        }
    }

    #[test]
    fn is_streaming_returns_true_for_mistral_realtime() {
        let t = Transcriber::MistralRealtime {
            api_key: String::new(),
            model: String::new(),
        };
        assert!(t.is_streaming());
    }

    #[test]
    fn is_streaming_returns_false_for_batch_providers() {
        let whisper = Transcriber::WhisperCpp {
            command: String::new(),
            args: vec![],
        };
        assert!(!whisper.is_streaming());

        let mistral = Transcriber::Mistral {
            client: reqwest::Client::new(),
            api_key: String::new(),
            model: String::new(),
            endpoint: String::new(),
        };
        assert!(!mistral.is_streaming());
    }

    #[test]
    fn parse_mistral_response_extracts_text() {
        let json = r#"{"text": " Hello, world! "}"#;
        let text = parse_mistral_response(json).unwrap();
        assert_eq!(text, " Hello, world! ");
    }

    #[test]
    fn parse_mistral_response_rejects_invalid_json() {
        let json = r#"{"error": "unauthorized"}"#;
        assert!(parse_mistral_response(json).is_err());
    }
}
