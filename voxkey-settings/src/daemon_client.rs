// ABOUTME: Connects to the voxkey daemon over session D-Bus.
// ABOUTME: Reads properties, calls methods, and forwards state changes to the GTK main loop.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, mpsc};

use futures_util::{StreamExt, stream::FuturesUnordered};
use voxkey_ipc::DaemonProxy;

const SERVICE_UNIT: &str = "voxkey.service";
const SERVICE_OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const DAEMON_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
// A first integrity check may hash a model artifact larger than 650 MB. Give
// that background-only lane enough time for slow disks without weakening the
// short deadline used by interactive daemon commands.
const MODEL_STATUS_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
// Secret Service may display an unlock prompt. Keep it bounded, but do not let
// the ordinary five-second D-Bus timeout report failure while the daemon is
// still legitimately completing the keyring operation.
const KEYRING_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(35);
// Endpoint checks include DNS, TLS, and a bounded network probe. Keep their
// D-Bus method timeout above the daemon's own eight-second probe deadline.
const ENDPOINT_CHECK_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);
const ENDPOINT_CHECK_CONCURRENCY: usize = 2;
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
    ModelDownloadVerifying {
        model_name: String,
    },
    ModelDownloadFinished {
        model_name: String,
        outcome: String,
        message: String,
    },
    ModelStatusResult {
        model_name: String,
        status: String,
    },
    ModelStatusFailed {
        model_name: String,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeferredCommandLane {
    Endpoint,
    Keyring,
}

impl DeferredCommandLane {
    fn concurrency(self) -> usize {
        match self {
            Self::Endpoint => ENDPOINT_CHECK_CONCURRENCY,
            // Reads and writes share one FIFO lane so a status query can never
            // overtake a key the user just saved or removed.
            Self::Keyring => 1,
        }
    }
}

/// Keeps potentially interactive or network-bound calls away from the main
/// daemon command loop while placing a strict bound on their resource use.
#[derive(Default)]
struct DeferredCommandScheduler {
    endpoint_active: usize,
    endpoint_waiting: VecDeque<CommandRequest>,
    keyring_active: usize,
    keyring_waiting: VecDeque<CommandRequest>,
}

impl DeferredCommandScheduler {
    fn lane_mut(
        &mut self,
        lane: DeferredCommandLane,
    ) -> (&mut usize, &mut VecDeque<CommandRequest>) {
        match lane {
            DeferredCommandLane::Endpoint => {
                (&mut self.endpoint_active, &mut self.endpoint_waiting)
            }
            DeferredCommandLane::Keyring => (&mut self.keyring_active, &mut self.keyring_waiting),
        }
    }

    /// Returns the request when it may start immediately; otherwise queues it
    /// in FIFO order for its lane.
    fn submit(&mut self, request: CommandRequest) -> Option<CommandRequest> {
        let lane = request
            .command
            .deferred_lane()
            .expect("only deferred commands may enter the deferred scheduler");
        if lane == DeferredCommandLane::Keyring {
            let service = match &request.command {
                // A newer read supersedes older reads for the same service.
                DaemonCommand::HasApiKey { service, .. } => Some(service.clone()),
                // A user-requested write makes every background status read
                // stale and should not wait behind one for another provider.
                DaemonCommand::SetApiKey { .. } | DaemonCommand::ClearApiKey { .. } => None,
                _ => unreachable!("every deferred keyring command must be classified"),
            };
            self.supersede_waiting_key_status(service.as_deref());
        }
        let limit = lane.concurrency();
        let (active, waiting) = self.lane_mut(lane);
        if *active < limit {
            *active += 1;
            Some(request)
        } else {
            waiting.push_back(request);
            None
        }
    }

    /// Releases a lane slot and returns its oldest queued request, if any. A
    /// replacement inherits the active slot so the counters stay exact.
    fn complete(&mut self, lane: DeferredCommandLane) -> Option<CommandRequest> {
        let (active, waiting) = self.lane_mut(lane);
        debug_assert!(*active > 0, "a deferred lane completed while idle");
        if let Some(next) = waiting.pop_front() {
            Some(next)
        } else {
            *active = (*active)
                .checked_sub(1)
                .expect("a deferred lane completed while idle");
            None
        }
    }

    /// A queued status read is already obsolete when any newer operation for
    /// that service arrives. Drop it instead of making a key save wait behind
    /// reads whose request IDs the UI will ignore.
    fn supersede_waiting_key_status(&mut self, service: Option<&str>) {
        let mut current = std::mem::take(&mut self.keyring_waiting);
        while let Some(request) = current.pop_front() {
            let superseded = matches!(
                &request.command,
                DaemonCommand::HasApiKey {
                    service: pending_service,
                    ..
                } if service.is_none_or(|service| pending_service == service)
            );
            if superseded {
                let _ = request.completion.send(Err(
                    "API key status check was superseded by a newer credential operation"
                        .to_string(),
                ));
            } else {
                self.keyring_waiting.push_back(request);
            }
        }
    }
}

struct DeferredCommandResult {
    lane: DeferredCommandLane,
    operation: &'static str,
    reports_failure_inline: bool,
    completion: tokio::sync::oneshot::Sender<Result<CommandResponse, String>>,
    result: Result<(CommandResponse, Option<DaemonUpdate>), String>,
}

type DeferredCommandTask =
    std::pin::Pin<Box<dyn std::future::Future<Output = DeferredCommandResult>>>;

struct PendingModelStatusRequest {
    request_id: u64,
    completions: Vec<tokio::sync::oneshot::Sender<Result<CommandResponse, String>>>,
}

#[derive(Default)]
struct PendingModelStatusRequests {
    requests: HashMap<String, PendingModelStatusRequest>,
    next_request_id: u64,
}

impl PendingModelStatusRequests {
    /// Register a status request. Returns an ID only when the caller must start
    /// a D-Bus query; duplicate requests share its eventual result.
    fn queue(
        &mut self,
        model_name: String,
        completion: tokio::sync::oneshot::Sender<Result<CommandResponse, String>>,
    ) -> Option<u64> {
        if let Some(pending) = self.requests.get_mut(&model_name) {
            pending.completions.push(completion);
            return None;
        }

        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        let request_id = self.next_request_id;
        self.requests.insert(
            model_name,
            PendingModelStatusRequest {
                request_id,
                completions: vec![completion],
            },
        );
        Some(request_id)
    }

    fn invalidate(&mut self, model_name: &str) {
        let Some(pending) = self.requests.remove(model_name) else {
            return;
        };
        let message = "Model status check was superseded by a newer model operation".to_string();
        for completion in pending.completions {
            let _ = completion.send(Err(message.clone()));
        }
    }

    fn finish(
        &mut self,
        request_id: u64,
        model_name: String,
        result: Result<String, String>,
        update_tx: &tokio::sync::mpsc::Sender<DaemonUpdate>,
    ) {
        let Some(pending) = self.requests.get(&model_name) else {
            tracing::debug!("Ignoring a superseded model status result for {model_name}");
            return;
        };
        if pending.request_id != request_id {
            tracing::debug!("Ignoring an older model status result for {model_name}");
            return;
        }
        let Some(pending) = self.requests.remove(&model_name) else {
            return;
        };
        let completions = pending.completions;

        match result {
            Ok(status) => {
                let _ = update_tx.try_send(DaemonUpdate::ModelStatusResult { model_name, status });
                for completion in completions {
                    let _ = completion.send(Ok(None));
                }
            }
            Err(message) => {
                tracing::error!("Check model status failed: {message}");
                let _ = update_tx.try_send(DaemonUpdate::ModelStatusFailed { model_name });
                for completion in completions {
                    let _ = completion.send(Err(message.clone()));
                }
            }
        }
    }
}

async fn connect_model_status_proxy() -> Result<DaemonProxy<'static>, String> {
    let builder = zbus::connection::Builder::session()
        .map_err(|error| error.to_string())?
        .method_timeout(MODEL_STATUS_CALL_TIMEOUT);
    let connection = tokio::time::timeout(DAEMON_CALL_TIMEOUT, builder.build())
        .await
        .map_err(|_| "Session bus connection for model checks timed out".to_string())?
        .map_err(|error| error.to_string())?;
    DaemonProxy::new(&connection)
        .await
        .map_err(|error| error.to_string())
}

