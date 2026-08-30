// ABOUTME: Connects to the voxkey daemon over session D-Bus.
// ABOUTME: Reads properties, calls methods, and forwards state changes to the GTK main loop.

use std::sync::mpsc;

use futures_util::StreamExt;
use voxkey_ipc::DaemonProxy;

const SERVICE_UNIT: &str = "voxkey.service";
const SERVICE_OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const DAEMON_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
// Secret Service may display an unlock prompt. Keep it bounded, but do not let
// the ordinary five-second D-Bus timeout report failure while the daemon is
// still legitimately completing the keyring operation.
const KEYRING_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(35);
// Endpoint checks include DNS, TLS, and a bounded network probe. Keep their
// D-Bus method timeout above the daemon's own eight-second probe deadline.
const ENDPOINT_CHECK_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);
const UPDATE_QUEUE_CAPACITY: usize = 256;
const COMMAND_QUEUE_CAPACITY: usize = 32;
const LIFECYCLE_QUEUE_CAPACITY: usize = 4;

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
trait SystemdManager {
    fn start_unit(&self, name: &str, mode: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

    fn restart_unit(&self, name: &str, mode: &str)
    -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

    fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

    fn unmask_unit_files(
        &self,
        files: Vec<&str>,
        runtime: bool,
    ) -> zbus::Result<Vec<(String, String, String)>>;

    fn reload(&self) -> zbus::Result<()>;
}

fn models_dir_from(
    xdg_data_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> std::path::PathBuf {
    let data_dir = xdg_data_home
        .filter(|path| !path.is_empty() && std::path::Path::new(path).is_absolute())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            home.map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from("~"))
                .join(".local/share")
        });
    data_dir.join("voxkey").join("models")
}

/// Complete daemon state captured when the settings client connects.
#[derive(Debug)]
pub struct DaemonSnapshot {
    pub state: String,
    pub shortcut_trigger: String,
    pub shortcut_description: String,
    pub transcriber_config: String,
    pub injection_config: String,
    pub preview_config: String,
    pub dictionary_config: String,
    pub transcription_history: String,
    pub audio_input_devices: String,
    pub audio_input_device: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub portal_connected: bool,
    pub last_transcript: String,
    pub last_error: String,
}

/// Messages sent from the D-Bus background thread to the GTK main loop.
#[derive(Debug)]
pub enum DaemonUpdate {
    Connected(Box<DaemonSnapshot>),
    Disconnected {
        message: String,
        can_unmask: bool,
    },
    ServiceStarting {
        unmasking: bool,
    },
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
    /// Reply to HasApiKey with whether a keyring entry exists for this service.
    ApiKeyStatus {
        service: String,
        present: bool,
        request_id: u64,
    },
    AudioDevices {
        devices_json: String,
        selected_device: String,
    },
    CommandFailed {
        operation: String,
        message: String,
    },
}

struct CommandRequest {
    command: DaemonCommand,
    completion: tokio::sync::oneshot::Sender<Result<CommandResponse, String>>,
}

type CommandResponse = Option<voxkey_ipc::EndpointCheckResult>;

/// Completion of a command submitted to the daemon background thread.
pub struct CommandCompletion {
    receiver: tokio::sync::oneshot::Receiver<Result<CommandResponse, String>>,
}

impl CommandCompletion {
    pub async fn wait(self) -> Result<(), String> {
        self.receiver
            .await
            .unwrap_or_else(|_| Err("Voxkey command channel closed before replying".to_string()))
            .map(|_| ())
    }

    pub async fn wait_endpoint_check(self) -> Result<voxkey_ipc::EndpointCheckResult, String> {
        self.receiver
            .await
            .unwrap_or_else(|_| Err("Voxkey command channel closed before replying".to_string()))?
            .ok_or_else(|| "Voxkey returned no endpoint-check result".to_string())
    }
}

/// Handle for sending commands to the daemon from the GTK thread.
#[derive(Clone)]
pub struct DaemonHandle {
    cmd_tx: tokio::sync::mpsc::Sender<CommandRequest>,
    lifecycle_tx: tokio::sync::mpsc::Sender<LifecycleCommand>,
}

enum LifecycleCommand {
    StartService { unmask: bool },
    Quit { ack: mpsc::Sender<()> },
}

enum ConnectionOutcome {
    ApplicationClosed,
    Retry(ServiceUnavailable),
}

/// Commands sent from the GTK thread to the D-Bus background thread.
pub enum DaemonCommand {
    CancelDictation,
    SetShortcut(String),
    SetTranscriberConfig(String),
    /// Persist a transcriber config after its endpoint passed a connectivity
    /// check. Failures are presented inline by the endpoint editor.
    SaveCheckedEndpoint(String),
    /// Check the selected network endpoint without persisting it.
    CheckEndpoint(String),
    SetInjectionConfig(String),
    SetPreviewConfig(String),
    SetDictionaryConfig(String),
    SetAudioInputDevice(String),
    RefreshAudioInputDevices,
    DeleteHistoryEntry(u64),
    ClearHistory,
    RetryHistoryEntry(u64),
    OpenRecordingFolder(String),
    ClearLastError,
    /// Store an API key in the system keyring for the named service.
    SetApiKey {
        service: String,
        key: String,
    },
    /// Remove the stored API key for the named service.
    ClearApiKey {
        service: String,
    },
    /// Ask whether an API key is stored for the named service. The key value
    /// itself is never sent to the GUI.
    HasApiKey {
        service: String,
        request_id: u64,
    },
    DownloadModel(String),
    DeleteModel(String),
    ModelStatus(String),
    OpenModelsDir,
    ReloadConfig,
    ClearRestoreToken,
}

