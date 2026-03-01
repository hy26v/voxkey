// ABOUTME: D-Bus interface exposing daemon state and configuration to the settings GUI.
// ABOUTME: Registered on the session bus so the GUI can read properties and call methods.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use crate::config::Config;
use crate::model_download::DownloadStatus;
use crate::shortcuts;
use crate::state::State;

/// Shared daemon state readable by the D-Bus interface and writable by the event loop.
#[derive(Clone)]
pub struct SharedState {
    inner: Arc<Mutex<SharedStateInner>>,
    restart_signal: Arc<tokio::sync::Notify>,
    shutdown_signal: Arc<tokio::sync::Notify>,
}

struct SharedStateInner {
    state: State,
    config: Config,
    portal_connected: bool,
    last_transcript: String,
    last_error: String,
    pending_injection: Option<String>,
    download_watchers: HashMap<String, watch::Receiver<DownloadStatus>>,
}

impl SharedState {
    pub fn new(config: Config) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SharedStateInner {
                state: State::Idle,
                config,
                portal_connected: false,
                last_transcript: String::new(),
                last_error: String::new(),
                pending_injection: None,
                download_watchers: HashMap::new(),
            })),
            restart_signal: Arc::new(tokio::sync::Notify::new()),
            shutdown_signal: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub fn state(&self) -> State {
        self.inner.lock().unwrap().state
    }

    pub fn set_state(&self, state: State) {
        self.inner.lock().unwrap().state = state;
    }

    pub fn set_portal_connected(&self, connected: bool) {
        self.inner.lock().unwrap().portal_connected = connected;
    }

    pub fn set_last_transcript(&self, text: String) {
        self.inner.lock().unwrap().last_transcript = text;
    }

    pub fn set_last_error(&self, text: String) {
        self.inner.lock().unwrap().last_error = text;
    }

    pub fn set_pending_injection(&self, text: Option<String>) {
        self.inner.lock().unwrap().pending_injection = text;
    }

    pub fn take_pending_injection(&self) -> Option<String> {
        self.inner.lock().unwrap().pending_injection.take()
    }

    pub fn config(&self) -> Config {
        self.inner.lock().unwrap().config.clone()
    }

    pub fn update_config(&self, config: Config) {
        self.inner.lock().unwrap().config = config;
    }

    fn portal_connected(&self) -> bool {
        self.inner.lock().unwrap().portal_connected
    }

    fn last_transcript(&self) -> String {
        self.inner.lock().unwrap().last_transcript.clone()
    }

    fn last_error(&self) -> String {
        self.inner.lock().unwrap().last_error.clone()
    }

    pub fn request_session_restart(&self) {
        self.restart_signal.notify_one();
    }

    pub async fn session_restart_requested(&self) {
        self.restart_signal.notified().await;
    }

    pub fn request_shutdown(&self) {
        self.shutdown_signal.notify_one();
    }

    pub async fn shutdown_requested(&self) {
        self.shutdown_signal.notified().await;
    }

    pub fn start_model_download(&self, model_name: String) -> watch::Receiver<DownloadStatus> {
        let rx = crate::model_download::start_download(model_name.clone());
        self.inner.lock().unwrap().download_watchers.insert(model_name, rx.clone());
        rx
    }

    pub fn model_download_status(&self, model_name: &str) -> Option<DownloadStatus> {
        let inner = self.inner.lock().unwrap();
        inner.download_watchers.get(model_name).map(|rx| rx.borrow().clone())
    }
}

/// D-Bus interface implementation served by the daemon.
pub struct DaemonInterface {
    shared: SharedState,
}

impl DaemonInterface {
    pub fn new(shared: SharedState) -> Self {
        Self { shared }
    }

    pub async fn notify_state(connection: &zbus::Connection) {
        let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        else {
            return;
        };
        let _ = iface_ref
            .get()
            .await
            .state_changed(iface_ref.signal_emitter())
            .await;
    }

