// ABOUTME: Converts transcript text to keysym press/release events for keyboard injection.
// ABOUTME: Simulates character-by-character typing with configurable delay via the portal.

use tokio::sync::mpsc;
use xkbcommon::xkb;
use xkbcommon::xkb::keysyms;

use crate::dbus::{DaemonInterface, SharedState};
use crate::desktop::DesktopInput;

/// Keysym constants for special control characters.
const XKB_KEY_RETURN: i32 = 0xff0d;
const XKB_KEY_TAB: i32 = 0xff09;
const MAX_INJECTION_CHARACTERS: usize = 32 * 1024;

/// Distinguishes fatal input-session errors from local text conversion errors.
pub enum InjectionError {
    /// The input session is closed and the daemon must fail closed.
    Portal(InjectionFailure),
    /// A local error, do not trigger session recovery.
    Local(InjectionFailure),
    /// The caller explicitly cancelled between synchronized key taps.
    Cancelled(InjectionFailure),
}

impl InjectionError {
    pub fn failure(&self) -> &InjectionFailure {
        match self {
            Self::Portal(failure) | Self::Local(failure) | Self::Cancelled(failure) => failure,
        }
    }
}

/// An insertion error together with the exact suffix that was not typed.
#[derive(Debug)]
pub struct InjectionFailure {
    source: Box<dyn std::error::Error + Send + Sync>,
    remaining_text: String,
}

impl InjectionFailure {
    pub(crate) fn new(
        source: Box<dyn std::error::Error + Send + Sync>,
        remaining_text: String,
    ) -> Self {
        Self {
            source,
            remaining_text,
        }
    }

    pub fn remaining_text(&self) -> &str {
        &self.remaining_text
    }
}

impl std::fmt::Display for InjectionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(f)
    }
}

impl std::error::Error for InjectionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Processes text injection requests serially via a channel.
pub struct Injector {
    tx: Option<mpsc::Sender<InjectionRequest>>,
    task: Option<tokio::task::JoinHandle<()>>,
    desktop: std::sync::Arc<DesktopInput>,
    cancel: tokio::sync::watch::Sender<bool>,
    shutdown: tokio::sync::watch::Sender<bool>,
}

enum InjectionRecord {
    NewTranscript {
        full_text: String,
        transcriber: Box<voxkey_ipc::TranscriberConfig>,
        outcome: voxkey_ipc::TranscriptOutcome,
        metrics: voxkey_ipc::HistoryMetrics,
    },
    ExistingTranscript {
        history_id: u64,
    },
}

struct InjectionRequest {
    text: String,
    record: InjectionRecord,
}

impl Injector {
    /// Create an injector that sends keysym events through the given desktop controller.
    /// Spawns a background task that processes the injection queue serially.
    pub fn new(
        desktop: std::sync::Arc<DesktopInput>,
        state_tx: mpsc::Sender<crate::state::Event>,
        shared: SharedState,
        connection: zbus::Connection,
        typing_delay: std::time::Duration,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel::<InjectionRequest>(32);
        let (cancel, _cancel_rx) = tokio::sync::watch::channel(false);
        let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);

        let task_desktop = desktop.clone();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                task_cancel.send_replace(false);
                if *shutdown_rx.borrow() {
                    break;
                }
                let cancel_rx = task_cancel.subscribe();
                let _ = state_tx.send(crate::state::Event::TranscriptReady).await;

                let result =
                    inject_text_with_cancel(&task_desktop, &request.text, typing_delay, cancel_rx)
                        .await;
                let pending_insertion = match &result {
                    Ok(()) => None,
                    Err(
                        InjectionError::Portal(failure)
                        | InjectionError::Local(failure)
                        | InjectionError::Cancelled(failure),
                    ) => Some(failure.remaining_text().to_string()),
                };
                let persistence = match request.record {
                    InjectionRecord::NewTranscript {
                        full_text,
                        transcriber,
                        outcome,
                        metrics,
                    } => {
                        let saved = shared.record_transcript_with_metrics(
                            full_text.clone(),
                            &transcriber,
                            outcome,
                            pending_insertion,
                            metrics,
                        );
                        if saved.is_ok() {
                            DaemonInterface::notify_transcription_complete(&connection, &full_text)
                                .await;
                        }
                        saved.map(|_| ())
                    }
                    InjectionRecord::ExistingTranscript { history_id } => shared
                        .set_pending_insertion(history_id, pending_insertion)
                        .and_then(|found| {
                            found.then_some(()).ok_or_else(|| {
                                std::io::Error::new(
                                    std::io::ErrorKind::NotFound,
                                    "the transcript being retried is no longer in history",
                                )
                            })
                        }),
                };
                if let Err(error) = persistence {
                    let message = format!("Failed to save typing status: {error}");
                    tracing::error!("{message}");
                    shared.set_last_error(message);
                    DaemonInterface::notify_last_error(&connection).await;
                } else {
                    DaemonInterface::notify_last_transcript(&connection).await;
                }