impl DaemonCommand {
    fn operation(&self) -> &'static str {
        match self {
            Self::CancelDictation => "Cancel dictation",
            Self::SetShortcut(_) => "Update shortcut",
            Self::SetTranscriberConfig(_) => "Update transcription settings",
            Self::SaveCheckedEndpoint(_) => "Save server address",
            Self::CheckEndpoint(_) => "Check server",
            Self::SetInjectionConfig(_) => "Update typing settings",
            Self::SetPreviewConfig(_) => "Update live preview settings",
            Self::SetDictionaryConfig(_) => "Update dictionary",
            Self::SetAudioInputDevice(_) => "Select microphone",
            Self::RefreshAudioInputDevices => "Refresh microphones",
            Self::DeleteHistoryEntry(_) => "Delete history entry",
            Self::ClearHistory => "Clear history",
            Self::RetryHistoryEntry(_) => "Retry transcription",
            Self::OpenRecordingFolder(_) => "Open recording folder",
            Self::ClearLastError => "Dismiss error",
            Self::SetApiKey { .. } => "Save API key",
            Self::ClearApiKey { .. } => "Clear API key",
            Self::HasApiKey { .. } => "Check API key",
            Self::DownloadModel(_) => "Download model",
            Self::DeleteModel(_) => "Delete model",
            Self::ModelStatus(_) => "Check model status",
            Self::OpenModelsDir => "Open models folder",
            Self::ReloadConfig => "Reload configuration",
            Self::ClearRestoreToken => "Reset desktop permission",
        }
    }

    fn rollback_property(&self) -> Option<&'static str> {
        match self {
            Self::SetShortcut(_) => Some("shortcut_trigger"),
            Self::SetTranscriberConfig(_) | Self::SaveCheckedEndpoint(_) => {
                Some("transcriber_config")
            }
            Self::SetInjectionConfig(_) => Some("injection_config"),
            Self::SetPreviewConfig(_) => Some("preview_config"),
            Self::SetDictionaryConfig(_) => Some("dictionary_config"),
            Self::SetAudioInputDevice(_) => Some("audio_input_device"),
            _ => None,
        }
    }

    fn reports_failure_inline(&self) -> bool {
        matches!(
            self,
            Self::SetShortcut(_) | Self::CheckEndpoint(_) | Self::SaveCheckedEndpoint(_)
        )
    }
}

impl std::fmt::Debug for DaemonCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CancelDictation => write!(f, "CancelDictation"),
            Self::SetShortcut(s) => f.debug_tuple("SetShortcut").field(s).finish(),
            Self::SetTranscriberConfig(_) => f.write_str("SetTranscriberConfig(<redacted>)"),
            Self::SaveCheckedEndpoint(_) => f.write_str("SaveCheckedEndpoint(<redacted>)"),
            Self::CheckEndpoint(_) => f.write_str("CheckEndpoint(<redacted>)"),
            Self::SetInjectionConfig(s) => f.debug_tuple("SetInjectionConfig").field(s).finish(),
            Self::SetPreviewConfig(s) => f.debug_tuple("SetPreviewConfig").field(s).finish(),
            Self::SetDictionaryConfig(s) => f.debug_tuple("SetDictionaryConfig").field(s).finish(),
            Self::SetAudioInputDevice(s) => f.debug_tuple("SetAudioInputDevice").field(s).finish(),
            Self::RefreshAudioInputDevices => write!(f, "RefreshAudioInputDevices"),
            Self::DeleteHistoryEntry(id) => f.debug_tuple("DeleteHistoryEntry").field(id).finish(),
            Self::ClearHistory => write!(f, "ClearHistory"),
            Self::RetryHistoryEntry(id) => f.debug_tuple("RetryHistoryEntry").field(id).finish(),
            Self::OpenRecordingFolder(_) => write!(f, "OpenRecordingFolder(<path>)"),
            Self::ClearLastError => write!(f, "ClearLastError"),
            Self::SetApiKey { service, .. } => f
                .debug_struct("SetApiKey")
                .field("service", service)
                .finish_non_exhaustive(),
            Self::ClearApiKey { service } => f
                .debug_struct("ClearApiKey")
                .field("service", service)
                .finish(),
            Self::HasApiKey {
                service,
                request_id,
            } => f
                .debug_struct("HasApiKey")
                .field("service", service)
                .field("request_id", request_id)
                .finish(),
            Self::DownloadModel(s) => f.debug_tuple("DownloadModel").field(s).finish(),
            Self::DeleteModel(s) => f.debug_tuple("DeleteModel").field(s).finish(),
            Self::ModelStatus(s) => f.debug_tuple("ModelStatus").field(s).finish(),
            Self::OpenModelsDir => write!(f, "OpenModelsDir"),
            Self::ReloadConfig => write!(f, "ReloadConfig"),
            Self::ClearRestoreToken => write!(f, "ClearRestoreToken"),
        }
    }
}

