// ABOUTME: Manages real-time streaming transcription via Mistral's WebSocket API.
// ABOUTME: Sends PCM audio chunks and injects text deltas as they arrive.

use std::sync::Arc;

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite;

use crate::dbus::{DaemonInterface, SharedState};
use crate::desktop::DesktopInput;
use crate::injector;
use crate::state::Event;
use voxkey_ipc::MistralRealtimeConfig;

type DynError = Box<dyn std::error::Error + Send + Sync>;
const REALTIME_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const REALTIME_SETUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const REALTIME_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const REALTIME_AUDIO_BATCH_DURATION: std::time::Duration = std::time::Duration::from_millis(100);
const REALTIME_PREVIEW_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
const MAX_REALTIME_TRANSCRIPT_BYTES: usize = 1024 * 1024;
const MAX_REALTIME_ERROR_BYTES: usize = 4 * 1024;

pub struct StreamingSession {
    pub config: MistralRealtimeConfig,
    pub sample_rate: u32,
    pub channels: u16,
    pub audio_rx: mpsc::Receiver<Arc<[i16]>>,
    pub capture_error_rx: tokio::sync::watch::Receiver<Option<String>>,
    pub desktop: Arc<DesktopInput>,
    pub state_tx: mpsc::Sender<Event>,
    pub stop_rx: oneshot::Receiver<()>,
    /// Cancels uncommitted provider output without flushing or recording it.
    /// An EIS key batch already in progress finishes first so every key is
    /// released and synchronized before the session reports cancellation.
    pub cancel_rx: oneshot::Receiver<()>,
    pub shared: SharedState,
    pub connection: zbus::Connection,
    pub typing_delay: std::time::Duration,
    pub transcript_generation: u64,
}

struct LiveTranscriptTarget<'a> {
    shared: &'a SharedState,
    connection: &'a zbus::Connection,
    transcriber_config: &'a voxkey_ipc::TranscriberConfig,
    generation: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum SetupMessage {
    Created,
    Failed(String),
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioEndEvent {
    StopRequested,
    ChannelClosed,
}

fn should_send_audio_end(event: AudioEndEvent) -> bool {
    matches!(event, AudioEndEvent::ChannelClosed)
}

fn require_drain_before_completion(draining: bool) -> Result<(), &'static str> {
    draining
        .then_some(())
        .ok_or("Realtime provider completed before dictation was stopped")
}

fn append_transcript_delta(
    accumulated: &mut String,
    pending: &mut String,
    delta: &str,
) -> std::io::Result<()> {
    let exceeds_limit = |current: usize| {
        current
            .checked_add(delta.len())
            .is_none_or(|length| length > MAX_REALTIME_TRANSCRIPT_BYTES)
    };
    if exceeds_limit(accumulated.len()) || exceeds_limit(pending.len()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Realtime transcript exceeded the {MAX_REALTIME_TRANSCRIPT_BYTES}-byte limit"),
        ));
    }
    accumulated.push_str(delta);
    pending.push_str(delta);
    Ok(())
}

fn realtime_authorization_value(api_key: &str) -> std::io::Result<String> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Add a Mistral Realtime API key in Voxkey settings before dictating",
        ));
    }
    Ok(format!("Bearer {api_key}"))
}

fn streaming_api_error(text: Option<&str>, raw: &str) -> String {
    let source = text.unwrap_or(raw);
    let mut safe = String::with_capacity(source.len().min(MAX_REALTIME_ERROR_BYTES));
    let mut truncated = false;
    for character in source.chars() {
        let character = if character.is_control() {
            '\u{FFFD}'
        } else {
            character
        };
        if safe.len() + character.len_utf8() > MAX_REALTIME_ERROR_BYTES {
            truncated = true;
            break;
        }
        safe.push(character);
    }
    if truncated {
        safe.push_str("… [truncated]");
    }
    format!("Streaming API error: {safe}")
}

fn classify_setup_message(message: &ServerMessage, raw: &str) -> SetupMessage {
    match message.r#type.as_str() {
        "session.created" => SetupMessage::Created,
        "error" => SetupMessage::Failed(streaming_api_error(message.text.as_deref(), raw)),
        _ => SetupMessage::Ignore,
    }
}

async fn wait_for_session_created<S, E>(
    source: &mut S,
    deadline: std::time::Duration,
) -> Result<(), DynError>
where
    S: futures_util::Stream<Item = Result<tungstenite::Message, E>> + Unpin,
    E: std::error::Error + Send + Sync + 'static,
{
    match tokio::time::timeout(deadline, async {
        loop {
            match source.next().await {
                Some(Ok(tungstenite::Message::Text(text))) => {
                    let message: ServerMessage = serde_json::from_str(&text)?;
                    match classify_setup_message(&message, &text) {
                        SetupMessage::Created => return Ok(()),
                        SetupMessage::Failed(error) => return Err(error.into()),
                        SetupMessage::Ignore => {}
                    }
                }
                Some(Ok(tungstenite::Message::Close(_))) => {
                    return Err("Connection closed before the realtime session started".into());
                }
                Some(Ok(_)) => continue,
                Some(Err(error)) => return Err(Box::new(error) as DynError),
                None => return Err("Connection closed before the realtime session started".into()),
            }
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(format!(
            "Realtime session setup timed out after {:.1}s",
            deadline.as_secs_f32()
        )
        .into()),
    }
}

async fn connect_with_timeout<F, T, E>(
    connection: F,
    deadline: std::time::Duration,
) -> Result<T, DynError>
where
    F: std::future::Future<Output = Result<T, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    match tokio::time::timeout(deadline, connection).await {
        Ok(result) => result.map_err(|error| Box::new(error) as DynError),
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "Realtime connection timed out after {:.1}s",
                deadline.as_secs_f64()
            ),
        )
        .into()),
    }
}

