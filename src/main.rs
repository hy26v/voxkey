// ABOUTME: Entry point for the voxkey Wayland dictation daemon.
// ABOUTME: Wires portal sessions, audio recording, transcription, and text injection into an event loop.

mod config;
mod dbus;
mod desktop;
mod injector;
mod persistence;
mod portal;
mod recorder;
mod registry;
mod shortcuts;
mod state;
mod streaming;
mod transcriber;

use std::sync::Arc;

use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use config::Config;
use dbus::{DaemonInterface, SharedState};
use desktop::DesktopController;
use injector::Injector;
use recorder::Recorder;
use shortcuts::ShortcutController;
use state::{Event, State};
use transcriber::Transcriber;

type DynError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    // Register signal handlers early so they work during startup
    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate(),
    ).expect("Failed to register SIGTERM handler");

    tokio::select! {
        result = run() => {
            if let Err(e) = result {
                tracing::error!("Fatal: {e}");
                std::process::exit(1);
            }
        }
        _ = sigterm.recv() => {
            tracing::info!("Received SIGTERM, shutting down");
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received SIGINT, shutting down");
        }
    }
}

async fn run() -> Result<(), DynError> {
    let config = Config::load()?;
    tracing::info!("Configuration loaded");

    let shared = SharedState::new(config.clone());

    // Register app_id with the portal and get the shared connection
    let connection = registry::connect_and_register().await?;

    // Serve the D-Bus interface for the settings GUI
    connection
        .object_server()
        .at(voxkey_ipc::OBJECT_PATH, DaemonInterface::new(shared.clone()))
        .await?;
    connection.request_name(voxkey_ipc::BUS_NAME).await?;
    tracing::info!("D-Bus interface registered at {}", voxkey_ipc::BUS_NAME);

    // Capability checks (using the same connection)
    portal::check_capabilities(connection.clone()).await.map_err(|e| -> DynError {
        tracing::error!("Portal capability check failed: {e}");
        e.into()
    })?;
    tracing::info!("Portal capabilities verified");

    // Run the daemon event loop with session recovery
    run_with_recovery(connection, shared).await
}

enum SessionOutcome {
    Restart,
}

/// Run the daemon with automatic session recovery on portal errors.
async fn run_with_recovery(connection: zbus::Connection, shared: SharedState) -> Result<(), DynError> {
    loop {
        let config = shared.config();
        tokio::select! {
            result = run_session(&config, connection.clone(), &shared) => {
                match result {
                    Ok(SessionOutcome::Restart) => {
                        tracing::info!("Restarting session for shortcut change");
                    }
                    Err(e) => {
                        tracing::error!("Session error: {e}");
                        shared.set_portal_connected(false);
                        DaemonInterface::notify_portal_connected(&connection).await;
                        update_state(State::RecoveringSession, &shared, &connection).await;
                        tracing::info!("Attempting session recovery in 2 seconds...");
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        update_state(State::Idle, &shared, &connection).await;
                    }
                }
            }
            _ = shared.shutdown_requested() => {
                tracing::info!("Shutdown requested via D-Bus");
                return Ok(());
            }
        }
    }
}