impl DaemonHandle {
    pub fn send(&self, command: DaemonCommand) -> CommandCompletion {
        let (completion, receiver) = tokio::sync::oneshot::channel();
        if let Err(error) = self.cmd_tx.try_send(CommandRequest {
            command,
            completion,
        }) {
            let message = match &error {
                tokio::sync::mpsc::error::TrySendError::Full(_) => "Voxkey command queue is full",
                tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                    "Voxkey command channel is not available"
                }
            };
            let _ = error.into_inner().completion.send(Err(message.to_string()));
        }
        CommandCompletion { receiver }
    }

    pub fn start_service(&self, unmask: bool) {
        let _ = self
            .lifecycle_tx
            .try_send(LifecycleCommand::StartService { unmask });
    }

    /// Ask the daemon to quit and block until the D-Bus or systemd request has
    /// completed (or the two-second UI shutdown deadline expires).
    pub fn send_quit_and_wait(&self) {
        let (ack_tx, ack_rx) = mpsc::channel();
        let _ = self
            .lifecycle_tx
            .try_send(LifecycleCommand::Quit { ack: ack_tx });
        let _ = ack_rx.recv_timeout(std::time::Duration::from_secs(2));
    }
}

/// Spawn a background tokio runtime that connects to the daemon D-Bus interface.
/// Returns an mpsc Receiver for updates and a DaemonHandle for sending commands.
pub fn connect() -> (tokio::sync::mpsc::Receiver<DaemonUpdate>, DaemonHandle) {
    let (update_tx, update_rx) = tokio::sync::mpsc::channel(UPDATE_QUEUE_CAPACITY);
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<CommandRequest>(COMMAND_QUEUE_CAPACITY);
    let (lifecycle_tx, lifecycle_rx) =
        tokio::sync::mpsc::channel::<LifecycleCommand>(LIFECYCLE_QUEUE_CAPACITY);

    let handle = DaemonHandle {
        cmd_tx,
        lifecycle_tx,
    };

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime");

        rt.block_on(run_client(update_tx, cmd_rx, lifecycle_rx));
    });

    (update_rx, handle)
}

#[derive(Clone)]
struct ServiceUnavailable {
    message: String,
    can_unmask: bool,
}

impl ServiceUnavailable {
    fn waiting() -> Self {
        Self {
            message: "Waiting for Voxkey…".to_string(),
            can_unmask: false,
        }
    }

    fn stopped() -> Self {
        Self {
            message: "Voxkey stopped. Select Start Voxkey to run it again.".to_string(),
            can_unmask: false,
        }
    }

    fn from_start_error(error: &str) -> Self {
        let can_unmask = error.to_ascii_lowercase().contains("masked");
        let message = if can_unmask {
            "Voxkey was turned off. Select Allow and start to use dictation.".to_string()
        } else {
            "Voxkey couldn’t start. Select Start Voxkey to try again.".to_string()
        };
        Self {
            message,
            can_unmask,
        }
    }
}

async fn daemon_name_is_owned(connection: &zbus::Connection) -> Result<bool, String> {
    let dbus = zbus::fdo::DBusProxy::new(connection)
        .await
        .map_err(|error| error.to_string())?;
    let daemon_name =
        zbus::names::BusName::try_from(voxkey_ipc::BUS_NAME).map_err(|error| error.to_string())?;
    dbus.name_has_owner(daemon_name)
        .await
        .map_err(|error| error.to_string())
}