    pub async fn notify_portal_connected(connection: &zbus::Connection) {
        let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        else {
            return;
        };
        let _ = iface_ref
            .get()
            .await
            .portal_connected_changed(iface_ref.signal_emitter())
            .await;
    }

    pub async fn notify_last_error(connection: &zbus::Connection) {
        let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        else {
            return;
        };
        let _ = iface_ref
            .get()
            .await
            .last_error_changed(iface_ref.signal_emitter())
            .await;
    }

    pub async fn notify_last_transcript(connection: &zbus::Connection) {
        let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        else {
            return;
        };
        let _ = iface_ref
            .get()
            .await
            .last_transcript_changed(iface_ref.signal_emitter())
            .await;
    }
}

#[zbus::interface(name = "io.github.hy26v.Voxkey.Daemon1")]
impl DaemonInterface {
    #[zbus(property)]
    fn state(&self) -> String {
        self.shared.state().to_string()
    }

    #[zbus(property)]
    fn shortcut_trigger(&self) -> String {
        self.shared.config().shortcut.trigger.clone()
    }

    #[zbus(property)]
    fn transcriber_config(&self) -> String {
        serde_json::to_string(&self.shared.config().transcriber)
            .unwrap_or_default()
    }

    #[zbus(property)]
    fn injection_config(&self) -> String {
        serde_json::to_string(&self.shared.config().injection)
            .unwrap_or_default()
    }

    #[zbus(property)]
    fn sample_rate(&self) -> u32 {
        self.shared.config().audio.sample_rate
    }

    #[zbus(property)]
    fn channels(&self) -> u16 {
        self.shared.config().audio.channels
    }

    #[zbus(property)]
    fn portal_connected(&self) -> bool {
        self.shared.portal_connected()
    }

    #[zbus(property)]
    fn last_transcript(&self) -> String {
        self.shared.last_transcript()
    }

    #[zbus(property)]
    fn last_error(&self) -> String {
        self.shared.last_error()
    }

    async fn set_shortcut(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        trigger: &str,
    ) -> zbus::fdo::Result<()> {
        {
            let mut inner = self.shared.inner.lock().unwrap();
            inner.config.shortcut.trigger = trigger.to_string();
        }
        let config = self.shared.config();
        config.save().map_err(|e| {
            zbus::fdo::Error::Failed(format!("Failed to save config: {e}"))
        })?;

        if let Err(e) = shortcuts::write_shortcut_dconf(&config.shortcut) {
            tracing::warn!("Failed to write shortcut to dconf (non-GNOME?): {e}");
        }

        if let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        {
            let _ = iface_ref
                .get()
                .await
                .shortcut_trigger_changed(iface_ref.signal_emitter())
                .await;
        }

        self.shared.request_session_restart();

        Ok(())
    }

    async fn set_transcriber_config(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        config_json: &str,
    ) -> zbus::fdo::Result<()> {
        let transcriber_config: voxkey_ipc::TranscriberConfig =
            serde_json::from_str(config_json).map_err(|e| {
                zbus::fdo::Error::InvalidArgs(format!("Invalid transcriber config JSON: {e}"))
            })?;
        {
            let mut inner = self.shared.inner.lock().unwrap();
            inner.config.transcriber = transcriber_config;
        }
        self.shared.config().save().map_err(|e| {
            zbus::fdo::Error::Failed(format!("Failed to save config: {e}"))
        })?;

        if let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        {
            let _ = iface_ref
                .get()
                .await
                .transcriber_config_changed(iface_ref.signal_emitter())
                .await;
        }

        self.shared.request_session_restart();

        Ok(())
    }

    async fn set_injection_config(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        config_json: &str,
    ) -> zbus::fdo::Result<()> {
        let injection_config: voxkey_ipc::InjectionConfig =
            serde_json::from_str(config_json).map_err(|e| {
                zbus::fdo::Error::InvalidArgs(format!("Invalid injection config JSON: {e}"))
            })?;
        {
            let mut inner = self.shared.inner.lock().unwrap();
            inner.config.injection = injection_config;
        }
        self.shared.config().save().map_err(|e| {
            zbus::fdo::Error::Failed(format!("Failed to save config: {e}"))
        })?;

        if let Ok(iface_ref) = connection
            .object_server()
            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
            .await
        {
            let _ = iface_ref
                .get()
                .await
                .injection_config_changed(iface_ref.signal_emitter())
                .await;
        }

        self.shared.request_session_restart();

        Ok(())
    }

