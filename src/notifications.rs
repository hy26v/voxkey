// ABOUTME: Sends desktop notifications via notify-rust for errors and download events.
// ABOUTME: Failures to deliver are logged and ignored; the daemon never blocks on a notification.

use notify_rust::{Notification, Timeout};
use std::sync::OnceLock;

const APP_NAME: &str = "Voxkey";
const APP_ICON: &str = "io.github.hy26v.Voxkey";
const NOTIFICATION_QUEUE_CAPACITY: usize = 16;

struct NotificationRequest {
    summary: String,
    body: String,
    timeout: Timeout,
}

static NOTIFICATION_WORKER: OnceLock<
    Result<std::sync::mpsc::SyncSender<NotificationRequest>, String>,
> = OnceLock::new();

/// Decide whether to fire an error notification when the last_error transitions
/// from `prev` to `next`. Suppresses notifications when clearing the error or
/// when the same error fires repeatedly in a row.
pub fn should_notify_error(prev: &str, next: &str) -> bool {
    !next.is_empty() && prev != next
}

/// Present the persistent daemon error as a concise desktop banner. The full
/// diagnostic remains available in Settings and History, where it can wrap
/// and be copied without being clipped by GNOME Shell.
pub fn last_error(details: &str) {
    let (summary, body) = last_error_presentation(details);
    error(summary, body);
}

fn last_error_presentation(details: &str) -> (&'static str, &'static str) {
    if details.starts_with("Transcription failed:") {
        let body = if details.contains("recording was saved in History") {
            "Recording saved in History. Open settings for details."
        } else {
            "Open Voxkey settings for details."
        };
        ("Transcription failed", body)
    } else if details.starts_with("Failed to start recording:")
        || details.starts_with("Audio capture failed:")
        || details.starts_with("Failed to stop recording:")
    {
        (
            "Could not record audio",
            "Check your microphone in Voxkey settings.",
        )
    } else if details.starts_with("Failed to start streaming:")
        || details.starts_with("Streaming error:")
    {
        (
            "Realtime dictation stopped",
            "Open Voxkey settings for details.",
        )
    } else if details.starts_with("Injection failed:")
        || details.starts_with("Failed to enqueue text:")
        || details.starts_with("Failed to enqueue the last transcript:")
    {
        (
            "Could not type the transcript",
            "Check History for the transcript and retry options.",
        )
    } else if details.starts_with("Download failed:") {
        (
            "Model download failed",
            "Open Voxkey settings to try again.",
        )
    } else if details.starts_with("Desktop access was lost") {
        ("Desktop access lost", "Open Voxkey settings to reconnect.")
    } else {
        (
            "Voxkey needs attention",
            "Open Voxkey settings for details.",
        )
    }
}

/// Send an error notification. Failures are logged and ignored.
fn error(summary: &str, body: &str) {
    show(summary.to_string(), body.to_string(), Timeout::Default);
}

/// Send an informational notification. Failures are logged and ignored.
pub fn info(summary: &str, body: &str) {
    show(
        summary.to_string(),
        body.to_string(),
        Timeout::Milliseconds(5000),
    );
}

/// Deliver notifications on one bounded detached OS worker. `notify_rust::show()` blocks
/// on an internal zbus call that spins up its own runtime, which panics with
/// "cannot start a runtime from within a runtime" if run on a tokio worker
/// thread. The daemon's event loop is exactly such a thread, so delivery runs
/// off-runtime. A stuck desktop notification service can occupy only this one
/// worker and sixteen queued messages; later duplicates are dropped instead of
/// creating unbounded threads.
fn show(summary: String, body: String, timeout: Timeout) {
    let worker = NOTIFICATION_WORKER.get_or_init(|| {
        let (sender, receiver) =
            std::sync::mpsc::sync_channel::<NotificationRequest>(NOTIFICATION_QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("voxkey-notifications".to_string())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    if let Err(error) = Notification::new()
                        .appname(APP_NAME)
                        .summary(&request.summary)
                        .body(&request.body)
                        .icon(APP_ICON)
                        .timeout(request.timeout)
                        .show()
                    {
                        tracing::warn!("Failed to send notification: {error}");
                    }
                }
            })
            .map_err(|error| format!("Could not start notification worker: {error}"))?;
        Ok(sender)
    });
    let Ok(worker) = worker else {
        tracing::warn!("{}", worker.as_ref().unwrap_err());
        return;
    };
    match worker.try_send(NotificationRequest {
        summary,
        body,
        timeout,
    }) {
        Ok(()) => {}
        Err(std::sync::mpsc::TrySendError::Full(_)) => {
            tracing::warn!("Dropping desktop notification because its bounded queue is full")
        }
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
            tracing::warn!("Desktop notification worker stopped unexpectedly")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_notify_when_clearing_error() {
        assert!(!should_notify_error("Recording failed", ""));
    }

    #[test]
    fn no_notify_when_no_prior_error_and_still_empty() {
        assert!(!should_notify_error("", ""));
    }

    #[test]
    fn notify_when_first_error_appears() {
        assert!(should_notify_error("", "Recording failed"));
    }

    #[test]
    fn no_notify_when_same_error_repeated() {
        assert!(!should_notify_error("Recording failed", "Recording failed"));
    }

    #[test]
    fn notify_when_error_changes() {
        assert!(should_notify_error(
            "Recording failed",
            "Transcription failed"
        ));
    }

    #[test]
    fn desktop_errors_keep_diagnostics_out_of_the_clipped_banner() {
        assert_eq!(
            last_error_presentation(
                "Transcription failed: backend returned a very long technical error. Your \
                 recording was saved in History."
            ),
            (
                "Transcription failed",
                "Recording saved in History. Open settings for details."
            )
        );
        assert_eq!(
            last_error_presentation("Audio capture failed: device disconnected"),
            (
                "Could not record audio",
                "Check your microphone in Voxkey settings."
            )
        );
        assert_eq!(
            last_error_presentation("Unexpected D-Bus failure"),
            (
                "Voxkey needs attention",
                "Open Voxkey settings for details."
            )
        );
    }

    /// The daemon's event loop runs inside a multi-threaded tokio runtime.
    /// Sending a notification from that context must not panic, block the
    /// executor, or otherwise disturb the loop; delivery itself may fail in
    /// a headless test environment and is allowed to be logged and dropped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn notifications_do_not_panic_inside_tokio_runtime() {
        error("Voxkey", "test error body");
        last_error("Transcription failed: test error body");
        info("Voxkey", "test info body");
    }
}