async fn start_service_inner(unmask: bool) -> Result<(), String> {
    let connection = zbus::Connection::session()
        .await
        .map_err(|error| error.to_string())?;
    let daemon_is_running = daemon_name_is_owned(&connection).await?;
    if daemon_is_running && !unmask {
        return Ok(());
    }

    let manager = SystemdManagerProxy::new(&connection)
        .await
        .map_err(|error| error.to_string())?;
    if unmask {
        manager
            .unmask_unit_files(vec![SERVICE_UNIT], false)
            .await
            .map_err(|error| error.to_string())?;
        manager.reload().await.map_err(|error| error.to_string())?;
    }
    if daemon_is_running {
        return Ok(());
    }
    manager
        .start_unit(SERVICE_UNIT, "replace")
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn start_service(
    update_tx: &tokio::sync::mpsc::Sender<DaemonUpdate>,
    unmask: bool,
) -> ServiceUnavailable {
    let _ = update_tx.try_send(DaemonUpdate::ServiceStarting { unmasking: unmask });
    match tokio::time::timeout(SERVICE_OPERATION_TIMEOUT, start_service_inner(unmask)).await {
        Ok(Ok(())) => ServiceUnavailable::waiting(),
        Ok(Err(error)) => {
            tracing::warn!("Could not start {SERVICE_UNIT}: {error}");
            ServiceUnavailable::from_start_error(&error)
        }
        Err(_) => {
            let error = format!(
                "systemd did not answer within {} seconds",
                SERVICE_OPERATION_TIMEOUT.as_secs()
            );
            tracing::warn!("Could not start {SERVICE_UNIT}: {error}");
            ServiceUnavailable::from_start_error(&error)
        }
    }
}

async fn restart_service_inner() -> Result<(), String> {
    let connection = zbus::Connection::session()
        .await
        .map_err(|error| error.to_string())?;
    let manager = SystemdManagerProxy::new(&connection)
        .await
        .map_err(|error| error.to_string())?;
    manager
        .restart_unit(SERVICE_UNIT, "replace")
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn restart_service(
    update_tx: &tokio::sync::mpsc::Sender<DaemonUpdate>,
) -> ServiceUnavailable {
    let _ = update_tx.try_send(DaemonUpdate::ServiceStarting { unmasking: false });
    match tokio::time::timeout(SERVICE_OPERATION_TIMEOUT, restart_service_inner()).await {
        Ok(Ok(())) => ServiceUnavailable::waiting(),
        Ok(Err(error)) => {
            tracing::warn!("Could not restart {SERVICE_UNIT}: {error}");
            ServiceUnavailable::from_start_error(&error)
        }
        Err(_) => {
            let error = format!(
                "systemd did not answer within {} seconds",
                SERVICE_OPERATION_TIMEOUT.as_secs()
            );
            tracing::warn!("Could not restart {SERVICE_UNIT}: {error}");
            ServiceUnavailable::from_start_error(&error)
        }
    }
}

fn is_unknown_method_error(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::MethodError(name, _, _) => {
            name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod"
        }
        zbus::Error::FDO(error) => {
            matches!(error.as_ref(), zbus::fdo::Error::UnknownMethod(_))
        }
        _ => false,
    }
}

async fn stop_service_inner() -> Result<(), String> {
    let connection = zbus::Connection::session()
        .await
        .map_err(|error| error.to_string())?;
    let manager = SystemdManagerProxy::new(&connection)
        .await
        .map_err(|error| error.to_string())?;
    manager
        .stop_unit(SERVICE_UNIT, "replace")
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn stop_service() {
    let timeout = std::time::Duration::from_millis(1500);
    match tokio::time::timeout(timeout, stop_service_inner()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!("Could not stop {SERVICE_UNIT}: {error}"),
        Err(_) => tracing::warn!("Timed out while stopping {SERVICE_UNIT}"),
    }
}

async fn run_client(
    update_tx: tokio::sync::mpsc::Sender<DaemonUpdate>,
    mut cmd_rx: tokio::sync::mpsc::Receiver<CommandRequest>,
    mut lifecycle_rx: tokio::sync::mpsc::Receiver<LifecycleCommand>,
) {
    let mut unavailable = start_service(&update_tx, false).await;
    let mut was_attached = false;
    let mut legacy_restart_attempted = false;

    loop {
        let mut attached_this_attempt = false;
        match try_connect(
            &update_tx,
            &mut cmd_rx,
            &mut lifecycle_rx,
            &mut attached_this_attempt,
            &mut legacy_restart_attempted,
        )
        .await
        {
            Ok(ConnectionOutcome::ApplicationClosed) => return,
            Ok(ConnectionOutcome::Retry(next_unavailable)) => {
                unavailable = next_unavailable;
                continue;
            }
            Err(error) => {
                tracing::warn!("Daemon connection failed: {error}");
                if attached_this_attempt || was_attached {
                    was_attached = true;
                    unavailable = ServiceUnavailable::stopped();
                }
                let _ = update_tx.try_send(DaemonUpdate::Disconnected {
                    message: unavailable.message.clone(),
                    can_unmask: unavailable.can_unmask,
                });
            }
        }

        if cmd_rx.is_closed() && lifecycle_rx.is_closed() {
            stop_service().await;
            return;
        }

        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
            command = lifecycle_rx.recv() => match command {
                Some(LifecycleCommand::StartService { unmask }) => {
                    unavailable = start_service(&update_tx, unmask).await;
                    legacy_restart_attempted = false;
                }
                Some(LifecycleCommand::Quit { ack }) => {
                    stop_service().await;
                    let _ = ack.send(());
                    return;
                }
                None => {
                    stop_service().await;
                    return;
                }
            }
        }
    }
}

async fn try_connect(
    update_tx: &tokio::sync::mpsc::Sender<DaemonUpdate>,
    cmd_rx: &mut tokio::sync::mpsc::Receiver<CommandRequest>,
    lifecycle_rx: &mut tokio::sync::mpsc::Receiver<LifecycleCommand>,
    attached: &mut bool,
    legacy_restart_attempted: &mut bool,
) -> Result<ConnectionOutcome, Box<dyn std::error::Error>> {
    let builder = zbus::connection::Builder::session()?.method_timeout(DAEMON_CALL_TIMEOUT);
    let connection = tokio::time::timeout(DAEMON_CALL_TIMEOUT, builder.build())
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Session bus connection timed out",
            )
        })??;
    let proxy = DaemonProxy::new(&connection).await?;
    let mut owner_stream = proxy.inner().receive_owner_changed().await?;

    // This is deliberately the first daemon method call. Once it succeeds, a
    // settings crash or SIGKILL is observed by the daemon independently of the
    // GUI process and triggers the same graceful shutdown path as Quit.
    match proxy.attach_settings().await {
        Ok(()) => *attached = true,
        Err(error) if is_unknown_method_error(&error) && !*legacy_restart_attempted => {
            // RPM upgrades do not forcibly restart a live user service. Hand
            // an older daemon off to the newly installed managed unit once,
            // so the user never has to discover and restart it manually.
            *legacy_restart_attempted = true;
            tracing::info!("Running daemon predates UI lifecycle support; restarting it");
            if let Err(quit_error) = proxy.quit().await {
                tracing::warn!("Could not ask the previous daemon to quit: {quit_error}");
            }
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(2), owner_stream.next()).await;
            return Ok(ConnectionOutcome::Retry(restart_service(update_tx).await));
        }
        Err(error) if is_unknown_method_error(&error) => {
            return Err(std::io::Error::other(
                "This Voxkey build still lacks UI lifecycle support after restart",
            )
            .into());
        }
        Err(error) => return Err(error.into()),
    }

    // Read initial state
    let state = proxy.state().await?;
    let shortcut_trigger = proxy.shortcut_trigger().await?;
    let shortcut_description = proxy
        .shortcut_description()
        .await
        .unwrap_or_else(|_| shortcut_trigger.clone());
    let transcriber_config = proxy.transcriber_config().await?;
    let injection_config = proxy.injection_config().await?;
    let preview_config = proxy.preview_config().await?;
    let dictionary_config = proxy.dictionary_config().await?;
    // These properties were added with the redesigned settings app. Defaults
    // keep the rest of the UI usable if an older daemon is still running
    // during a package upgrade; the service will expose them after restart.
    let transcription_history = proxy
        .transcription_history()
        .await
        .unwrap_or_else(|_| "[]".to_string());
    let audio_input_devices = proxy
        .audio_input_devices()
        .await
        .unwrap_or_else(|_| "[]".to_string());
    let audio_input_device = proxy.audio_input_device().await.unwrap_or_default();
    let sample_rate = proxy.sample_rate().await?;
    let channels = proxy.channels().await?;
    let portal_connected = proxy.portal_connected().await?;
    let last_transcript = proxy.last_transcript().await?;
    let last_error = proxy.last_error().await?;

    update_tx.try_send(DaemonUpdate::Connected(Box::new(DaemonSnapshot {
        state,
        shortcut_trigger,
        shortcut_description,
        transcriber_config,
        injection_config,
        preview_config,
        dictionary_config,
        transcription_history,
        audio_input_devices,
        audio_input_device,
        sample_rate,
        channels,
        portal_connected,
        last_transcript,
        last_error,
    })))?;

    // Subscribe to property change streams
    let mut state_stream = proxy.receive_state_changed().await;
    let mut transcript_stream = proxy.receive_last_transcript_changed().await;
    let mut portal_stream = proxy.receive_portal_connected_changed().await;
    let mut shortcut_stream = proxy.receive_shortcut_trigger_changed().await;
    let mut shortcut_description_stream = proxy.receive_shortcut_description_changed().await;
    let mut transcriber_stream = proxy.receive_transcriber_config_changed().await;
    let mut error_stream = proxy.receive_last_error_changed().await;
    let mut injection_stream = proxy.receive_injection_config_changed().await;
    let mut preview_stream = proxy.receive_preview_config_changed().await;
    let mut dictionary_stream = proxy.receive_dictionary_config_changed().await;
    let mut history_stream = proxy.receive_transcription_history_changed().await;
    let mut audio_input_stream = proxy.receive_audio_input_device_changed().await;
    let mut sample_rate_stream = proxy.receive_sample_rate_changed().await;
    let mut channels_stream = proxy.receive_channels_changed().await;
    let mut download_stream = proxy.receive_download_progress().await?;

    loop {
        tokio::select! {
            Some(change) = state_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.try_send(DaemonUpdate::StateChanged(val));
                }
            }
            Some(change) = transcript_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.try_send(DaemonUpdate::PropertyChanged {
                        name: "last_transcript".to_string(),
                        value: val,
                    });
                }
            }
            Some(change) = portal_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.try_send(DaemonUpdate::PropertyChanged {
                        name: "portal_connected".to_string(),
                        value: val.to_string(),
                    });
                }
            }
            Some(change) = shortcut_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.try_send(DaemonUpdate::PropertyChanged {
                        name: "shortcut_trigger".to_string(),
                        value: val,
                    });
                }
            }
            Some(change) = shortcut_description_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.try_send(DaemonUpdate::PropertyChanged {
                        name: "shortcut_description".to_string(),
                        value: val,
                    });
                }
            }
            Some(change) = transcriber_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.try_send(DaemonUpdate::PropertyChanged {
                        name: "transcriber_config".to_string(),
                        value: val,
                    });
                }
            }
            Some(change) = error_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.try_send(DaemonUpdate::PropertyChanged {
                        name: "last_error".to_string(),
                        value: val,
                    });
                }
            }
            Some(change) = injection_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.try_send(DaemonUpdate::PropertyChanged {
                        name: "injection_config".to_string(),
                        value: val,
                    });
                }
            }
            Some(change) = preview_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.try_send(DaemonUpdate::PropertyChanged {
                        name: "preview_config".to_string(),
                        value: val,
                    });
                }
            }
            Some(change) = dictionary_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.try_send(DaemonUpdate::PropertyChanged {
                        name: "dictionary_config".to_string(),
                        value: val,
                    });
                }
            }
            Some(change) = history_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.try_send(DaemonUpdate::PropertyChanged {
                        name: "transcription_history".to_string(),
                        value: val,
                    });
                }
            }
            Some(change) = audio_input_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.try_send(DaemonUpdate::PropertyChanged {
                        name: "audio_input_device".to_string(),
                        value: val,
                    });
                }
            }
            Some(change) = sample_rate_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.try_send(DaemonUpdate::PropertyChanged {
                        name: "sample_rate".to_string(),
                        value: val.to_string(),
                    });
                }
            }
            Some(change) = channels_stream.next() => {
                if let Ok(val) = change.get().await {
                    let _ = update_tx.try_send(DaemonUpdate::PropertyChanged {
                        name: "channels".to_string(),
                        value: val.to_string(),
                    });
                }
            }
            Some(signal) = download_stream.next() => {
                if let Ok(args) = signal.args() {
                    let _ = update_tx.try_send(DaemonUpdate::DownloadProgress {
                        model_name: args.model_name.to_string(),
                        percent: args.percent,
                    });
                }
            }
            command = cmd_rx.recv() => {
                match command {
                    Some(request) => {
                        let operation = request.command.operation().to_string();
                        let rollback_property = request.command.rollback_property();
                        let reports_failure_inline = request.command.reports_failure_inline();
                        let command = handle_command(&proxy, update_tx, request.command);
                        tokio::pin!(command);
                        let result = loop {
                            tokio::select! {
                                result = &mut command => {
                                    break result.map_err(|error| error.to_string());
                                }
                                lifecycle = lifecycle_rx.recv() => {
                                    match lifecycle {
                                        Some(LifecycleCommand::StartService { .. }) => {}
                                        Some(LifecycleCommand::Quit { ack }) => {
                                            if let Err(error) = proxy.quit().await {
                                                tracing::warn!(
                                                    "Failed to send quit to daemon: {error}"
                                                );
                                                stop_service().await;
                                            }
                                            let _ = ack.send(());
                                            return Ok(ConnectionOutcome::ApplicationClosed);
                                        }
                                        None => {
                                            let _ = proxy.quit().await;
                                            return Ok(ConnectionOutcome::ApplicationClosed);
                                        }
                                    }
                                }
                                _ = owner_stream.next() => {
                                    return Err("Voxkey daemon D-Bus owner changed".into());
                                }
                            }
                        };
                        if let Err(message) = &result {
                            if let Some(property) = rollback_property {
                                publish_authoritative_property(&proxy, update_tx, property).await;
                            }
                            tracing::error!("{operation} failed: {message}");
                            if !reports_failure_inline {
                                let _ = update_tx.try_send(DaemonUpdate::CommandFailed {
                                    operation,
                                    message: message.clone(),
                                });
                            }
                        }
                        let _ = request.completion.send(result);
                    }
                    None => {
                        let _ = proxy.quit().await;
                        return Ok(ConnectionOutcome::ApplicationClosed);
                    }
                }
            }
            command = lifecycle_rx.recv() => {
                match command {
                    Some(LifecycleCommand::StartService { .. }) => {}
                    Some(LifecycleCommand::Quit { ack }) => {
                        if let Err(error) = proxy.quit().await {
                            tracing::warn!("Failed to send quit to daemon: {error}");
                            stop_service().await;
                        }
                        let _ = ack.send(());
                        return Ok(ConnectionOutcome::ApplicationClosed);
                    }
                    None => {
                        let _ = proxy.quit().await;
                        return Ok(ConnectionOutcome::ApplicationClosed);
                    }
                }
            }
            _ = owner_stream.next() => {
                return Err("Voxkey daemon D-Bus owner changed".into());
            }
        }
    }
}

