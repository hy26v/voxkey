// ABOUTME: Acquires RemoteDesktop only while text is being inserted.
// ABOUTME: Owns each short-lived portal grant and compositor-tracked EIS connection.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::eis::InjectionFault;
use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop};
use ashpd::desktop::{PersistMode, Session};

type DynError = Box<dyn std::error::Error + Send + Sync>;

struct EisWorker {
    tx: tokio::sync::mpsc::Sender<crate::eis::EisCommand>,
    shutdown: tokio::sync::watch::Sender<bool>,
    done: tokio::sync::oneshot::Receiver<()>,
    thread: std::thread::JoinHandle<()>,
}

/// Deep input module: callers request one insertion without managing portal
/// grants, restore tokens, EIS workers, or cleanup ordering themselves.
pub struct DesktopInput {
    connection: zbus::Connection,
    token_path: std::path::PathBuf,
    active: tokio::sync::Mutex<Option<Arc<DesktopSession>>>,
    injection_gate: tokio::sync::Mutex<()>,
    accepting_injection: AtomicBool,
}

impl DesktopInput {
    pub fn new(connection: zbus::Connection, token_path: std::path::PathBuf) -> Self {
        Self {
            connection,
            token_path,
            active: tokio::sync::Mutex::new(None),
            injection_gate: tokio::sync::Mutex::new(()),
            accepting_injection: AtomicBool::new(true),
        }
    }

    async fn open_session(&self) -> Result<DesktopSession, DynError> {
        let restore_token = crate::persistence::load_restore_token(&self.token_path);
        let session = create_with_restore_retry(restore_token, &self.token_path, |token| {
            let connection = self.connection.clone();
            async move { DesktopSession::new(connection, token.as_deref()).await }
        })
        .await?;

        if let Some(token) = session.restore_token()
            && let Err(error) = crate::persistence::save_restore_token(&self.token_path, token)
        {
            tracing::warn!("Failed to save RemoteDesktop restore token: {error}");
        }
        Ok(session)
    }

    /// Insert one keysym batch and release RemoteDesktop before returning.
    pub async fn inject_keysyms(
        &self,
        keysyms: &[i32],
        delay: std::time::Duration,
        request_cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), InjectionFault> {
        if keysyms.is_empty() {
            return Ok(());
        }

        let _guard = self.injection_gate.lock().await;
        if !self.accepting_injection.load(Ordering::Acquire) {
            return Err(InjectionFault::Cancelled {
                inserted_keysyms: 0,
            });
        }

        let session =
            Arc::new(
                self.open_session()
                    .await
                    .map_err(|error| InjectionFault::Session {
                        message: format!("Could not acquire desktop access for typing: {error}"),
                        inserted_keysyms: 0,
                    })?,
            );

        if !self.accepting_injection.load(Ordering::Acquire) {
            let _ = session.close().await;
            return Err(InjectionFault::Cancelled {
                inserted_keysyms: 0,
            });
        }
        *self.active.lock().await = Some(session.clone());

        let result = session.inject_keysyms(keysyms, delay, request_cancel).await;
        let close_result = session.close().await;
        let mut active = self.active.lock().await;
        if active
            .as_ref()
            .is_some_and(|candidate| Arc::ptr_eq(candidate, &session))
        {
            active.take();
        }
        drop(active);

        match close_result {
            Ok(()) => result,
            Err(close_error) => {
                let inserted_keysyms = result
                    .as_ref()
                    .err()
                    .map_or(keysyms.len(), InjectionFault::inserted_keysyms);
                let prior = result
                    .err()
                    .map(|fault| format!("{fault}; "))
                    .unwrap_or_default();
                Err(InjectionFault::Session {
                    message: format!(
                        "{prior}failed to release desktop access after typing: {close_error}"
                    ),
                    inserted_keysyms,
                })
            }
        }
    }

    pub fn stop_accepting(&self) {
        self.accepting_injection.store(false, Ordering::Release);
    }

    /// Force-release an in-flight grant during bounded daemon teardown.
    pub async fn close_active(&self) -> Result<(), DynError> {
        match self.active.lock().await.take() {
            Some(session) => session.close().await,
            None => Ok(()),
        }
    }
}

