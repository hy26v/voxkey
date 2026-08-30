// ABOUTME: Manages the GlobalShortcuts portal session for toggle-to-dictate.
// ABOUTME: Binds shortcuts and preserves portal signal ordering in one event stream.

use std::collections::HashMap;
use std::time::Duration;

use ashpd::desktop::Session;
use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut, Shortcut as PortalShortcut};
use futures_util::{Stream, StreamExt};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::config::ShortcutConfig;

type DynError = Box<dyn std::error::Error + Send + Sync>;

fn bound_shortcut_description(
    config: &ShortcutConfig,
    shortcuts: &[(&str, &str)],
) -> Result<String, String> {
    bound_shortcut_description_for_id(&config.id, shortcuts)
}

fn bound_shortcut_description_for_id(
    shortcut_id: &str,
    shortcuts: &[(&str, &str)],
) -> Result<String, String> {
    let trigger = shortcuts
        .iter()
        .find_map(|(id, trigger)| (*id == shortcut_id).then_some(*trigger))
        .ok_or_else(|| {
            format!(
                "portal response did not include requested shortcut '{}'",
                shortcut_id
            )
        })?;

    if trigger.trim().is_empty() {
        return Err(format!(
            "portal returned requested shortcut '{}' without a trigger",
            shortcut_id
        ));
    }

    if voxkey_ipc::conflicts_with_gnome_input_source(trigger) {
        return Err(format!(
            "portal bound '{}' to GNOME's input-source shortcut '{}'",
            shortcut_id, trigger
        ));
    }
    Ok(trigger.trim().to_string())
}

/// Holds the connection and active GlobalShortcuts session.
pub struct ShortcutController {
    connection: zbus::Connection,
    session_handle: String,
    shortcut_id: String,
    trigger_description: String,
    // Kept alive so the portal session remains valid
    #[allow(dead_code)]
    session: Session<GlobalShortcuts>,
}

impl ShortcutController {
    /// Prove that the portal accepts a shortcut without changing the active
    /// session. The temporary session is closed before this returns.
    pub async fn validate_binding(
        connection: zbus::Connection,
        config: &ShortcutConfig,
    ) -> Result<(), DynError> {
        let controller = Self::new(connection, config).await?;
        controller.close().await.map_err(|error| {
            format!("validated shortcut but failed to close test session: {error}").into()
        })
    }

    /// Create a new GlobalShortcuts session and bind the configured shortcut.
    pub async fn new(
        connection: zbus::Connection,
        config: &ShortcutConfig,
    ) -> Result<Self, DynError> {
        voxkey_ipc::validate_shortcut_trigger(&config.trigger)
            .map_err(|error| format!("invalid shortcut '{}': {error}", config.trigger))?;
        let proxy = crate::deadline::run(
            "GlobalShortcuts proxy setup",
            crate::deadline::BUS_CALL,
            GlobalShortcuts::with_connection(connection.clone()),
        )
        .await?;

        let session = crate::deadline::run(
            "GlobalShortcuts session creation",
            crate::deadline::PORTAL_REQUEST,
            proxy.create_session(),
        )
        .await?;
        // ashpd deliberately keeps the session proxy's path private, but its
        // public Serialize implementation is the D-Bus object-path string.
        let session_handle = serde_json::to_value(&session)?
            .as_str()
            .ok_or("GlobalShortcuts session did not serialize as an object path")?
            .to_owned();
        tracing::debug!("GlobalShortcuts session created");

        let shortcut = NewShortcut::new(&config.id, &config.description)
            .preferred_trigger(config.trigger.as_str());

        let response = match crate::deadline::run(
            "GlobalShortcuts binding response",
            crate::deadline::PORTAL_REQUEST,
            async {
                proxy
                    .bind_shortcuts(&session, &[shortcut], None)
                    .await?
                    .response()
            },
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                let close_error = close_session(&session).await.err();
                return Err(match close_error {
                    Some(close_error) => format!(
                        "GlobalShortcuts binding failed: {error}; also failed to close the \
                         partial session: {close_error}"
                    )
                    .into(),
                    None => error,
                });
            }
        };

        let shortcut_refs: Vec<_> = response
            .shortcuts()
            .iter()
            .map(|shortcut| (shortcut.id(), shortcut.trigger_description()))
            .collect();
        let trigger_description = match bound_shortcut_description(config, &shortcut_refs) {
            Ok(description) => description,
            Err(error) => {
                let close_error = close_session(&session).await.err();
                return Err(match close_error {
                    Some(close_error) => {
                        format!("{error}; also failed to close the shortcut session: {close_error}")
                            .into()
                    }
                    None => error.into(),
                });
            }
        };

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

