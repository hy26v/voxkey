// ABOUTME: Applies named whole-operation deadlines to external and teardown work.
// ABOUTME: Keeps cancellation guarantees composable across D-Bus, portals, and sockets.

use std::future::Future;
use std::time::Duration;

pub type DynError = Box<dyn std::error::Error + Send + Sync>;

pub const BUS_CALL: Duration = Duration::from_secs(10);
pub const PORTAL_REQUEST: Duration = Duration::from_secs(30);
pub const KEYRING_OPERATION: Duration = Duration::from_secs(30);
pub const WEBSOCKET_SEND: Duration = Duration::from_secs(5);
pub const INJECTION_OPERATION: Duration = Duration::from_secs(30);
pub const SESSION_TEARDOWN: Duration = Duration::from_secs(10);
/// CPAL can block inside the host audio stack while opening a device. Keep
/// that work off the async controller and stop waiting well before D-Bus's
/// control-request deadline so the UI can recover on its own.
pub const AUDIO_CAPTURE_START: Duration = Duration::from_secs(3);

pub async fn run<T, E, F>(name: &'static str, limit: Duration, future: F) -> Result<T, DynError>
where
    E: std::error::Error + Send + Sync + 'static,
    F: Future<Output = Result<T, E>>,
{
    match tokio::time::timeout(limit, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) if is_timeout_error(&error) => Err(timeout_error(name, limit)),
        Ok(Err(error)) => Err(std::io::Error::other(format!("{name} failed: {error}")).into()),
        Err(_) => Err(timeout_error(name, limit)),
    }
}

fn timeout_error(name: &'static str, limit: Duration) -> DynError {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("{name} timed out after at most {:.1}s", limit.as_secs_f32()),
    )
    .into()
}

fn is_timeout_error<E>(error: &E) -> bool
where
    E: std::error::Error + 'static,
{
    if let Some(error) = (error as &dyn std::any::Any).downcast_ref::<ashpd::Error>() {
        return match error {
            ashpd::Error::Portal(ashpd::PortalError::ZBus(error)) | ashpd::Error::Zbus(error) => {
                zbus_timeout(error)
            }
            ashpd::Error::IO(error) => error.kind() == std::io::ErrorKind::TimedOut,
            _ => false,
        };
    }

    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(error) = source {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::TimedOut)
        {
            return true;
        }
        source = error.source();
    }
    false
}

fn zbus_timeout(error: &zbus::Error) -> bool {
    matches!(
        error,
        zbus::Error::InputOutput(error) if error.kind() == std::io::ErrorKind::TimedOut
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_never_completing_operation_hits_its_own_deadline() {
        let operation = run(
            "test operation",
            Duration::from_millis(10),
            std::future::pending::<Result<(), std::io::Error>>(),
        );

        let result = tokio::time::timeout(Duration::from_millis(50), operation)
            .await
            .expect("the deadline wrapper itself never returned")
            .expect_err("a never-completing operation unexpectedly succeeded");

        assert!(result.to_string().contains("test operation"), "{result}");
        assert!(result.to_string().contains("timed out"), "{result}");
    }

    #[tokio::test]
    async fn an_ashpd_wrapped_method_timeout_keeps_its_operation_name() {
        let io_error = std::io::Error::new(std::io::ErrorKind::TimedOut, "reply deadline");
        let error = ashpd::Error::Portal(ashpd::PortalError::ZBus(zbus::Error::InputOutput(
            std::sync::Arc::new(io_error),
        )));

        let result = run(
            "portal method",
            Duration::from_secs(30),
            std::future::ready(Err::<(), _>(error)),
        )
        .await
        .expect_err("a wrapped method timeout unexpectedly succeeded");

        assert!(result.to_string().contains("portal method"), "{result}");
        assert!(result.to_string().contains("timed out"), "{result}");
    }
}
