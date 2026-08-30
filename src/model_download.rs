// ABOUTME: Downloads Parakeet ONNX model files from HuggingFace to the local data directory.
// ABOUTME: Supports progress callbacks and cancellation for GUI integration.

use std::time::Duration;
use tokio::sync::watch;

const DOWNLOAD_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const DOWNLOAD_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_MODEL_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

#[derive(Debug, Clone)]
pub enum DownloadStatus {
    /// Download in progress. Percent is 0-100 across all files.
    InProgress(u8),
    /// Download completed successfully.
    Complete,
    /// Download failed.
    Failed(String),
}

impl DownloadStatus {
    pub fn reported_percent(&self) -> Option<u8> {
        match self {
            Self::InProgress(percent) => Some((*percent).min(99)),
            Self::Complete => Some(100),
            Self::Failed(_) => None,
        }
    }
}

fn download_progress_percent(
    completed_bytes: u64,
    total_bytes: u64,
    current_file_downloaded: u64,
) -> Option<u8> {
    if total_bytes == 0 {
        return None;
    }
    let downloaded = completed_bytes
        .saturating_add(current_file_downloaded)
        .min(total_bytes);
    Some(((u128::from(downloaded) * 100) / u128::from(total_bytes)) as u8)
}

fn base_url(model_name: &str) -> Result<String, String> {
    let manifest = crate::models::manifest(model_name)
        .ok_or_else(|| format!("Unknown model: {model_name}"))?;
    Ok(format!(
        "https://huggingface.co/{}/resolve/{}",
        manifest.repository, manifest.revision
    ))
}

/// Validate a model name against Voxkey's immutable download catalogue.
/// D-Bus callers use this before spawning work so invalid input is reported
/// synchronously instead of becoming a later desktop notification.
pub fn validate_model_name(model_name: &str) -> Result<(), String> {
    base_url(model_name).map(|_| ())
}

fn can_skip_download(
    path: &std::path::Path,
    expected: Option<crate::models::ModelArtifact>,
) -> bool {
    match expected {
        Some(expected) => crate::models::verify_artifact(path, expected).unwrap_or(false),
        None => std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.file_type().is_file() && metadata.len() > 0),
    }
}

struct PartialDownload {
    path: std::path::PathBuf,
    published: bool,
}

impl PartialDownload {
    fn new(path: std::path::PathBuf) -> Self {
        Self {
            path,
            published: false,
        }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn mark_published(&mut self) {
        self.published = true;
    }
}

impl Drop for PartialDownload {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => tracing::info!(
                "Removed incomplete model download at {}",
                self.path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                "Failed to discard the partial download at {}: {error}",
                self.path.display()
            ),
        }
    }
}

/// Start downloading a model. Returns a watch receiver for progress updates.
/// The download runs on a tokio task.
pub fn start_download(model_name: String) -> watch::Receiver<DownloadStatus> {
    let (tx, rx) = watch::channel(DownloadStatus::InProgress(0));
    tokio::spawn(async move {
        match download_model(&model_name, &tx).await {
            Ok(()) => {
                let _ = tx.send(DownloadStatus::Complete);
            }
            Err(e) => {
                let _ = tx.send(DownloadStatus::Failed(e.to_string()));
            }
        }
    });
    rx
}

async fn download_model(
    model_name: &str,
    progress: &watch::Sender<DownloadStatus>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let base = base_url(model_name)?;
    let manifest = crate::models::manifest(model_name)
        .ok_or_else(|| format!("Unknown model: {model_name}"))?;
    let dest_dir = crate::models::model_dir(model_name);
    prepare_model_directory(&dest_dir)?;

    let client = reqwest::Client::new();
    let total_bytes = manifest
        .artifacts
        .iter()
        .fold(0_u64, |total, artifact| total.saturating_add(artifact.size));
    let mut completed_bytes = 0_u64;

    for artifact in manifest.artifacts.iter().copied() {
        let file_name = artifact.name;
        let url = format!("{base}/{file_name}");
        let dest_path = dest_dir.join(file_name);

        // Skip already-downloaded files
        if can_skip_download(&dest_path, Some(artifact)) {
            completed_bytes = completed_bytes.saturating_add(artifact.size);
            if let Some(percent) = download_progress_percent(completed_bytes, total_bytes, 0) {
                let _ = progress.send(DownloadStatus::InProgress(percent));
            }
            continue;
        }

        tracing::info!("Downloading {file_name} from {url}");

        let completed_before_file = completed_bytes;
        download_file(
            &client,
            &url,
            &dest_path,
            Some(artifact),
            &mut |downloaded, _total| {
                if let Some(percent) =
                    download_progress_percent(completed_before_file, total_bytes, downloaded)
                {
                    let _ = progress.send(DownloadStatus::InProgress(percent));
                }
            },
        )
        .await?;
        completed_bytes = completed_bytes.saturating_add(artifact.size);
    }

    Ok(())
}