                match result {
                    Ok(()) => {
                        let _ = state_tx.send(crate::state::Event::InjectionDone).await;
                    }
                    Err(InjectionError::Portal(e)) => {
                        tracing::error!("Injection failed (portal): {e}");
                        let _ = state_tx.send(crate::state::Event::Error).await;
                    }
                    Err(InjectionError::Local(e)) => {
                        tracing::error!("Injection failed: {e}");
                        shared.set_last_error(format!("Injection failed: {e}"));
                        DaemonInterface::notify_last_error(&connection).await;
                        let _ = state_tx.send(crate::state::Event::InjectionDone).await;
                    }
                    Err(InjectionError::Cancelled(e)) => {
                        tracing::info!(
                            remaining_chars = e.remaining_text().chars().count(),
                            "Injection cancelled between key taps"
                        );
                        let _ = state_tx.send(crate::state::Event::InjectionDone).await;
                    }
                }
                if *shutdown_rx.borrow() {
                    break;
                }
            }
        });

        Self {
            tx: Some(tx),
            task: Some(task),
            desktop,
            cancel,
            shutdown,
        }
    }

    /// Enqueue text for injection. Returns immediately.
    pub async fn enqueue_transcript(
        &self,
        text: String,
        transcriber: voxkey_ipc::TranscriberConfig,
        outcome: voxkey_ipc::TranscriptOutcome,
        metrics: voxkey_ipc::HistoryMetrics,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.tx
            .as_ref()
            .ok_or("injector is shutting down")?
            .send(InjectionRequest {
                text: text.clone(),
                record: InjectionRecord::NewTranscript {
                    full_text: text,
                    transcriber: Box::new(transcriber),
                    outcome,
                    metrics,
                },
            })
            .await?;
        Ok(())
    }

    pub async fn enqueue_last(
        &self,
        insertion: crate::dbus::LastInsertion,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.tx
            .as_ref()
            .ok_or("injector is shutting down")?
            .send(InjectionRequest {
                text: insertion.text,
                record: InjectionRecord::ExistingTranscript {
                    history_id: insertion.history_id,
                },
            })
            .await?;
        Ok(())
    }

    /// Stop accepting work and let any in-flight, fully acknowledged key tap
    /// finish. If the portal stops responding, close the RemoteDesktop session
    /// before aborting the task so Mutter destroys the virtual keyboard first.
    pub async fn shutdown(&mut self) {
        self.stop_accepting();
        let Some(mut task) = self.task.take() else {
            return;
        };

        if tokio::time::timeout(std::time::Duration::from_secs(5), &mut task)
            .await
            .is_err()
        {
            tracing::warn!(
                "Injection task did not stop in time; closing RemoteDesktop session before abort"
            );
            if let Err(e) = self.desktop.close_active().await {
                tracing::warn!(
                    "Failed to close RemoteDesktop session during injector shutdown: {e}"
                );
            }
            task.abort();
            let _ = task.await;
        }
    }

    pub fn stop_accepting(&mut self) {
        self.tx.take();
        self.shutdown.send_replace(true);
        self.cancel.send_replace(true);
        self.desktop.stop_accepting();
    }

    pub fn cancel_current(&self) {
        self.cancel.send_replace(true);
    }
}

pub async fn inject_text_with_cancel(
    desktop: &DesktopInput,
    text: &str,
    keystroke_delay: std::time::Duration,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<(), InjectionError> {
    if let Err(error) = validate_injection_size(text) {
        return Err(InjectionError::Local(InjectionFailure::new(
            error.into(),
            text.to_string(),
        )));
    }
    let mapped = map_text_to_keysyms(text);
    let started = std::time::Instant::now();
    if let Err(fault) = desktop
        .inject_keysyms(&mapped.keysyms, keystroke_delay, cancel)
        .await
    {
        return Err(injection_error_for(fault, text, &mapped.source_starts));
    }

    tracing::info!(
        "Injected {} chars in {:?} (delay={}ms)",
        mapped.keysyms.len(),
        started.elapsed(),
        keystroke_delay.as_millis(),
    );
    Ok(())
}

fn validate_injection_size(text: &str) -> Result<(), std::io::Error> {
    if text.chars().take(MAX_INJECTION_CHARACTERS + 1).count() > MAX_INJECTION_CHARACTERS {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("Transcript exceeds the {MAX_INJECTION_CHARACTERS}-character typing limit"),
        ));
    }
    Ok(())
}

struct MappedText {
    keysyms: Vec<i32>,
    source_starts: Vec<usize>,
}

fn map_text_to_keysyms(text: &str) -> MappedText {
    let mut keysyms = Vec::with_capacity(text.len());
    let mut source_starts = Vec::with_capacity(text.len());
    let mut characters = text.char_indices().peekable();
    while let Some((source_start, ch)) = characters.next() {
        let keysym = if ch == '\r' {
            if characters.peek().is_some_and(|(_, next)| *next == '\n') {
                characters.next();
            }
            XKB_KEY_RETURN
        } else {
            char_to_keysym(ch)
        };
        if keysym == 0 {
            tracing::debug!("Skipping character with no keysym: U+{:04X}", ch as u32);
        } else {
            keysyms.push(keysym);
            source_starts.push(source_start);
        }
    }
    MappedText {
        keysyms,
        source_starts,
    }
}

