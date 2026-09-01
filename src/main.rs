// ABOUTME: Entry point for the voxkey Wayland dictation daemon.
// ABOUTME: Wires portal sessions, audio recording, transcription, and text injection into an event loop.

mod agreement;
mod audio_signal;
mod config;
mod dbus;
mod deadline;
mod desktop;
mod dictionary;
mod eis;
mod endpoint_check;
mod history;
mod injector;
mod local_streaming;
mod model_download;
mod models;
mod notifications;
mod persistence;
mod portal;
mod preview;
mod recorder;
mod registry;
mod screen_lock;
mod secret_store;
mod segmentation;
mod shortcuts;
mod state;
mod streaming;
mod transcriber;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use config::Config;
use dbus::{DaemonInterface, DictationAction, DictationRequest, SharedState};
use desktop::DesktopInput;
use injector::Injector;
use recorder::Recorder;
use shortcuts::{ShortcutController, ShortcutEvent};
use state::{Event, State};
use transcriber::Transcriber;

type DynError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let settings_managed = std::env::args_os().any(|argument| argument == "--settings-managed");
    if let Err(e) = run(settings_managed).await {
        tracing::error!("Fatal: {e}");
        std::process::exit(1);
    }
}

async fn run(settings_managed: bool) -> Result<(), DynError> {
    let mut config = Config::load()?;
    tracing::info!("Configuration loaded");

    transcriber::remove_legacy_hotwords_file();

    migrate_plaintext_api_keys(&mut config).await;

    let shared = SharedState::new(config.clone());

    // Convert process signals into the same graceful-shutdown path used by
    // D-Bus Quit. Do not cancel `run()` from an outer select: cancellation
    // would skip explicit portal-session cleanup.
    let signal_shared = shared.clone();
    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to register SIGTERM handler");
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("Received SIGTERM, requesting graceful shutdown"),
            _ = tokio::signal::ctrl_c() => tracing::info!("Received SIGINT, requesting graceful shutdown"),
        }
        signal_shared.request_shutdown();
    });

    // Register app_id with the portal and get the shared connection
    let connection = registry::connect_and_register().await?;

    // Serve the D-Bus interface for the settings GUI
    let (control_tx, mut control_rx) = mpsc::channel::<DictationRequest>(16);
    connection
        .object_server()
        .at(
            voxkey_ipc::OBJECT_PATH,
            DaemonInterface::new(shared.clone(), control_tx),
        )
        .await?;
    connection.request_name(voxkey_ipc::BUS_NAME).await?;
    tracing::info!("D-Bus interface registered at {}", voxkey_ipc::BUS_NAME);

    // The settings UI starts this service. Link to it as early as possible so
    // even a SIGKILL during portal setup cannot leave an orphaned daemon. A
    // manually started daemon remains valid when no settings application owns
    // the corresponding D-Bus name.
    match dbus::attach_settings_lifecycle(&connection, shared.clone()).await {
        Ok(true) => tracing::info!("Daemon lifetime linked to settings application at startup"),
        Ok(false) if settings_managed => {
            tracing::info!("Settings application exited before daemon startup completed");
            return Ok(());
        }
        Ok(false) => {}
        Err(error) if settings_managed => {
            return Err(std::io::Error::other(format!(
                "Could not enforce settings-managed lifecycle: {error}"
            ))
            .into());
        }
        Err(error) => tracing::warn!("Could not attach settings lifecycle at startup: {error}"),
    }

    // Capability checks (using the same connection)
    portal::check_capabilities(connection.clone())
        .await
        .map_err(|e| -> DynError {
            tracing::error!("Portal capability check failed: {e}");
            e.into()
        })?;
    tracing::info!("Portal capabilities verified");

    // Run the daemon event loop. Portal/input errors fail closed; only an
    // explicit configuration change may recreate sessions in-process.
    run_with_recovery(connection, shared, &mut control_rx).await
}

enum SessionOutcome {
    Restart,
    Shutdown,
}

/// Run sessions until a controlled restart or shutdown is requested.
/// Screen locks and configuration changes rebuild sessions in-process. Other
/// portal or input errors remain fatal and fail closed after explicit cleanup.
async fn run_with_recovery(
    connection: zbus::Connection,
    shared: SharedState,
    control_rx: &mut mpsc::Receiver<DictationRequest>,
) -> Result<(), DynError> {
    loop {
        let config = shared.config();
        match run_session(&config, connection.clone(), &shared, control_rx).await {
            Ok(SessionOutcome::Restart) => {
                tracing::info!("Restarting portal input session");
            }
            Ok(SessionOutcome::Shutdown) => {
                tracing::info!("Graceful shutdown complete");
                return Ok(());
            }
            Err(e) => {
                tracing::error!("Session error: {e}");
                shared.set_portal_connected(false);
                DaemonInterface::notify_portal_connected(&connection).await;
                update_state(State::Idle, &shared, &connection).await;
                shared.set_last_error(format!(
                    "Portal input session stopped for safety: {e}. Restart Voxkey manually after checking the desktop."
                ));
                DaemonInterface::notify_last_error(&connection).await;
                return Err(e);
            }
        }
    }
}

