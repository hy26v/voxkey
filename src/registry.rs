// ABOUTME: Registers voxkey's app_id with xdg-desktop-portal before any portal calls.
// ABOUTME: Required by GNOME's GlobalShortcuts backend, which rejects apps without a valid app_id.

use std::collections::HashMap;

use zbus::zvariant::OwnedValue;

pub const APP_ID: &str = "io.github.hy26v.Voxkey";

/// Create a D-Bus session connection and register our app_id with the portal.
/// Returns the connection for reuse by all portal proxies.
pub async fn connect_and_register() -> Result<zbus::Connection, Box<dyn std::error::Error + Send + Sync>> {
    let connection = zbus::Connection::session().await?;

    let proxy: zbus::Proxy<'_> = zbus::proxy::Builder::new(&connection)
        .destination("org.freedesktop.portal.Desktop")?
        .path("/org/freedesktop/portal/desktop")?
        .interface("org.freedesktop.host.portal.Registry")?
        .build()
        .await?;

    let options: HashMap<String, OwnedValue> = HashMap::new();
    proxy.call_noreply("Register", &(APP_ID, options)).await?;

    tracing::info!("Registered app_id '{APP_ID}' with portal");
    Ok(connection)
}
