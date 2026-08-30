// ABOUTME: Persists Voxkey's private user data through atomic, owner-only files.
// ABOUTME: Handles restore-token loading, validation, rotation, and recovery.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Atomically replace `path` with owner-only `contents`.
///
/// The sibling temporary file is private before any bytes are written. This
/// prevents API keys, transcripts, portal credentials, and vocabulary from
/// passing through a world-readable destination or stale scratch file. The
/// final rename also keeps the previous complete file visible until the new
/// contents have been flushed successfully.
pub fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if parent != Path::new(".") {
        fs::create_dir_all(parent)?;
    }

    let mut temporary = tempfile::Builder::new()
        .prefix(".voxkey-private-")
        .permissions(fs::Permissions::from_mode(0o600))
        .tempfile_in(parent)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temporary.write_all(contents)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    // Persist the directory entry as well as the file contents. Without this,
    // a power loss can forget the successful rename even though the new file
    // itself was fsynced.
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Atomically copy a file to an owner-only destination without loading it all
/// into memory. This is used for recoverable recordings, which may be tens of
/// megabytes long.
pub fn copy_private(source: &Path, destination: &Path) -> std::io::Result<()> {
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if parent != Path::new(".") {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }

    let mut input = fs::File::open(source)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".voxkey-private-recording-")
        .permissions(fs::Permissions::from_mode(0o600))
        .tempfile_in(parent)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    std::io::copy(&mut input, &mut temporary)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(destination)
        .map_err(|error| error.error)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Load a restore token from disk, returning None if missing or unreadable.
pub fn load_restore_token(path: &Path) -> Option<String> {
    match fs::read_to_string(path) {
        Ok(content) => {
            if content.trim().is_empty() {
                tracing::info!("Restore token file is empty, starting fresh");
                None
            } else if content.contains('\0') {
                // Restore tokens are sent as D-Bus strings, whose wire format
                // forbids embedded NUL bytes. Passing one to zbus can make the
                // bus close the daemon connection before the tokenless portal
                // retry has a chance to run.
                tracing::warn!("Restore token contains an embedded NUL (will start fresh)");
                None
            } else {
                tracing::info!("Loaded restore token from {}", path.display());
                Some(content)
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("No restore token file found, starting fresh");
            None
        }
        Err(e) => {
            tracing::warn!("Failed to read restore token (will start fresh): {e}");
            None
        }
    }
}

/// Save a restore token to disk with 0600 permissions.
pub fn save_restore_token(
    path: &Path,
    token: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if token.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "restore token must not be blank",
        )
        .into());
    }

    write_private(path, token.as_bytes())?;

    tracing::info!("Saved restore token to {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn private_writes_never_reuse_a_world_readable_destination() {
        use std::io::Read;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private-data");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let mut exposed_reader = fs::File::open(&path).unwrap();

        write_private(&path, b"new secret").unwrap();

        let mut exposed = String::new();
        exposed_reader.read_to_string(&mut exposed).unwrap();
        assert_eq!(exposed, "old");
        assert_eq!(fs::read_to_string(&path).unwrap(), "new secret");
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn private_copy_is_atomic_and_owner_only() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.wav");
        let destination = temp.path().join("private/recording.wav");
        fs::write(&source, b"captured audio").unwrap();

        copy_private(&source, &destination).unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"captured audio");
        assert_eq!(mode(&destination), 0o600);
        assert_eq!(mode(destination.parent().unwrap()), 0o700);
        assert_eq!(fs::read(&source).unwrap(), b"captured audio");
    }

    #[test]
    fn private_copy_never_overwrites_an_existing_recording() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.wav");
        let destination = temp.path().join("recording.wav");
        fs::write(&source, b"new audio").unwrap();
        fs::write(&destination, b"existing audio").unwrap();

        assert!(copy_private(&source, &destination).is_err());
        assert_eq!(fs::read(&destination).unwrap(), b"existing audio");
    }

    #[test]
    fn failed_private_publication_leaves_no_scratch_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private-data");
        fs::create_dir(&path).unwrap();

        assert!(write_private(&path, b"secret").is_err());

        let entries = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, ["private-data"]);
    }

    #[test]
    fn saving_restore_token_replaces_the_file_atomically() {
        use std::io::Read;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("restore_token");
        fs::write(&path, "old-token").unwrap();
        let mut old_reader = fs::File::open(&path).unwrap();

        save_restore_token(&path, "new-token").unwrap();

        let mut old_contents = String::new();
        old_reader.read_to_string(&mut old_contents).unwrap();
        assert_eq!(old_contents, "old-token");
        assert_eq!(fs::read_to_string(&path).unwrap(), "new-token");
    }

    #[test]
    fn blank_restore_token_cannot_replace_a_valid_credential() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("restore_token");
        fs::write(&path, "valid-token").unwrap();

        assert!(save_restore_token(&path, " \t\n").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "valid-token");
    }

    #[test]
    fn nonblank_restore_token_round_trips_without_normalization() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("restore_token");
        let token = " token payload with significant whitespace \n";

        save_restore_token(&path, token).unwrap();

        assert_eq!(load_restore_token(&path).as_deref(), Some(token));
    }

    #[test]
    fn a_restore_token_with_an_embedded_nul_is_ignored_without_deleting_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("restore_token");
        let invalid = b"invalid\0portal-token";
        fs::write(&path, invalid).unwrap();

        assert_eq!(load_restore_token(&path), None);
        assert_eq!(
            fs::read(&path).unwrap(),
            invalid,
            "validation must not destroy a token before a fresh portal session succeeds"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_temporarily_unreadable_restore_token_is_not_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("restore_token");
        fs::write(&path, "still-valid-token").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

        assert_eq!(load_restore_token(&path), None);
        assert!(
            path.exists(),
            "a read error must not destroy a token that may become readable again"
        );
    }
}
