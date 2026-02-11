// ABOUTME: Manages the RemoteDesktop portal session for keyboard injection.
// ABOUTME: Handles session lifecycle, device selection, and keysym notification.

use ashpd::desktop::remote_desktop::{DeviceType, KeyState, RemoteDesktop};
use ashpd::desktop::{PersistMode, Session};

type DynError = Box<dyn std::error::Error + Send + Sync>;

/// Holds the RemoteDesktop proxy and active session.
pub struct DesktopController {
    proxy: RemoteDesktop,
    session: Session<RemoteDesktop>,
    restore_token: Option<String>,
}

impl DesktopController {
    /// Create a RemoteDesktop session, select keyboard device, and start.
    /// Returns the controller and any restore token received from the portal.
    pub async fn new(
        connection: zbus::Connection,
        restore_token: Option<&str>,
    ) -> Result<Self, DynError> {
        let proxy = RemoteDesktop::with_connection(connection).await?;

        let session = proxy.create_session().await?;
        tracing::info!("RemoteDesktop session created");

        proxy
            .select_devices(
                &session,
                DeviceType::Keyboard.into(),
                restore_token,
                PersistMode::ExplicitlyRevoked,
            )
            .await?
            .response()?;

        tracing::info!("Keyboard device selected");

        let start_response = proxy.start(&session, None).await?.response()?;

        let devices = start_response.devices();
        if !devices.contains(DeviceType::Keyboard) {
            return Err(format!(
                "Keyboard not granted after Start: devices={devices:?}"
            )
            .into());
        }

        let new_token = start_response.restore_token().map(|s| s.to_string());
        if new_token.is_some() {
            tracing::info!("Received restore token from portal");
        }

        tracing::info!("RemoteDesktop session started, devices: {devices:?}");

        Ok(Self {
            proxy,
            session,
            restore_token: new_token,
        })
    }

    /// Send a keysym press event.
    pub async fn press_keysym(&self, keysym: i32) -> Result<(), DynError> {
        self.proxy
            .notify_keyboard_keysym(&self.session, keysym, KeyState::Pressed)
            .await?;
        Ok(())
    }

    /// Send a keysym release event.
    pub async fn release_keysym(&self, keysym: i32) -> Result<(), DynError> {
        self.proxy
            .notify_keyboard_keysym(&self.session, keysym, KeyState::Released)
            .await?;
        Ok(())
    }

    /// Send a keysym press then release.
    pub async fn tap_keysym(&self, keysym: i32) -> Result<(), DynError> {
        self.press_keysym(keysym).await?;
        self.release_keysym(keysym).await?;
        Ok(())
    }

    /// The restore token received from Start, if any.
    pub fn restore_token(&self) -> Option<&str> {
        self.restore_token.as_deref()
    }
}