/// Run one set of portal input sessions. Returns Ok(Restart) for a controlled
/// lock/configuration rebuild and Err for unexpected portal/session failures.
async fn run_session(
    config: &Config,
    connection: zbus::Connection,
    shared: &SharedState,
    control_rx: &mut mpsc::Receiver<DictationRequest>,
) -> Result<SessionOutcome, DynError> {
    // Subscribe before inspecting the current lock state so a lock/unlock
    // transition cannot slip between the query and signal registration.
    let (mut screen_lock_events, screen_is_locked, screen_lock_monitored) =
        match screen_lock::subscribe(&connection).await {
            Ok((events, active)) => (events, active, true),
            Err(error) => {
                tracing::info!(
                    "GNOME screen-lock monitor unavailable; relying on portal closure signals: \
                     {error}"
                );
                (screen_lock::unavailable(), false, false)
            }
        };

    // Creating a RemoteDesktop session behind the lock screen can race the
    // compositor's teardown of the previous input session. Stay completely
    // disconnected until the real graphical session is unlocked.
    if screen_is_locked {
        tracing::info!("Screen is locked; waiting to create portal input sessions");
        update_state(State::RecoveringSession, shared, &connection).await;
        loop {
            tokio::select! {
                event = screen_lock_events.next() => match event {
                    Some(Ok(false)) => {
                        tracing::info!("Screen unlocked; rebuilding portal input sessions");
                        return Ok(SessionOutcome::Restart);
                    }
                    Some(Ok(true)) => {}
                    Some(Err(error)) => {
                        return Err(format!("GNOME screen-lock signal failed: {error}").into());
                    }
                    None => {
                        return Err("GNOME screen-lock signal stream ended while locked".into());
                    }
                },
                _ = shared.shutdown_requested() => {
                    tracing::info!("Shutdown requested while screen was locked");
                    return Ok(SessionOutcome::Shutdown);
                }
            }
        }
    }

    let token_path = config
        .token_path()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;

    // GlobalShortcuts stays ready for dictation. RemoteDesktop is deliberately
    // absent here: DesktopInput acquires it only around actual key insertion.
    let shortcuts = ShortcutController::new(connection.clone(), &config.shortcut).await?;
    let initial_shortcut_description = shortcuts.trigger_description().to_string();
    tracing::info!("GlobalShortcuts session ready");
    let desktop = Arc::new(DesktopInput::new(connection.clone(), token_path));
    tracing::info!("RemoteDesktop will be acquired only while inserting text");
    let recorder = Recorder::new(&config.audio);
    let runtime_transcriber_config = resolve_runtime_transcriber_config(&config.transcriber).await;
    let transcriber = Arc::new(Transcriber::from_config(
        &runtime_transcriber_config,
        config.audio.sample_rate,
        &config.dictionary.vocabulary,
    ));

    tracing::info!("Transcription backend: {}", transcriber.describe());
    let mut preload_task = if config.transcriber.provider
        == voxkey_ipc::TranscriberProvider::Parakeet
        && config.transcriber.parakeet.backend == voxkey_ipc::ParakeetBackend::Local
        && config.transcriber.parakeet.preload_model
    {
        let transcriber = transcriber.clone();
        Some(tokio::spawn(async move {
            let started = std::time::Instant::now();
            match transcriber.preload_local_model().await {
                Ok(true) => tracing::info!(
                    elapsed_ms = started.elapsed().as_millis(),
                    "Local transcription model is ready"
                ),
                Ok(false) => {}
                Err(error) => tracing::warn!("Could not preload the local model: {error}"),
            }
        }))
    } else {
        None
    };

    // Previews repeatedly re-decode the growing recording (the open tail or
    // the whole stream, per the configured strategy). That is cheap on this
    // machine and expensive on a metered API, so network-backed providers
    // stay out unless the configuration opts them in.
    let preview_capture = if config.preview.allows(transcriber.runs_locally()) {
        if config.preview.max_audio_seconds == 0 {
            tracing::info!(
                "Live previews on, refreshing every {}ms with no recording-length cap",
                config.preview.interval().as_millis()
            );
        } else {
            tracing::info!(
                "Live previews on, refreshing every {}ms for up to {}s of unconfirmed audio",
                config.preview.interval().as_millis(),
                config.preview.max_audio_seconds
            );
        }
        recorder::PreviewCapture::Enabled
    } else {
        tracing::info!(
            "Live previews are off for this backend (preview mode {:?})",
            config.preview.mode
        );
        recorder::PreviewCapture::Disabled
    };

    let typing_delay = std::time::Duration::from_millis(config.injection.typing_delay_ms as u64);

    // State management channel
    let (state_tx, mut state_rx) = mpsc::channel::<Event>(32);
    let (batch_result_tx, mut batch_result_rx) = mpsc::channel::<BatchTranscriptionResult>(1);

    // Injector with its own background task
    let mut injector = Injector::new(
        desktop.clone(),
        state_tx.clone(),
        shared.clone(),
        connection.clone(),
        typing_delay,
    );

    // Set up every persistent signal stream before announcing the portal as
    // connected. RemoteDesktop sessions are short-lived inside DesktopInput.
    let streams_result: Result<_, DynError> = async {
        let shortcut_events = shortcuts.event_stream().await?;
        let shortcuts_closed = shortcuts.receive_closed().await?;
        Ok((shortcut_events, shortcuts_closed))
    }
    .await;
    let (shortcut_events, mut shortcuts_closed) = match streams_result {
        Ok(streams) => streams,
        Err(error) => {
            let shortcuts_close_error = shortcuts.close().await.err();
            return Err(format!(
                "Portal signal setup failed: {error}; GlobalShortcuts cleanup error: \
                 {shortcuts_close_error:?}"
            )
            .into());
        }
    };
    let mut shortcut_events = Box::pin(shortcut_events);

    shared.set_shortcut_description(initial_shortcut_description);
    shared.set_portal_connected(true);
    DaemonInterface::notify_portal_connected(&connection).await;
    let session_generation = shared.session_generation();

    let mut current_state = State::Idle;
    update_state(current_state, shared, &connection).await;

    let mut batch_recording: Option<BatchRecordingState> = None;
    let mut batch_transcription: Option<BatchTranscriptionState> = None;
    let mut streaming_handle: Option<StreamingState> = None;
    let dictation_context = DictationContext {
        config,
        recorder: &recorder,
        transcriber: &transcriber,
        runtime_transcriber_config: &runtime_transcriber_config,
        preview_capture,
        desktop: &desktop,
        state_tx: &state_tx,
        typing_delay,
        shared,
        connection: &connection,
    };
    let mut audio_level_tick = tokio::time::interval(std::time::Duration::from_millis(50));
    audio_level_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    audio_level_tick.tick().await;

    let shortcut_id = config.shortcut.id.clone();

    // The compositor may repeat Activated for as long as the shortcut is
    // held, so only a press after Deactivated counts as a new press.
    let mut shortcut_presses = ShortcutPressTracker::default();

    let session_result: Result<SessionOutcome, DynError> = loop {
        tokio::select! {
            // Lock and shutdown invalidate every lower-priority operation.
            // Biased polling ensures already-queued shortcut or completion
            // events cannot win after either safety boundary is observable.
            biased;

            _ = shared.shutdown_requested() => {
                tracing::info!("Shutdown requested; closing portal sessions");
                break Ok(SessionOutcome::Shutdown);
            }

            lock_event = screen_lock_events.next() => {
                match lock_event {
                    Some(Ok(true)) => {
                        tracing::info!(
                            "Screen locked; retiring portal input sessions before unlock"
                        );
                        current_state = State::RecoveringSession;
                        update_state(current_state, shared, &connection).await;
                        break Ok(SessionOutcome::Restart);
                    }
                    Some(Ok(false)) => {
                        // An unlock without a preceding observed lock does not
                        // invalidate the sessions we just created.
                    }
                    Some(Err(error)) => {
                        break Err(format!("GNOME screen-lock signal failed: {error}").into());
                    }
                    None if screen_lock_monitored => {
                        break Err("GNOME screen-lock signal stream ended".into());
                    }
                    None => unreachable!("the unavailable lock stream never ends"),
                }
            }

            // GNOME may close GlobalShortcuts just before its lock signal.
            // Confirm the bounded race before treating the loss as fatal.
            signal = shortcuts_closed.next() => {
                match signal {
                    Some(()) => tracing::warn!("GlobalShortcuts session closed by portal"),
                    None => tracing::warn!("GlobalShortcuts Closed stream ended"),
                }
                if screen_lock_confirmed_after_session_loss(
                    &connection,
                    &mut screen_lock_events,
                    screen_lock_monitored,
                ).await? {
                    tracing::info!(
                        "Screen lock confirmed after GlobalShortcuts session loss; rebuilding after unlock"
                    );
                    current_state = State::RecoveringSession;
                    update_state(current_state, shared, &connection).await;
                    break Ok(SessionOutcome::Restart);
                }
                break Err("GlobalShortcuts session is no longer observable".into());
            }

            // Shell and other D-Bus controls enter through the same serialized
            // event loop as the global shortcut. The session generation makes
            // a request queued during teardown harmless after reconnection.
            Some(request) = control_rx.recv() => {
                let result = if request.expired() {
                    Err("The dictation request expired before it could run".to_string())
                } else if request.session_generation != session_generation {
                    Err("The desktop session changed before the request could run".to_string())
                } else {
                    match request.action {
                        DictationAction::Start if current_state == State::Idle => {
                            start_dictation(
                                &dictation_context,
                                &mut current_state,
                                &mut batch_recording,
                                &mut streaming_handle,
                            ).await
                        }
                        DictationAction::Start => Err(format!(
                            "Cannot start dictation while Voxkey is {current_state}"
                        )),
                        DictationAction::Stop if current_state == State::Recording => {
                            stop_recording(
                                &mut current_state,
                                &mut batch_recording,
                                &mut batch_transcription,
                                transcriber.clone(),
                                batch_result_tx.clone(),
                                shared,
                                &connection,
                            ).await;
                            Ok(())
                        }
                        DictationAction::Stop
                            if matches!(current_state, State::Connecting | State::Streaming) =>
                        {
                            stop_streaming(
                                &mut current_state,
                                &mut streaming_handle,
                                shared,
                                &connection,
                            ).await;
                            Ok(())
                        }
                        DictationAction::Stop => Err(format!(
                            "There is no active recording to stop (Voxkey is {current_state})"
                        )),
                        DictationAction::Cancel => cancel_dictation(
                            &mut current_state,
                            &mut batch_recording,
                            &mut batch_transcription,
                            &mut streaming_handle,
                            &injector,
                            shared,
                            &connection,
                        ).await,
                        DictationAction::InsertLastTranscript if current_state == State::Idle => {
                            insert_last_transcript(
                                &mut current_state,
                                &injector,
                                shared,
                                &connection,
                            ).await
                        }
                        DictationAction::InsertLastTranscript => Err(format!(
                            "Cannot insert the last transcript while Voxkey is {current_state}"
                        )),
                        DictationAction::RetryHistoryEntry(id) if current_state == State::Idle => {
                            retry_history_entry(
                                id,
                                &mut current_state,
                                &mut batch_transcription,
                                transcriber.clone(),
                                batch_result_tx.clone(),
                                shared,
                                &connection,
                            )
                            .await
                        }
                        DictationAction::RetryHistoryEntry(_) => Err(format!(
                            "Cannot retry a transcription while Voxkey is {current_state}"
                        )),
                    }
                };
                request.respond(result);
            }

            _ = audio_level_tick.tick() => {
                let (signal, should_auto_stop) = if let Some(batch) = batch_recording.as_mut() {
                    let signal = batch.recording.signal_snapshot();
                    let should_stop = current_state == State::Recording
                        && batch.auto_stop.observe(
                            signal,
                            config.audio.sample_rate,
                            config.audio.channels,
                            batch.capture_started.elapsed(),
                        );
                    (Some(signal), should_stop)
                } else if let Some(streaming) = streaming_handle.as_mut().filter(|_| {
                    matches!(current_state, State::Connecting | State::Streaming)
                }) {
                    let signal = streaming.recording.signal_snapshot();
                    let should_stop = streaming.auto_stop.observe(
                        signal,
                        config.audio.sample_rate,
                        config.audio.channels,
                        streaming.capture_started.elapsed(),
                    );
                    (Some(signal), should_stop)
                } else {
                    (None, false)
                };
                let level = signal.map(|signal| signal.latest_peak).unwrap_or(0.0);
                if shared.set_audio_level(level) {
                    DaemonInterface::notify_audio_level(&connection).await;
                }
                let quality = signal
                    .map(|signal| {
                        signal.quality(config.audio.sample_rate, config.audio.channels)
                    })
                    .unwrap_or(voxkey_ipc::AudioSignalQuality::Silent);
                if shared.set_audio_signal(quality) {
                    DaemonInterface::notify_audio_signal(&connection).await;
                }
                if should_auto_stop {
                    tracing::info!(
                        silence_ms = config.audio.behavior.auto_stop_silence_ms,
                        "Stopping dictation after the configured quiet interval"
                    );
                    if current_state == State::Recording {
                        stop_recording(
                            &mut current_state,
                            &mut batch_recording,
                            &mut batch_transcription,
                            transcriber.clone(),
                            batch_result_tx.clone(),
                            shared,
                            &connection,
                        ).await;
                    } else if matches!(current_state, State::Connecting | State::Streaming) {
                        stop_streaming(
                            &mut current_state,
                            &mut streaming_handle,
                            shared,
                            &connection,
                        ).await;
                    }
                }
            }

            // Shortcut events stay on one stream so release/repress ordering
            // cannot be inverted when both signals are already queued.
            event = shortcut_events.next() => {
                let Some(event) = event else {
                    break Err("GlobalShortcuts event stream ended".into());
                };
                let event = event?;
                let (event_id, timestamp) = match event {
                    ShortcutEvent::ShortcutsChanged { trigger_description } => {
                        let description = trigger_description;
                        tracing::info!(
                            "Portal shortcut binding changed: description={description:?}"
                        );
                        shared.set_shortcut_description(description);
                        DaemonInterface::notify_shortcut_description(&connection).await;
                        continue;
                    }
                    ShortcutEvent::Activated {
                        shortcut_id,
                        timestamp,
                    } => (shortcut_id, timestamp),
                    ShortcutEvent::Deactivated { shortcut_id: event_id, .. } => {
                        if event_id == shortcut_id {
                            let ended_press = shortcut_presses.deactivated();
                            if ended_press
                                && config.shortcut.mode == voxkey_ipc::ShortcutMode::PushToTalk
                                && matches!(
                                    current_state,
                                    State::Recording | State::Connecting | State::Streaming
                                )
                            {
                                if current_state == State::Recording {
                                    stop_recording(
                                        &mut current_state,
                                        &mut batch_recording,
                                        &mut batch_transcription,
                                        transcriber.clone(),
                                        batch_result_tx.clone(),
                                        shared,
                                        &connection,
                                    ).await;
                                } else {
                                    stop_streaming(
                                        &mut current_state,
                                        &mut streaming_handle,
                                        shared,
                                        &connection,
                                    ).await;
                                }
                            }
                        }
                        continue;
                    }
                };
                tracing::debug!("Activated signal received: shortcut_id={event_id:?}");
                if event_id != shortcut_id {
                    continue;
                }

                let repeat = shortcut_presses.activated(timestamp);
                if repeat {
                    continue;
                }

                if matches!(
                    current_state,
                    State::Recording | State::Connecting | State::Streaming
                ) {
                    if config.shortcut.mode == voxkey_ipc::ShortcutMode::Toggle {
                        if current_state == State::Recording {
                            stop_recording(
                                &mut current_state,
                                &mut batch_recording,
                                &mut batch_transcription,
                                transcriber.clone(),
                                batch_result_tx.clone(),
                                shared,
                                &connection,
                            ).await;
                        } else {
                            stop_streaming(
                                &mut current_state,
                                &mut streaming_handle,
                                shared,
                                &connection,
                            ).await;
                        }
                    }
                    continue;
                }

                if let Err(error) = start_dictation(
                    &dictation_context,
                    &mut current_state,
                    &mut batch_recording,
                    &mut streaming_handle,
                ).await {
                    tracing::debug!("Shortcut did not start dictation: {error}");
                }
            }

            // State machine events from injector or streaming session
            Some(event) = state_rx.recv() => {
                if let Event::BatchCaptureFailed {
                    transcript_generation,
                    ref message,
                } = event
                {
                    let belongs_to_active_recording = batch_capture_failure_is_current(
                        current_state,
                        batch_recording
                            .as_ref()
                            .map(|batch| batch.transcript_generation),
                        transcript_generation,
                    );
                    if belongs_to_active_recording {
                        let batch = batch_recording
                            .take()
                            .expect("matching active batch recording disappeared");
                        batch.capture_monitor.abort();
                        if let Some(preview) = batch.preview {
                            preview.stop().await;
                        }
                        clear_live_transcript(
                            batch.transcript_generation,
                            shared,
                            &connection,
                        ).await;
                        batch.recording.discard().await;
                        current_state = State::Idle;
                        update_state(current_state, shared, &connection).await;
                        shared.set_last_error(format!("Audio capture failed: {message}"));
                        DaemonInterface::notify_last_error(&connection).await;
                    }
                    continue;
                }
                let is_error = matches!(event, Event::Error);
                if let Some(new_state) = current_state.transition(&event) {
                    if new_state == State::Idle && streaming_handle.is_some() {
                        streaming_handle = None;
                    }
                    current_state = new_state;
                    update_state(current_state, shared, &connection).await;
                }
                if is_error {
                    break Err("Portal session error during injection".into());
                }
            }

            // Batch providers run outside this loop so shutdown, screen lock,
            // and portal closure can cancel them immediately.
            Some(completion) = batch_result_rx.recv() => {
                let Some(transcription) = take_batch_transcription_for_completion(
                    &mut batch_transcription,
                    completion.transcript_generation,
                ) else {
                    tracing::warn!("Ignoring a stale batch-transcription completion");
                    continue;
                };
                if let Err(error) = transcription.task.await {
                    tracing::error!("Batch transcription task failed after reporting completion: {error}");
                }
                finish_batch_transcription(
                    &mut current_state,
                    completion,
                    &runtime_transcriber_config,
                    &injector,
                    shared,
                    &connection,
                ).await;
            }

            // Session restart requested (e.g. shortcut changed via GUI)
            _ = shared.session_restart_requested() => {
                tracing::info!("Session restart requested");
                break Ok(SessionOutcome::Restart);
            }
        }
    };

    // Stop audio production and new queue admission synchronously, then give
    // the entire asynchronous teardown one shared budget.
    injector.stop_accepting();
    if let Some(mut streaming) = streaming_handle.take() {
        streaming.deliberate_teardown.store(true, Ordering::Release);
        streaming.recording.stop_capture();
        streaming.stop_tx.take();
        if let Some(cancel_tx) = streaming.cancel_tx.take() {
            let _ = cancel_tx.send(());
        }
        streaming_handle = Some(streaming);
    }

    let teardown = async {
        // Restore local audio state before any nonresponsive portal call can
        // consume the rest of the shared teardown budget.
        let mut streaming = streaming_handle.take();
        if let Some(streaming) = streaming.as_mut() {
            streaming.recording.restore_system_audio().await;
        }

        if let Some(batch) = batch_recording.take() {
            let BatchRecordingState {
                recording,
                preview,
                transcript_generation,
                capture_monitor,
                ..
            } = batch;
            capture_monitor.abort();
            recording.discard().await;
            if let Some(preview) = preview {
                preview.stop().await;
            }
            clear_live_transcript(transcript_generation, shared, &connection).await;
        }

        // Release any short-lived input grant before aborting work that might
        // still own its compositor-tracked virtual keyboard.
        if let Err(e) = desktop.close_active().await {
            tracing::warn!("Failed to close RemoteDesktop session during teardown: {e}");
        }

        if let Some(mut streaming) = streaming.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), &mut streaming.task).await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) if error.is_panic() => {
                    tracing::error!(
                        "Realtime transcription task panicked during teardown: {error}"
                    );
                }
                Ok(Err(_)) => {}
                Err(_) => {
                    streaming.task.abort();
                    let _ = streaming.task.await;
                }
            }
        }

        if let Some(transcription) = batch_transcription.take() {
            transcription.task.abort();
            if let Err(error) = transcription.task.await
                && error.is_panic()
            {
                tracing::error!("Batch transcription task panicked during teardown: {error}");
            }
        }
        if let Some(preload) = preload_task.take() {
            preload.abort();
            if let Err(error) = preload.await
                && error.is_panic()
            {
                tracing::error!("Local model preload task panicked: {error}");
            }
        }
        injector.shutdown().await;
        if let Err(e) = shortcuts.close().await {
            tracing::warn!("Failed to close GlobalShortcuts session: {e}");
        }
    };
    if tokio::time::timeout(crate::deadline::SESSION_TEARDOWN, teardown)
        .await
        .is_err()
    {
        tracing::error!("Session teardown exceeded its total deadline; dropping portal owners");
    }
    shared.set_portal_connected(false);
    DaemonInterface::notify_portal_connected(&connection).await;

    session_result
}

