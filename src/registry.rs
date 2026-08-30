// ABOUTME: Registers voxkey's app_id with xdg-desktop-portal before any portal calls.
// ABOUTME: Required by GNOME's GlobalShortcuts backend, which rejects apps without a valid app_id.

use std::collections::HashMap;

use zbus::zvariant::OwnedValue;

pub const APP_ID: &str = voxkey_ipc::SETTINGS_BUS_NAME;

/// The portal interface that binds an app_id to this session's portal calls.
const PORTAL_REGISTRY_INTERFACE: &str = "org.freedesktop.host.portal.Registry";

/// Create a D-Bus session connection and register our app_id with the portal.
/// Returns the connection for reuse by all portal proxies.
pub async fn connect_and_register()
-> Result<zbus::Connection, Box<dyn std::error::Error + Send + Sync>> {
    let builder = zbus::connection::Builder::session()?;
    let connection = crate::deadline::run(
        "session bus connection",
        crate::deadline::BUS_CALL,
        builder.method_timeout(crate::deadline::BUS_CALL).build(),
    )
    .await?;

    let proxy: zbus::Proxy<'_> = crate::deadline::run(
        "portal registry proxy setup",
        crate::deadline::BUS_CALL,
        zbus::proxy::Builder::new(&connection)
            .destination("org.freedesktop.portal.Desktop")?
            .path("/org/freedesktop/portal/desktop")?
            .interface(PORTAL_REGISTRY_INTERFACE)?
            .build(),
    )
    .await?;

    let options: HashMap<String, OwnedValue> = HashMap::new();
    crate::deadline::run(
        "portal application registration",
        crate::deadline::BUS_CALL,
        proxy.call_noreply("Register", &(APP_ID, options)),
    )
    .await?;

    tracing::info!("Registered app_id '{APP_ID}' with portal");
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_matches_the_desktop_identity() {
        // GNOME's GlobalShortcuts backend matches the shortcut binding against
        // the installed desktop file; a mismatch silently breaks activation.
        assert_eq!(APP_ID, "io.github.hy26v.Voxkey");
        let desktop_file = std::path::Path::new("data/io.github.hy26v.Voxkey.desktop");
        assert!(
            desktop_file.exists(),
            "desktop file must stay in lock-step with APP_ID"
        );
    }

    #[test]
    fn registry_interface_targets_the_host_portal_registry() {
        assert_eq!(
            PORTAL_REGISTRY_INTERFACE,
            "org.freedesktop.host.portal.Registry"
        );
    }
}
