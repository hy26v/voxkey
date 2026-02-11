// ABOUTME: Checks XDG Desktop Portal capabilities required by voxkey.
// ABOUTME: Validates GlobalShortcuts and RemoteDesktop interface versions and device support.

use ashpd::desktop::global_shortcuts::GlobalShortcuts;
use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop};

/// Minimum required version for GlobalShortcuts interface.
const MIN_GLOBAL_SHORTCUTS_VERSION: u32 = 1;
/// Minimum required version for RemoteDesktop interface.
const MIN_REMOTE_DESKTOP_VERSION: u32 = 2;

/// Verify that all required portal interfaces are available with sufficient versions.
pub async fn check_capabilities(connection: zbus::Connection) -> Result<(), String> {
    check_global_shortcuts(connection.clone()).await?;
    check_remote_desktop(connection).await?;
    Ok(())
}

async fn check_global_shortcuts(connection: zbus::Connection) -> Result<(), String> {
    let proxy = GlobalShortcuts::with_connection(connection).await.map_err(|e| {
        format!("GlobalShortcuts interface not available: {e}")
    })?;

    let version: u32 = proxy.get_property::<u32>("version").await.map_err(|e| {
        format!("Failed to query GlobalShortcuts version: {e}")
    })?;

    if version < MIN_GLOBAL_SHORTCUTS_VERSION {
        return Err(format!(
            "GlobalShortcuts version {version} < {MIN_GLOBAL_SHORTCUTS_VERSION}"
        ));
    }

    tracing::info!("GlobalShortcuts version: {version}");
    Ok(())
}

async fn check_remote_desktop(connection: zbus::Connection) -> Result<(), String> {
    let proxy = RemoteDesktop::with_connection(connection).await.map_err(|e| {
        format!("RemoteDesktop interface not available: {e}")
    })?;

    let version: u32 = proxy.get_property::<u32>("version").await.map_err(|e| {
        format!("Failed to query RemoteDesktop version: {e}")
    })?;

    if version < MIN_REMOTE_DESKTOP_VERSION {
        return Err(format!(
            "RemoteDesktop version {version} < {MIN_REMOTE_DESKTOP_VERSION}"
        ));
    }

    let device_types = proxy.available_device_types().await.map_err(|e| {
        format!("Failed to query AvailableDeviceTypes: {e}")
    })?;

    if !device_types.contains(DeviceType::Keyboard) {
        return Err(format!(
            "Keyboard not in AvailableDeviceTypes: {device_types:?}"
        ));
    }

    tracing::info!("RemoteDesktop version: {version}, devices: {device_types:?}");
    Ok(())
}
