// ABOUTME: Converts transcript text to keysym press/release events for keyboard injection.
// ABOUTME: Simulates character-by-character typing with configurable delay via the portal.

use tokio::sync::mpsc;
use xkbcommon::xkb;
use xkbcommon::xkb::keysyms;

use crate::dbus::SharedState;
use crate::desktop::DesktopController;

/// Keysym constants for special control characters.
const XKB_KEY_RETURN: i32 = 0xff0d;
const XKB_KEY_TAB: i32 = 0xff09;

/// Distinguishes portal session errors (recoverable by session restart) from local errors.
pub enum InjectionError {
    /// The portal session is dead, trigger recovery and retry later.
    Portal(Box<dyn std::error::Error + Send + Sync>),
    /// A local error, do not trigger session recovery.
    #[allow(dead_code)]
    Local(Box<dyn std::error::Error + Send + Sync>),
}

/// Processes text injection requests serially via a channel.
pub struct Injector {
    tx: mpsc::Sender<String>,
}

impl Injector {
    /// Create an injector that sends keysym events through the given desktop controller.
    /// Spawns a background task that processes the injection queue serially.
    pub fn new(
        desktop: std::sync::Arc<DesktopController>,
        state_tx: mpsc::Sender<crate::state::Event>,
        shared: SharedState,
        typing_delay: std::time::Duration,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel::<String>(32);

        tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                let _ = state_tx.send(crate::state::Event::TranscriptReady).await;

                match inject_text(&desktop, &text, typing_delay).await {
                    Ok(()) => {
                        let _ = state_tx.send(crate::state::Event::InjectionDone).await;
                    }
                    Err(InjectionError::Portal(e)) => {
                        tracing::error!("Injection failed (portal): {e}");
                        shared.set_pending_injection(Some(text));
                        let _ = state_tx.send(crate::state::Event::Error).await;
                    }
                    Err(InjectionError::Local(e)) => {
                        tracing::error!("Injection failed: {e}");
                        shared.set_last_error(format!("Injection failed: {e}"));
                        let _ = state_tx.send(crate::state::Event::InjectionDone).await;
                    }
                }
            }
        });

        Self { tx }
    }

    /// Enqueue text for injection. Returns immediately.
    pub async fn enqueue(&self, text: String) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.tx.send(text).await?;
        Ok(())
    }
}

/// Inject the given text by mapping each character to a keysym and sending press/release.
pub async fn inject_text(
    desktop: &DesktopController,
    text: &str,
    keystroke_delay: std::time::Duration,
) -> Result<(), InjectionError> {
    for ch in text.chars() {
        let keysym = char_to_keysym(ch);

        if keysym == 0 {
            tracing::debug!("Skipping character with no keysym: U+{:04X}", ch as u32);
            continue;
        }

        desktop.tap_keysym(keysym).await.map_err(|e| InjectionError::Portal(e))?;
        tokio::time::sleep(keystroke_delay).await;
    }

    Ok(())
}

/// Map a Unicode character to its keysym value.
fn char_to_keysym(ch: char) -> i32 {
    match ch {
        '\n' => XKB_KEY_RETURN,
        '\t' => XKB_KEY_TAB,
        '\r' => 0, // Skip carriage returns (normalize to \n only)
        _ => {
            let keysym = xkb::utf32_to_keysym(ch as u32);
            if keysym.raw() == keysyms::KEY_NoSymbol {
                0
            } else {
                keysym.raw() as i32
            }
        }
    }
}