async fn create_with_restore_retry<T, E, F, Fut>(
    restore_token: Option<String>,
    token_path: &std::path::Path,
    mut create: F,
) -> Result<T, E>
where
    E: std::fmt::Display,
    F: FnMut(Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    match create(restore_token.clone()).await {
        Ok(session) => Ok(session),
        Err(error) if restore_token.is_some() => {
            tracing::warn!(
                "RemoteDesktop with restore token failed ({error}), retrying without token"
            );
            let retry = create(None).await;
            if retry.is_ok()
                && let Err(remove_error) = std::fs::remove_file(token_path)
                && remove_error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(
                    "Connected without the rejected restore token but could not remove it: \
                     {remove_error}"
                );
            }
            retry
        }
        Err(error) => Err(error),
    }
}

/// One RemoteDesktop grant and its EIS keyboard, owned for one insertion.
struct DesktopSession {
    proxy: RemoteDesktop,
    session: Session<RemoteDesktop>,
    restore_token: Option<String>,
    eis: tokio::sync::Mutex<Option<EisWorker>>,
    injection_gate: tokio::sync::Mutex<()>,
    accepting_injection: AtomicBool,
    close_confirmed: AtomicBool,
    close_gate: tokio::sync::Mutex<()>,
}

impl DesktopSession {
    /// Create a RemoteDesktop session, select keyboard device, and start.
    /// Returns the controller and any restore token received from the portal.
    pub async fn new(
        connection: zbus::Connection,
        restore_token: Option<&str>,
    ) -> Result<Self, DynError> {
        let proxy = crate::deadline::run(
            "RemoteDesktop proxy setup",
            crate::deadline::BUS_CALL,
            RemoteDesktop::with_connection(connection),
        )
        .await?;

        let session = crate::deadline::run(
            "RemoteDesktop session creation",
            crate::deadline::PORTAL_REQUEST,
            proxy.create_session(),
        )
        .await?;
        tracing::info!("RemoteDesktop session created");

        let setup_result: Result<_, DynError> = async {
            crate::deadline::run(
                "RemoteDesktop device-selection response",
                crate::deadline::PORTAL_REQUEST,
                async {
                    proxy
                        .select_devices(
                            &session,
                            DeviceType::Keyboard.into(),
                            restore_token,
                            PersistMode::ExplicitlyRevoked,
                        )
                        .await?
                        .response()
                },
            )
            .await?;

            tracing::info!("Keyboard device selected");
            crate::deadline::run(
                "RemoteDesktop start response",
                crate::deadline::PORTAL_REQUEST,
                async { proxy.start(&session, None).await?.response() },
            )
            .await
        }
        .await;

        let start_response = match setup_result {
            Ok(response) => response,
            Err(error) => {
                let close_error = close_session(&session).await.err();
                return Err(match close_error {
                    Some(close_error) => format!(
                        "RemoteDesktop setup failed: {error}; also failed to close the partial \
                         session: {close_error}"
                    )
                    .into(),
                    None => error,
                });
            }
        };

        let devices = start_response.devices();
        if !devices.contains(DeviceType::Keyboard) {
            let error = format!("Keyboard not granted after Start: devices={devices:?}");
            let close_error = close_session(&session).await.err();
            return Err(match close_error {
                Some(close_error) => format!(
                    "{error}; also failed to close the unusable RemoteDesktop session: \
                     {close_error}"
                )
                .into(),
                None => error.into(),
            });
        }

        let new_token = start_response.restore_token().map(|s| s.to_string());
        if new_token.is_some() {
            tracing::info!("Received restore token from portal");
        }

        tracing::info!("RemoteDesktop session started, devices: {devices:?}");
        Ok(Self {
            proxy,
            session,
            restore_token: new_token,
            eis: tokio::sync::Mutex::new(None),
            injection_gate: tokio::sync::Mutex::new(()),
            accepting_injection: AtomicBool::new(true),
            close_confirmed: AtomicBool::new(false),
            close_gate: tokio::sync::Mutex::new(()),
        })
    }

