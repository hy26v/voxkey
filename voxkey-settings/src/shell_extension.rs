// ABOUTME: Enables the packaged GNOME Shell extension once for a new Voxkey user.
// ABOUTME: Shows a safe restart notice on failure while respecting later user-initiated disables.

use std::ffi::OsStr;
use std::process::Command;
use std::sync::Once;

use gtk4::glib;

const EXTENSION_UUID: &str = "voxkey@hy26v.github.io";
const ENABLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

static ONBOARDING_STARTED: Once = Once::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnableAttempt {
    Enabled,
    Rejected,
    Failed,
    TimedOut,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OnboardingAction {
    Complete,
    ShowRestartNotice,
}

fn onboarding_action(attempt: EnableAttempt) -> OnboardingAction {
    match attempt {
        EnableAttempt::Enabled => OnboardingAction::Complete,
        EnableAttempt::Rejected | EnableAttempt::Failed | EnableAttempt::TimedOut => {
            OnboardingAction::ShowRestartNotice
        }
    }
}

fn logout_command() -> (&'static str, [&'static str; 1]) {
    // Omitting --no-prompt is intentional: GNOME must always ask for final
    // confirmation before closing the user's session.
    ("gnome-session-quit", ["--logout"])
}

pub fn request_logout() -> std::io::Result<()> {
    let (program, arguments) = logout_command();
    let mut child = Command::new(program).args(arguments).spawn()?;
    std::thread::spawn(move || {
        if let Err(error) = child.wait() {
            tracing::warn!("Could not wait for the GNOME logout dialog helper: {error}");
        }
    });
    Ok(())
}

#[zbus::proxy(
    interface = "org.gnome.Shell.Extensions",
    default_service = "org.gnome.Shell.Extensions",
    default_path = "/org/gnome/Shell/Extensions"
)]
trait ShellExtensions {
    fn enable_extension(&self, uuid: &str) -> zbus::Result<bool>;
}

fn desktop_includes_gnome(desktop: Option<&OsStr>) -> bool {
    desktop.and_then(OsStr::to_str).is_some_and(|desktop| {
        desktop
            .split(':')
            .any(|component| component.eq_ignore_ascii_case("gnome"))
    })
}

async fn enable_extension() -> Result<bool, String> {
    let connection = zbus::Connection::session()
        .await
        .map_err(|error| error.to_string())?;
    let proxy = ShellExtensionsProxy::new(&connection)
        .await
        .map_err(|error| error.to_string())?;
    proxy
        .enable_extension(EXTENSION_UUID)
        .await
        .map_err(|error| error.to_string())
}

/// Enable the packaged overlay on the first Voxkey UI launch in GNOME.
///
/// A successful attempt leaves a per-user marker. Subsequent launches never
/// re-enable the extension, so disabling it later in GNOME Extensions remains
/// an explicit and durable user choice.
pub fn onboard_once(show_restart_notice: impl FnOnce() + 'static) {
    if crate::gui_settings::shell_extension_onboarded()
        || !desktop_includes_gnome(std::env::var_os("XDG_CURRENT_DESKTOP").as_deref())
    {
        return;
    }

    ONBOARDING_STARTED.call_once(|| {
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let attempt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => match runtime.block_on(async {
                    tokio::time::timeout(ENABLE_TIMEOUT, enable_extension()).await
                }) {
                    Ok(Ok(true)) => EnableAttempt::Enabled,
                    Ok(Ok(false)) => {
                        tracing::warn!(
                            "GNOME Shell did not enable the packaged Voxkey extension"
                        );
                        EnableAttempt::Rejected
                    }
                    Ok(Err(error)) => {
                        tracing::warn!("Could not enable the Voxkey GNOME extension: {error}");
                        EnableAttempt::Failed
                    }
                    Err(_) => {
                        tracing::warn!("Timed out while enabling the Voxkey GNOME extension");
                        EnableAttempt::TimedOut
                    }
                },
                Err(error) => {
                    tracing::warn!("Could not prepare GNOME extension onboarding: {error}");
                    EnableAttempt::Failed
                }
            };

            let action = onboarding_action(attempt);
            if action == OnboardingAction::Complete {
                if let Err(error) = crate::gui_settings::mark_shell_extension_onboarded() {
                    tracing::warn!(
                        "Enabled the Voxkey GNOME extension but could not save onboarding state: {error}"
                    );
                } else {
                    tracing::info!("Enabled the Voxkey GNOME extension for this user");
                }
            }
            let _ = result_tx.send(action);
        });

        glib::spawn_future_local(async move {
            if result_rx.await == Ok(OnboardingAction::ShowRestartNotice) {
                show_restart_notice();
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_gnome_in_composite_desktop_names() {
        assert!(desktop_includes_gnome(Some(OsStr::new("GNOME"))));
        assert!(desktop_includes_gnome(Some(OsStr::new("ubuntu:GNOME"))));
        assert!(desktop_includes_gnome(Some(OsStr::new(
            "GNOME-Classic:GNOME"
        ))));
    }

    #[test]
    fn skips_other_or_missing_desktops() {
        assert!(!desktop_includes_gnome(Some(OsStr::new("KDE"))));
        assert!(!desktop_includes_gnome(Some(OsStr::new("sway"))));
        assert!(!desktop_includes_gnome(None));
    }

    #[test]
    fn unsuccessful_enable_attempts_require_a_visible_restart_notice() {
        for attempt in [
            EnableAttempt::Rejected,
            EnableAttempt::Failed,
            EnableAttempt::TimedOut,
        ] {
            assert_eq!(
                onboarding_action(attempt),
                OnboardingAction::ShowRestartNotice
            );
        }
    }

    #[test]
    fn successful_enable_attempt_completes_onboarding_without_a_notice() {
        assert_eq!(
            onboarding_action(EnableAttempt::Enabled),
            OnboardingAction::Complete
        );
    }

    #[test]
    fn logout_request_preserves_gnomes_native_confirmation() {
        let (program, arguments) = logout_command();

        assert_eq!(program, "gnome-session-quit");
        assert_eq!(arguments, ["--logout"]);
        assert!(!arguments.contains(&"--no-prompt"));
        assert!(!arguments.contains(&"--force"));
    }
}