    async fn set_audio(
        &self,
        sample_rate: u32,
        channels: u16,
    ) -> zbus::fdo::Result<()> {
        {
            let mut inner = self.shared.inner.lock().unwrap();
            inner.config.audio.sample_rate = sample_rate;
            inner.config.audio.channels = channels;
        }
        self.shared.config().save().map_err(|e| {
            zbus::fdo::Error::Failed(format!("Failed to save config: {e}"))
        })?;
        self.shared.request_session_restart();
        Ok(())
    }

    async fn reload_config(&self) -> zbus::fdo::Result<()> {
        let config = Config::load().map_err(|e| {
            zbus::fdo::Error::Failed(format!("Failed to reload config: {e}"))
        })?;
        self.shared.update_config(config);
        tracing::info!("Configuration reloaded via D-Bus");
        Ok(())
    }

    async fn quit(&self) -> zbus::fdo::Result<()> {
        tracing::info!("Quit requested via D-Bus");
        self.shared.request_shutdown();
        Ok(())
    }

    async fn clear_restore_token(&self) -> zbus::fdo::Result<()> {
        let token_path = self.shared.config().token_path();
        if token_path.exists() {
            std::fs::remove_file(&token_path).map_err(|e| {
                zbus::fdo::Error::Failed(format!("Failed to remove token: {e}"))
            })?;
            tracing::info!("Restore token cleared via D-Bus");
        }
        Ok(())
    }

    async fn download_model(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        model_name: &str,
    ) -> zbus::fdo::Result<()> {
        let model_name = model_name.to_string();
        let mut rx = self.shared.start_model_download(model_name.clone());
        let connection = connection.clone();
        let shared = self.shared.clone();

        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let status = rx.borrow().clone();
                match &status {
                    DownloadStatus::InProgress(pct) => {
                        if let Ok(iface_ref) = connection
                            .object_server()
                            .interface::<_, DaemonInterface>(voxkey_ipc::OBJECT_PATH)
                            .await
                        {
                            let _ = DaemonInterface::download_progress(
                                iface_ref.signal_emitter(),
                                &model_name,
                                *pct,
                            ).await;
                        }
                    }
                    DownloadStatus::Complete => {
                        tracing::info!("Model download complete: {model_name}");
                        break;
                    }
                    DownloadStatus::Failed(msg) => {
                        tracing::error!("Model download failed: {msg}");
                        shared.set_last_error(format!("Download failed: {msg}"));
                        DaemonInterface::notify_last_error(&connection).await;
                        break;
                    }
                }
            }
            shared.inner.lock().unwrap().download_watchers.remove(&model_name);
        });

        Ok(())
    }

    async fn delete_model(&self, model_name: &str) -> zbus::fdo::Result<()> {
        crate::model_download::delete_model(model_name).map_err(|e| {
            zbus::fdo::Error::Failed(format!("Failed to delete model: {e}"))
        })
    }

    fn model_status(&self, model_name: &str) -> zbus::fdo::Result<String> {
        if let Some(status) = self.shared.model_download_status(model_name) {
            return Ok(match status {
                DownloadStatus::InProgress(_) => "downloading".to_string(),
                DownloadStatus::Complete => "available".to_string(),
                DownloadStatus::Failed(_) => "not_downloaded".to_string(),
            });
        }
        if crate::models::is_model_available(model_name) {
            Ok("available".to_string())
        } else {
            Ok("not_downloaded".to_string())
        }
    }

    #[zbus(signal)]
    async fn transcription_complete(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
        text: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn error_occurred(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
        message: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn download_progress(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
        model_name: &str,
        percent: u8,
    ) -> zbus::Result<()>;
}