async fn query_model_status(
    proxy: Arc<tokio::sync::OnceCell<DaemonProxy<'static>>>,
    gate: Arc<tokio::sync::Semaphore>,
    request_id: u64,
    model_name: String,
) -> (u64, String, Result<String, String>) {
    let result = async {
        let _permit = gate
            .acquire_owned()
            .await
            .map_err(|_| "Model status queue stopped before replying".to_string())?;
        let proxy = proxy.get_or_try_init(connect_model_status_proxy).await?;
        proxy
            .model_status(&model_name)
            .await
            .map_err(|error| error.to_string())
    }
    .await;
    (request_id, model_name, result)
}

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
    /// Dismiss only the error whose details the UI displayed.
    DismissLastError(String),
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
    CancelModelDownload(String),
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
            Self::DismissLastError(_) => "Dismiss error",
            Self::SetApiKey { .. } => "Save API key",
            Self::ClearApiKey { .. } => "Clear API key",
            Self::HasApiKey { .. } => "Check API key",
            Self::DownloadModel(_) => "Download model",
            Self::CancelModelDownload(_) => "Cancel model download",
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

    fn deferred_lane(&self) -> Option<DeferredCommandLane> {
        match self {
            Self::CheckEndpoint(_) => Some(DeferredCommandLane::Endpoint),
            Self::SetApiKey { .. } | Self::ClearApiKey { .. } | Self::HasApiKey { .. } => {
                Some(DeferredCommandLane::Keyring)
            }
            _ => None,
        }
    }

    fn supersedes_model_status(&self) -> Option<&str> {
        match self {
            Self::DownloadModel(model_name)
            | Self::CancelModelDownload(model_name)
            | Self::DeleteModel(model_name) => Some(model_name),
            _ => None,
        }
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
            Self::DismissLastError(_) => f.write_str("DismissLastError(<redacted>)"),
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
            Self::CancelModelDownload(s) => f.debug_tuple("CancelModelDownload").field(s).finish(),
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

fn is_unknown_property_error(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::MethodError(name, _, _) => {
            name.as_str() == "org.freedesktop.DBus.Error.UnknownProperty"
        }
        zbus::Error::FDO(error) => {
            matches!(error.as_ref(), zbus::fdo::Error::UnknownProperty(_))
        }
        _ => false,
    }
}

