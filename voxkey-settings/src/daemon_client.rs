// ABOUTME: Connects to the voxkey daemon over session D-Bus.
// ABOUTME: Reads properties, calls methods, and forwards state changes to the GTK main loop.

use std::sync::mpsc;
use std::sync::Arc;

use futures_util::StreamExt;
use voxkey_ipc::DaemonProxy;

/// Messages sent from the D-Bus background thread to the GTK main loop.
#[derive(Debug)]
pub enum DaemonUpdate {
    Connected {
        state: String,
        shortcut_trigger: String,
        transcriber_config: String,
        injection_config: String,
        portal_connected: bool,
        last_transcript: String,
        last_error: String,
    },
    Disconnected,
    StateChanged(String),
    PropertyChanged {
        name: String,
        value: String,
    },
    DownloadProgress {
        model_name: String,
        percent: u8,
    },
    ModelStatusResult {
        model_name: String,
        status: String,
    },
}

/// Handle for sending commands to the daemon from the GTK thread.
#[derive(Clone)]
pub struct DaemonHandle {
    cmd_tx: Arc<std::sync::Mutex<mpsc::Sender<DaemonCommand>>>,
}

/// Commands sent from the GTK thread to the D-Bus background thread.
pub enum DaemonCommand {
    SetShortcut(String),
    SetTranscriberConfig(String),
    SetInjectionConfig(String),
    DownloadModel(String),
    DeleteModel(String),
    ModelStatus(String),
    OpenModelsDir,
    ReloadConfig,
    ClearRestoreToken,
    QuitDaemon { ack: mpsc::Sender<()> },
}

impl std::fmt::Debug for DaemonCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SetShortcut(s) => f.debug_tuple("SetShortcut").field(s).finish(),
            Self::SetTranscriberConfig(s) => f.debug_tuple("SetTranscriberConfig").field(s).finish(),
            Self::SetInjectionConfig(s) => f.debug_tuple("SetInjectionConfig").field(s).finish(),
            Self::DownloadModel(s) => f.debug_tuple("DownloadModel").field(s).finish(),
            Self::DeleteModel(s) => f.debug_tuple("DeleteModel").field(s).finish(),
            Self::ModelStatus(s) => f.debug_tuple("ModelStatus").field(s).finish(),
            Self::OpenModelsDir => write!(f, "OpenModelsDir"),
            Self::ReloadConfig => write!(f, "ReloadConfig"),
            Self::ClearRestoreToken => write!(f, "ClearRestoreToken"),
            Self::QuitDaemon { .. } => write!(f, "QuitDaemon"),
        }
    }
}

impl DaemonHandle {
    pub fn send(&self, cmd: DaemonCommand) {
        let _ = self.cmd_tx.lock().unwrap().send(cmd);
    }

    /// Send QuitDaemon and block until the D-Bus call completes (or 2s timeout).
    pub fn send_quit_and_wait(&self) {
        let (ack_tx, ack_rx) = mpsc::channel();
        let _ = self.cmd_tx.lock().unwrap().send(DaemonCommand::QuitDaemon { ack: ack_tx });
        let _ = ack_rx.recv_timeout(std::time::Duration::from_secs(2));
    }
}

/// Spawn a background tokio runtime that connects to the daemon D-Bus interface.
/// Returns an mpsc Receiver for updates and a DaemonHandle for sending commands.
pub fn connect() -> (mpsc::Receiver<DaemonUpdate>, DaemonHandle) {
    let (update_tx, update_rx) = mpsc::channel();
    let (cmd_tx, cmd_rx) = mpsc::channel::<DaemonCommand>();

    let handle = DaemonHandle {
        cmd_tx: Arc::new(std::sync::Mutex::new(cmd_tx)),
    };

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime");

        rt.block_on(run_client(update_tx, cmd_rx));
    });

    (update_rx, handle)
}