fn prepare_model_directory(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => return Ok(()),
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "model download path is not a real directory",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    std::fs::create_dir_all(path)?;
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir()) {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "model download path is not a real directory",
        ))
    }
}

/// Fetch one file to `dest_path`, reporting bytes received as they arrive.
///
/// The transfer lands in a scratch file that is removed if anything goes
/// wrong, so a connection dropped part way through cannot leave hundreds of
/// megabytes of unusable data in the model directory where nothing would ever
/// clean it up. Only a complete file is moved into place.
async fn download_file(
    client: &reqwest::Client,
    url: &str,
    dest_path: &std::path::Path,
    expected: Option<crate::models::ModelArtifact>,
    progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let response =
        wait_for_download_response(client.get(url).send(), DOWNLOAD_RESPONSE_TIMEOUT).await?;
    if !response.status().is_success() {
        return Err(format!("HTTP {} downloading {url}", response.status()).into());
    }

    let total_size = response.content_length();
    if let Some(expected) = expected
        && total_size.is_some_and(|size| size != expected.size)
    {
        return Err(format!(
            "{} has unexpected size (expected {} bytes, server advertised {:?})",
            expected.name, expected.size, total_size
        )
        .into());
    }
    let mut partial = PartialDownload::new(dest_path.with_extension("part"));

    let transfer = receive_to_file(response, partial.path(), total_size, progress).await;
    transfer?;

    if let Some(expected) = expected {
        let partial_path = partial.path().to_path_buf();
        let verified = tokio::task::spawn_blocking(move || {
            crate::models::verify_artifact(&partial_path, expected)
        })
        .await??;
        if !verified {
            return Err(format!(
                "Downloaded {} did not match Voxkey's pinned size and SHA-256",
                expected.name
            )
            .into());
        }
    }

    publish_partial(partial.path(), dest_path).await?;
    partial.mark_published();
    Ok(())
}

async fn wait_for_download_response<F, T, E>(
    request: F,
    timeout: Duration,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>>
where
    F: std::future::Future<Output = Result<T, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    match tokio::time::timeout(timeout, request).await {
        Ok(result) => {
            result.map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
        }
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "download response timed out after {:.1}s",
                timeout.as_secs_f32()
            ),
        )
        .into()),
    }
}

async fn publish_partial(
    partial_path: &std::path::Path,
    dest_path: &std::path::Path,
) -> std::io::Result<()> {
    match tokio::fs::rename(partial_path, dest_path).await {
        Ok(()) => {
            let Some(parent) = dest_path.parent() else {
                return Ok(());
            };
            if let Err(error) = sync_directory(parent).await {
                if let Err(cleanup) = tokio::fs::remove_file(dest_path).await {
                    tracing::warn!(
                        "Failed to discard model file after directory sync failed at {}: {cleanup}",
                        dest_path.display()
                    );
                }
                return Err(error);
            }
            Ok(())
        }
        Err(error) => {
            if let Err(cleanup) = tokio::fs::remove_file(partial_path).await {
                tracing::warn!(
                    "Failed to discard unpublished download at {}: {cleanup}",
                    partial_path.display()
                );
            }
            Err(error)
        }
    }
}

async fn sync_directory(path: &std::path::Path) -> std::io::Result<()> {
    let directory = tokio::fs::File::open(path).await?;
    directory.sync_all().await
}

async fn receive_to_file(
    response: reqwest::Response,
    partial_path: &std::path::Path,
    total_size: Option<u64>,
    progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    receive_to_file_with_idle_timeout(
        response,
        partial_path,
        total_size,
        progress,
        DOWNLOAD_IDLE_TIMEOUT,
    )
    .await
}