async fn publish_authoritative_property(
    proxy: &DaemonProxy<'_>,
    update_tx: &tokio::sync::mpsc::Sender<DaemonUpdate>,
    property: &str,
) {
    let value = match property {
        "shortcut_trigger" => proxy.shortcut_trigger().await,
        "transcriber_config" => proxy.transcriber_config().await,
        "injection_config" => proxy.injection_config().await,
        "preview_config" => proxy.preview_config().await,
        "dictionary_config" => proxy.dictionary_config().await,
        "audio_input_device" => proxy.audio_input_device().await,
        _ => return,
    };
    match value {
        Ok(value) => {
            let _ = update_tx.try_send(DaemonUpdate::PropertyChanged {
                name: property.to_string(),
                value,
            });
        }
        Err(error) => tracing::warn!(
            "Could not refresh authoritative {property} after a rejected update: {error}"
        ),
    }
}

async fn handle_command(
    proxy: &DaemonProxy<'_>,
    update_tx: &tokio::sync::mpsc::Sender<DaemonUpdate>,
    cmd: DaemonCommand,
) -> Result<CommandResponse, Box<dyn std::error::Error>> {
    match cmd {
        DaemonCommand::CancelDictation => {
            proxy.cancel_dictation().await?;
        }
        DaemonCommand::SetShortcut(trigger) => {
            proxy.set_shortcut(&trigger).await?;
        }
        DaemonCommand::SetTranscriberConfig(config_json) => {
            proxy.set_transcriber_config(&config_json).await?;
        }
        DaemonCommand::SaveCheckedEndpoint(config_json) => {
            proxy.set_transcriber_config(&config_json).await?;
        }
        DaemonCommand::CheckEndpoint(config_json) => {
            let connection = endpoint_check_connection().await?;
            let check_proxy = DaemonProxy::new(&connection).await?;
            let result_json = check_proxy.check_transcriber_endpoint(&config_json).await?;
            let result = serde_json::from_str::<voxkey_ipc::EndpointCheckResult>(&result_json)?;
            return Ok(Some(result));
        }
        DaemonCommand::SetInjectionConfig(config_json) => {
            proxy.set_injection_config(&config_json).await?;
        }
        DaemonCommand::SetPreviewConfig(config_json) => {
            proxy.set_preview_config(&config_json).await?;
        }
        DaemonCommand::SetDictionaryConfig(config_json) => {
            proxy.set_dictionary_config(&config_json).await?;
        }
        DaemonCommand::SetAudioInputDevice(device_name) => {
            proxy.set_audio_input_device(&device_name).await?;
        }
        DaemonCommand::RefreshAudioInputDevices => {
            let devices_json = proxy.audio_input_devices().await?;
            let selected_device = proxy.audio_input_device().await?;
            let _ = update_tx.try_send(DaemonUpdate::AudioDevices {
                devices_json,
                selected_device,
            });
        }
        DaemonCommand::DeleteHistoryEntry(id) => {
            proxy.delete_history_entry(id).await?;
        }
        DaemonCommand::ClearHistory => {
            proxy.clear_transcription_history().await?;
        }
        DaemonCommand::RetryHistoryEntry(id) => {
            proxy.retry_history_entry(id).await?;
        }
        DaemonCommand::OpenRecordingFolder(path) => {
            let path = std::path::PathBuf::from(path);
            if !path.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "The saved recording is no longer available",
                )
                .into());
            }
            let folder = path.parent().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "The saved recording has no containing folder",
                )
            })?;
            tokio::process::Command::new("xdg-open")
                .arg(folder)
                .spawn()?;
        }
        DaemonCommand::ClearLastError => {
            proxy.clear_last_error().await?;
        }
        DaemonCommand::SetApiKey { service, key } => {
            let connection = keyring_connection().await?;
            DaemonProxy::new(&connection)
                .await?
                .set_api_key(&service, &key)
                .await?;
        }
        DaemonCommand::ClearApiKey { service } => {
            let connection = keyring_connection().await?;
            DaemonProxy::new(&connection)
                .await?
                .clear_api_key(&service)
                .await?;
        }
        DaemonCommand::HasApiKey {
            service,
            request_id,
        } => {
            let connection = keyring_connection().await?;
            let present = DaemonProxy::new(&connection)
                .await?
                .has_api_key(&service)
                .await?;
            let _ = update_tx.try_send(DaemonUpdate::ApiKeyStatus {
                service,
                present,
                request_id,
            });
        }
        DaemonCommand::DownloadModel(name) => {
            proxy.download_model(&name).await?;
        }
        DaemonCommand::DeleteModel(name) => {
            proxy.delete_model(&name).await?;
        }
        DaemonCommand::ModelStatus(name) => {
            let status = proxy.model_status(&name).await?;
            let _ = update_tx.try_send(DaemonUpdate::ModelStatusResult {
                model_name: name,
                status,
            });
        }
        DaemonCommand::OpenModelsDir => {
            let models_dir = models_dir_from(
                std::env::var_os("XDG_DATA_HOME").as_deref(),
                std::env::var_os("HOME").as_deref(),
            );
            std::fs::create_dir_all(&models_dir)?;
            tokio::process::Command::new("xdg-open")
                .arg(&models_dir)
                .spawn()?;
        }
        DaemonCommand::ReloadConfig => {
            proxy.reload_config().await?;
        }
        DaemonCommand::ClearRestoreToken => {
            proxy.clear_restore_token().await?;
        }
    }
    Ok(None)
}

