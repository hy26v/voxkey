// ABOUTME: Runs downloaded streaming speech models against live microphone PCM.
// ABOUTME: Publishes revisable live text, then records and injects the final hypothesis.

use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};

use crate::dbus::{DaemonInterface, SharedState};
use crate::desktop::DesktopInput;
use crate::injector::{self, InjectionError};
use crate::state::Event;
use crate::transcriber::LocalStreamingModel;

type DynError = Box<dyn std::error::Error + Send + Sync>;
const LOCAL_STREAM_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const MAX_LOCAL_TRANSCRIPT_BYTES: usize = 1024 * 1024;
const MAX_LOCAL_SETUP_AUDIO_BYTES: usize = 32 * 1024 * 1024;

pub struct LocalStreamingSession {
    pub model: LocalStreamingModel,
    pub transcriber_config: voxkey_ipc::TranscriberConfig,
    pub sample_rate: u32,
    pub channels: u16,
    pub audio_rx: mpsc::Receiver<Vec<i16>>,
    pub capture_error_rx: tokio::sync::watch::Receiver<Option<String>>,
    pub desktop: Arc<DesktopInput>,
    pub state_tx: mpsc::Sender<Event>,
    pub stop_rx: oneshot::Receiver<()>,
    pub cancel_rx: oneshot::Receiver<()>,
    pub shared: SharedState,
    pub connection: zbus::Connection,
    pub typing_delay: std::time::Duration,
    pub transcript_generation: u64,
}

struct DecodeState {
    recognizer: Arc<sherpa_onnx::OnlineRecognizer>,
    stream: sherpa_onnx::OnlineStream,
}

async fn decode_audio(
    state: Arc<Mutex<DecodeState>>,
    sample_rate: u32,
    samples: Vec<f32>,
    finish: bool,
) -> Result<String, DynError> {
    tokio::task::spawn_blocking(move || {
        let sample_rate = i32::try_from(sample_rate)
            .map_err(|_| std::io::Error::other("audio sample rate is too large"))?;
        let state = state
            .lock()
            .map_err(|_| std::io::Error::other("local streaming recognizer lock was poisoned"))?;
        if !samples.is_empty() {
            state.stream.accept_waveform(sample_rate, &samples);
        }
        if finish {
            state.stream.input_finished();
        }
        while state.recognizer.is_ready(&state.stream) {
            state.recognizer.decode(&state.stream);
        }
        let text = state
            .recognizer
            .get_result(&state.stream)
            .map(|result| result.text)
            .unwrap_or_default();
        if text.len() > MAX_LOCAL_TRANSCRIPT_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Local transcript exceeded the {MAX_LOCAL_TRANSCRIPT_BYTES}-byte limit"),
            ));
        }
        Ok(text)
    })
    .await
    .map_err(|error| std::io::Error::other(format!("local inference task failed: {error}")))?
    .map_err(Into::into)
}

fn normalized_mono(samples: &[i16], channels: u16) -> Result<Vec<f32>, String> {
    let channels = usize::from(channels);
    if channels == 0 {
        return Err("local streaming audio must have at least one channel".to_string());
    }
    if !samples.len().is_multiple_of(channels) {
        return Err(format!(
            "local streaming audio has {} samples, which is not a whole number of {channels}-channel frames",
            samples.len()
        ));
    }

    Ok(samples
        .chunks_exact(channels)
        .map(|frame| {
            let sum = frame.iter().map(|sample| i64::from(*sample)).sum::<i64>();
            (sum as f32 / channels as f32) / i16::MAX as f32
        })
        .collect())
}

fn buffer_setup_audio(buffer: &mut Vec<i16>, samples: Vec<i16>) -> Result<(), String> {
    let max_samples = MAX_LOCAL_SETUP_AUDIO_BYTES / std::mem::size_of::<i16>();
    if buffer.len().saturating_add(samples.len()) > max_samples {
        return Err(format!(
            "Local model setup audio exceeded the {} MiB safety limit",
            MAX_LOCAL_SETUP_AUDIO_BYTES / (1024 * 1024)
        ));
    }
    buffer.extend(samples);
    Ok(())
}

async fn publish_hypothesis(
    raw: &str,
    replacements: &[voxkey_ipc::WordReplacement],
    shared: &SharedState,
    connection: &zbus::Connection,
    generation: u64,
) -> String {
    let corrected = crate::dictionary::process_transcription_output(raw, replacements);
    if shared.update_live_transcript(generation, corrected.clone()) {
        DaemonInterface::notify_live_transcript(connection).await;
    }
    corrected
}

async fn record_final(
    text: String,
    config: &voxkey_ipc::TranscriberConfig,
    outcome: voxkey_ipc::TranscriptOutcome,
    pending_insertion: Option<String>,
    shared: &SharedState,
    connection: &zbus::Connection,
) -> Result<(), DynError> {
    if text.is_empty() {
        return Ok(());
    }
    shared.record_transcript(text.clone(), config, outcome, pending_insertion)?;
    DaemonInterface::notify_transcription_complete(connection, &text).await;
    DaemonInterface::notify_last_transcript(connection).await;
    Ok(())
}