#[cfg(test)]
fn text_to_keysyms(text: &str) -> Vec<i32> {
    map_text_to_keysyms(text).keysyms
}

/// Decide whether an injection failure ends the input session.
///
/// A refusal means the text could not be typed with the compositor's current
/// layout, or that the user was holding a modifier. The session is unharmed,
/// so the daemon reports the problem and keeps dictating. Only a protocol
/// failure warrants failing closed.
fn injection_error_for(
    fault: crate::eis::InjectionFault,
    text: &str,
    source_starts: &[usize],
) -> InjectionError {
    let inserted = fault.inserted_keysyms().min(source_starts.len());
    let remaining_start = source_starts.get(inserted).copied().unwrap_or(text.len());
    let remaining_text = text[remaining_start..].to_string();
    let cancelled = matches!(&fault, crate::eis::InjectionFault::Cancelled { .. });
    let safe_interruption = matches!(
        &fault,
        crate::eis::InjectionFault::DeclinedBeforeWrite(_)
            | crate::eis::InjectionFault::Interrupted { .. }
    );
    let failure = InjectionFailure::new(Box::new(fault), remaining_text);
    if cancelled {
        InjectionError::Cancelled(failure)
    } else if safe_interruption {
        InjectionError::Local(failure)
    } else {
        InjectionError::Portal(failure)
    }
}

/// Map a Unicode character to its keysym value.
fn char_to_keysym(ch: char) -> i32 {
    match ch {
        '\n' => XKB_KEY_RETURN,
        '\t' => XKB_KEY_TAB,
        '\r' => 0, // Skip carriage returns (normalize to \n only)
        _ if ch.is_control() => 0,
        _ => {
            let keysym = xkb::utf32_to_keysym(ch as u32);
            if keysym.raw() == keysyms::KEY_NoSymbol {
                0
            } else {
                keysym.raw() as i32
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_transcript_is_refused_before_any_mapping_or_write() {
        let text = "a".repeat(MAX_INJECTION_CHARACTERS + 1);
        let error = validate_injection_size(&text).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("typing limit"));
    }

    #[test]
    fn newline_maps_to_return_keysym() {
        assert_eq!(char_to_keysym('\n'), XKB_KEY_RETURN);
    }

    #[test]
    fn tab_maps_to_tab_keysym() {
        assert_eq!(char_to_keysym('\t'), XKB_KEY_TAB);
    }

    #[test]
    fn carriage_return_skipped() {
        assert_eq!(char_to_keysym('\r'), 0);
    }

    #[test]
    fn ascii_a_maps_to_keysym() {
        assert_ne!(char_to_keysym('a'), 0);
    }

    #[test]
    fn control_characters_cannot_trigger_editing_keys() {
        assert_eq!(char_to_keysym('\u{0008}'), 0, "backspace must be ignored");
        assert_eq!(char_to_keysym('\u{001b}'), 0, "escape must be ignored");
        assert_eq!(char_to_keysym('\u{007f}'), 0, "delete must be ignored");
    }

    #[test]
    fn every_line_ending_style_injects_one_return() {
        assert_eq!(
            text_to_keysyms("one\rtwo\r\nthree")
                .iter()
                .filter(|keysym| **keysym == XKB_KEY_RETURN)
                .count(),
            2
        );
    }

    #[test]
    fn partial_injection_returns_only_the_untyped_suffix() {
        let text = "ab";
        let mapped = map_text_to_keysyms(text);
        let error = injection_error_for(
            crate::eis::InjectionFault::Partial {
                message: "modifier became active".to_string(),
                inserted_keysyms: 1,
            },
            text,
            &mapped.source_starts,
        );

        assert!(matches!(error, InjectionError::Portal(_)));
        assert_eq!(error.failure().remaining_text(), "b");
    }

    #[test]
    fn refusal_before_write_keeps_the_whole_text_retryable() {
        let text = "ab";
        let mapped = map_text_to_keysyms(text);
        let error = injection_error_for(
            crate::eis::InjectionFault::DeclinedBeforeWrite("modifier held".to_string()),
            text,
            &mapped.source_starts,
        );

        assert!(matches!(error, InjectionError::Local(_)));
        assert_eq!(error.failure().remaining_text(), "ab");
    }

    #[test]
    fn an_unknown_worker_offset_never_offers_a_potentially_duplicate_retry() {
        let text = "ab";
        let mapped = map_text_to_keysyms(text);
        let error = injection_error_for(
            crate::eis::InjectionFault::Session {
                message: "worker disappeared".to_string(),
                inserted_keysyms: mapped.keysyms.len(),
            },
            text,
            &mapped.source_starts,
        );

        assert!(matches!(error, InjectionError::Portal(_)));
        assert_eq!(error.failure().remaining_text(), "");
    }
}