async fn receive_to_file_with_idle_timeout(
    response: reqwest::Response,
    partial_path: &std::path::Path,
    total_size: Option<u64>,
    progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
    idle_timeout: Duration,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    if let Some(total_size) = total_size {
        validate_model_file_size(total_size)?;
    }
    let mut stream = response.bytes_stream();
    let mut file = create_partial_file(partial_path).await?;
    let mut downloaded: u64 = 0;

    loop {
        let next = tokio::time::timeout(idle_timeout, stream.next())
            .await
            .map_err(|_| {
                format!(
                    "download stalled for {:.1}s without receiving data",
                    idle_timeout.as_secs_f32()
                )
            })?;
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk?;
        let next_downloaded = checked_downloaded_size(downloaded, chunk.len())?;
        file.write_all(&chunk).await?;
        downloaded = next_downloaded;
        progress(downloaded, total_size);
    }

    validate_downloaded_size(downloaded, total_size)?;
    file.flush().await?;
    // The rename below publishes the file. Flush both userspace buffers and
    // the kernel's dirty pages first so a completed model cannot become an
    // empty/truncated artifact after power loss.
    file.sync_all().await?;
    Ok(())
}

fn checked_downloaded_size(
    downloaded: u64,
    chunk_size: usize,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let downloaded = downloaded
        .checked_add(chunk_size as u64)
        .ok_or("downloaded artifact size overflowed")?;
    validate_model_file_size(downloaded)?;
    Ok(downloaded)
}

fn validate_model_file_size(size: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if size > MAX_MODEL_FILE_BYTES {
        return Err(format!(
            "downloaded artifact exceeds the {MAX_MODEL_FILE_BYTES}-byte file size limit"
        )
        .into());
    }
    Ok(())
}

async fn create_partial_file(path: &std::path::Path) -> std::io::Result<tokio::fs::File> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
}

fn validate_downloaded_size(
    downloaded: u64,
    expected: Option<u64>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if downloaded == 0 {
        return Err("downloaded artifact is empty".into());
    }
    if let Some(expected) = expected
        && downloaded != expected
    {
        return Err(format!(
            "downloaded artifact size mismatch: received {downloaded} of {expected} bytes"
        )
        .into());
    }
    Ok(())
}

/// Delete a downloaded model's directory.
pub fn delete_model(model_name: &str) -> Result<(), std::io::Error> {
    delete_model_from(&crate::models::models_dir(), model_name)
}