/// Run a single daemon session. Returns Ok(Restart) when a config change requires
/// session recreation, Err on portal/session errors.
async fn run_session(config: &Config, connection: zbus::Connection, shared: &SharedState) -> Result<SessionOutcome, DynError> {
    // Load restore token
    let token_path = config.token_path();
    let restore_token = persistence::load_restore_token(&token_path);

    // Ensure GNOME's dconf has the shortcut trigger before creating the session
    if let Err(e) = shortcuts::write_shortcut_dconf(&config.shortcut) {
        tracing::warn!("Failed to write shortcut to dconf (non-GNOME?): {e}");
    }

    // Create portal sessions
    let shortcuts = ShortcutController::new(connection.clone(), &config.shortcut).await?;
    tracing::info!("GlobalShortcuts session ready");

    // Try with restore token first; if that fails, retry without it
    let desktop = match DesktopController::new(connection.clone(), restore_token.as_deref()).await {
        Ok(d) => d,
        Err(e) if restore_token.is_some() => {
            tracing::warn!("RemoteDesktop with restore token failed ({e}), retrying without token");
            // Delete the stale token
            let _ = std::fs::remove_file(&token_path);
            DesktopController::new(connection.clone(), None).await?
        }
        Err(e) => return Err(e),
    };
    tracing::info!("RemoteDesktop session ready");

    // Save new restore token if provided
    if let Some(token) = desktop.restore_token() {
        if let Err(e) = persistence::save_restore_token(&token_path, token) {
            tracing::warn!("Failed to save restore token: {e}");
        }
    }

    let desktop = Arc::new(desktop);
    let recorder = Recorder::new(&config.audio);
    let transcriber = Transcriber::from_config(&config.transcriber);

    // State management channel
    let (state_tx, mut state_rx) = mpsc::channel::<Event>(32);

    // Injector with its own background task
    let injector = Injector::new(desktop.clone(), state_tx.clone());

    // Signal streams
    let mut activated = shortcuts.activated_stream().await?;
    let mut deactivated = shortcuts.deactivated_stream().await?;

    shared.set_portal_connected(true);
    DaemonInterface::notify_portal_connected(&connection).await;

    let mut current_state = State::Idle;
    update_state(current_state, shared, &connection).await;

    let mut recording_handle: Option<recorder::RecordingHandle> = None;
    let mut streaming_handle: Option<StreamingState> = None;

    let shortcut_id = config.shortcut.id.clone();

    // Toggle mode: press shortcut once to start, press again to stop.
    // GNOME sends Activated every ~30ms as key repeat while held.
    // A gap above REPEAT_THRESHOLD between consecutive Activated signals
    // indicates a new intentional press rather than key repeat.
    const REPEAT_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(100);
    let mut last_activated = std::time::Instant::now();

    loop {
        tokio::select! {
            // Shortcut activated (pressed or repeat)
            Some(signal) = activated.next() => {
                tracing::debug!("Activated signal received: shortcut_id={:?}", signal.shortcut_id());
                if signal.shortcut_id() != shortcut_id {
                    continue;
                }

                if current_state == State::Recording || current_state == State::Streaming {
                    let now = std::time::Instant::now();
                    let gap = now.duration_since(last_activated);
                    last_activated = now;
                    if gap <= REPEAT_THRESHOLD {
                        continue; // key repeat, ignore
                    }
                    // New press detected → stop
                    if current_state == State::Recording {
                        stop_recording(
                            &mut current_state,
                            &mut recording_handle,
                            &transcriber,
                            &injector,
                            shared,
                            &connection,
                        ).await;
                    } else {
                        stop_streaming(&mut current_state, &mut streaming_handle, shared, &connection).await;
                    }
                    continue;
                }

                match current_state.transition(&Event::Activated) {
                    Some(new_state) => {
                        last_activated = std::time::Instant::now();

                        if transcriber.is_streaming() {
                            // Streaming flow: start audio + WebSocket session
                            match recorder.start_streaming() {
                                Ok(mut handle) => {
                                    let audio_rx = handle.take_rx().expect("rx already taken");
                                    let (stop_tx, stop_rx) = oneshot::channel();
                                    let task = tokio::spawn({
                                        let rt_config = config.transcriber.mistral_realtime.clone();
                                        let sample_rate = config.audio.sample_rate;
                                        let desktop = desktop.clone();
                                        let state_tx = state_tx.clone();
                                        let shared = shared.clone();
                                        let connection = connection.clone();
                                        async move {
                                            if let Err(e) = streaming::run_streaming_session(
                                                &rt_config,
                                                sample_rate,
                                                audio_rx,
                                                desktop,
                                                state_tx.clone(),
                                                stop_rx,
                                                shared.clone(),
                                                connection.clone(),
                                            ).await {
                                                tracing::error!("Streaming session error: {e}");
                                                shared.set_last_error(format!("Streaming error: {e}"));
                                                DaemonInterface::notify_last_error(&connection).await;
                                                let _ = state_tx.send(Event::InjectionDone).await;
                                            }
                                        }
                                    });
                                    streaming_handle = Some(StreamingState {
                                        recording: handle,
                                        stop_tx: Some(stop_tx),
                                        task,
                                    });
                                    current_state = State::Streaming;
                                    shared.set_last_error(String::new());
                                    DaemonInterface::notify_last_error(&connection).await;
                                    update_state(current_state, shared, &connection).await;
                                }
                                Err(e) => {
                                    tracing::error!("Failed to start streaming recording: {e}");
                                    shared.set_last_error(format!("Failed to start streaming: {e}"));
                                    DaemonInterface::notify_last_error(&connection).await;
                                    current_state = State::Idle;
                                    update_state(current_state, shared, &connection).await;
                                }
                            }
                        } else {
                            // Batch flow: start recording to WAV
                            current_state = new_state;
                            update_state(current_state, shared, &connection).await;

                            match recorder.start() {
                                Ok(handle) => {
                                    recording_handle = Some(handle);
                                    shared.set_last_error(String::new());
                                    DaemonInterface::notify_last_error(&connection).await;
                                }
                                Err(e) => {
                                    tracing::error!("Failed to start recording: {e}");
                                    shared.set_last_error(format!("Failed to start recording: {e}"));
                                    DaemonInterface::notify_last_error(&connection).await;
                                    current_state = State::Idle;
                                    update_state(current_state, shared, &connection).await;
                                }
                            }
                        }
                    }
                    None => {
                        tracing::debug!("Ignoring Activated in state {current_state}");
                    }
                }
            }

            // Shortcut deactivated (released) — ignored in toggle mode, must drain the stream
            Some(_signal) = deactivated.next() => {}

            // State machine events from injector or streaming session
            Some(event) = state_rx.recv() => {
                if let Some(new_state) = current_state.transition(&event) {
                    if new_state == State::Idle && streaming_handle.is_some() {
                        streaming_handle = None;
                    }
                    current_state = new_state;
                    update_state(current_state, shared, &connection).await;
                }
            }

            // Session restart requested (e.g. shortcut changed via GUI)
            _ = shared.session_restart_requested() => {
                tracing::info!("Session restart requested");
                return Ok(SessionOutcome::Restart);
            }
        }
    }
}