    /// Lazily establish the one EIS connection permitted for this portal
    /// session. A dedicated thread owns all non-Send reis/xkbcommon objects.
    async fn eis_sender(
        &self,
    ) -> Result<tokio::sync::mpsc::Sender<crate::eis::EisCommand>, DynError> {
        let mut slot = self.eis.lock().await;
        if !self.accepting_injection.load(Ordering::Acquire) {
            return Err("RemoteDesktop session is closed".into());
        }
        if let Some(worker) = slot.as_ref() {
            return Ok(worker.tx.clone());
        }

        let fd = crate::deadline::run(
            "RemoteDesktop ConnectToEIS",
            crate::deadline::BUS_CALL,
            self.proxy.connect_to_eis(&self.session),
        )
        .await?;
        // close() marks the controller retired before waiting for this slot.
        // Recheck after the portal round-trip so an injection admitted just
        // before teardown cannot install a fresh worker behind close().
        if !self.accepting_injection.load(Ordering::Acquire) {
            drop(fd);
            return Err("RemoteDesktop session closed while connecting to EIS".into());
        }
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("voxkey-eis".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ =
                            ready_tx.send(Err(format!("failed to create EIS runtime: {error}")));
                        let _ = done_tx.send(());
                        return;
                    }
                };
                runtime.block_on(async move {
                    match crate::eis::EisSession::connect(fd).await {
                        Ok(session) => {
                            let _ = ready_tx.send(Ok(()));
                            crate::eis::run_worker(session, rx, shutdown_rx).await;
                        }
                        Err(error) => {
                            let _ = ready_tx.send(Err(error.to_string()));
                        }
                    }
                });
                let _ = done_tx.send(());
            })?;

        let ready = tokio::time::timeout(std::time::Duration::from_secs(10), ready_rx).await;
        if let Err(error) = match ready {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("EIS worker exited during startup".to_string()),
            Err(_) => Err("timed out starting EIS worker".to_string()),
        } {
            let _ = shutdown.send(true);
            drop(tx);
            // A stuck native EIS thread must not pin the Tokio runtime or
            // graceful process exit. Dropping detaches it; closing the owned
            // socket and shutdown watch still give normal workers a clean exit.
            drop(thread);
            return Err(format!("EIS setup failed: {error}").into());
        }

        let sender = tx.clone();
        *slot = Some(EisWorker {
            tx,
            shutdown,
            done: done_rx,
            thread,
        });
        Ok(sender)
    }

    /// Inject a batch through the session's compositor-tracked EIS keyboard.
    ///
    /// Only one caller may own the virtual keyboard at a time, so each batch
    /// is complete before another begins. Every key is explicitly released
    /// and synchronized before this method returns.
    pub async fn inject_keysyms(
        &self,
        keysyms: &[i32],
        delay: std::time::Duration,
        request_cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), InjectionFault> {
        let _guard = self.injection_gate.lock().await;
        if !self.accepting_injection.load(Ordering::Acquire) {
            return Err(InjectionFault::Session {
                message: "RemoteDesktop session is closed".to_string(),
                inserted_keysyms: 0,
            });
        }

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let result: Result<(), InjectionFault> = async {
            let sender = self
                .eis_sender()
                .await
                .map_err(|error| InjectionFault::Session {
                    message: error.to_string(),
                    inserted_keysyms: 0,
                })?;
            sender
                .send(crate::eis::EisCommand::Inject {
                    keysyms: keysyms.to_vec(),
                    delay,
                    deadline: tokio::time::Instant::now() + crate::deadline::INJECTION_OPERATION,
                    cancel: request_cancel,
                    result: result_tx,
                })
                .await
                .map_err(|_| InjectionFault::Session {
                    message: "EIS worker is not available".to_string(),
                    inserted_keysyms: 0,
                })?;
            match tokio::time::timeout(
                crate::deadline::INJECTION_OPERATION + std::time::Duration::from_secs(5),
                result_rx,
            )
            .await
            {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(InjectionFault::Session {
                    message: "EIS worker exited during injection".to_string(),
                    // The worker accepted the request but did not report an
                    // offset. Conservatively assume every keysym may have
                    // reached the compositor so an explicit retry cannot
                    // duplicate an unknown prefix.
                    inserted_keysyms: keysyms.len(),
                }),
                Err(_) => Err(InjectionFault::Session {
                    message: "EIS worker exceeded the injection completion deadline".to_string(),
                    inserted_keysyms: keysyms.len(),
                }),
            }
        }
        .await;

        let Err(fault) = result else {
            return Ok(());
        };

        // A pre-write refusal leaves the virtual keyboard untouched. Every
        // partial or protocol failure retires the portal session below.
        if matches!(
            &fault,
            InjectionFault::DeclinedBeforeWrite(_)
                | InjectionFault::Interrupted { .. }
                | InjectionFault::Cancelled { .. }
        ) {
            return Err(fault);
        }

        // Closing the portal session is the final fail-closed boundary.
        // Mutter disconnects the EIS client first and releases its tracked
        // keys; no later injection is attempted with this session.
        let close_error = self.close().await.err();
        let message = match close_error {
            Some(close_error) => format!(
                "EIS keyboard injection failed: {fault}; also failed to close the \
                 RemoteDesktop session: {close_error}"
            ),
            None => format!("EIS keyboard injection failed: {fault}; RemoteDesktop session closed"),
        };
        Err(match fault {
            InjectionFault::Partial {
                inserted_keysyms, ..
            } => InjectionFault::Partial {
                message,
                inserted_keysyms,
            },
            InjectionFault::Interrupted { .. } => {
                unreachable!("safe interruptions return before portal teardown")
            }
            InjectionFault::Cancelled { .. } => {
                unreachable!("safe cancellation returns before portal teardown")
            }
            InjectionFault::Session {
                inserted_keysyms, ..
            } => InjectionFault::Session {
                message,
                inserted_keysyms,
            },
            InjectionFault::DeclinedBeforeWrite(_) => {
                unreachable!("pre-write refusals return before portal teardown")
            }
        })
    }

    /// Explicitly close the portal session and destroy its virtual keyboard.
    pub async fn close(&self) -> Result<(), DynError> {
        self.accepting_injection.store(false, Ordering::Release);
        let _guard = self.close_gate.lock().await;
        if self.close_confirmed.load(Ordering::Acquire) {
            return Ok(());
        }
        let worker = self.eis.lock().await.take();
        let worker_error = match worker {
            Some(worker) => {
                let _ = worker.shutdown.send(true);
                match tokio::time::timeout(std::time::Duration::from_secs(3), worker.done).await {
                    Ok(Ok(())) => None,
                    Ok(Err(_)) => {
                        Some("EIS worker exited without completion confirmation".to_string())
                    }
                    Err(_) => {
                        drop(worker.thread);
                        Some("timed out disconnecting EIS worker".to_string())
                    }
                }
            }
            None => None,
        };

        let session_result = close_session(&self.session).await;
        if let Err(error) = session_result {
            return Err(match worker_error {
                Some(worker_error) => {
                    format!("{worker_error}; failed to close RemoteDesktop session: {error}").into()
                }
                None => error,
            });
        }
        self.close_confirmed.store(true, Ordering::Release);
        match worker_error {
            Some(error) => Err(error.into()),
            None => Ok(()),
        }
    }

    /// The restore token received from Start, if any.
    pub fn restore_token(&self) -> Option<&str> {
        self.restore_token.as_deref()
    }
}

async fn close_session(session: &Session<RemoteDesktop>) -> Result<(), DynError> {
    crate::deadline::run(
        "RemoteDesktop session close",
        crate::deadline::BUS_CALL,
        session.close(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_tokenless_retry_keeps_the_restore_token() {
        let temp = tempfile::tempdir().unwrap();
        let token_path = temp.path().join("restore_token");
        std::fs::write(&token_path, "still-valid-token").unwrap();
        let attempts = Arc::new(std::sync::Mutex::new(Vec::new()));

        let result =
            create_with_restore_retry(Some("still-valid-token".to_string()), &token_path, {
                let attempts = attempts.clone();
                move |token| {
                    attempts.lock().unwrap().push(token);
                    std::future::ready(Err::<(), _>("portal unavailable"))
                }
            })
            .await;

        assert!(result.is_err());
        assert_eq!(attempts.lock().unwrap().len(), 2);
        assert!(
            token_path.exists(),
            "a transient portal failure erased the token"
        );
    }
}