/// Portal and EIS teardown can lead GNOME's screen-lock signal by a small
/// amount. Once a session-loss event has won the main select, stop processing
/// all ordinary work and give the already-subscribed lock stream a bounded
/// opportunity to confirm that this is the expected lock transition.
async fn screen_lock_confirmed_after_session_loss(
    connection: &zbus::Connection,
    screen_lock_events: &mut screen_lock::LockEventStream,
    screen_lock_monitored: bool,
) -> Result<bool, DynError> {
    if !screen_lock_monitored {
        return Ok(false);
    }

    if screen_lock::is_active(connection).await? {
        return Ok(true);
    }

    if screen_lock::observe_lock_within(screen_lock_events, std::time::Duration::from_millis(750))
        .await?
    {
        return Ok(true);
    }

    screen_lock::is_active(connection).await
}

#[derive(Default)]
struct ShortcutPressTracker {
    pressed: bool,
}

impl ShortcutPressTracker {
    fn activated(&mut self, _activated_at: std::time::Duration) -> bool {
        let repeat = self.pressed;
        self.pressed = true;
        repeat
    }

    fn deactivated(&mut self) -> bool {
        let was_pressed = self.pressed;
        self.pressed = false;
        was_pressed
    }
}

struct DictationContext<'a> {
    config: &'a Config,
    recorder: &'a Recorder,
    transcriber: &'a Arc<Transcriber>,
    runtime_transcriber_config: &'a voxkey_ipc::TranscriberConfig,
    preview_capture: recorder::PreviewCapture,
    desktop: &'a Arc<DesktopInput>,
    state_tx: &'a mpsc::Sender<Event>,
    typing_delay: std::time::Duration,
    shared: &'a SharedState,
    connection: &'a zbus::Connection,
}

