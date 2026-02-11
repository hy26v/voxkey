// ABOUTME: Manages the RemoteDesktop restore token on disk.
// ABOUTME: Handles saving with 0600 permissions, loading, rotation, and corrupt token recovery.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Load a restore token from disk, returning None if missing or unreadable.
pub fn load_restore_token(path: &Path) -> Option<String> {
    match fs::read_to_string(path) {
        Ok(content) => {
            let token = content.trim().to_string();
            if token.is_empty() {
                tracing::info!("Restore token file is empty, starting fresh");
                None
            } else {
                tracing::info!("Loaded restore token from {}", path.display());
                Some(token)
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("No restore token file found, starting fresh");
            None
        }
        Err(e) => {
            tracing::warn!("Failed to read restore token (will start fresh): {e}");
            // Remove corrupt file
            let _ = fs::remove_file(path);
            None
        }
    }
}

/// Save a restore token to disk with 0600 permissions.
pub fn save_restore_token(path: &Path, token: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(path, token)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;

    tracing::info!("Saved restore token to {}", path.display());
    Ok(())
}