async fn wait_until_cancelled<F, T>(
    cancel: &mut tokio::sync::watch::Receiver<bool>,
    operation: F,
) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    if *cancel.borrow() {
        return None;
    }
    tokio::select! {
        biased;
        changed = cancel.changed() => {
            let _ = changed;
            None
        }
        result = operation => Some(result),
    }
}

async fn send_with_deadline<S>(
    sink: &mut S,
    message: tungstenite::Message,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<bool, DynError>
where
    S: futures_util::Sink<tungstenite::Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    if *cancel.borrow() {
        return Ok(false);
    }
    tokio::select! {
        biased;
        changed = cancel.changed() => {
            let _ = changed;
            Ok(false)
        }
        result = crate::deadline::run(
            "Realtime WebSocket send",
            crate::deadline::WEBSOCKET_SEND,
            sink.send(message),
        ) => result.map(|()| true),
    }
}

async fn send_audio_append<S>(
    sink: &mut S,
    samples: &[i16],
    channels: u16,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<bool, DynError>
where
    S: futures_util::Sink<tungstenite::Message> + Unpin,
    S::Error: std::error::Error + Send + Sync + 'static,
{
    let encoded = encode_pcm_samples(samples, channels)?;
    let message = AudioAppend {
        r#type: "input_audio.append",
        audio: &encoded,
    };
    let json = serde_json::to_string(&message)?;
    send_with_deadline(sink, tungstenite::Message::Text(json.into()), cancel).await
}

async fn wait_for_drain_deadline(
    deadline: Option<tokio::time::Instant>,
    limit: std::time::Duration,
) -> Result<(), DynError> {
    let Some(deadline) = deadline else {
        return std::future::pending().await;
    };
    tokio::time::sleep_until(deadline).await;
    Err(format!(
        "Realtime transcription drain timed out after {:.1}s",
        limit.as_secs_f32()
    )
    .into())
}

async fn wait_for_preview_deadline(deadline: Option<tokio::time::Instant>) {
    let Some(deadline) = deadline else {
        return std::future::pending().await;
    };
    tokio::time::sleep_until(deadline).await;
}

pub(crate) fn streaming_url(
    base_url: &str,
    model: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let base_url = base_url.trim();
    let base_url = if base_url.is_empty() {
        MistralRealtimeConfig::DEFAULT_ENDPOINT
    } else {
        base_url
    };
    let mut url = reqwest::Url::parse(base_url)?;
    if !matches!(url.scheme(), "ws" | "wss") {
        return Err("Realtime server address must use ws:// or wss://".into());
    }
    // The API key travels as a bearer header on this connection, so a remote
    // plaintext endpoint would leak it to the network. Only loopback servers
    // may use plain ws:// — that is how the local mock and self-hosted
    // gateways are reached.
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let is_loopback_host = matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]");
    if url.scheme() == "ws" && !is_loopback_host {
        return Err(
            "Realtime server address must use wss:// unless it points at this machine \
             (ws:// is only allowed for localhost/127.0.0.1/[::1])"
                .into(),
        );
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Realtime server address must not contain embedded credentials".into());
    }
    url.set_fragment(None);
    let retained_query: Vec<_> = url
        .query_pairs()
        .filter(|(name, _)| name != "model")
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        for (name, value) in retained_query {
            query.append_pair(&name, &value);
        }
        query.append_pair("model", model);
    }
    Ok(url.into())
}