/// Start capture from either the portal shortcut or an acknowledged D-Bus
/// request. Keeping this in one function prevents the two control surfaces
/// from acquiring subtly different state-machine behavior.
async fn start_dictation(
    context: &DictationContext<'_>,
    current_state: &mut State,
    batch_recording: &mut Option<BatchRecordingState>,
    streaming_handle: &mut Option<StreamingState>,
) -> Result<(), String> {
    let Some(new_state) = current_state.transition(&Event::Activated) else {
        return Err(format!(
            "Cannot start dictation while Voxkey is {current_state}"
        ));
    };
    let admitted_state = if context.transcriber.is_streaming() {
        State::Connecting
    } else {
        new_state
    };
    context.shared.try_begin_dictation(admitted_state)?;
    *current_state = admitted_state;
    let transcript_generation = context.shared.begin_live_transcript();
    DaemonInterface::notify_live_transcript(context.connection).await;

    if context.transcriber.is_streaming() {
        let mut handle = match context.recorder.start_streaming().await {
            Ok(handle) => handle,
            Err(error) => {
                let message = format!("Failed to start streaming: {error}");
                tracing::error!("{message}");
                context.shared.set_last_error(message.clone());
                DaemonInterface::notify_last_error(context.connection).await;
                *current_state = State::Idle;
                update_state(*current_state, context.shared, context.connection).await;
                return Err(message);
            }
        };
        let audio_rx = handle.take_rx().expect("rx already taken");
        let capture_error_rx = handle
            .take_capture_error_rx()
            .expect("capture error rx already taken");
        let (stop_tx, stop_rx) = oneshot::channel();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let deliberate_teardown = Arc::new(AtomicBool::new(false));
        let task_teardown = deliberate_teardown.clone();
        let task = tokio::spawn({
            let rt_config = context.runtime_transcriber_config.mistral_realtime.clone();
            let transcriber_config = context.runtime_transcriber_config.clone();
            let local_model = context.transcriber.local_streaming_model();
            let sample_rate = context.config.audio.sample_rate;
            let channels = context.config.audio.channels;
            let desktop = context.desktop.clone();
            let state_tx = context.state_tx.clone();
            let shared = context.shared.clone();
            let connection = context.connection.clone();
            let typing_delay = context.typing_delay;
            async move {
                let result = if let Some(model) = local_model {
                    local_streaming::run_local_streaming_session(
                        local_streaming::LocalStreamingSession {
                            model,
                            transcriber_config,
                            sample_rate,
                            channels,
                            audio_rx,
                            capture_error_rx,
                            desktop,
                            state_tx: state_tx.clone(),
                            stop_rx,
                            cancel_rx,
                            shared: shared.clone(),
                            connection: connection.clone(),
                            typing_delay,
                            transcript_generation,
                        },
                    )
                    .await
                } else {
                    streaming::run_streaming_session(streaming::StreamingSession {
                        config: rt_config,
                        sample_rate,
                        channels,
                        audio_rx,
                        capture_error_rx,
                        desktop,
                        state_tx: state_tx.clone(),
                        stop_rx,
                        cancel_rx,
                        shared: shared.clone(),
                        connection: connection.clone(),
                        typing_delay,
                        transcript_generation,
                    })
                    .await
                };
                if let Err(error) = result {
                    if !should_publish_streaming_error(task_teardown.load(Ordering::Acquire)) {
                        tracing::info!(
                            "Streaming session ended during deliberate teardown: {error}"
                        );
                    } else {
                        tracing::error!("Streaming session error: {error}");
                        shared.set_last_error(format!("Streaming error: {error}"));
                        DaemonInterface::notify_last_error(&connection).await;
                    }
                    let _ = state_tx.send(Event::StreamingDone).await;
                }
            }
        });
        *streaming_handle = Some(StreamingState {
            recording: handle,
            stop_tx: Some(stop_tx),
            cancel_tx: Some(cancel_tx),
            transcript_generation,
            deliberate_teardown,
            task,
            capture_started: std::time::Instant::now(),
            auto_stop: crate::audio_signal::VoiceActivityStopwatch::new(
                context.config.audio.behavior.auto_stop_silence_ms,
            ),
        });
        update_state(*current_state, context.shared, context.connection).await;
    } else {
        update_state(*current_state, context.shared, context.connection).await;

        let mut handle = match context.recorder.start(context.preview_capture).await {
            Ok(handle) => handle,
            Err(error) => {
                let message = format!("Failed to start recording: {error}");
                tracing::error!("{message}");
                context.shared.set_last_error(message.clone());
                DaemonInterface::notify_last_error(context.connection).await;
                *current_state = State::Idle;
                update_state(*current_state, context.shared, context.connection).await;
                return Err(message);
            }
        };
        let preview = handle.take_preview_rx().map(|preview_rx| {
            preview::start(
                preview_rx,
                preview::PreviewSession {
                    sample_rate: context.config.audio.sample_rate,
                    channels: context.config.audio.channels,
                    transcriber: context.transcriber.clone(),
                    replacement_rules: context.config.dictionary.replacements.clone(),
                    shared: context.shared.clone(),
                    connection: context.connection.clone(),
                    generation: transcript_generation,
                    interval: context.config.preview.interval(),
                    max_audio: std::time::Duration::from_secs(
                        context.config.preview.max_audio_seconds as u64,
                    ),
                    strategy: context.config.preview.strategy,
                },
            )
        });
        let mut capture_errors = handle
            .take_capture_error_rx()
            .expect("batch capture error receiver already taken");
        let capture_events = context.state_tx.clone();
        let capture_monitor = tokio::spawn(async move {
            loop {
                let current_error = { capture_errors.borrow().clone() };
                if let Some(error) = current_error {
                    let _ = capture_events
                        .send(Event::BatchCaptureFailed {
                            transcript_generation,
                            message: error,
                        })
                        .await;
                    break;
                }
                if capture_errors.changed().await.is_err() {
                    break;
                }
            }
        });
        *batch_recording = Some(BatchRecordingState {
            recording: handle,
            preview,
            transcript_generation,
            capture_monitor,
            capture_started: std::time::Instant::now(),
            auto_stop: crate::audio_signal::VoiceActivityStopwatch::new(
                context.config.audio.behavior.auto_stop_silence_ms,
            ),
        });
    }

    context.shared.set_last_error(String::new());
    DaemonInterface::notify_last_error(context.connection).await;
    Ok(())
}