async fn keyring_connection() -> Result<zbus::Connection, Box<dyn std::error::Error>> {
    let builder = zbus::connection::Builder::session()?.method_timeout(KEYRING_CALL_TIMEOUT);
    Ok(tokio::time::timeout(DAEMON_CALL_TIMEOUT, builder.build())
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Session bus connection for keyring operation timed out",
            )
        })??)
}

async fn endpoint_check_connection() -> Result<zbus::Connection, Box<dyn std::error::Error>> {
    let builder = zbus::connection::Builder::session()?.method_timeout(ENDPOINT_CHECK_CALL_TIMEOUT);
    Ok(tokio::time::timeout(DAEMON_CALL_TIMEOUT, builder.build())
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Session bus connection for endpoint check timed out",
            )
        })??)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn daemon_handle_sends_without_a_polling_timer() {
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(1);
        let (lifecycle_tx, _lifecycle_rx) = tokio::sync::mpsc::channel(1);
        let handle = DaemonHandle {
            cmd_tx,
            lifecycle_tx,
        };

        let completion = handle.send(DaemonCommand::ReloadConfig);
        let request = cmd_rx.recv().await.expect("command must be queued");
        assert!(matches!(request.command, DaemonCommand::ReloadConfig));
        request.completion.send(Ok(None)).unwrap();
        completion.wait().await.unwrap();
    }

    #[tokio::test]
    async fn endpoint_check_completion_returns_the_structured_result() {
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(1);
        let (lifecycle_tx, _lifecycle_rx) = tokio::sync::mpsc::channel(1);
        let handle = DaemonHandle {
            cmd_tx,
            lifecycle_tx,
        };

        let completion = handle.send(DaemonCommand::CheckEndpoint("{}".to_string()));
        let request = cmd_rx.recv().await.expect("command must be queued");
        let expected = voxkey_ipc::EndpointCheckResult::reachable("Server responded in 12 ms.");
        request.completion.send(Ok(Some(expected.clone()))).unwrap();

        assert_eq!(completion.wait_endpoint_check().await.unwrap(), expected);
    }

    #[tokio::test]
    async fn daemon_handle_reports_a_closed_command_channel() {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(1);
        let (lifecycle_tx, _lifecycle_rx) = tokio::sync::mpsc::channel(1);
        let handle = DaemonHandle {
            cmd_tx,
            lifecycle_tx,
        };
        drop(cmd_rx);

        let error = handle
            .send(DaemonCommand::ReloadConfig)
            .wait()
            .await
            .unwrap_err();
        assert!(error.contains("not available"), "{error}");
    }

    #[tokio::test]
    async fn daemon_handle_reports_a_full_command_queue() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(1);
        let (lifecycle_tx, _lifecycle_rx) = tokio::sync::mpsc::channel(1);
        let handle = DaemonHandle {
            cmd_tx,
            lifecycle_tx,
        };

        let _first = handle.send(DaemonCommand::ReloadConfig);
        let error = handle
            .send(DaemonCommand::ReloadConfig)
            .wait()
            .await
            .unwrap_err();

        assert!(error.contains("queue is full"), "{error}");
    }

    #[test]
    fn masked_service_errors_offer_an_explicit_recovery_action() {
        let unavailable = ServiceUnavailable::from_start_error(
            "org.freedesktop.systemd1.UnitMasked: Unit voxkey.service is masked.",
        );

        assert!(unavailable.can_unmask);
        assert!(unavailable.message.contains("turned off"));
    }

    #[test]
    fn unrelated_start_errors_never_offer_to_override_a_mask() {
        let unavailable = ServiceUnavailable::from_start_error("Connection refused");

        assert!(!unavailable.can_unmask);
        assert_eq!(
            unavailable.message,
            "Voxkey couldn’t start. Select Start Voxkey to try again."
        );
        assert!(!unavailable.message.contains("Connection refused"));
    }

    #[test]
    fn only_unknown_method_requests_the_upgrade_handoff() {
        let unknown_method = zbus::Error::FDO(Box::new(zbus::fdo::Error::UnknownMethod(
            "AttachSettings is not available".to_string(),
        )));
        let disconnected = zbus::Error::FDO(Box::new(zbus::fdo::Error::Disconnected(
            "session bus closed".to_string(),
        )));

        assert!(is_unknown_method_error(&unknown_method));
        assert!(!is_unknown_method_error(&disconnected));
    }

    #[test]
    fn transcriber_config_command_debug_output_is_redacted() {
        let secret = "sk-legacy-key-never-log-this";
        let command = DaemonCommand::SetTranscriberConfig(format!(
            r#"{{"mistral":{{"api_key":"{secret}"}}}}"#
        ));

        let debug = format!("{command:?}");

        assert!(debug.contains("SetTranscriberConfig"));
        assert!(!debug.contains(secret));
        assert!(!debug.contains("api_key"));
    }

    #[test]
    fn endpoint_check_command_debug_output_is_redacted() {
        let secret_query = "private-query-value";
        let command = DaemonCommand::CheckEndpoint(format!(
            r#"{{"mistral":{{"endpoint":"https://example.test/?token={secret_query}"}}}}"#
        ));

        let debug = format!("{command:?}");

        assert_eq!(debug, "CheckEndpoint(<redacted>)");
        assert!(!debug.contains(secret_query));
    }

    #[test]
    fn rejected_preview_update_refreshes_the_authoritative_property() {
        let command = DaemonCommand::SetPreviewConfig(
            r#"{"mode":"always","strategy":"whole","interval_ms":1000,"max_audio_seconds":0}"#
                .to_string(),
        );

        assert_eq!(command.rollback_property(), Some("preview_config"));
        assert_eq!(command.operation(), "Update live preview settings");
    }

    #[test]
    fn blank_xdg_data_home_uses_the_home_directory_for_models() {
        assert_eq!(
            models_dir_from(
                Some(std::ffi::OsStr::new("")),
                Some(std::ffi::OsStr::new("/home/test")),
            ),
            std::path::PathBuf::from("/home/test/.local/share/voxkey/models")
        );
        assert_eq!(
            models_dir_from(
                Some(std::ffi::OsStr::new("relative-data")),
                Some(std::ffi::OsStr::new("/home/test")),
            ),
            std::path::PathBuf::from("/home/test/.local/share/voxkey/models")
        );
    }
}