async fn run_client(
    update_tx: mpsc::Sender<DaemonUpdate>,
    cmd_rx: mpsc::Receiver<DaemonCommand>,
) {
    // Wrap cmd_rx so we can use it across iterations
    let cmd_rx = Arc::new(std::sync::Mutex::new(cmd_rx));

    loop {
        match try_connect(&update_tx, &cmd_rx).await {
            Ok(()) => return,
            Err(e) => {
                tracing::warn!("Daemon connection failed: {e}");
                let _ = update_tx.send(DaemonUpdate::Disconnected);
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
}

async fn try_connect(
    update_tx: &mpsc::Sender<DaemonUpdate>,
    cmd_rx: &Arc<std::sync::Mutex<mpsc::Receiver<DaemonCommand>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let connection = zbus::Connection::session().await?;
    let proxy = DaemonProxy::new(&connection).await?;

    // Read initial state
    let state = proxy.state().await?;
    let shortcut_trigger = proxy.shortcut_trigger().await?;
    let transcriber_config = proxy.transcriber_config().await?;
    let injection_config = proxy.injection_config().await?;
    let portal_connected = proxy.portal_connected().await?;
    let last_transcript = proxy.last_transcript().await?;
    let last_error = proxy.last_error().await?;

    update_tx.send(DaemonUpdate::Connected {
        state,
        shortcut_trigger,
        transcriber_config,
        injection_config,
        portal_connected,
        last_transcript,
        last_error,
    })?;

    // Subscribe to property change streams
    let mut state_stream = proxy.receive_state_changed().await;
    let mut transcript_stream = proxy.receive_last_transcript_changed().await;
    let mut portal_stream = proxy.receive_portal_connected_changed().await;
    let mut shortcut_stream = proxy.receive_shortcut_trigger_changed().await;
    let mut transcriber_stream = proxy.receive_transcriber_config_changed().await;
    let mut error_stream = proxy.receive_last_error_changed().await;
    let mut injection_stream = proxy.receive_injection_config_changed().await;
    let mut download_stream = proxy.receive_download_progress().await?;

    // Poll for commands periodically
    let mut cmd_interval = tokio::time::interval(std::time::Duration::from_millis(50));

    loop {
        tokio::select! {
            Some(change) = state_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.send(DaemonUpdate::StateChanged(val));
                }
            }
            Some(change) = transcript_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.send(DaemonUpdate::PropertyChanged {
                        name: "last_transcript".to_string(),
                        value: val,
                    });
                }
            }
            Some(change) = portal_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.send(DaemonUpdate::PropertyChanged {
                        name: "portal_connected".to_string(),
                        value: val.to_string(),
                    });
                }
            }
            Some(change) = shortcut_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.send(DaemonUpdate::PropertyChanged {
                        name: "shortcut_trigger".to_string(),
                        value: val,
                    });
                }
            }
            Some(change) = transcriber_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.send(DaemonUpdate::PropertyChanged {
                        name: "transcriber_config".to_string(),
                        value: val,
                    });
                }
            }
            Some(change) = error_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.send(DaemonUpdate::PropertyChanged {
                        name: "last_error".to_string(),
                        value: val,
                    });
                }
            }
            Some(change) = injection_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.send(DaemonUpdate::PropertyChanged {
                        name: "injection_config".to_string(),
                        value: val,
                    });
                }
            }
            Some(signal) = download_stream.next() => {
                if let Ok(args) = signal.args() {
                    let _ = update_tx.send(DaemonUpdate::DownloadProgress {
                        model_name: args.model_name.to_string(),
                        percent: args.percent,
                    });
                }
            }
            _ = cmd_interval.tick() => {
                // Drain all pending commands
                let rx = cmd_rx.lock().unwrap();
                while let Ok(cmd) = rx.try_recv() {
                    if let Err(e) = handle_command(&proxy, update_tx, cmd).await {
                        tracing::error!("D-Bus command failed: {e}");
                    }
                }
            }
        }
    }
}

async fn handle_command(
    proxy: &DaemonProxy<'_>,
    update_tx: &mpsc::Sender<DaemonUpdate>,
    cmd: DaemonCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        DaemonCommand::SetShortcut(trigger) => {
            proxy.set_shortcut(&trigger).await?;
        }
        DaemonCommand::SetTranscriberConfig(config_json) => {
            proxy.set_transcriber_config(&config_json).await?;
        }
        DaemonCommand::SetInjectionConfig(config_json) => {
            proxy.set_injection_config(&config_json).await?;
        }
        DaemonCommand::DownloadModel(name) => {
            proxy.download_model(&name).await?;
        }
        DaemonCommand::DeleteModel(name) => {
            proxy.delete_model(&name).await?;
        }
        DaemonCommand::ModelStatus(name) => {
            let status = proxy.model_status(&name).await?;
            let _ = update_tx.send(DaemonUpdate::ModelStatusResult {
                model_name: name,
                status,
            });
        }
        DaemonCommand::OpenModelsDir => {
            let data_dir = std::env::var("XDG_DATA_HOME")
                .unwrap_or_else(|_| {
                    let home = std::env::var("HOME").unwrap_or_else(|_| "~".to_string());
                    format!("{home}/.local/share")
                });
            let models_dir = format!("{data_dir}/voxkey/models");
            let _ = std::fs::create_dir_all(&models_dir);
            let _ = tokio::process::Command::new("xdg-open")
                .arg(&models_dir)
                .spawn();
        }
        DaemonCommand::ReloadConfig => {
            proxy.reload_config().await?;
        }
        DaemonCommand::ClearRestoreToken => {
            proxy.clear_restore_token().await?;
        }
        DaemonCommand::QuitDaemon { ack } => {
            if let Err(e) = proxy.quit().await {
                tracing::warn!("Failed to send quit to daemon: {e}");
            }
            let _ = ack.send(());
        }
    }
    Ok(())
}