/// Holds state for an active streaming session.
struct StreamingState {
    recording: recorder::StreamingRecordingHandle,
    stop_tx: Option<oneshot::Sender<()>>,
    cancel_tx: Option<oneshot::Sender<()>>,
    transcript_generation: u64,
    deliberate_teardown: Arc<AtomicBool>,
    task: JoinHandle<()>,
    capture_started: std::time::Instant,
    auto_stop: crate::audio_signal::VoiceActivityStopwatch,
}

fn should_publish_streaming_error(deliberate_teardown: bool) -> bool {
    !deliberate_teardown
}

/// Holds the lossless batch recording together with its replaceable preview.
/// The preview is absent when the configuration or provider rules them out.
struct BatchRecordingState {
    recording: recorder::RecordingHandle,
    preview: Option<preview::PreviewHandle>,
    transcript_generation: u64,
    capture_monitor: JoinHandle<()>,
    capture_started: std::time::Instant,
    auto_stop: crate::audio_signal::VoiceActivityStopwatch,
}

/// Owns the cancellable final-transcription task for one stopped recording.
struct BatchTranscriptionState {
    task: JoinHandle<()>,
    transcript_generation: u64,
}

struct BatchTranscriptionResult {
    transcript_generation: u64,
    result: Result<String, String>,
    /// Present only for a newly captured recording whose transcription
    /// failed. Ownership keeps cancellation and stale completions private.
    failed_recording: Option<TemporaryRecording>,
    metrics: voxkey_ipc::HistoryMetrics,
}

fn audio_duration_millis(recorded_samples: u64, sample_rate: u32, channels: u16) -> Option<u64> {
    if sample_rate == 0 || channels == 0 {
        return None;
    }
    let samples_per_second = u128::from(sample_rate) * u128::from(channels);
    Some(
        ((u128::from(recorded_samples) * 1_000) / samples_per_second).min(u128::from(u64::MAX))
            as u64,
    )
}

fn duration_millis_u64(duration: std::time::Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

struct TemporaryRecording {
    path: Option<PathBuf>,
}

impl TemporaryRecording {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("temporary recording ownership already transferred")
    }

    fn keep(mut self) -> PathBuf {
        self.path
            .take()
            .expect("temporary recording ownership already transferred")
    }
}

impl Drop for TemporaryRecording {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        if let Err(error) = std::fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                "Failed to remove temporary recording at {}: {error}",
                path.display()
            );
        }
    }
}

fn take_batch_transcription_for_completion(
    active: &mut Option<BatchTranscriptionState>,
    completion_generation: u64,
) -> Option<BatchTranscriptionState> {
    let active_generation = active
        .as_ref()
        .map(|transcription| transcription.transcript_generation)?;
    if active_generation != completion_generation {
        tracing::warn!(
            completion_generation,
            active_generation,
            "Ignoring a batch completion that does not own the active task"
        );
        return None;
    }
    active.take()
}

fn batch_capture_failure_is_current(
    state: State,
    active_generation: Option<u64>,
    failure_generation: u64,
) -> bool {
    state == State::Recording && active_generation == Some(failure_generation)
}

/// Discard the preview for a recording that produced no inserted text.
async fn clear_live_transcript(
    generation: u64,
    shared: &SharedState,
    connection: &zbus::Connection,
) {
    if shared.update_live_transcript(generation, String::new()) {
        DaemonInterface::notify_live_transcript(connection).await;
    }
}

/// Discard work that has not reached the user's application yet. Realtime
/// text already committed as complete deltas cannot be removed safely, but
/// cancellation prevents any later provider output from being flushed.
async fn cancel_dictation(
    current_state: &mut State,
    batch_recording: &mut Option<BatchRecordingState>,
    batch_transcription: &mut Option<BatchTranscriptionState>,
    streaming_handle: &mut Option<StreamingState>,
    injector: &Injector,
    shared: &SharedState,
    connection: &zbus::Connection,
) -> Result<(), String> {
    match *current_state {
        State::Recording => {
            let Some(batch) = batch_recording.take() else {
                return Err("The active recording handle is unavailable".to_string());
            };
            batch.capture_monitor.abort();
            if let Some(preview) = batch.preview {
                preview.stop().await;
            }
            batch.recording.discard().await;
            // Incrementing the generation also rejects a preview result that
            // was already on its way to the D-Bus task during cancellation.
            shared.begin_live_transcript();
            DaemonInterface::notify_live_transcript(connection).await;
            *current_state = State::Idle;
            update_state(*current_state, shared, connection).await;
            tracing::info!("Batch recording cancelled and discarded");
            Ok(())
        }
        State::Transcribing if batch_transcription.is_some() => {
            let transcription = batch_transcription
                .take()
                .expect("checked batch transcription presence");
            transcription.task.abort();
            if let Err(error) = transcription.task.await
                && error.is_panic()
            {
                tracing::error!("Batch transcription task panicked during cancellation: {error}");
            }
            shared.begin_live_transcript();
            DaemonInterface::notify_live_transcript(connection).await;
            *current_state = State::Idle;
            update_state(*current_state, shared, connection).await;
            tracing::info!(
                generation = transcription.transcript_generation,
                "Batch transcription cancelled"
            );
            Ok(())
        }
        State::Connecting | State::Streaming | State::Transcribing
            if streaming_handle.is_some() =>
        {
            let streaming = streaming_handle
                .as_mut()
                .expect("checked streaming handle presence");
            streaming.deliberate_teardown.store(true, Ordering::Release);
            streaming.recording.stop_capture();
            streaming.stop_tx.take();
            if let Some(cancel_tx) = streaming.cancel_tx.take() {
                let _ = cancel_tx.send(());
            }
            streaming.recording.restore_system_audio().await;
            shared.begin_live_transcript();
            DaemonInterface::notify_live_transcript(connection).await;
            *current_state = State::Transcribing;
            update_state(*current_state, shared, connection).await;
            tracing::info!(
                generation = streaming.transcript_generation,
                "Realtime dictation cancellation requested"
            );
            Ok(())
        }
        State::Injecting => {
            injector.cancel_current();
            tracing::info!("Text insertion cancellation requested");
            Ok(())
        }
        _ => Err(format!(
            "There is no recording or transcription to cancel (Voxkey is {current_state})"
        )),
    }
}