fn daemon_protocol_requires_upgrade(running_version: Option<u32>) -> bool {
    !running_version.is_some_and(|version| version >= voxkey_ipc::DAEMON_PROTOCOL_VERSION)
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
    let mut upgrade_restart_attempted = false;

    loop {
        let mut attached_this_attempt = false;
        match try_connect(
            &update_tx,
            &mut cmd_rx,
            &mut lifecycle_rx,
            &mut attached_this_attempt,
            &mut upgrade_restart_attempted,
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
                    upgrade_restart_attempted = false;
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
    upgrade_restart_attempted: &mut bool,
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
    let mut upgrade_reason = match proxy.attach_settings().await {
        Ok(()) => {
            *attached = true;
            None
        }
        Err(error) if is_unknown_method_error(&error) => {
            Some("predates Settings lifecycle support".to_string())
        }
        Err(error) => return Err(error.into()),
    };

    if upgrade_reason.is_none() {
        let running_protocol = match proxy.protocol_version().await {
            Ok(version) => Some(version),
            Err(error) if is_unknown_property_error(&error) => None,
            Err(error) => return Err(error.into()),
        };
        if daemon_protocol_requires_upgrade(running_protocol) {
            upgrade_reason = Some(match running_protocol {
                Some(version) => format!(
                    "uses protocol {version}, but Settings requires {}",
                    voxkey_ipc::DAEMON_PROTOCOL_VERSION
                ),
                None => "does not report a protocol version".to_string(),
            });
        }
    }

    if let Some(reason) = upgrade_reason {
        if *upgrade_restart_attempted {
            return Err(std::io::Error::other(format!(
                "The installed Voxkey daemon is still incompatible after restart: {reason}"
            ))
            .into());
        }

        // RPM upgrades deliberately do not restart active user services. Hand
        // the stale process off to the newly installed managed unit once so
        // new Settings never stays attached to an older interface silently.
        *upgrade_restart_attempted = true;
        tracing::info!("Running daemon {reason}; restarting it after the package upgrade");
        if let Err(quit_error) = proxy.quit().await {
            tracing::warn!("Could not ask the previous daemon to quit: {quit_error}");
        }
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), owner_stream.next()).await;
        return Ok(ConnectionOutcome::Retry(restart_service(update_tx).await));
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
    // Keep optional display-only properties tolerant for third-party clients
    // and forward compatibility. The protocol handshake above has already
    // replaced an older packaged daemon before authoritative state is read.
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
    // All model-transfer states arrive on one signal stream. Separate progress
    // and terminal streams can be selected out of bus order when both are
    // buffered, allowing stale progress to overwrite a terminal result.
    let mut model_download_stream = proxy.receive_model_download_changed().await?;
    let model_status_gate = Arc::new(tokio::sync::Semaphore::new(1));
    let model_status_proxy = Arc::new(tokio::sync::OnceCell::new());
    let mut model_status_tasks = FuturesUnordered::new();
    let mut pending_model_status = PendingModelStatusRequests::default();
    let mut deferred_command_tasks = FuturesUnordered::<DeferredCommandTask>::new();
    let mut deferred_command_scheduler = DeferredCommandScheduler::default();

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
            Some(signal) = model_download_stream.next() => {
                if let Ok(args) = signal.args() {
                    let model_name = args.model_name.to_string();
                    pending_model_status.invalidate(&model_name);
                    let state_wire = args.state.to_string();
                    let message = args.message.to_string();
                    match voxkey_ipc::ModelDownloadState::from_wire_value(&state_wire) {
                        Some(voxkey_ipc::ModelDownloadState::Downloading) => {
                            // Byte progress is intentionally lossy when GTK is
                            // behind; a later update supersedes it.
                            let _ = update_tx.try_send(DaemonUpdate::DownloadProgress {
                                model_name,
                                percent: args.percent,
                            });
                        }
                        Some(voxkey_ipc::ModelDownloadState::Verifying) => {
                            let _ = update_tx.send(DaemonUpdate::ModelDownloadVerifying {
                                model_name,
                            }).await;
                        }
                        Some(download_state) => {
                            let Some(outcome) = download_state.terminal_outcome() else {
                                tracing::warn!(
                                    "Ignoring an unhandled non-terminal model download state"
                                );
                                continue;
                            };
                            // Terminal results are not lossy. Waiting for
                            // capacity guarantees a row cannot remain busy.
                            let _ = update_tx.send(DaemonUpdate::ModelDownloadFinished {
                                model_name,
                                outcome: outcome.as_wire_value().to_string(),
                                message,
                            }).await;
                        }
                        None => {
                            // A future state triggers the same authoritative
                            // scan fallback as a future terminal outcome.
                            let _ = update_tx.send(DaemonUpdate::ModelDownloadFinished {
                                model_name,
                                outcome: state_wire,
                                message,
                            }).await;
                        }
                    }
                }
            }
            Some((request_id, model_name, result)) = model_status_tasks.next(),
                if !model_status_tasks.is_empty() =>
            {
                pending_model_status.finish(request_id, model_name, result, update_tx);
            }
            Some(outcome) = deferred_command_tasks.next(),
                if !deferred_command_tasks.is_empty() =>
            {
                let lane = outcome.lane;
                let next = deferred_command_scheduler.complete(lane);
                finish_deferred_command(outcome, update_tx);
                if let Some(next) = next {
                    deferred_command_tasks.push(Box::pin(run_deferred_command(next)));
                }
            }
            command = cmd_rx.recv() => {
                match command {
                    Some(CommandRequest {
                        command: DaemonCommand::ModelStatus(model_name),
                        completion,
                    }) => {
                        if let Some(request_id) =
                            pending_model_status.queue(model_name.clone(), completion)
                        {
                            model_status_tasks.push(query_model_status(
                                model_status_proxy.clone(),
                                model_status_gate.clone(),
                                request_id,
                                model_name,
                            ));
                        }
                    }
                    Some(request) if request.command.deferred_lane().is_some() => {
                        if let Some(request) = deferred_command_scheduler.submit(request) {
                            deferred_command_tasks.push(Box::pin(run_deferred_command(request)));
                        }
                    }
                    Some(request) => {
                        if let Some(model_name) = request.command.supersedes_model_status() {
                            pending_model_status.invalidate(model_name);
                        }
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

async fn run_deferred_command(request: CommandRequest) -> DeferredCommandResult {
    let lane = request
        .command
        .deferred_lane()
        .expect("only deferred commands may be started in a deferred lane");
    let operation = request.command.operation();
    let reports_failure_inline = request.command.reports_failure_inline();
    let result = handle_deferred_command(request.command)
        .await
        .map_err(|error| error.to_string());
    DeferredCommandResult {
        lane,
        operation,
        reports_failure_inline,
        completion: request.completion,
        result,
    }
}

async fn handle_deferred_command(
    command: DaemonCommand,
) -> Result<(CommandResponse, Option<DaemonUpdate>), Box<dyn std::error::Error>> {
    match command {
        DaemonCommand::CheckEndpoint(config_json) => {
            let connection = endpoint_check_connection().await?;
            let check_proxy = DaemonProxy::new(&connection).await?;
            let result_json = check_proxy.check_transcriber_endpoint(&config_json).await?;
            let result = serde_json::from_str::<voxkey_ipc::EndpointCheckResult>(&result_json)?;
            Ok((Some(result), None))
        }
        DaemonCommand::SetApiKey { service, key } => {
            let connection = keyring_connection().await?;
            DaemonProxy::new(&connection)
                .await?
                .set_api_key(&service, &key)
                .await?;
            Ok((None, None))
        }
        DaemonCommand::ClearApiKey { service } => {
            let connection = keyring_connection().await?;
            DaemonProxy::new(&connection)
                .await?
                .clear_api_key(&service)
                .await?;
            Ok((None, None))
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
            Ok((
                None,
                Some(DaemonUpdate::ApiKeyStatus {
                    service,
                    present,
                    request_id,
                }),
            ))
        }
        _ => unreachable!("ordinary commands cannot run in a deferred lane"),
    }
}

fn finish_deferred_command(
    outcome: DeferredCommandResult,
    update_tx: &tokio::sync::mpsc::Sender<DaemonUpdate>,
) {
    let result = match outcome.result {
        Ok((response, update)) => {
            if let Some(update) = update {
                let _ = update_tx.try_send(update);
            }
            Ok(response)
        }
        Err(message) => {
            tracing::error!("{} failed: {message}", outcome.operation);
            if !outcome.reports_failure_inline {
                let _ = update_tx.try_send(DaemonUpdate::CommandFailed {
                    operation: outcome.operation.to_string(),
                    message: message.clone(),
                });
            }
            Err(message)
        }
    };
    let _ = outcome.completion.send(result);
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
        DaemonCommand::CheckEndpoint(_) => {
            unreachable!("endpoint checks must use their deferred command lane")
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
        DaemonCommand::DismissLastError(expected) => {
            proxy.dismiss_last_error(&expected).await?;
        }
        DaemonCommand::SetApiKey { .. }
        | DaemonCommand::ClearApiKey { .. }
        | DaemonCommand::HasApiKey { .. } => {
            unreachable!("keyring calls must use their deferred command lane")
        }
        DaemonCommand::DownloadModel(name) => {
            proxy.download_model(&name).await?;
        }
        DaemonCommand::CancelModelDownload(name) => {
            proxy.cancel_model_download(&name).await?;
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

    fn command_request(
        command: DaemonCommand,
    ) -> (
        CommandRequest,
        tokio::sync::oneshot::Receiver<Result<CommandResponse, String>>,
    ) {
        let (completion, receiver) = tokio::sync::oneshot::channel();
        (
            CommandRequest {
                command,
                completion,
            },
            receiver,
        )
    }

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

    #[test]
    fn slow_commands_are_assigned_to_their_isolated_lane() {
        assert_eq!(
            DaemonCommand::CheckEndpoint("{}".to_string()).deferred_lane(),
            Some(DeferredCommandLane::Endpoint)
        );
        for command in [
            DaemonCommand::SetApiKey {
                service: "service".to_string(),
                key: "secret".to_string(),
            },
            DaemonCommand::ClearApiKey {
                service: "service".to_string(),
            },
            DaemonCommand::HasApiKey {
                service: "service".to_string(),
                request_id: 1,
            },
        ] {
            assert_eq!(command.deferred_lane(), Some(DeferredCommandLane::Keyring));
        }
        assert_eq!(DaemonCommand::ReloadConfig.deferred_lane(), None);
    }

    #[test]
    fn endpoint_scheduler_is_bounded_and_starts_waiters_in_fifo_order() {
        let mut scheduler = DeferredCommandScheduler::default();
        for request_id in 1..=ENDPOINT_CHECK_CONCURRENCY {
            let (request, _receiver) =
                command_request(DaemonCommand::CheckEndpoint(request_id.to_string()));
            assert_eq!(
                scheduler
                    .submit(request)
                    .map(|request| request.command.operation()),
                Some("Check server")
            );
        }
        let (waiting, _receiver) =
            command_request(DaemonCommand::CheckEndpoint("oldest waiter".to_string()));
        assert!(scheduler.submit(waiting).is_none());
        assert_eq!(scheduler.endpoint_active, ENDPOINT_CHECK_CONCURRENCY);
        assert_eq!(scheduler.endpoint_waiting.len(), 1);

        let next = scheduler
            .complete(DeferredCommandLane::Endpoint)
            .expect("the oldest endpoint waiter must inherit the freed slot");
        assert!(matches!(
            next.command,
            DaemonCommand::CheckEndpoint(ref value) if value == "oldest waiter"
        ));
        assert_eq!(scheduler.endpoint_active, ENDPOINT_CHECK_CONCURRENCY);
        assert!(scheduler.endpoint_waiting.is_empty());

        assert!(scheduler.complete(DeferredCommandLane::Endpoint).is_none());
        assert_eq!(scheduler.endpoint_active, ENDPOINT_CHECK_CONCURRENCY - 1);
        assert!(scheduler.complete(DeferredCommandLane::Endpoint).is_none());
        assert_eq!(scheduler.endpoint_active, 0);
    }

    #[test]
    fn keyring_scheduler_serializes_reads_and_writes_in_fifo_order() {
        let mut scheduler = DeferredCommandScheduler::default();
        let commands = [
            DaemonCommand::HasApiKey {
                service: "service".to_string(),
                request_id: 1,
            },
            DaemonCommand::SetApiKey {
                service: "service".to_string(),
                key: "replacement".to_string(),
            },
            DaemonCommand::HasApiKey {
                service: "service".to_string(),
                request_id: 2,
            },
        ];
        for (index, command) in commands.into_iter().enumerate() {
            let (request, _receiver) = command_request(command);
            assert_eq!(scheduler.submit(request).is_some(), index == 0);
        }
        assert_eq!(scheduler.keyring_active, 1);
        assert_eq!(scheduler.keyring_waiting.len(), 2);

        let write = scheduler
            .complete(DeferredCommandLane::Keyring)
            .expect("the write must follow the first read");
        assert!(matches!(write.command, DaemonCommand::SetApiKey { .. }));
        let read = scheduler
            .complete(DeferredCommandLane::Keyring)
            .expect("the final read must follow the write");
        assert!(matches!(
            read.command,
            DaemonCommand::HasApiKey { request_id: 2, .. }
        ));
        assert!(scheduler.complete(DeferredCommandLane::Keyring).is_none());
        assert_eq!(scheduler.keyring_active, 0);
    }

    #[test]
    fn newer_keyring_operations_supersede_only_stale_waiting_reads() {
        let mut scheduler = DeferredCommandScheduler::default();
        let (active, _active_receiver) = command_request(DaemonCommand::HasApiKey {
            service: "service".to_string(),
            request_id: 1,
        });
        assert!(scheduler.submit(active).is_some());

        let (stale_read, mut stale_receiver) = command_request(DaemonCommand::HasApiKey {
            service: "service".to_string(),
            request_id: 2,
        });
        assert!(scheduler.submit(stale_read).is_none());
        let (other_read, mut other_receiver) = command_request(DaemonCommand::HasApiKey {
            service: "other-service".to_string(),
            request_id: 1,
        });
        assert!(scheduler.submit(other_read).is_none());
        let (write, _write_receiver) = command_request(DaemonCommand::SetApiKey {
            service: "service".to_string(),
            key: "replacement".to_string(),
        });
        assert!(scheduler.submit(write).is_none());
        assert!(
            matches!(stale_receiver.try_recv(), Ok(Err(message)) if message.contains("superseded"))
        );
        assert!(
            matches!(other_receiver.try_recv(), Ok(Err(message)) if message.contains("superseded"))
        );

        let (older_read, mut older_receiver) = command_request(DaemonCommand::HasApiKey {
            service: "service".to_string(),
            request_id: 3,
        });
        assert!(scheduler.submit(older_read).is_none());
        let (latest_read, _latest_receiver) = command_request(DaemonCommand::HasApiKey {
            service: "service".to_string(),
            request_id: 4,
        });
        assert!(scheduler.submit(latest_read).is_none());
        assert!(
            matches!(older_receiver.try_recv(), Ok(Err(message)) if message.contains("superseded"))
        );
        assert_eq!(scheduler.keyring_waiting.len(), 2);

        let write = scheduler
            .complete(DeferredCommandLane::Keyring)
            .expect("the credential write must be next");
        assert!(matches!(write.command, DaemonCommand::SetApiKey { .. }));
        let latest_read = scheduler
            .complete(DeferredCommandLane::Keyring)
            .expect("the latest status read must follow the write");
        assert!(matches!(
            latest_read.command,
            DaemonCommand::HasApiKey { request_id: 4, .. }
        ));
        assert!(scheduler.complete(DeferredCommandLane::Keyring).is_none());
        assert_eq!(scheduler.keyring_active, 0);
    }

    #[tokio::test]
    async fn deferred_results_preserve_updates_completions_and_inline_errors() {
        let (update_tx, mut update_rx) = tokio::sync::mpsc::channel(2);
        let (completion, receiver) = tokio::sync::oneshot::channel();
        finish_deferred_command(
            DeferredCommandResult {
                lane: DeferredCommandLane::Keyring,
                operation: "Check API key",
                reports_failure_inline: false,
                completion,
                result: Ok((
                    None,
                    Some(DaemonUpdate::ApiKeyStatus {
                        service: "service".to_string(),
                        present: true,
                        request_id: 7,
                    }),
                )),
            },
            &update_tx,
        );
        assert!(matches!(receiver.await.unwrap(), Ok(None)));
        assert!(matches!(
            update_rx.recv().await,
            Some(DaemonUpdate::ApiKeyStatus {
                service,
                present: true,
                request_id: 7,
            }) if service == "service"
        ));

        let (completion, receiver) = tokio::sync::oneshot::channel();
        finish_deferred_command(
            DeferredCommandResult {
                lane: DeferredCommandLane::Endpoint,
                operation: "Check server",
                reports_failure_inline: true,
                completion,
                result: Err("probe failed".to_string()),
            },
            &update_tx,
        );
        assert_eq!(receiver.await.unwrap().unwrap_err(), "probe failed");
        assert!(update_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn deferred_keyring_failures_still_publish_command_errors() {
        let (update_tx, mut update_rx) = tokio::sync::mpsc::channel(1);
        let (completion, receiver) = tokio::sync::oneshot::channel();
        finish_deferred_command(
            DeferredCommandResult {
                lane: DeferredCommandLane::Keyring,
                operation: "Save API key",
                reports_failure_inline: false,
                completion,
                result: Err("keyring unavailable".to_string()),
            },
            &update_tx,
        );

        assert_eq!(receiver.await.unwrap().unwrap_err(), "keyring unavailable");
        assert!(matches!(
            update_rx.recv().await,
            Some(DaemonUpdate::CommandFailed { operation, message })
                if operation == "Save API key" && message == "keyring unavailable"
        ));
    }

    #[tokio::test]
    async fn duplicate_model_status_requests_share_one_result() {
        let mut pending = PendingModelStatusRequests::default();
        let (first_tx, first_rx) = tokio::sync::oneshot::channel();
        let (second_tx, second_rx) = tokio::sync::oneshot::channel();
        let (update_tx, mut update_rx) = tokio::sync::mpsc::channel(2);

        let request_id = pending
            .queue("model".to_string(), first_tx)
            .expect("the first request must start the shared query");
        assert_eq!(pending.queue("model".to_string(), second_tx), None);
        pending.finish(
            request_id,
            "model".to_string(),
            Ok("available".to_string()),
            &update_tx,
        );

        assert!(matches!(first_rx.await.unwrap(), Ok(None)));
        assert!(matches!(second_rx.await.unwrap(), Ok(None)));
        assert!(matches!(
            update_rx.recv().await,
            Some(DaemonUpdate::ModelStatusResult { model_name, status })
                if model_name == "model" && status == "available"
        ));
        assert!(update_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn failed_shared_model_status_request_replies_to_every_waiter() {
        let mut pending = PendingModelStatusRequests::default();
        let (first_tx, first_rx) = tokio::sync::oneshot::channel();
        let (second_tx, second_rx) = tokio::sync::oneshot::channel();
        let (update_tx, mut update_rx) = tokio::sync::mpsc::channel(2);

        let request_id = pending
            .queue("model".to_string(), first_tx)
            .expect("the first request must start the shared query");
        assert_eq!(pending.queue("model".to_string(), second_tx), None);
        pending.finish(
            request_id,
            "model".to_string(),
            Err("status unavailable".to_string()),
            &update_tx,
        );

        assert_eq!(first_rx.await.unwrap().unwrap_err(), "status unavailable");
        assert_eq!(second_rx.await.unwrap().unwrap_err(), "status unavailable");
        assert!(matches!(
            update_rx.recv().await,
            Some(DaemonUpdate::ModelStatusFailed { model_name }) if model_name == "model"
        ));
        assert!(update_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_superseded_model_status_result_cannot_complete_a_newer_request() {
        let mut pending = PendingModelStatusRequests::default();
        let (old_tx, old_rx) = tokio::sync::oneshot::channel();
        let (update_tx, mut update_rx) = tokio::sync::mpsc::channel(2);
        let old_request_id = pending
            .queue("model".to_string(), old_tx)
            .expect("the first request must start a query");

        pending.invalidate("model");
        assert!(old_rx.await.unwrap().is_err());
        let (new_tx, mut new_rx) = tokio::sync::oneshot::channel();
        let new_request_id = pending
            .queue("model".to_string(), new_tx)
            .expect("an invalidated request must be replaceable");
        assert_ne!(old_request_id, new_request_id);

        pending.finish(
            old_request_id,
            "model".to_string(),
            Ok("available".to_string()),
            &update_tx,
        );
        assert!(matches!(
            new_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(update_rx.try_recv().is_err());

        pending.finish(
            new_request_id,
            "model".to_string(),
            Ok("downloading".to_string()),
            &update_tx,
        );
        assert!(matches!(new_rx.await.unwrap(), Ok(None)));
        assert!(matches!(
            update_rx.recv().await,
            Some(DaemonUpdate::ModelStatusResult { model_name, status })
                if model_name == "model" && status == "downloading"
        ));
    }

    #[test]
    fn model_integrity_checks_have_a_separate_bounded_deadline() {
        assert!(MODEL_STATUS_CALL_TIMEOUT > DAEMON_CALL_TIMEOUT);
        assert!(MODEL_STATUS_CALL_TIMEOUT >= std::time::Duration::from_secs(60));
        assert!(MODEL_STATUS_CALL_TIMEOUT <= std::time::Duration::from_secs(5 * 60));
    }

    #[test]
    fn model_file_operations_supersede_pending_status_results() {
        for command in [
            DaemonCommand::DownloadModel("model".to_string()),
            DaemonCommand::CancelModelDownload("model".to_string()),
            DaemonCommand::DeleteModel("model".to_string()),
        ] {
            assert_eq!(command.supersedes_model_status(), Some("model"));
        }
        assert_eq!(
            DaemonCommand::ModelStatus("model".to_string()).supersedes_model_status(),
            None
        );
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
    fn only_missing_interface_members_request_the_upgrade_handoff() {
        let unknown_method = zbus::Error::FDO(Box::new(zbus::fdo::Error::UnknownMethod(
            "AttachSettings is not available".to_string(),
        )));
        let unknown_property = zbus::Error::FDO(Box::new(zbus::fdo::Error::UnknownProperty(
            "ProtocolVersion is not available".to_string(),
        )));
        let disconnected = zbus::Error::FDO(Box::new(zbus::fdo::Error::Disconnected(
            "session bus closed".to_string(),
        )));

        assert!(is_unknown_method_error(&unknown_method));
        assert!(is_unknown_property_error(&unknown_property));
        assert!(!is_unknown_method_error(&disconnected));
        assert!(!is_unknown_property_error(&disconnected));
    }

    #[test]
    fn protocol_handoff_accepts_current_and_newer_daemons_only() {
        assert!(daemon_protocol_requires_upgrade(None));
        assert!(daemon_protocol_requires_upgrade(Some(0)));
        assert!(!daemon_protocol_requires_upgrade(Some(
            voxkey_ipc::DAEMON_PROTOCOL_VERSION
        )));
        assert!(!daemon_protocol_requires_upgrade(Some(
            voxkey_ipc::DAEMON_PROTOCOL_VERSION + 1
        )));
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
    fn conditional_error_dismissal_debug_output_is_redacted() {
        let details = "Transcription failed at /private/recordings/sensitive.wav";
        let debug = format!("{:?}", DaemonCommand::DismissLastError(details.to_string()));

        assert_eq!(debug, "DismissLastError(<redacted>)");
        assert!(!debug.contains(details));
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