/// Holds state for an active streaming session.
struct StreamingState {
    recording: recorder::StreamingRecordingHandle,
    stop_tx: Option<oneshot::Sender<()>>,
    #[allow(dead_code)]
    task: JoinHandle<()>,
}

/// Stop recording, transcribe, and enqueue for injection.
async fn stop_recording(
    current_state: &mut State,
    recording_handle: &mut Option<recorder::RecordingHandle>,
    transcriber: &Transcriber,
    injector: &Injector,
    shared: &SharedState,
    connection: &zbus::Connection,
) {
    *current_state = State::Transcribing;
    update_state(*current_state, shared, connection).await;

    if let Some(handle) = recording_handle.take() {
        match handle.stop() {
            Ok(audio_path) => {
                match transcriber.transcribe(&audio_path).await {
                    Ok(transcript) => {
                        if transcript.is_empty() {
                            tracing::info!("Empty transcript, returning to idle");
                            *current_state = State::Idle;
                            update_state(*current_state, shared, connection).await;
                        } else {
                            shared.set_last_transcript(transcript.clone());
                            DaemonInterface::notify_last_transcript(connection).await;
                            if let Err(e) = injector.enqueue(transcript).await {
                                tracing::error!("Failed to enqueue text: {e}");
                                shared.set_last_error(format!("Failed to enqueue text: {e}"));
                                DaemonInterface::notify_last_error(connection).await;
                                *current_state = State::Idle;
                                update_state(*current_state, shared, connection).await;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Transcription failed: {e}");
                        shared.set_last_error(format!("Transcription failed: {e}"));
                        DaemonInterface::notify_last_error(connection).await;
                        let _ = std::fs::remove_file(&audio_path);
                        *current_state = State::Idle;
                        update_state(*current_state, shared, connection).await;
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to stop recording: {e}");
                shared.set_last_error(format!("Failed to stop recording: {e}"));
                DaemonInterface::notify_last_error(connection).await;
                *current_state = State::Idle;
                update_state(*current_state, shared, connection).await;
            }
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

    if let Some(mut handle) = streaming_handle.take() {
        handle.recording.stop();
        if let Some(stop_tx) = handle.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        // The streaming task will send InjectionDone when transcription.done arrives
    }
}

/// Log state change, update shared D-Bus state, and emit PropertiesChanged.
async fn update_state(state: State, shared: &SharedState, connection: &zbus::Connection) {
    shared.set_state(state);
    eprintln!("STATE: {state}");
    DaemonInterface::notify_state(connection).await;
}