fn delete_model_from(models_dir: &std::path::Path, model_name: &str) -> Result<(), std::io::Error> {
    // Deletion is intentionally narrower than path validation: only a model
    // in Voxkey's immutable catalogue is ever a valid target. This prevents a
    // future caller from turning an arbitrary file below models_dir into a
    // deletion target merely by supplying one safe-looking path component.
    base_url(model_name)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;

    let dir = models_dir.join(model_name);
    match std::fs::symlink_metadata(&dir) {
        Ok(metadata) if metadata.file_type().is_dir() => std::fs::remove_dir_all(&dir)?,
        Ok(_) => std::fs::remove_file(&dir)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_resolves_v2() {
        assert!(base_url("parakeet-tdt-0.6b-v2").unwrap().contains("v2"));
    }

    #[test]
    fn base_url_resolves_v3() {
        assert!(base_url("parakeet-tdt-0.6b-v3").unwrap().contains("v3"));
    }

    #[test]
    fn base_urls_resolve_every_streaming_library_model() {
        let unified = base_url("parakeet-unified-en-0.6b").unwrap();
        assert!(unified.contains("streaming-560ms"));
        assert!(unified.contains("7551fd26fc810cc1e4e043e608db4d13b59be31e"));

        let nemotron = base_url("nemotron-3.5-asr-streaming-0.6b").unwrap();
        assert!(nemotron.contains("nemotron-3.5-asr-streaming"));
        assert!(nemotron.contains("ab43d895f5985b1bbab8b6eac8607fcdc05343f3"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_artifacts_are_not_treated_as_completed_downloads() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside.onnx");
        let artifact = temp.path().join("encoder.int8.onnx");
        std::fs::write(&outside, b"model data").unwrap();
        symlink(&outside, &artifact).unwrap();

        assert!(!can_skip_download(&artifact, None));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cannot_redirect_model_downloads_outside_the_models_directory() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        let model_dir = temp.path().join("parakeet-tdt-0.6b-v3");
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, &model_dir).unwrap();

        assert!(prepare_model_directory(&model_dir).is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn base_url_rejects_unknown_model() {
        assert!(base_url("unknown-model").is_err());
    }

    #[test]
    fn delete_model_ignores_nonexistent_dir() {
        let temp = tempfile::tempdir().unwrap();
        assert!(delete_model_from(temp.path(), "parakeet-tdt-0.6b-v3").is_ok());
    }

    #[test]
    fn delete_model_rejects_paths_outside_the_models_directory() {
        let temp = tempfile::tempdir().unwrap();
        let models = temp.path().join("models");
        let sibling = temp.path().join("keep-me");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("important"), b"data").unwrap();

        let error = delete_model_from(&models, "../keep-me")
            .expect_err("a model name must not escape the models directory");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(sibling.join("important").exists());
    }

    #[test]
    fn delete_model_rejects_an_uncatalogued_file_inside_the_models_directory() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("not-a-voxkey-model");
        std::fs::write(&path, b"important").unwrap();

        let error = delete_model_from(temp.path(), "not-a-voxkey-model")
            .expect_err("an uncatalogued name must never become a deletion target");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(std::fs::read(&path).unwrap(), b"important");
    }

    #[cfg(unix)]
    #[test]
    fn delete_model_removes_symlink_entries_without_following_them() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let models = temp.path().join("models");
        let outside = temp.path().join("outside");
        std::fs::create_dir(&models).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("important"), b"keep").unwrap();

        symlink(&outside, models.join("parakeet-tdt-0.6b-v2")).unwrap();
        delete_model_from(&models, "parakeet-tdt-0.6b-v2").unwrap();
        assert!(outside.join("important").exists());
        assert!(std::fs::symlink_metadata(models.join("parakeet-tdt-0.6b-v2")).is_err());

        symlink(
            temp.path().join("missing"),
            models.join("parakeet-tdt-0.6b-v3"),
        )
        .unwrap();
        delete_model_from(&models, "parakeet-tdt-0.6b-v3").unwrap();
        assert!(std::fs::symlink_metadata(models.join("parakeet-tdt-0.6b-v3")).is_err());
    }

    #[test]
    fn an_empty_existing_artifact_must_be_downloaded_again() {
        let temp = tempfile::tempdir().unwrap();
        let artifact = temp.path().join("encoder.int8.onnx");
        std::fs::write(&artifact, b"").unwrap();

        assert!(!can_skip_download(&artifact, None));
    }

    #[test]
    fn a_same_size_corrupt_artifact_must_be_downloaded_again() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("artifact");
        std::fs::write(&path, b"model datb").unwrap();
        let expected = crate::models::ModelArtifact {
            name: "artifact",
            size: 10,
            sha256: "6dbdb6a147ad4d808455652bf5a10120161678395f6bfbd21eb6fe4e731aceeb",
        };

        assert!(!can_skip_download(&path, Some(expected)));
    }

    #[test]
    fn a_zero_byte_response_is_not_a_complete_artifact() {
        let error = validate_downloaded_size(0, Some(0))
            .expect_err("an empty response must not be published as a model file");
        assert!(error.to_string().contains("empty"), "{error}");
    }

    #[test]
    fn a_short_response_is_not_published_as_a_complete_model_file() {
        let error = validate_downloaded_size(9, Some(5_000))
            .expect_err("a response shorter than Content-Length must not be published");

        assert!(error.to_string().contains("5000"), "{error}");
    }

    #[test]
    fn a_model_file_cannot_grow_past_the_download_limit() {
        let error = checked_downloaded_size(MAX_MODEL_FILE_BYTES - 1, 2)
            .expect_err("an unbounded response must not fill the disk indefinitely");

        assert!(error.to_string().contains("limit"), "{error}");
    }

    #[test]
    fn only_completed_downloads_report_one_hundred_percent() {
        assert_eq!(DownloadStatus::InProgress(100).reported_percent(), Some(99));
        assert_eq!(DownloadStatus::Complete.reported_percent(), Some(100));
        assert_eq!(
            DownloadStatus::Failed("network error".to_string()).reported_percent(),
            None
        );
    }

    #[test]
    fn a_zero_model_size_does_not_produce_download_progress() {
        assert_eq!(download_progress_percent(0, 0, 1), None);
    }

    #[test]
    fn model_progress_is_weighted_by_downloaded_bytes() {
        assert_eq!(download_progress_percent(0, 1_000, 500), Some(50));
        assert_eq!(download_progress_percent(900, 1_000, 50), Some(95));
        assert_eq!(download_progress_percent(900, 1_000, 500), Some(100));
    }

    #[tokio::test]
    async fn a_failed_publish_removes_the_partial_file() {
        let temp = tempfile::tempdir().unwrap();
        let partial = temp.path().join("encoder.int8.part");
        let destination = temp.path().join("encoder.int8.onnx");
        std::fs::write(&partial, b"complete model bytes").unwrap();
        std::fs::create_dir(&destination).unwrap();

        assert!(publish_partial(&partial, &destination).await.is_err());
        assert!(
            !partial.exists(),
            "failed publication left scratch data behind"
        );
    }

    #[test]
    fn cancelling_a_download_removes_its_partial_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("encoder.int8.part");
        std::fs::write(&path, b"incomplete model bytes").unwrap();

        let partial = PartialDownload::new(path.clone());
        drop(partial);

        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_partial_file_cannot_truncate_its_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("keep.bin");
        let partial = temp.path().join("encoder.int8.part");
        std::fs::write(&outside, b"keep this data").unwrap();
        symlink(&outside, &partial).unwrap();

        drop(create_partial_file(&partial).await.unwrap());

        assert_eq!(std::fs::read(&outside).unwrap(), b"keep this data");
        assert!(
            std::fs::symlink_metadata(&partial)
                .unwrap()
                .file_type()
                .is_file()
        );
    }

    /// Serve `body` after announcing `announced_length` bytes, then hang up.
    /// Announcing more than is sent simulates a connection dropped mid-file.
    async fn serve_once(body: &'static [u8], announced_length: usize) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await;
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                 Content-Length: {announced_length}\r\n\r\n"
            );
            let _ = socket.write_all(headers.as_bytes()).await;
            let _ = socket.write_all(body).await;
            let _ = socket.flush().await;
            drop(socket);
        });
        format!("http://{address}/encoder.int8.onnx")
    }

    #[tokio::test]
    async fn a_complete_download_lands_at_its_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("encoder.int8.onnx");
        let url = serve_once(b"model-bytes", 11).await;

        let mut seen = Vec::new();
        download_file(
            &reqwest::Client::new(),
            &url,
            &destination,
            None,
            &mut |downloaded, total| seen.push((downloaded, total)),
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read(&destination).unwrap(), b"model-bytes");
        assert_eq!(seen.last(), Some(&(11, Some(11))));
        assert!(!destination.with_extension("part").exists());
    }

    /// A dropped connection must not leave a partial model file behind: it can
    /// be hundreds of megabytes and nothing would ever clean it up.
    #[tokio::test]
    async fn an_interrupted_download_leaves_nothing_behind() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("encoder.int8.onnx");
        // Promises 5000 bytes, delivers 9, then closes the connection.
        let url = serve_once(b"truncated", 5000).await;

        let error = download_file(
            &reqwest::Client::new(),
            &url,
            &destination,
            None,
            &mut |_, _| {},
        )
        .await
        .expect_err("a truncated response must fail the download");

        assert!(
            !destination.exists(),
            "an incomplete file was published: {error}"
        );
        assert!(
            !destination.with_extension("part").exists(),
            "a partial download was left on disk"
        );
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            0,
            "the model directory must be left clean"
        );
    }

    #[tokio::test]
    async fn a_stalled_download_hits_its_idle_timeout() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = socket.read(&mut [0_u8; 2048]).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nx")
                .await
                .unwrap();
            socket.flush().await.unwrap();
            std::future::pending::<()>().await;
        });
        let response = reqwest::Client::new()
            .get(format!("http://{address}/stalled.onnx"))
            .send()
            .await
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let partial = directory.path().join("stalled.part");
        let outcome = tokio::time::timeout(
            Duration::from_millis(100),
            receive_to_file_with_idle_timeout(
                response,
                &partial,
                Some(2),
                &mut |_, _| {},
                Duration::from_millis(10),
            ),
        )
        .await;

        assert!(
            matches!(outcome, Ok(Err(ref error)) if error.to_string().contains("stalled")),
            "download did not enforce its idle deadline: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_download_that_never_returns_headers_hits_its_response_timeout() {
        let never = std::future::pending::<Result<(), std::io::Error>>();
        let outcome = tokio::time::timeout(
            Duration::from_millis(100),
            wait_for_download_response(never, Duration::from_millis(10)),
        )
        .await;

        assert!(
            matches!(outcome, Ok(Err(ref error)) if error.to_string().contains("timed out")),
            "download response did not enforce its own deadline: {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_rejected_download_writes_no_file() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("encoder.int8.onnx");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = socket.read(&mut [0u8; 2048]).await;
            let _ = socket
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .await;
        });

        let error = download_file(
            &reqwest::Client::new(),
            &format!("http://{address}/missing.onnx"),
            &destination,
            None,
            &mut |_, _| {},
        )
        .await
        .expect_err("a 404 must fail the download");

        assert!(error.to_string().contains("404"), "{error}");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
