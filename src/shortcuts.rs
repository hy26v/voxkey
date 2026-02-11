// ABOUTME: Manages the GlobalShortcuts portal session for hold-to-dictate.
// ABOUTME: Creates sessions, binds shortcuts, and provides Activated/Deactivated signal streams.

use ashpd::desktop::global_shortcuts::{
    Activated, Deactivated, GlobalShortcuts, NewShortcut,
};
use ashpd::desktop::Session;
use futures_util::Stream;

use crate::config::ShortcutConfig;

type DynError = Box<dyn std::error::Error + Send + Sync>;

/// Holds the GlobalShortcuts proxy and active session.
pub struct ShortcutController {
    proxy: GlobalShortcuts,
    // Kept alive so the portal session remains valid
    #[allow(dead_code)]
    session: Session<GlobalShortcuts>,
}

impl ShortcutController {
    /// Create a new GlobalShortcuts session and bind the configured shortcut.
    pub async fn new(
        connection: zbus::Connection,
        config: &ShortcutConfig,
    ) -> Result<Self, DynError> {
        let proxy = GlobalShortcuts::with_connection(connection).await?;

        let session = proxy.create_session().await?;
        tracing::debug!("GlobalShortcuts session created");

        let shortcut = NewShortcut::new(&config.id, &config.description)
            .preferred_trigger(config.trigger.as_str());

        let response = proxy
            .bind_shortcuts(&session, &[shortcut], None)
            .await?
            .response()?;

        for s in response.shortcuts() {
            tracing::info!(
                "Bound shortcut: id={:?}, description={:?}, trigger_description={:?}",
                s.id(),
                s.description(),
                s.trigger_description(),
            );
        }
        let bound_ids: Vec<&str> = response.shortcuts().iter().map(|s| s.id()).collect();
        tracing::info!("Bound shortcuts: {bound_ids:?}");

        if !bound_ids.contains(&config.id.as_str()) {
            tracing::warn!(
                "Shortcut '{}' not in bound list; compositor may have assigned a different trigger",
                config.id
            );
        }

        Ok(Self { proxy, session })
    }

    /// Stream of shortcut activation events.
    pub async fn activated_stream(
        &self,
    ) -> Result<impl Stream<Item = Activated> + '_, DynError> {
        Ok(self.proxy.receive_activated().await?)
    }

    /// Stream of shortcut deactivation events.
    pub async fn deactivated_stream(
        &self,
    ) -> Result<impl Stream<Item = Deactivated> + '_, DynError> {
        Ok(self.proxy.receive_deactivated().await?)
    }
}

/// Format a shortcut config as a GVariant text value for GNOME's dconf schema.
fn format_dconf_value(config: &ShortcutConfig) -> String {
    format!(
        "[('{}', {{'shortcuts': <['{}']>, 'description': <'{}'>}})]",
        config.id, config.trigger, config.description
    )
}

/// Write shortcut binding to GNOME's dconf so the portal picks it up on next session creation.
/// Fails gracefully on non-GNOME compositors where dconf may not exist.
pub fn write_shortcut_dconf(config: &ShortcutConfig) -> Result<(), DynError> {
    let path = format!(
        "/org/gnome/settings-daemon/global-shortcuts/{}/shortcuts",
        crate::registry::APP_ID
    );
    let value = format_dconf_value(config);

    let output = std::process::Command::new("dconf")
        .args(["write", &path, &value])
        .output()?;

    if !output.status.success() {
        return Err(format!(
            "dconf write failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    tracing::info!("Updated dconf shortcut to '{}'", config.trigger);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_dconf_value_matches_gnome_schema() {
        let config = ShortcutConfig {
            id: "dictate_hold".to_string(),
            description: "Dictate".to_string(),
            trigger: "<Super>t".to_string(),
        };

        let value = format_dconf_value(&config);

        assert_eq!(
            value,
            "[('dictate_hold', {'shortcuts': <['<Super>t']>, 'description': <'Dictate'>})]"
        );
    }
}