async fn finish_and_inject(
    raw: &str,
    session: &LocalStreamingSession,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<(), DynError> {
    let replacements = session.shared.config().dictionary.replacements.clone();
    let corrected = publish_hypothesis(
        raw,
        &replacements,
        &session.shared,
        &session.connection,
        session.transcript_generation,
    )
    .await;
    if corrected.is_empty() {
        let _ = session.state_tx.send(Event::StreamingDone).await;
        return Ok(());
    }

    let result = injector::inject_text_with_cancel(
        &session.desktop,
        &corrected,
        session.typing_delay,
        cancel,
    )
    .await;
    let pending = result
        .as_ref()
        .err()
        .map(|error| error.failure().remaining_text().to_string());
    let outcome = if matches!(result, Err(InjectionError::Cancelled(_))) {
        voxkey_ipc::TranscriptOutcome::Cancelled
    } else {
        voxkey_ipc::TranscriptOutcome::Completed
    };
    record_final(
        corrected,
        &session.transcriber_config,
        outcome,
        pending,
        &session.shared,
        &session.connection,
    )
    .await?;

    match result {
        Ok(()) => {
            let _ = session.state_tx.send(Event::StreamingDone).await;
            Ok(())
        }
        Err(InjectionError::Portal(error)) => {
            let _ = session.state_tx.send(Event::Error).await;
            Err(format!("Desktop access error while typing local transcription: {error}").into())
        }
        Err(InjectionError::Local(error)) => {
            let _ = session.state_tx.send(Event::StreamingDone).await;
            Err(format!("Could not type local transcription: {error}").into())
        }
        Err(InjectionError::Cancelled(_)) => {
            let _ = session.state_tx.send(Event::StreamingDone).await;
            Ok(())
        }
    }
}

async fn finish_cancelled(raw: &str, session: &LocalStreamingSession) -> Result<(), DynError> {
    let replacements = session.shared.config().dictionary.replacements.clone();
    let corrected = publish_hypothesis(
        raw,
        &replacements,
        &session.shared,
        &session.connection,
        session.transcript_generation,
    )
    .await;
    if !corrected.is_empty() {
        record_final(
            corrected.clone(),
            &session.transcriber_config,
            voxkey_ipc::TranscriptOutcome::Cancelled,
            Some(corrected),
            &session.shared,
            &session.connection,
        )
        .await?;
    }
    let _ = session.state_tx.send(Event::StreamingDone).await;
    Ok(())
}

async fn decode_and_publish(
    decoder: Arc<Mutex<DecodeState>>,
    samples: Vec<i16>,
    session: &LocalStreamingSession,
    last_raw: &mut String,
) -> Result<(), DynError> {
    let samples = normalized_mono(&samples, session.channels)?;
    let hypothesis = decode_audio(decoder, session.sample_rate, samples, false).await?;
    if hypothesis != *last_raw {
        let replacements = session.shared.config().dictionary.replacements.clone();
        publish_hypothesis(
            &hypothesis,
            &replacements,
            &session.shared,
            &session.connection,
            session.transcript_generation,
        )
        .await;
        *last_raw = hypothesis;
    }
    Ok(())
}

/// Run one live dictation with an installed online model.
pub async fn run_local_streaming_session(
    mut session: LocalStreamingSession,
) -> Result<(), DynError> {
    if let Some(error) = session.capture_error_rx.borrow().clone() {
        return Err(format!("Audio capture failed: {error}").into());
    }

    let (cancel_tx, mut cancel_watch) = tokio::sync::watch::channel(false);
    let cancel_rx = std::mem::replace(&mut session.cancel_rx, oneshot::channel().1);
    tokio::spawn(async move {
        let _ = cancel_rx.await;
        cancel_tx.send_replace(true);
    });

    let mut capture_errors_open = true;
    let mut stop_rx = std::mem::replace(&mut session.stop_rx, oneshot::channel().1);
    let mut stop_requested = false;
    let mut audio_open = true;
    let mut setup_audio = Vec::new();
    let recognizer = {
        let recognizer = session.model.recognizer();
        tokio::pin!(recognizer);
        loop {
            tokio::select! {
                biased;

                changed = cancel_watch.changed() => {
                    let _ = changed;
                    return finish_cancelled("", &session).await;
                }

                changed = session.capture_error_rx.changed(), if capture_errors_open => {
                    match changed {
                        Ok(()) => {
                            let capture_error = { session.capture_error_rx.borrow().clone() };
                            if let Some(error) = capture_error {
                                return Err(format!("Audio capture failed: {error}").into());
                            }
                        }
                        Err(_) => capture_errors_open = false,
                    }
                }

                _ = &mut stop_rx, if !stop_requested => {
                    stop_requested = true;
                }

                chunk = session.audio_rx.recv(), if audio_open => {
                    match chunk {
                        Some(samples) => buffer_setup_audio(&mut setup_audio, samples)?,
                        None => audio_open = false,
                    }
                }

                result = &mut recognizer => break result?,
            }
        }
    };
    let stream = recognizer.create_stream();
    if session.model.model_name == "nemotron-3.5-asr-streaming-0.6b" {
        stream.set_option("language", "auto");
    }
    let decoder = Arc::new(Mutex::new(DecodeState { recognizer, stream }));
    if !stop_requested {
        let _ = session.state_tx.send(Event::StreamingReady).await;
    }

    let mut last_raw = String::new();
    if !setup_audio.is_empty() {
        decode_and_publish(decoder.clone(), setup_audio, &session, &mut last_raw).await?;
    }

    while !stop_requested && audio_open {
        tokio::select! {
            biased;

            changed = cancel_watch.changed() => {
                let _ = changed;
                return finish_cancelled(&last_raw, &session).await;
            }

            changed = session.capture_error_rx.changed(), if capture_errors_open => {
                match changed {
                    Ok(()) => {
                        let capture_error = { session.capture_error_rx.borrow().clone() };
                        if let Some(error) = capture_error {
                            if !last_raw.is_empty() {
                                let replacements = session.shared.config().dictionary.replacements.clone();
                                let corrected = publish_hypothesis(
                                    &last_raw,
                                    &replacements,
                                    &session.shared,
                                    &session.connection,
                                    session.transcript_generation,
                                ).await;
                                record_final(
                                    corrected.clone(),
                                    &session.transcriber_config,
                                    voxkey_ipc::TranscriptOutcome::PartialFailure,
                                    Some(corrected),
                                    &session.shared,
                                    &session.connection,
                                ).await?;
                            }
                            return Err(format!("Audio capture failed: {error}").into());
                        }
                    }
                    Err(_) => capture_errors_open = false,
                }
            }

            _ = &mut stop_rx => stop_requested = true,

            chunk = session.audio_rx.recv() => {
                match chunk {
                    Some(samples) => {
                        decode_and_publish(decoder.clone(), samples, &session, &mut last_raw).await?;
                    }
                    None => audio_open = false,
                }
            }
        }
    }

    // Capture is stopped before the main loop sends stop_rx. Drain chunks that
    // were already queued so releasing the shortcut cannot clip the last word.
    let drain_deadline = tokio::time::Instant::now() + LOCAL_STREAM_DRAIN_TIMEOUT;
    while audio_open {
        tokio::select! {
            biased;

            changed = cancel_watch.changed() => {
                let _ = changed;
                return finish_cancelled(&last_raw, &session).await;
            }

            changed = session.capture_error_rx.changed(), if capture_errors_open => {
                match changed {
                    Ok(()) => {
                        let capture_error = { session.capture_error_rx.borrow().clone() };
                        if let Some(error) = capture_error {
                            if !last_raw.is_empty() {
                                let replacements = session.shared.config().dictionary.replacements.clone();
                                let corrected = publish_hypothesis(
                                    &last_raw,
                                    &replacements,
                                    &session.shared,
                                    &session.connection,
                                    session.transcript_generation,
                                ).await;
                                record_final(
                                    corrected.clone(),
                                    &session.transcriber_config,
                                    voxkey_ipc::TranscriptOutcome::PartialFailure,
                                    Some(corrected),
                                    &session.shared,
                                    &session.connection,
                                ).await?;
                            }
                            return Err(format!("Audio capture failed: {error}").into());
                        }
                    }
                    Err(_) => capture_errors_open = false,
                }
            }

            _ = tokio::time::sleep_until(drain_deadline) => {
                return Err("Local streaming audio did not finish draining".into());
            }

            chunk = session.audio_rx.recv() => {
                match chunk {
                    Some(samples) => {
                        decode_and_publish(decoder.clone(), samples, &session, &mut last_raw).await?;
                    }
                    None => audio_open = false,
                }
            }
        }
    }

    let final_raw = decode_audio(decoder, session.sample_rate, Vec::new(), true).await?;
    finish_and_inject(&final_raw, &session, cancel_watch).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_audio_is_downmixed_and_normalized() {
        let mono = normalized_mono(&[i16::MAX, i16::MIN + 1, 16_384, 16_384], 2).unwrap();
        assert_eq!(mono.len(), 2);
        assert!(mono[0].abs() < 0.0001);
        assert!((mono[1] - 0.5).abs() < 0.001);
    }

    #[test]
    fn malformed_interleaved_audio_is_rejected() {
        assert!(normalized_mono(&[1, 2, 3], 2).is_err());
        assert!(normalized_mono(&[], 0).is_err());
    }

    #[test]
    fn model_setup_audio_buffer_is_bounded() {
        let mut buffer = vec![0_i16; MAX_LOCAL_SETUP_AUDIO_BYTES / 2];
        assert!(buffer_setup_audio(&mut buffer, vec![1]).is_err());
        let mut small = Vec::new();
        buffer_setup_audio(&mut small, vec![1, 2, 3]).unwrap();
        assert_eq!(small, [1, 2, 3]);
    }
}