/// Run a streaming transcription session over WebSocket.
///
/// Connects to the Mistral realtime API, sends PCM audio from `audio_rx`,
/// injects text deltas as they arrive, and signals completion via `state_tx`.
pub async fn run_streaming_session(
    session: StreamingSession,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let StreamingSession {
        config,
        sample_rate,
        channels,
        mut audio_rx,
        mut capture_error_rx,
        desktop,
        state_tx,
        stop_rx,
        cancel_rx,
        shared,
        connection,
        typing_delay,
        transcript_generation,
    } = session;
    let (cancel_tx, mut cancel_watch) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _ = cancel_rx.await;
        cancel_tx.send_replace(true);
    });
    if let Some(error) = capture_error_rx.borrow().clone() {
        return Err(format!("Audio capture failed: {error}").into());
    }
    let replacement_rules = shared.config().dictionary.replacements.clone();
    let base_url = if config.endpoint.is_empty() {
        MistralRealtimeConfig::DEFAULT_ENDPOINT
    } else {
        &config.endpoint
    };
    let url = streaming_url(base_url, &config.model)?;

    // Extract host from wss://host/... for the Host header
    let host = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("api.mistral.ai");
    let authorization = realtime_authorization_value(&config.api_key)?;
    let transcriber_config = voxkey_ipc::TranscriberConfig {
        provider: voxkey_ipc::TranscriberProvider::MistralRealtime,
        mistral_realtime: config.clone(),
        ..Default::default()
    };

    let request = http::Request::builder()
        .uri(&url)
        .header("Authorization", authorization)
        .header("Host", host)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tungstenite::handshake::client::generate_key(),
        )
        .body(())?;

    let Some(connection_result) = wait_until_cancelled(
        &mut cancel_watch,
        connect_with_timeout(
            tokio_tungstenite::connect_async(request),
            REALTIME_CONNECT_TIMEOUT,
        ),
    )
    .await
    else {
        let _ = state_tx.send(Event::StreamingDone).await;
        return Ok(());
    };
    let (ws_stream, _response) = connection_result?;
    let (mut ws_sink, mut ws_source) = ws_stream.split();

    tracing::info!("WebSocket connected to Mistral Realtime API");

    let Some(setup_result) = wait_until_cancelled(
        &mut cancel_watch,
        wait_for_session_created(&mut ws_source, REALTIME_SETUP_TIMEOUT),
    )
    .await
    else {
        let _ = state_tx.send(Event::StreamingDone).await;
        return Ok(());
    };
    setup_result?;
    tracing::info!("Streaming session created");

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
    if !send_with_deadline(
        &mut ws_sink,
        tungstenite::Message::Text(update_json.into()),
        &mut cancel_watch,
    )
    .await?
    {
        let _ = state_tx.send(Event::StreamingDone).await;
        return Ok(());
    }
    // Do not advertise Streaming until the provider has accepted the session
    // configuration. Audio is buffered by the bounded recorder channel while
    // the handshake is in flight.
    let _ = state_tx.send(Event::StreamingReady).await;

    // Main loop
    let mut accumulated_transcript = String::new();
    let mut pending = String::new();
    let mut draining = false;
    let mut drain_deadline = None;
    let mut stop_rx = Some(stop_rx);
    let mut preview_deadline = None;
    let mut capture_errors_open = true;
    let mut audio_batcher =
        crate::audio_batch::PcmBatcher::new(sample_rate, channels, REALTIME_AUDIO_BATCH_DURATION)?;
    let live_transcript = LiveTranscriptTarget {
        shared: &shared,
        connection: &connection,
        transcriber_config: &transcriber_config,
        generation: transcript_generation,
    };

    // Terminal messages record before returning. Any other failure still has
    // to preserve deltas already received from the provider.
    let mut transcript_recorded = false;
    let mut terminal_outcome = voxkey_ipc::TranscriptOutcome::PartialFailure;
    let mut retry_override: Option<String> = None;
    let outcome: Result<(), DynError> = async {
        loop {
            tokio::select! {
            biased;

            changed = cancel_watch.changed() => {
                let _ = changed;
                tracing::info!("Realtime dictation cancelled; discarding uncommitted text");
                record_transcript(
                    &accumulated_transcript,
                    &replacement_rules,
                    &live_transcript,
                    voxkey_ipc::TranscriptOutcome::Cancelled,
                    Some(pending_output(&pending, &replacement_rules)),
                ).await;
                transcript_recorded = true;
                let _ = state_tx.send(Event::StreamingDone).await;
                return Ok(());
            }

            changed = capture_error_rx.changed(), if capture_errors_open => {
                match changed {
                    Ok(()) => {
                        if let Some(error) = capture_error_rx.borrow().clone() {
                            return Err(format!("Audio capture failed: {error}").into());
                        }
                    }
                    Err(_) => capture_errors_open = false,
                }
            }

            // Audio chunk from recorder
            chunk = audio_rx.recv(), if !draining => {
                match chunk {
                    Some(samples) => {
                        for batch in audio_batcher.push(&samples)? {
                            if !send_audio_append(
                                &mut ws_sink,
                                &batch,
                                channels,
                                &mut cancel_watch,
                            )
                            .await
                            .inspect_err(|_| {
                                terminal_outcome =
                                    voxkey_ipc::TranscriptOutcome::PartialTransportClose;
                            })?
                            {
                                terminal_outcome = voxkey_ipc::TranscriptOutcome::Cancelled;
                                let _ = state_tx.send(Event::StreamingDone).await;
                                return Ok(());
                            }
                        }
                    }
                    None => {
                        if let Some(batch) = audio_batcher.flush()
                            && !send_audio_append(
                                &mut ws_sink,
                                &batch,
                                channels,
                                &mut cancel_watch,
                            )
                            .await
                            .inspect_err(|_| {
                                terminal_outcome =
                                    voxkey_ipc::TranscriptOutcome::PartialTransportClose;
                            })?
                        {
                            terminal_outcome = voxkey_ipc::TranscriptOutcome::Cancelled;
                            let _ = state_tx.send(Event::StreamingDone).await;
                            return Ok(());
                        }
                        // Audio channel closed — treat as stop
                        if should_send_audio_end(AudioEndEvent::ChannelClosed) {
                            tracing::info!("Audio channel closed, sending input_audio.end");
                            let end_msg = r#"{"type":"input_audio.end"}"#;
                            if !send_with_deadline(
                                &mut ws_sink,
                                tungstenite::Message::Text(end_msg.into()),
                                &mut cancel_watch,
                            )
                            .await
                            .inspect_err(|_| {
                                terminal_outcome =
                                    voxkey_ipc::TranscriptOutcome::PartialTransportClose;
                            })?
                            {
                                terminal_outcome = voxkey_ipc::TranscriptOutcome::Cancelled;
                                let _ = state_tx.send(Event::StreamingDone).await;
                                return Ok(());
                            }
                            draining = true;
                            drain_deadline = Some(tokio::time::Instant::now() + REALTIME_DRAIN_TIMEOUT);
                        }
                    }
                }
            }

            // Stop signal from main loop (key released)
            result = async { stop_rx.as_mut().unwrap().await }, if stop_rx.is_some() && !draining => {
                let _ = result;
                if should_send_audio_end(AudioEndEvent::StopRequested) {
                    tracing::info!("Stop signal received, sending input_audio.end");
                    let end_msg = r#"{"type":"input_audio.end"}"#;
                    if !send_with_deadline(
                        &mut ws_sink,
                        tungstenite::Message::Text(end_msg.into()),
                        &mut cancel_watch,
                    )
                    .await
                    .inspect_err(|_| {
                        terminal_outcome =
                            voxkey_ipc::TranscriptOutcome::PartialTransportClose;
                    })?
                    {
                        terminal_outcome = voxkey_ipc::TranscriptOutcome::Cancelled;
                        let _ = state_tx.send(Event::StreamingDone).await;
                        return Ok(());
                    }
                    draining = true;
                    drain_deadline = Some(tokio::time::Instant::now() + REALTIME_DRAIN_TIMEOUT);
                } else {
                    tracing::info!("Stop signal received, draining queued audio");
                }
                stop_rx = None;
            }

            result = wait_for_drain_deadline(drain_deadline, REALTIME_DRAIN_TIMEOUT), if drain_deadline.is_some() => {
                return result;
            }

            // WebSocket messages from server
            ws_msg = ws_source.next() => {
                match ws_msg {
                    Some(Ok(tungstenite::Message::Text(text))) => {
                        let msg: ServerMessage = serde_json::from_str(&text)?;
                        match msg.r#type.as_str() {
                            "transcription.text.delta" => {
                                if let Some(delta) = msg.text {
                                    append_transcript_delta(
                                        &mut accumulated_transcript,
                                        &mut pending,
                                        &delta,
                                    )?;
                                    preview_deadline.get_or_insert_with(|| {
                                        tokio::time::Instant::now() + REALTIME_PREVIEW_INTERVAL
                                    });
                                    let (ready, rest) = crate::dictionary::split_ready(&pending, &replacement_rules);
                                    let rest = rest.to_string();
                                    if !ready.is_empty() {
                                        let corrected = crate::dictionary::process_streaming_output(
                                            ready,
                                            &replacement_rules,
                                        );
                                        match delta_injection_result(
                                            injector::inject_text_with_cancel(
                                                &desktop,
                                                &corrected,
                                                typing_delay,
                                                cancel_watch.clone(),
                                            ).await,
                                        ) {
                                            Ok(()) => {}
                                            Err(injector::InjectionError::Portal(e)) => {
                                                let mut retry = e.remaining_text().to_string();
                                                retry.push_str(&pending_output(&rest, &replacement_rules));
                                                retry_override = Some(retry);
                                                let _ = state_tx.send(Event::Error).await;
                                                return Err(format!("Desktop access error while typing live text: {e}").into());
                                            }
                                            Err(injector::InjectionError::Local(e)) => {
                                                let mut retry = e.remaining_text().to_string();
                                                retry.push_str(&pending_output(&rest, &replacement_rules));
                                                retry_override = Some(retry);
                                                return Err(format!("Could not type live text: {e}").into());
                                            }
                                            Err(injector::InjectionError::Cancelled(e)) => {
                                                let mut retry = e.remaining_text().to_string();
                                                retry.push_str(&pending_output(&rest, &replacement_rules));
                                                retry_override = Some(retry);
                                                terminal_outcome =
                                                    voxkey_ipc::TranscriptOutcome::Cancelled;
                                                let _ = state_tx.send(Event::StreamingDone).await;
                                                return Ok(());
                                            }
                                        }
                                        pending = rest;
                                    }
                                }
                            }
                            "transcription.done" => {
                                if let Err(error) = require_drain_before_completion(draining) {
                                    terminal_outcome =
                                        voxkey_ipc::TranscriptOutcome::PartialProviderError;
                                    return Err(error.into());
                                }
                                tracing::info!("Streaming transcription complete ({} chars)", accumulated_transcript.len());
                                let injection_result = flush_pending(
                                    &mut pending,
                                    &replacement_rules,
                                    &desktop,
                                    typing_delay,
                                    cancel_watch.clone(),
                                )
                                .await;
                                let pending_insertion = injection_result
                                    .as_ref()
                                    .err()
                                    .map(|error| error.failure().remaining_text().to_string());
                                record_transcript(
                                    &accumulated_transcript,
                                    &replacement_rules,
                                    &live_transcript,
                                    voxkey_ipc::TranscriptOutcome::Completed,
                                    pending_insertion,
                                )
                                .await;
                                transcript_recorded = true;
                                return signal_final_injection_result(injection_result, &state_tx)
                                    .await;
                            }
                            "error" => {
                                terminal_outcome =
                                    voxkey_ipc::TranscriptOutcome::PartialProviderError;
                                let error = streaming_api_error(msg.text.as_deref(), &text);
                                tracing::error!("{error}");
                                return Err(error.into());
                            }
                            other => {
                                tracing::debug!("Ignoring WebSocket message type: {other}");
                            }
                        }
                    }
                    Some(Ok(tungstenite::Message::Close(_))) => {
                        terminal_outcome =
                            voxkey_ipc::TranscriptOutcome::PartialTransportClose;
                        return Err("Connection closed before transcription finished".into());
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => {
                        terminal_outcome =
                            voxkey_ipc::TranscriptOutcome::PartialTransportClose;
                        tracing::error!("WebSocket error: {e}");
                        return Err(e.into());
                    }
                    None => {
                        terminal_outcome =
                            voxkey_ipc::TranscriptOutcome::PartialTransportClose;
                        return Err("Connection ended before transcription finished".into());
                    }
                }
            }

            _ = wait_for_preview_deadline(preview_deadline), if preview_deadline.is_some() => {
                publish_live_preview(
                    &accumulated_transcript,
                    &replacement_rules,
                    &live_transcript,
                )
                .await;
                preview_deadline = None;
            }
            }
        }
    }
    .await;

    if !transcript_recorded && (!accumulated_transcript.is_empty() || outcome.is_err()) {
        let pending_insertion =
            retry_override.or_else(|| Some(pending_output(&pending, &replacement_rules)));
        record_transcript(
            &accumulated_transcript,
            &replacement_rules,
            &live_transcript,
            terminal_outcome,
            pending_insertion,
        )
        .await;
    }
    outcome
}