async fn insert_last_transcript(
    current_state: &mut State,
    injector: &Injector,
    shared: &SharedState,
    connection: &zbus::Connection,
) -> Result<(), String> {
    let insertion = shared
        .last_insertion()
        .ok_or_else(|| "There is no transcript text waiting to be inserted".to_string())?;

    *current_state = State::Injecting;
    update_state(*current_state, shared, connection).await;
    if let Err(error) = injector.enqueue_last(insertion).await {
        let message = format!("Failed to enqueue the last transcript: {error}");
        shared.set_last_error(message.clone());
        DaemonInterface::notify_last_error(connection).await;
        *current_state = State::Idle;
        update_state(*current_state, shared, connection).await;
        return Err(message);
    }
    Ok(())
}

async fn retry_history_entry(
    id: u64,
    current_state: &mut State,
    batch_transcription: &mut Option<BatchTranscriptionState>,
    transcriber: Arc<Transcriber>,
    result_tx: mpsc::Sender<BatchTranscriptionResult>,
    shared: &SharedState,
    connection: &zbus::Connection,
) -> Result<(), String> {
    if transcriber.is_streaming() {
        return Err(
            "Saved recordings cannot be retried with Mistral Realtime. Choose Whisper.cpp, \
             Parakeet, or Mistral in settings."
                .to_string(),
        );
    }
    let audio_path = shared.failed_recording_path(id)?;
    let prior_metrics = shared.history_metrics(id);
    shared.try_begin_transcription()?;
    let transcript_generation = shared.begin_live_transcript();
    *current_state = State::Transcribing;
    update_state(*current_state, shared, connection).await;
    shared.set_last_error(String::new());
    DaemonInterface::notify_last_error(connection).await;

    let task = tokio::spawn(async move {
        let processing_started = std::time::Instant::now();
        let result = transcriber
            .transcribe_recording(&audio_path)
            .await
            .map_err(|error| error.to_string());
        let _ = result_tx
            .send(BatchTranscriptionResult {
                transcript_generation,
                result,
                // The source is already the durable History recording. A
                // failed retry must leave it available for another attempt.
                failed_recording: None,
                metrics: voxkey_ipc::HistoryMetrics {
                    audio_duration_ms: prior_metrics.audio_duration_ms,
                    processing_duration_ms: Some(duration_millis_u64(processing_started.elapsed())),
                },
            })
            .await;
    });
    *batch_transcription = Some(BatchTranscriptionState {
        task,
        transcript_generation,
    });
    Ok(())
}

/// Stop recording, transcribe, and enqueue for injection.
async fn stop_recording(
    current_state: &mut State,
    batch_recording: &mut Option<BatchRecordingState>,
    batch_transcription: &mut Option<BatchTranscriptionState>,
    transcriber: Arc<Transcriber>,
    result_tx: mpsc::Sender<BatchTranscriptionResult>,
    shared: &SharedState,
    connection: &zbus::Connection,
) {
    *current_state = State::Transcribing;
    update_state(*current_state, shared, connection).await;

    if let Some(batch) = batch_recording.take() {
        let BatchRecordingState {
            recording,
            preview,
            transcript_generation,
            capture_monitor,
            ..
        } = batch;
        capture_monitor.abort();

        // The recording stops first so the preview supervisor can consume the
        // final chunks; its newest decode may then cover all captured audio
        // and serve as the transcript the user already sees.
        let dropped_chunks = recording.preview_chunks_dropped();
        match recording.stop_with_summary().await {
            Ok(recording) => {
                let recorder::FinalizedRecording {
                    path: audio_path,
                    recorded_samples,
                    signal,
                } = recording;
                if shared.config().audio.behavior.no_speech_guard
                    && !signal.has_meaningful_audio(
                        shared.config().audio.sample_rate,
                        shared.config().audio.channels,
                    )
                {
                    if let Some(preview) = preview {
                        preview.stop().await;
                    }
                    let _recording_cleanup = TemporaryRecording::new(audio_path);
                    clear_live_transcript(transcript_generation, shared, connection).await;
                    let message = "No speech was detected. Check the selected microphone or run the signal test in Settings.";
                    tracing::info!(
                        max_peak = signal.max_peak,
                        active_samples = signal.active_samples,
                        "Skipping transcription for a silent recording"
                    );
                    shared.set_last_error(message.to_string());
                    DaemonInterface::notify_last_error(connection).await;
                    *current_state = State::Idle;
                    update_state(*current_state, shared, connection).await;
                    return;
                }
                let metrics = voxkey_ipc::HistoryMetrics {
                    audio_duration_ms: audio_duration_millis(
                        recorded_samples,
                        shared.config().audio.sample_rate,
                        shared.config().audio.channels,
                    ),
                    processing_duration_ms: None,
                };
                let processing_started = std::time::Instant::now();
                let finalization = match preview {
                    Some(preview) => preview.finish().await,
                    None => None,
                };
                let task = tokio::spawn(async move {
                    let recording = TemporaryRecording::new(audio_path);
                    // Reuse the newest whole-recording preview decode when it
                    // already covers all captured microphone audio: the inserted text is
                    // then exactly what the overlay showed. Anything less
                    // (audio captured after the last preview, dropped preview
                    // chunks, or a segmented preview) falls back to a fresh
                    // decode of the whole recording, because decoding
                    // silence-split utterances individually and stitching
                    // their texts yields noticeably worse results than the
                    // full stream.
                    let preview_samples = finalization.as_ref().map(|(samples, _)| *samples);
                    let reusable = preview::reusable_preview_final(
                        finalization,
                        recorded_samples,
                        dropped_chunks,
                    );
                    if reusable.is_none() {
                        tracing::debug!(
                            ?preview_samples,
                            recorded_samples,
                            dropped_chunks,
                            "Final preview did not cover the captured recording"
                        );
                    }
                    let result = match reusable {
                        Some(text) => {
                            tracing::info!("Reusing the last preview decode as the transcript");
                            Ok(text)
                        }
                        None => transcriber
                            .transcribe_recording(recording.path())
                            .await
                            .map_err(|error| error.to_string()),
                    };
                    let failed_recording = if result.is_err() {
                        Some(recording)
                    } else {
                        drop(recording);
                        None
                    };
                    let _ = result_tx
                        .send(BatchTranscriptionResult {
                            transcript_generation,
                            result,
                            failed_recording,
                            metrics: voxkey_ipc::HistoryMetrics {
                                processing_duration_ms: Some(duration_millis_u64(
                                    processing_started.elapsed(),
                                )),
                                ..metrics
                            },
                        })
                        .await;
                });
                *batch_transcription = Some(BatchTranscriptionState {
                    task,
                    transcript_generation,
                });
            }
            Err(e) => {
                if let Some(preview) = preview {
                    preview.stop().await;
                }
                tracing::error!("Failed to stop recording: {e}");
                shared.set_last_error(format!("Failed to stop recording: {e}"));
                DaemonInterface::notify_last_error(connection).await;
                clear_live_transcript(transcript_generation, shared, connection).await;
                *current_state = State::Idle;
                update_state(*current_state, shared, connection).await;
            }
        }
    } else {
        tracing::error!("Recording stopped without an active recording handle");
        *current_state = State::Idle;
        update_state(*current_state, shared, connection).await;
    }
}

