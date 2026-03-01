// ABOUTME: Manages real-time streaming transcription via Mistral's WebSocket API.
// ABOUTME: Sends PCM audio chunks and injects text deltas as they arrive.

use std::sync::Arc;

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite;

use crate::dbus::{DaemonInterface, SharedState};
use crate::desktop::DesktopController;
use crate::injector;
use crate::state::Event;
use voxkey_ipc::MistralRealtimeConfig;

/// Run a streaming transcription session over WebSocket.
///
/// Connects to the Mistral realtime API, sends PCM audio from `audio_rx`,
/// injects text deltas as they arrive, and signals completion via `state_tx`.
pub async fn run_streaming_session(
    config: &MistralRealtimeConfig,
    sample_rate: u32,
    mut audio_rx: mpsc::Receiver<Vec<i16>>,
    desktop: Arc<DesktopController>,
    state_tx: mpsc::Sender<Event>,
    stop_rx: oneshot::Receiver<()>,
    shared: SharedState,
    connection: zbus::Connection,
    typing_delay: std::time::Duration,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let base_url = if config.endpoint.is_empty() {
        MistralRealtimeConfig::DEFAULT_ENDPOINT
    } else {
        &config.endpoint
    };
    let url = format!("{base_url}?model={}", config.model);

    // Extract host from wss://host/... for the Host header
    let host = url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("api.mistral.ai");

    let request = http::Request::builder()
        .uri(&url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .header("Host", host)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tungstenite::handshake::client::generate_key(),
        )
        .body(())?;

    let (ws_stream, _response) = tokio_tungstenite::connect_async(request).await?;
    let (mut ws_sink, mut ws_source) = ws_stream.split();

    tracing::info!("WebSocket connected to Mistral Realtime API");

    // Wait for session.created
    loop {
        match ws_source.next().await {
            Some(Ok(tungstenite::Message::Text(text))) => {
                let msg: ServerMessage = serde_json::from_str(&text)?;
                if msg.r#type == "session.created" {
                    tracing::info!("Streaming session created");
                    break;
                }
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
            None => return Err("WebSocket closed before session.created".into()),
        }
    }

    // Send session.update with audio format
    let session_update = SessionUpdate {
        r#type: "session.update",
        session: SessionConfig {
            audio_format: AudioFormat {
                encoding: "pcm_s16le",
                sample_rate,
            },
        },
    };
    let update_json = serde_json::to_string(&session_update)?;
    ws_sink
        .send(tungstenite::Message::Text(update_json.into()))
        .await?;

    // Main loop
    let mut accumulated_transcript = String::new();
    let mut draining = false;
    let mut stop_rx = Some(stop_rx);

    loop {
        tokio::select! {
            // Audio chunk from recorder
            chunk = audio_rx.recv(), if !draining => {
                match chunk {
                    Some(samples) => {
                        let encoded = encode_pcm_samples(&samples);
                        let msg = AudioAppend {
                            r#type: "input_audio.append",
                            audio: &encoded,
                        };
                        let json = serde_json::to_string(&msg)?;
                        ws_sink.send(tungstenite::Message::Text(json.into())).await?;
                    }
                    None => {
                        // Audio channel closed — treat as stop
                        tracing::info!("Audio channel closed, sending input_audio.end");
                        let end_msg = r#"{"type":"input_audio.end"}"#;
                        ws_sink.send(tungstenite::Message::Text(end_msg.into())).await?;
                        draining = true;
                    }
                }
            }

            // Stop signal from main loop (key released)
            result = async { stop_rx.as_mut().unwrap().await }, if stop_rx.is_some() && !draining => {
                let _ = result;
                tracing::info!("Stop signal received, sending input_audio.end");
                let end_msg = r#"{"type":"input_audio.end"}"#;
                ws_sink.send(tungstenite::Message::Text(end_msg.into())).await?;
                draining = true;
                stop_rx = None;
            }

            // WebSocket messages from server
            ws_msg = ws_source.next() => {
                match ws_msg {
                    Some(Ok(tungstenite::Message::Text(text))) => {
                        let msg: ServerMessage = serde_json::from_str(&text)?;
                        match msg.r#type.as_str() {
                            "transcription.text.delta" => {
                                if let Some(delta) = msg.text {
                                    match injector::inject_text(&desktop, &delta, typing_delay).await {
                                        Ok(()) => {}
                                        Err(injector::InjectionError::Portal(e)) => {
                                            return Err(format!("Portal error during streaming injection: {e}").into());
                                        }
                                        Err(injector::InjectionError::Local(e)) => {
                                            tracing::error!("Failed to inject text delta: {e}");
                                        }
                                    }
                                    accumulated_transcript.push_str(&delta);
                                }
                            }
                            "transcription.done" => {
                                tracing::info!("Streaming transcription complete ({} chars)", accumulated_transcript.len());
                                if !accumulated_transcript.is_empty() {
                                    shared.set_last_transcript(accumulated_transcript);
                                    DaemonInterface::notify_last_transcript(&connection).await;
                                }
                                let _ = state_tx.send(Event::InjectionDone).await;
                                return Ok(());
                            }
                            "error" => {
                                let error_text = msg.text.unwrap_or_else(|| text.to_string());
                                tracing::error!("Streaming API error: {error_text}");
                                return Err(format!("Streaming API error: {error_text}").into());
                            }
                            other => {
                                tracing::debug!("Ignoring WebSocket message type: {other}");
                            }
                        }
                    }
                    Some(Ok(tungstenite::Message::Close(_))) => {
                        tracing::info!("WebSocket closed by server");
                        if !accumulated_transcript.is_empty() {
                            shared.set_last_transcript(accumulated_transcript);
                            DaemonInterface::notify_last_transcript(&connection).await;
                        }
                        let _ = state_tx.send(Event::InjectionDone).await;
                        return Ok(());
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => {
                        tracing::error!("WebSocket error: {e}");
                        return Err(e.into());
                    }
                    None => {
                        tracing::info!("WebSocket stream ended");
                        let _ = state_tx.send(Event::InjectionDone).await;
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Encode i16 PCM samples as little-endian bytes then base64.
fn encode_pcm_samples(samples: &[i16]) -> String {
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(&bytes)
}

// -- Client -> Server message types --

#[derive(serde::Serialize)]
struct SessionUpdate<'a> {
    r#type: &'a str,
    session: SessionConfig<'a>,
}

#[derive(serde::Serialize)]
struct SessionConfig<'a> {
    audio_format: AudioFormat<'a>,
}

#[derive(serde::Serialize)]
struct AudioFormat<'a> {
    encoding: &'a str,
    sample_rate: u32,
}

#[derive(serde::Serialize)]
struct AudioAppend<'a> {
    r#type: &'a str,
    audio: &'a str,
}

// -- Server -> Client message types --

#[derive(serde::Deserialize)]
struct ServerMessage {
    r#type: String,
    #[serde(default)]
    text: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_pcm_samples_produces_correct_base64() {
        // Two samples: 0x0100 (256) and 0xFF7F (32767)
        let samples: Vec<i16> = vec![256, 32767];
        let encoded = encode_pcm_samples(&samples);
        // 256 in LE = [0x00, 0x01], 32767 in LE = [0xFF, 0x7F]
        let expected = base64::engine::general_purpose::STANDARD.encode([0x00, 0x01, 0xFF, 0x7F]);
        assert_eq!(encoded, expected);
    }

    #[test]
    fn encode_pcm_samples_handles_negative_values() {
        let samples: Vec<i16> = vec![-1, -32768];
        let encoded = encode_pcm_samples(&samples);
        // -1 in LE = [0xFF, 0xFF], -32768 in LE = [0x00, 0x80]
        let expected = base64::engine::general_purpose::STANDARD.encode([0xFF, 0xFF, 0x00, 0x80]);
        assert_eq!(encoded, expected);
    }

    #[test]
    fn encode_pcm_samples_empty_input() {
        let samples: Vec<i16> = vec![];
        let encoded = encode_pcm_samples(&samples);
        assert_eq!(encoded, "");
    }

    #[test]
    fn session_update_serializes_correctly() {
        let update = SessionUpdate {
            r#type: "session.update",
            session: SessionConfig {
                audio_format: AudioFormat {
                    encoding: "pcm_s16le",
                    sample_rate: 16000,
                },
            },
        };
        let json = serde_json::to_string(&update).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "session.update");
        assert_eq!(parsed["session"]["audio_format"]["encoding"], "pcm_s16le");
        assert_eq!(parsed["session"]["audio_format"]["sample_rate"], 16000);
    }

    #[test]
    fn audio_append_serializes_correctly() {
        let msg = AudioAppend {
            r#type: "input_audio.append",
            audio: "AQID",
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "input_audio.append");
        assert_eq!(parsed["audio"], "AQID");
    }

    #[test]
    fn server_message_deserializes_text_delta() {
        let json = r#"{"type":"transcription.text.delta","text":"hello "}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.r#type, "transcription.text.delta");
        assert_eq!(msg.text.unwrap(), "hello ");
    }

    #[test]
    fn server_message_deserializes_session_created() {
        let json = r#"{"type":"session.created","session":{"id":"abc123"}}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.r#type, "session.created");
        assert!(msg.text.is_none());
    }

    #[test]
    fn server_message_deserializes_transcription_done() {
        let json = r#"{"type":"transcription.done"}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.r#type, "transcription.done");
    }

    #[test]
    fn server_message_deserializes_error() {
        let json = r#"{"type":"error","text":"invalid audio format"}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.r#type, "error");
        assert_eq!(msg.text.unwrap(), "invalid audio format");
    }
}
