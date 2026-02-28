// ABOUTME: Converts transcript text to keysym press/release events for keyboard injection.
// ABOUTME: Maps Unicode codepoints to keysyms via libxkbcommon, handles special controls.

use std::process::Stdio;

use tokio::sync::mpsc;
use xkbcommon::xkb;
use xkbcommon::xkb::keysyms;

use crate::dbus::SharedState;
use crate::desktop::DesktopController;

/// Keysym constants for special control characters.
const XKB_KEY_RETURN: i32 = 0xff0d;
const XKB_KEY_TAB: i32 = 0xff09;

/// Keysym constants for clipboard paste (Ctrl+V).
const XKB_KEY_CONTROL_L: i32 = 0xffe3;
const XKB_KEY_V_LOWER: i32 = 0x0076;

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

                match paste_text(&desktop, &text).await {
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

/// Paste text via the Wayland clipboard (wl-copy) and Ctrl+V through the portal.
async fn paste_text(
    desktop: &DesktopController,
    text: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Set clipboard content via wl-copy
    let mut child = tokio::process::Command::new("wl-copy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to run wl-copy (is wl-clipboard installed?): {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(text.as_bytes()).await?;
    }

    let status = child.wait().await?;
    if !status.success() {
        return Err("wl-copy failed".into());
    }

    // Brief pause to let the clipboard settle
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Simulate Ctrl+V
    desktop.press_keysym(XKB_KEY_CONTROL_L).await?;
    desktop.tap_keysym(XKB_KEY_V_LOWER).await?;
    desktop.release_keysym(XKB_KEY_CONTROL_L).await?;

    Ok(())
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
