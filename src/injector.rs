// ABOUTME: Converts transcript text to keysym press/release events for keyboard injection.
// ABOUTME: Maps Unicode codepoints to keysyms via libxkbcommon, handles special controls.

use tokio::sync::mpsc;
use xkbcommon::xkb;
use xkbcommon::xkb::keysyms;

use crate::dbus::SharedState;
use crate::desktop::DesktopController;

/// Keysym constants for special control characters.
const XKB_KEY_RETURN: i32 = 0xff0d;
const XKB_KEY_TAB: i32 = 0xff09;

/// Small delay between keystrokes to avoid compositor dropping events.
const KEYSTROKE_DELAY: std::time::Duration = std::time::Duration::from_millis(5);

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
    ) -> Self {
        let (tx, mut rx) = mpsc::channel::<String>(32);

        tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                let _ = state_tx.send(crate::state::Event::TranscriptReady).await;

                match inject_text(&desktop, &text).await {
                    Ok(()) => {
                        let _ = state_tx.send(crate::state::Event::InjectionDone).await;
                    }
                    Err(e) => {
                        tracing::error!("Injection failed: {e}");
                        shared.set_pending_injection(Some(text));
                        let _ = state_tx.send(crate::state::Event::Error).await;
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
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    for ch in text.chars() {
        let keysym = char_to_keysym(ch);

        if keysym == 0 {
            tracing::debug!("Skipping character with no keysym: U+{:04X}", ch as u32);
            continue;
        }

        desktop.tap_keysym(keysym).await?;
        tokio::time::sleep(KEYSTROKE_DELAY).await;
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