/// Inject the held-back partial word after the provider's explicit completion.
async fn flush_pending(
    pending: &mut String,
    replacement_rules: &[voxkey_ipc::WordReplacement],
    desktop: &DesktopInput,
    typing_delay: std::time::Duration,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<(), injector::InjectionError> {
    if !pending.is_empty() {
        let corrected = crate::dictionary::process_streaming_output(pending, replacement_rules);
        let result = final_injection_result(
            injector::inject_text_with_cancel(desktop, &corrected, typing_delay, cancel).await,
        );
        pending.clear();
        result
    } else {
        Ok(())
    }
}

fn pending_output(pending: &str, replacement_rules: &[voxkey_ipc::WordReplacement]) -> String {
    crate::dictionary::process_streaming_output(pending, replacement_rules)
}

async fn record_transcript(
    accumulated_transcript: &str,
    replacement_rules: &[voxkey_ipc::WordReplacement],
    live_transcript: &LiveTranscriptTarget<'_>,
    outcome: voxkey_ipc::TranscriptOutcome,
    pending_insertion: Option<String>,
) {
    let corrected_transcript =
        crate::dictionary::process_transcription_output(accumulated_transcript, replacement_rules);
    if live_transcript
        .shared
        .update_live_transcript(live_transcript.generation, corrected_transcript.clone())
    {
        DaemonInterface::notify_live_transcript(live_transcript.connection).await;
    }
    if !corrected_transcript.is_empty() {
        let saved = live_transcript.shared.record_transcript(
            corrected_transcript.clone(),
            live_transcript.transcriber_config,
            outcome,
            pending_insertion,
        );
        if let Err(error) = saved {
            let message = format!("Failed to save transcription history: {error}");
            tracing::error!("{message}");
            live_transcript.shared.set_last_error(message);
            DaemonInterface::notify_last_error(live_transcript.connection).await;
        }
        DaemonInterface::notify_transcription_complete(
            live_transcript.connection,
            &corrected_transcript,
        )
        .await;
    }
}

fn final_injection_result(
    result: Result<(), injector::InjectionError>,
) -> Result<(), injector::InjectionError> {
    match result {
        Ok(()) => Ok(()),
        Err(injector::InjectionError::Portal(error)) => {
            tracing::error!("Failed to inject final streaming text: {error}");
            Err(injector::InjectionError::Portal(error))
        }
        Err(injector::InjectionError::Local(error)) => {
            tracing::error!("Failed to inject final streaming text: {error}");
            Err(injector::InjectionError::Local(error))
        }
        Err(injector::InjectionError::Cancelled(error)) => {
            Err(injector::InjectionError::Cancelled(error))
        }
    }
}

fn delta_injection_result(
    result: Result<(), injector::InjectionError>,
) -> Result<(), injector::InjectionError> {
    if let Err(injector::InjectionError::Local(error)) = &result {
        tracing::error!("Failed to inject streaming text delta: {error}");
    }
    result
}

async fn signal_final_injection_result(
    result: Result<(), injector::InjectionError>,
    state_tx: &mpsc::Sender<Event>,
) -> Result<(), DynError> {
    match result {
        Ok(()) => {
            let _ = state_tx.send(Event::StreamingDone).await;
            Ok(())
        }
        Err(injector::InjectionError::Portal(error)) => {
            let _ = state_tx.send(Event::Error).await;
            Err(format!("Desktop access error while typing the final text: {error}").into())
        }
        Err(injector::InjectionError::Local(error)) => {
            let _ = state_tx.send(Event::StreamingDone).await;
            Err(format!("Could not type the final text: {error}").into())
        }
        Err(injector::InjectionError::Cancelled(_)) => {
            let _ = state_tx.send(Event::StreamingDone).await;
            Ok(())
        }
    }
}

async fn publish_live_preview(
    accumulated_transcript: &str,
    replacement_rules: &[voxkey_ipc::WordReplacement],
    live_transcript: &LiveTranscriptTarget<'_>,
) {
    let corrected =
        crate::dictionary::process_transcription_output(accumulated_transcript, replacement_rules);
    if live_transcript
        .shared
        .update_live_transcript(live_transcript.generation, corrected)
    {
        DaemonInterface::notify_live_transcript(live_transcript.connection).await;
    }
}

/// Downmix interleaved capture frames to the mono PCM expected by the realtime
/// API, then encode the little-endian bytes as base64.
fn encode_pcm_samples(samples: &[i16], channels: u16) -> Result<String, String> {
    let channels = usize::from(channels);
    if channels == 0 {
        return Err("realtime audio must have at least one channel".to_string());
    }
    if !samples.len().is_multiple_of(channels) {
        return Err(format!(
            "realtime audio has {} samples, which is not a whole number of \
             {channels}-channel frames",
            samples.len()
        ));
    }

    let mut bytes = Vec::with_capacity(samples.len() / channels * 2);
    for frame in samples.chunks_exact(channels) {
        let mono = frame.iter().map(|sample| i64::from(*sample)).sum::<i64>() / channels as i64;
        bytes.extend_from_slice(&(mono as i16).to_le_bytes());
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
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
        let encoded = encode_pcm_samples(&samples, 1).unwrap();
        // 256 in LE = [0x00, 0x01], 32767 in LE = [0xFF, 0x7F]
        let expected = base64::engine::general_purpose::STANDARD.encode([0x00, 0x01, 0xFF, 0x7F]);
        assert_eq!(encoded, expected);
    }

    #[test]
    fn encode_pcm_samples_handles_negative_values() {
        let samples: Vec<i16> = vec![-1, -32768];
        let encoded = encode_pcm_samples(&samples, 1).unwrap();
        // -1 in LE = [0xFF, 0xFF], -32768 in LE = [0x00, 0x80]
        let expected = base64::engine::general_purpose::STANDARD.encode([0xFF, 0xFF, 0x00, 0x80]);
        assert_eq!(encoded, expected);
    }

    #[test]
    fn encode_pcm_samples_empty_input() {
        let samples: Vec<i16> = vec![];
        let encoded = encode_pcm_samples(&samples, 1).unwrap();
        assert_eq!(encoded, "");
    }

    #[test]
    fn realtime_pcm_is_mono_even_when_capture_is_stereo() {
        let encoded = encode_pcm_samples(&[12_000, -12_000], 2).unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();

        assert_eq!(decoded, 0_i16.to_le_bytes());
    }

    #[test]
    fn realtime_callbacks_are_coalesced_into_tenth_second_messages() {
        let mut batcher =
            crate::audio_batch::PcmBatcher::new(16_000, 1, REALTIME_AUDIO_BATCH_DURATION).unwrap();
        let mut messages = Vec::new();
        for _ in 0..20 {
            messages.extend(batcher.push(&[1_i16; 80]).unwrap());
        }

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].len(), 1_600);
        assert_eq!(batcher.flush(), None);
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
    fn realtime_transcript_rejects_a_delta_beyond_its_memory_limit() {
        let mut accumulated = "x".repeat(MAX_REALTIME_TRANSCRIPT_BYTES - 1);
        let mut pending = String::new();

        assert!(append_transcript_delta(&mut accumulated, &mut pending, "yz").is_err());
        assert_eq!(accumulated.len(), MAX_REALTIME_TRANSCRIPT_BYTES - 1);
        assert!(pending.is_empty());
    }

    #[test]
    fn realtime_authorization_rejects_a_blank_api_key_before_connecting() {
        for api_key in ["", "  \t\n"] {
            let error = realtime_authorization_value(api_key)
                .expect_err("blank credentials must not reach the WebSocket request");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("Add a Mistral Realtime API key"));
        }
        assert_eq!(
            realtime_authorization_value("sk-rt").unwrap(),
            "Bearer sk-rt"
        );
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
    fn provider_completion_is_only_valid_after_user_stop() {
        assert!(require_drain_before_completion(true).is_ok());
        assert_eq!(
            require_drain_before_completion(false),
            Err("Realtime provider completed before dictation was stopped")
        );
    }

    #[test]
    fn server_message_deserializes_error() {
        let json = r#"{"type":"error","text":"invalid audio format"}"#;
        let msg: ServerMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.r#type, "error");
        assert_eq!(msg.text.unwrap(), "invalid audio format");
    }

    #[test]
    fn stop_waits_for_queued_audio_before_ending_the_stream() {
        assert!(!should_send_audio_end(AudioEndEvent::StopRequested));
        assert!(should_send_audio_end(AudioEndEvent::ChannelClosed));
    }

    #[tokio::test]
    async fn final_portal_injection_failure_is_not_reported_as_success() {
        let result = final_injection_result(Err(injector::InjectionError::Portal(
            injector::InjectionFailure::new(
                std::io::Error::other("portal session closed").into(),
                "remaining".to_string(),
            ),
        )));

        assert!(matches!(&result, Err(injector::InjectionError::Portal(_))));
        let (state_tx, mut state_rx) = mpsc::channel(1);
        let outcome = signal_final_injection_result(result, &state_tx).await;

        assert!(outcome.is_err());
        assert!(matches!(state_rx.recv().await, Some(Event::Error)));

        let (success_tx, mut success_rx) = mpsc::channel(1);
        assert!(
            signal_final_injection_result(Ok(()), &success_tx)
                .await
                .is_ok()
        );
        assert!(matches!(
            success_rx.recv().await,
            Some(Event::StreamingDone)
        ));
    }

    #[test]
    fn local_delta_injection_failure_is_not_discarded_as_success() {
        let result = delta_injection_result(Err(injector::InjectionError::Local(
            injector::InjectionFailure::new(
                std::io::Error::other("unsupported character").into(),
                "remaining".to_string(),
            ),
        )));

        assert!(matches!(result, Err(injector::InjectionError::Local(_))));
    }

    #[test]
    fn setup_errors_fail_instead_of_waiting_for_session_created() {
        let raw = r#"{"type":"error","text":"invalid API key"}"#;
        let message: ServerMessage = serde_json::from_str(raw).unwrap();

        assert_eq!(
            classify_setup_message(&message, raw),
            SetupMessage::Failed("Streaming API error: invalid API key".to_string())
        );
    }

    #[test]
    fn realtime_provider_errors_are_bounded_and_control_safe() {
        let provider_text = format!("{}\nforged\r\x1b[31m", "x".repeat(8 * 1024));
        let message = streaming_api_error(Some(&provider_text), "unused raw message");

        assert!(
            message.len() <= MAX_REALTIME_ERROR_BYTES + 64,
            "message retained {} bytes",
            message.len()
        );
        assert!(!message.chars().any(char::is_control), "{message:?}");
    }

    #[tokio::test]
    async fn silent_realtime_server_cannot_stall_session_setup() {
        let mut silent =
            futures_util::stream::pending::<Result<tungstenite::Message, std::io::Error>>();

        let outer = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            wait_for_session_created(&mut silent, std::time::Duration::from_millis(10)),
        )
        .await;

        assert!(
            matches!(outer, Ok(Err(ref error)) if error.to_string().contains("timed out")),
            "setup did not enforce its own deadline: {outer:?}"
        );
    }

    #[tokio::test]
    async fn a_close_frame_ends_realtime_setup_immediately() {
        let close = futures_util::stream::iter([Ok::<_, std::io::Error>(
            tungstenite::Message::Close(None),
        )]);
        let silent =
            futures_util::stream::pending::<Result<tungstenite::Message, std::io::Error>>();
        let mut source = close.chain(silent);

        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            wait_for_session_created(&mut source, std::time::Duration::from_secs(1)),
        )
        .await
        .expect("a close frame was ignored until the setup deadline")
        .expect_err("a closed WebSocket cannot create a realtime session");

        assert!(outcome.to_string().contains("closed"), "{outcome}");
    }

    #[tokio::test]
    async fn unreachable_realtime_endpoint_cannot_stall_connection() {
        let never = std::future::pending::<Result<(), std::io::Error>>();
        let outer = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            connect_with_timeout(never, std::time::Duration::from_millis(10)),
        )
        .await;

        assert!(
            matches!(outer, Ok(Err(ref error)) if error.to_string().contains("timed out")),
            "connection did not enforce its own deadline: {outer:?}"
        );
    }

    #[tokio::test]
    async fn cancellation_preempts_realtime_connection_and_setup_awaits() {
        let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            cancel_tx.send_replace(true);
        });

        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            wait_until_cancelled(&mut cancel_rx, std::future::pending::<()>()),
        )
        .await
        .expect("cancellation did not preempt the pending socket operation");

        assert!(outcome.is_none());
    }

    #[tokio::test]
    async fn cancellation_preempts_a_backpressured_websocket_send() {
        let mut sink = Box::pin(futures_util::sink::unfold(
            (),
            |(), _message: tungstenite::Message| async move {
                std::future::pending::<Result<(), std::io::Error>>().await?;
                Ok::<(), std::io::Error>(())
            },
        ));
        let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            cancel_tx.send_replace(true);
        });

        let sent = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            send_with_deadline(
                &mut sink,
                tungstenite::Message::Text("audio".into()),
                &mut cancel_rx,
            ),
        )
        .await
        .expect("cancellation did not preempt the WebSocket send")
        .unwrap();

        assert!(!sent);
    }

    #[test]
    fn streaming_url_preserves_options_and_encodes_the_model() {
        let url = streaming_url(
            "wss://realtime.example.test/v1?language=en",
            "model one & two",
        )
        .unwrap();
        let parsed = reqwest::Url::parse(&url).unwrap();
        let query: Vec<_> = parsed.query_pairs().into_owned().collect();

        assert_eq!(
            query,
            [
                ("language".to_string(), "en".to_string()),
                ("model".to_string(), "model one & two".to_string())
            ]
        );
    }

    #[test]
    fn streaming_url_replaces_a_stale_model_parameter() {
        let url = streaming_url(
            "wss://realtime.example.test/v1?model=old-model&language=en",
            "new-model",
        )
        .unwrap();
        let parsed = reqwest::Url::parse(&url).unwrap();
        let query: Vec<_> = parsed.query_pairs().into_owned().collect();

        assert_eq!(
            query,
            [
                ("language".to_string(), "en".to_string()),
                ("model".to_string(), "new-model".to_string())
            ]
        );
    }

    #[test]
    fn streaming_url_does_not_send_endpoint_fragments() {
        let url = streaming_url(
            "wss://realtime.example.test/v1?language=en#local-note",
            "realtime-model",
        )
        .unwrap();
        let parsed = reqwest::Url::parse(&url).unwrap();

        assert_eq!(parsed.fragment(), None);
    }

    #[test]
    fn streaming_url_rejects_embedded_credentials() {
        let error = streaming_url(
            "wss://alice:secret@realtime.example.test/v1",
            "realtime-model",
        )
        .expect_err("credentials must not be accepted in a realtime endpoint");

        assert!(error.to_string().contains("credentials"), "{error}");
    }

    #[test]
    fn streaming_url_rejects_non_websocket_schemes() {
        for endpoint in [
            "https://realtime.example.test/v1",
            "file:///tmp/realtime.sock",
        ] {
            let error = streaming_url(endpoint, "realtime-model")
                .expect_err("a realtime endpoint must use a WebSocket scheme");
            assert!(error.to_string().contains("ws:// or wss://"), "{error}");
        }
    }

    #[test]
    fn streaming_url_rejects_plaintext_websocket_to_remote_hosts() {
        for endpoint in [
            "ws://realtime.example.test/v1",
            "ws://10.0.0.7:9000/v1",
            "ws://192.168.1.20/v1",
        ] {
            let error = streaming_url(endpoint, "realtime-model")
                .expect_err("a plaintext WebSocket to a remote host would leak the bearer key");
            assert!(error.to_string().contains("wss://"), "{error}");
        }
    }

    #[test]
    fn streaming_url_allows_plaintext_websocket_only_for_loopback() {
        for endpoint in [
            "ws://127.0.0.1:8907/v1/audio/transcriptions/realtime",
            "ws://localhost:8907/v1",
            "ws://[::1]:8907/v1",
        ] {
            let url = streaming_url(endpoint, "realtime-model")
                .unwrap_or_else(|error| panic!("{endpoint} is loopback and must pass: {error}"));
            assert_eq!(reqwest::Url::parse(&url).unwrap().scheme(), "ws");
        }
    }

    #[test]
    fn whitespace_only_realtime_endpoint_uses_the_default() {
        let url = streaming_url("  \t ", "realtime-model").unwrap();
        let parsed = reqwest::Url::parse(&url).unwrap();

        assert_eq!(parsed.scheme(), "wss");
        assert_eq!(parsed.host_str(), Some("api.mistral.ai"));
        assert_eq!(parsed.path(), "/v1/audio/transcriptions/realtime");
    }

    #[tokio::test]
    async fn realtime_drain_deadline_errors_instead_of_hanging() {
        let limit = std::time::Duration::from_millis(10);
        let deadline = tokio::time::Instant::now() + limit;
        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            wait_for_drain_deadline(Some(deadline), limit),
        )
        .await
        .expect("the drain deadline itself did not resolve")
        .expect_err("an expired drain must be an error");

        assert!(outcome.to_string().contains("timed out"), "{outcome}");
    }
}