async fn finish_batch_transcription(
    current_state: &mut State,
    completion: BatchTranscriptionResult,
    transcriber_config: &voxkey_ipc::TranscriberConfig,
    injector: &Injector,
    shared: &SharedState,
    connection: &zbus::Connection,
) {
    let BatchTranscriptionResult {
        transcript_generation,
        result,
        failed_recording,
        metrics,
    } = completion;
    match result {
        Ok(transcript) => {
            let transcript = crate::dictionary::process_transcription_output(
                &transcript,
                &shared.config().dictionary.replacements,
            );
            if shared.update_live_transcript(transcript_generation, transcript.clone()) {
                DaemonInterface::notify_live_transcript(connection).await;
            }
            if transcript.is_empty() {
                tracing::info!("Empty transcript, returning to idle");
                *current_state = State::Idle;
                update_state(*current_state, shared, connection).await;
            } else {
                if let Err(e) = injector
                    .enqueue_transcript(
                        transcript.clone(),
                        transcriber_config.clone(),
                        voxkey_ipc::TranscriptOutcome::Completed,
                        metrics,
                    )
                    .await
                {
                    tracing::error!("Failed to enqueue text: {e}");
                    let persistence_error = shared
                        .record_transcript_with_metrics(
                            transcript.clone(),
                            transcriber_config,
                            voxkey_ipc::TranscriptOutcome::Completed,
                            Some(transcript.clone()),
                            metrics,
                        )
                        .err();
                    DaemonInterface::notify_transcription_complete(connection, &transcript).await;
                    let message = match persistence_error {
                        Some(persistence_error) => format!(
                            "Failed to enqueue text: {e}; also failed to save the transcript: \
                             {persistence_error}"
                        ),
                        None => format!("Failed to enqueue text: {e}"),
                    };
                    shared.set_last_error(message);
                    DaemonInterface::notify_last_error(connection).await;
                    *current_state = State::Idle;
                    update_state(*current_state, shared, connection).await;
                }
            }
        }
        Err(error) => {
            tracing::error!("Transcription failed: {error}");
            let message = match failed_recording {
                Some(recording) => match shared.record_failed_transcription_with_metrics(
                    recording.path(),
                    transcriber_config,
                    error.clone(),
                    metrics,
                ) {
                    Ok(_) => {
                        DaemonInterface::notify_last_transcript(connection).await;
                        format!(
                            "Transcription failed: {error}. Your recording was saved in History."
                        )
                    }
                    Err(preservation_error) if preservation_error.saved_path.is_some() => {
                        format!("Transcription failed: {error}; {preservation_error}")
                    }
                    Err(preservation_error) => {
                        let temporary_path = recording.keep();
                        format!(
                            "Transcription failed: {error}; {preservation_error}. The temporary WAV remains at {}",
                            temporary_path.display()
                        )
                    }
                },
                None => format!("Transcription failed: {error}"),
            };
            shared.set_last_error(message);
            DaemonInterface::notify_last_error(connection).await;
            // Drop any preview hypothesis: nothing was inserted, so leaving
            // text on screen would misreport what happened.
            clear_live_transcript(transcript_generation, shared, connection).await;
            *current_state = State::Idle;
            update_state(*current_state, shared, connection).await;
        }
    }
}

/// Stop streaming audio capture and signal the WebSocket session to drain.
async fn stop_streaming(
    current_state: &mut State,
    streaming_handle: &mut Option<StreamingState>,
    shared: &SharedState,
    connection: &zbus::Connection,
) {
    *current_state = State::Transcribing;
    update_state(*current_state, shared, connection).await;

    if let Some(handle) = streaming_handle.as_mut() {
        handle.recording.stop_capture();
    }
    begin_streaming_drain(streaming_handle, |handle| {
        if let Some(stop_tx) = handle.stop_tx.take() {
            let _ = stop_tx.send(());
        }
    });
    if let Some(handle) = streaming_handle.as_mut() {
        handle.recording.restore_system_audio().await;
    }
    // The streaming task will send StreamingDone when transcription.done arrives
}

fn begin_streaming_drain<T, F>(streaming: &mut Option<T>, stop: F) -> bool
where
    F: FnOnce(&mut T),
{
    let Some(active) = streaming.as_mut() else {
        return false;
    };
    stop(active);
    true
}

/// Log state change, update shared D-Bus state, and emit PropertiesChanged.
async fn update_state(state: State, shared: &SharedState, connection: &zbus::Connection) {
    shared.set_state(state);
    let capture_is_active = matches!(
        state,
        State::Recording | State::Connecting | State::Streaming
    );
    if !capture_is_active {
        if shared.set_audio_level(0.0) {
            DaemonInterface::notify_audio_level(connection).await;
        }
        if shared.set_audio_signal(voxkey_ipc::AudioSignalQuality::Silent) {
            DaemonInterface::notify_audio_signal(connection).await;
        }
    }
    eprintln!("STATE: {state}");
    DaemonInterface::notify_state(connection).await;
}

/// Migrate any plaintext API keys still present in the persisted Config to the
/// system keyring, then clear them from the config and save. If the keyring is
/// unavailable, the plaintext value is left in place so the user is not stuck
/// without a working transcription provider.
fn plaintext_api_key_for_migration(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

async fn migrate_plaintext_api_keys(config: &mut Config) {
    let previous = config.clone();
    let mut migrated = false;

    if let Some(api_key) = plaintext_api_key_for_migration(&config.transcriber.mistral.api_key) {
        match secret_store::set(secret_store::SERVICE_MISTRAL, api_key).await {
            Ok(()) => {
                tracing::info!("Migrated Mistral API key from config.toml to keyring");
                config.transcriber.mistral.api_key.clear();
                migrated = true;
            }
            Err(e) => {
                tracing::warn!(
                    "Could not migrate Mistral API key to keyring (leaving in config.toml): {e}"
                );
            }
        }
    }

    if let Some(api_key) =
        plaintext_api_key_for_migration(&config.transcriber.mistral_realtime.api_key)
    {
        match secret_store::set(secret_store::SERVICE_MISTRAL_REALTIME, api_key).await {
            Ok(()) => {
                tracing::info!("Migrated Mistral Realtime API key from config.toml to keyring");
                config.transcriber.mistral_realtime.api_key.clear();
                migrated = true;
            }
            Err(e) => {
                tracing::warn!(
                    "Could not migrate Mistral Realtime API key to keyring (leaving in config.toml): {e}"
                );
            }
        }
    }

    if let Some(api_key) = plaintext_api_key_for_migration(&config.transcriber.parakeet.api_key) {
        match secret_store::set(secret_store::SERVICE_MODEL_SERVER, api_key).await {
            Ok(()) => {
                tracing::info!("Migrated model-server API key from config.toml to keyring");
                config.transcriber.parakeet.api_key.clear();
                migrated = true;
            }
            Err(e) => {
                tracing::warn!(
                    "Could not migrate model-server API key to keyring (leaving in config.toml): {e}"
                );
            }
        }
    }

    if migrated && let Err(e) = Config::save_delta(&previous, config) {
        tracing::warn!("Failed to save config after API key migration: {e}");
    }
}

/// Build a per-session copy of the transcriber config with API keys looked up
/// from the keyring. Persisted config never carries the keys; this populates
/// them in memory only, fresh for every session start.
fn selected_api_key_service(config: &voxkey_ipc::TranscriberConfig) -> Option<&'static str> {
    match config.provider {
        voxkey_ipc::TranscriberProvider::Mistral => Some(secret_store::SERVICE_MISTRAL),
        voxkey_ipc::TranscriberProvider::MistralRealtime => {
            Some(secret_store::SERVICE_MISTRAL_REALTIME)
        }
        voxkey_ipc::TranscriberProvider::Parakeet
            if config.parakeet.backend == voxkey_ipc::ParakeetBackend::Http =>
        {
            Some(secret_store::SERVICE_MODEL_SERVER)
        }
        voxkey_ipc::TranscriberProvider::Parakeet => None,
        voxkey_ipc::TranscriberProvider::WhisperCpp => None,
    }
}

