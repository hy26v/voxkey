// ABOUTME: Observes GNOME's screen-shield state so stale input sessions are retired on lock.
// ABOUTME: Subscribes before querying the current state to avoid missing lock/unlock races.

use std::pin::Pin;

use futures_util::{Stream, StreamExt};

type DynError = Box<dyn std::error::Error + Send + Sync>;

pub type LockEventStream = Pin<Box<dyn Stream<Item = Result<bool, DynError>> + Send>>;

#[zbus::proxy(
    interface = "org.gnome.ScreenSaver",
    default_service = "org.gnome.ScreenSaver",
    default_path = "/org/gnome/ScreenSaver",
    gen_blocking = false
)]
trait ScreenSaver {
    fn get_active(&self) -> zbus::Result<bool>;

    #[zbus(signal)]
    fn active_changed(&self, active: bool) -> zbus::Result<()>;
}

/// Subscribe to future lock transitions, then read the current state.
///
/// This ordering makes both races safe: a transition before `GetActive` is
/// reflected in the method result, while a transition after it is queued in
/// the signal stream. Desktops without GNOME's screen-shield interface return
/// an error and continue to rely on their portal `Closed` signals.
pub async fn subscribe(connection: &zbus::Connection) -> Result<(LockEventStream, bool), DynError> {
    let proxy = crate::deadline::run(
        "GNOME screen-lock proxy setup",
        crate::deadline::BUS_CALL,
        ScreenSaverProxy::new(connection),
    )
    .await?;
    let events = crate::deadline::run(
        "GNOME screen-lock signal subscription",
        crate::deadline::BUS_CALL,
        proxy.receive_active_changed(),
    )
    .await?;
    let active = crate::deadline::run(
        "GNOME screen-lock state query",
        crate::deadline::BUS_CALL,
        proxy.get_active(),
    )
    .await?;
    let events = events.map(|signal| {
        signal
            .args()
            .map(|args| args.active)
            .map_err(|error| Box::new(error) as DynError)
    });
    Ok((Box::pin(events), active))
}

/// Read GNOME's current screen-shield state without replacing an existing
/// signal subscription.
pub async fn is_active(connection: &zbus::Connection) -> Result<bool, DynError> {
    let proxy = crate::deadline::run(
        "GNOME screen-lock proxy setup",
        crate::deadline::BUS_CALL,
        ScreenSaverProxy::new(connection),
    )
    .await?;
    crate::deadline::run(
        "GNOME screen-lock state query",
        crate::deadline::BUS_CALL,
        proxy.get_active(),
    )
    .await
}

/// Watch the lock-event stream for up to `window`, reporting whether a lock
/// transition was observed.
///
/// Used after a portal session dies unexpectedly: observing a lock means the
/// daemon should rebuild once the user unlocks instead of treating the loss
/// as a failure. An unlock event is skipped; a signal error or a stream end
/// is fatal because the classification would otherwise be guesswork.
pub async fn observe_lock_within(
    events: &mut LockEventStream,
    window: std::time::Duration,
) -> Result<bool, DynError> {
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }

        match tokio::time::timeout(remaining, events.next()).await {
            Ok(Some(Ok(true))) => return Ok(true),
            Ok(Some(Ok(false))) => continue,
            Ok(Some(Err(error))) => {
                return Err(format!(
                    "GNOME screen-lock signal failed while classifying portal loss: {error}"
                )
                .into());
            }
            Ok(None) => {
                return Err(
                    "GNOME screen-lock signal stream ended while classifying portal loss".into(),
                );
            }
            Err(_) => return Ok(false),
        }
    }
}

/// A never-ready fallback used when the desktop has no GNOME lock monitor.
pub fn unavailable() -> LockEventStream {
    Box::pin(futures_util::stream::pending())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    fn events(items: Vec<Result<bool, DynError>>) -> LockEventStream {
        Box::pin(stream::iter(items))
    }

    #[tokio::test]
    async fn a_lock_event_within_the_window_confirms_the_lock() {
        let mut events = events(vec![Ok(false), Ok(true)]);
        assert!(
            observe_lock_within(&mut events, std::time::Duration::from_secs(1))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn an_unlock_event_does_not_stop_the_watch() {
        let mut events = Box::pin(
            stream::iter(vec![Ok(false)])
                .chain(stream::pending())
                .fuse(),
        ) as LockEventStream;
        assert!(
            !observe_lock_within(&mut events, std::time::Duration::from_millis(20))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn silence_for_the_whole_window_reports_no_lock() {
        let mut events = unavailable();
        assert!(
            !observe_lock_within(&mut events, std::time::Duration::from_millis(20))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_failing_signal_is_fatal_not_silence() {
        let mut events = events(vec![Err("bus went away".into())]);
        let error = observe_lock_within(&mut events, std::time::Duration::from_secs(1))
            .await
            .expect_err("a broken lock monitor must not be read as unlocked");
        assert!(error.to_string().contains("bus went away"), "{error}");
    }

    #[tokio::test]
    async fn a_stream_that_ends_is_fatal_not_silence() {
        let mut events = events(vec![]);
        let error = observe_lock_within(&mut events, std::time::Duration::from_secs(1))
            .await
            .expect_err("a dead lock monitor must not be read as unlocked");
        assert!(error.to_string().contains("stream ended"), "{error}");
    }
}
