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
    match tokio::time::timeout(crate::deadline::PORTAL_REQUEST, async {
        check_global_shortcuts(connection.clone()).await?;
        check_remote_desktop(connection).await?;
        Ok(())
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err("Portal capability check timed out".to_string()),
    }
}

fn check_interface_version(interface: &str, version: u32, minimum: u32) -> Result<(), String> {
    if version < minimum {
        return Err(format!("{interface} version {version} < {minimum}"));
    }
    Ok(())
}

fn check_keyboard_support(
    device_types: ashpd::enumflags2::BitFlags<DeviceType>,
) -> Result<(), String> {
    if !device_types.contains(DeviceType::Keyboard) {
        return Err(format!(
            "Keyboard not in AvailableDeviceTypes: {device_types:?}"
        ));
    }
    Ok(())
}

async fn check_global_shortcuts(connection: zbus::Connection) -> Result<(), String> {
    let proxy = GlobalShortcuts::with_connection(connection)
        .await
        .map_err(|e| format!("GlobalShortcuts interface not available: {e}"))?;

    let version: u32 = proxy
        .get_property::<u32>("version")
        .await
        .map_err(|e| format!("Failed to query GlobalShortcuts version: {e}"))?;

    check_interface_version("GlobalShortcuts", version, MIN_GLOBAL_SHORTCUTS_VERSION)?;
    tracing::info!("GlobalShortcuts version: {version}");
    Ok(())
}

async fn check_remote_desktop(connection: zbus::Connection) -> Result<(), String> {
    let proxy = RemoteDesktop::with_connection(connection)
        .await
        .map_err(|e| format!("RemoteDesktop interface not available: {e}"))?;

    let version: u32 = proxy
        .get_property::<u32>("version")
        .await
        .map_err(|e| format!("Failed to query RemoteDesktop version: {e}"))?;

    check_interface_version("RemoteDesktop", version, MIN_REMOTE_DESKTOP_VERSION)?;

    let device_types = proxy
        .available_device_types()
        .await
        .map_err(|e| format!("Failed to query AvailableDeviceTypes: {e}"))?;

    check_keyboard_support(device_types)?;

    tracing::info!("RemoteDesktop version: {version}, devices: {device_types:?}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_version_at_the_minimum_passes() {
        assert!(
            check_interface_version("GlobalShortcuts", MIN_GLOBAL_SHORTCUTS_VERSION, 1).is_ok()
        );
        assert!(check_interface_version("RemoteDesktop", MIN_REMOTE_DESKTOP_VERSION, 2).is_ok());
    }

    #[test]
    fn interface_version_below_the_minimum_names_the_shortfall() {
        let error = check_interface_version("GlobalShortcuts", 0, 1)
            .expect_err("a missing interface must be rejected");
        assert_eq!(error, "GlobalShortcuts version 0 < 1");

        let error = check_interface_version("RemoteDesktop", 1, 2)
            .expect_err("an old RemoteDesktop must be rejected");
        assert_eq!(error, "RemoteDesktop version 1 < 2");
    }

    #[test]
    fn remote_desktop_without_keyboard_support_is_rejected() {
        let error = check_keyboard_support(enumflags2_empty())
            .expect_err("a pointer-only compositor cannot receive dictation");
        assert!(error.contains("Keyboard"), "{error}");

        assert!(check_keyboard_support(DeviceType::Keyboard.into()).is_ok());
        // Keyboard support among other devices is enough.
        assert!(check_keyboard_support(DeviceType::Keyboard | DeviceType::Pointer).is_ok());
    }

    /// An empty device-type set.
    fn enumflags2_empty() -> ashpd::enumflags2::BitFlags<DeviceType> {
        ashpd::enumflags2::BitFlags::default()
    }

    #[test]
    fn required_minimum_versions_match_the_documented_portal_baselines() {
        assert_eq!(MIN_GLOBAL_SHORTCUTS_VERSION, 1);
        // Version 2 introduced AvailableDeviceTypes.
        assert_eq!(MIN_REMOTE_DESKTOP_VERSION, 2);
    }
}