        Ok(Self {
            connection,
            session_handle,
            shortcut_id: config.id.clone(),
            trigger_description,
            session,
        })
    }

    /// User-facing description of the shortcut the portal actually bound.
    pub fn trigger_description(&self) -> &str {
        &self.trigger_description
    }

    /// One ordered stream of activation, deactivation, and binding-change events.
    ///
    /// Subscribing to the two members separately lets both streams become
    /// ready together, after which `select!` may observe a repress before its
    /// preceding release. A single D-Bus stream retains the portal's wire
    /// order and therefore the physical press boundary.
    pub async fn event_stream(
        &self,
    ) -> Result<impl Stream<Item = Result<ShortcutEvent, DynError>> + use<>, DynError> {
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.freedesktop.portal.Desktop")?
            .interface("org.freedesktop.portal.GlobalShortcuts")?
            .path("/org/freedesktop/portal/desktop")?
            .build();
        let messages = crate::deadline::run(
            "GlobalShortcuts event subscription",
            crate::deadline::BUS_CALL,
            zbus::MessageStream::for_match_rule(rule, &self.connection, None),
        )
        .await?;
        let session_handle = self.session_handle.clone();
        let shortcut_id = self.shortcut_id.clone();

        Ok(messages.filter_map(move |message| {
            let session_handle = session_handle.clone();
            let shortcut_id = shortcut_id.clone();
            async move {
                let message = match message {
                    Ok(message) => message,
                    Err(error) => return Some(Err(Box::new(error) as DynError)),
                };
                let member = message
                    .header()
                    .member()
                    .map(|member| member.as_str().to_owned());
                if !matches!(
                    member.as_deref(),
                    Some("Activated" | "Deactivated" | "ShortcutsChanged")
                ) {
                    return None;
                }

                match member.as_deref() {
                    Some("Activated" | "Deactivated") => {
                        let body = message.body().deserialize::<(
                            OwnedObjectPath,
                            String,
                            u64,
                            HashMap<String, OwnedValue>,
                        )>();
                        let (event_session, event_id, timestamp, _options) = match body {
                            Ok(body) => body,
                            Err(error) => return Some(Err(Box::new(error) as DynError)),
                        };
                        if event_session.as_str() != session_handle {
                            return None;
                        }

                        let timestamp = Duration::from_millis(timestamp);
                        Some(Ok(match member.as_deref() {
                            Some("Activated") => ShortcutEvent::Activated {
                                shortcut_id: event_id,
                                timestamp,
                            },
                            Some("Deactivated") => ShortcutEvent::Deactivated {
                                shortcut_id: event_id,
                                timestamp,
                            },
                            _ => unreachable!("signal member was filtered above"),
                        }))
                    }
                    Some("ShortcutsChanged") => {
                        let body = message
                            .body()
                            .deserialize::<(OwnedObjectPath, Vec<PortalShortcut>)>();
                        let (event_session, shortcuts) = match body {
                            Ok(body) => body,
                            Err(error) => return Some(Err(Box::new(error) as DynError)),
                        };
                        if event_session.as_str() != session_handle {
                            return None;
                        }

                        let shortcut_refs: Vec<_> = shortcuts
                            .iter()
                            .map(|shortcut| (shortcut.id(), shortcut.trigger_description()))
                            .collect();
                        let trigger_description =
                            match bound_shortcut_description_for_id(&shortcut_id, &shortcut_refs) {
                                Ok(description) => description,
                                Err(error) => {
                                    return Some(Err(std::io::Error::other(error).into()));
                                }
                            };
                        Some(Ok(ShortcutEvent::ShortcutsChanged {
                            trigger_description,
                        }))
                    }
                    _ => unreachable!("signal member was filtered above"),
                }
            }
        }))
    }

    /// Stream that fires when the portal closes this session (e.g. screen lock).
    pub async fn receive_closed(&self) -> Result<impl Stream<Item = ()> + '_, DynError> {
        crate::deadline::run(
            "GlobalShortcuts close subscription",
            crate::deadline::BUS_CALL,
            self.session.receive_closed(),
        )
        .await
    }

    /// Explicitly close the portal session and release its global shortcut.
    pub async fn close(&self) -> Result<(), DynError> {
        close_session(&self.session).await
    }
}

async fn close_session(session: &Session<GlobalShortcuts>) -> Result<(), DynError> {
    crate::deadline::run(
        "GlobalShortcuts session close",
        crate::deadline::BUS_CALL,
        session.close(),
    )
    .await
}

#[derive(Debug, PartialEq, Eq)]
pub enum ShortcutEvent {
    Activated {
        shortcut_id: String,
        timestamp: Duration,
    },
    Deactivated {
        shortcut_id: String,
        timestamp: Duration,
    },
    ShortcutsChanged {
        trigger_description: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_portal_response_without_the_requested_shortcut_is_rejected() {
        let config = ShortcutConfig::default();
        assert!(bound_shortcut_description(&config, &[]).is_err());
    }

    #[test]
    fn a_portal_response_with_an_unbound_shortcut_is_rejected() {
        let config = ShortcutConfig::default();
        let shortcuts = [(config.id.as_str(), "")];

        assert!(bound_shortcut_description(&config, &shortcuts).is_err());
    }

    #[test]
    fn the_portal_effective_shortcut_description_is_retained() {
        let config = ShortcutConfig::default();
        let shortcuts = [(config.id.as_str(), "Press F13")];

        assert_eq!(
            bound_shortcut_description(&config, &shortcuts).unwrap(),
            "Press F13"
        );
    }
}