async fn resolve_runtime_transcriber_config(
    persisted: &voxkey_ipc::TranscriberConfig,
) -> voxkey_ipc::TranscriberConfig {
    let mut runtime = persisted.clone();
    let Some(service) = selected_api_key_service(persisted) else {
        return runtime;
    };
    if let Some(key) = secret_store::get(service).await {
        match &persisted.provider {
            voxkey_ipc::TranscriberProvider::Mistral => runtime.mistral.api_key = key,
            voxkey_ipc::TranscriberProvider::MistralRealtime => {
                runtime.mistral_realtime.api_key = key;
            }
            voxkey_ipc::TranscriberProvider::Parakeet => runtime.parakeet.api_key = key,
            voxkey_ipc::TranscriberProvider::WhisperCpp => {
                unreachable!("providers without credentials return before querying the keyring")
            }
        }
    }
    runtime
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    #[test]
    fn the_first_activation_is_always_a_press() {
        let mut presses = ShortcutPressTracker::default();
        assert!(!presses.activated(at(1_000)));
    }

    #[test]
    fn temporary_recording_cleanup_can_be_transferred_to_the_user() {
        let discarded = tempfile::NamedTempFile::new()
            .unwrap()
            .into_temp_path()
            .keep()
            .unwrap();
        drop(TemporaryRecording::new(discarded.clone()));
        assert!(!discarded.exists());

        let retained = tempfile::NamedTempFile::new()
            .unwrap()
            .into_temp_path()
            .keep()
            .unwrap();
        let exposed = TemporaryRecording::new(retained.clone()).keep();
        assert_eq!(exposed, retained);
        assert!(retained.exists());
        std::fs::remove_file(retained).unwrap();
    }

    #[test]
    fn activations_while_the_shortcut_stays_held_are_repeats() {
        let mut presses = ShortcutPressTracker::default();
        assert!(!presses.activated(at(1_000)));
        assert!(presses.activated(at(1_030)));
        assert!(presses.activated(at(1_100)));
    }

    /// Transcribing blocks the event loop for longer than the repeat
    /// interval. A repeat that queued up behind it is still a repeat, however
    /// late the daemon gets to it.
    #[test]
    fn a_repeat_delayed_behind_transcription_is_still_a_repeat() {
        let mut presses = ShortcutPressTracker::default();
        // Pressed at 1000ms, repeated at 1030ms, but only handled once the
        // transcription that started at 1000ms finished.
        assert!(!presses.activated(at(1_000)));
        assert!(presses.activated(at(1_030)));
    }

    /// Some portals never fill the activation time in. Their zeroes carry no
    /// information, so the release event remains the authoritative boundary
    /// between physical presses.
    #[test]
    fn a_portal_without_activation_times_still_toggles() {
        let mut presses = ShortcutPressTracker::default();
        assert!(!presses.activated(at(0)));
        assert!(presses.activated(at(0)));
        presses.deactivated();
        assert!(!presses.activated(at(0)));
    }

    #[test]
    fn release_makes_a_fast_repress_a_new_toggle() {
        let mut presses = ShortcutPressTracker::default();
        assert!(!presses.activated(at(1_000)));
        assert!(presses.activated(at(1_030)));

        presses.deactivated();

        assert!(!presses.activated(at(1_050)));
    }

    #[test]
    fn release_reports_whether_a_real_press_ended() {
        let mut presses = ShortcutPressTracker::default();
        assert!(!presses.deactivated());
        assert!(!presses.activated(at(10)));
        assert!(presses.deactivated());
        assert!(!presses.deactivated());
    }

    #[test]
    fn history_audio_duration_uses_frames_not_interleaved_samples() {
        assert_eq!(audio_duration_millis(16_000, 16_000, 1), Some(1_000));
        assert_eq!(audio_duration_millis(96_000, 48_000, 2), Some(1_000));
        assert_eq!(audio_duration_millis(8_000, 16_000, 1), Some(500));
        assert_eq!(audio_duration_millis(1, 0, 1), None);
        assert_eq!(audio_duration_millis(1, 16_000, 0), None);
    }

    #[test]
    fn a_delayed_first_repeat_does_not_toggle_while_the_key_is_held() {
        let mut presses = ShortcutPressTracker::default();
        assert!(!presses.activated(at(1_000)));

        // Desktop repeat delay is commonly much longer than the interval
        // between subsequent repeats. Without a release, this is still the
        // original physical press and must not stop the recording.
        assert!(presses.activated(at(1_500)));
    }

    #[test]
    fn streaming_drain_keeps_task_owned_until_completion() {
        let mut streaming = Some("join handle");

        assert!(begin_streaming_drain(&mut streaming, |_| {}));

        assert_eq!(streaming, Some("join handle"));
    }

    #[test]
    fn deliberate_streaming_teardown_does_not_become_a_user_error() {
        assert!(!should_publish_streaming_error(true));
        assert!(should_publish_streaming_error(false));
    }

    #[tokio::test]
    async fn stale_batch_completion_cannot_consume_the_next_generation() {
        let task = tokio::spawn(std::future::pending::<()>());
        let mut active = Some(BatchTranscriptionState {
            task,
            transcript_generation: 2,
        });

        let taken = take_batch_transcription_for_completion(&mut active, 1);

        assert!(
            taken.is_none(),
            "the stale generation consumed the active task"
        );
        let active = active.expect("the next generation must remain owned");
        assert_eq!(active.transcript_generation, 2);
        active.task.abort();
    }

    #[test]
    fn stale_capture_failure_cannot_stop_the_next_recording() {
        assert!(!batch_capture_failure_is_current(
            State::Recording,
            Some(2),
            1
        ));
        assert!(batch_capture_failure_is_current(
            State::Recording,
            Some(2),
            2
        ));
        assert!(!batch_capture_failure_is_current(
            State::Transcribing,
            Some(2),
            2
        ));
    }

    #[test]
    fn blank_plaintext_api_keys_are_never_migrated_over_keyring_entries() {
        assert_eq!(plaintext_api_key_for_migration("  \t\n"), None);
        assert_eq!(
            plaintext_api_key_for_migration("  sk-legacy-key \n"),
            Some("sk-legacy-key")
        );
    }

    #[test]
    fn runtime_keyring_lookup_is_scoped_to_the_selected_provider() {
        use voxkey_ipc::TranscriberProvider;

        let config = voxkey_ipc::TranscriberConfig {
            provider: TranscriberProvider::Mistral,
            ..Default::default()
        };
        assert_eq!(
            selected_api_key_service(&config),
            Some(secret_store::SERVICE_MISTRAL)
        );
        let config = voxkey_ipc::TranscriberConfig {
            provider: TranscriberProvider::MistralRealtime,
            ..Default::default()
        };
        assert_eq!(
            selected_api_key_service(&config),
            Some(secret_store::SERVICE_MISTRAL_REALTIME)
        );
        let config = voxkey_ipc::TranscriberConfig {
            provider: TranscriberProvider::WhisperCpp,
            ..Default::default()
        };
        assert_eq!(selected_api_key_service(&config), None);
        let config = voxkey_ipc::TranscriberConfig {
            provider: TranscriberProvider::Parakeet,
            ..Default::default()
        };
        assert_eq!(selected_api_key_service(&config), None);
        let mut server = config;
        server.parakeet.backend = voxkey_ipc::ParakeetBackend::Http;
        assert_eq!(
            selected_api_key_service(&server),
            Some(secret_store::SERVICE_MODEL_SERVER)
        );
    }
}
