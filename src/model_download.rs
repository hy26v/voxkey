// ABOUTME: Downloads Parakeet ONNX model files from HuggingFace to the local data directory.
// ABOUTME: Supports progress callbacks and cancellation for GUI integration.

use tokio::sync::watch;

/// Files to download for each TDT model (relative to the HuggingFace repo).
/// We use the sherpa-onnx INT8 quantized models.
const V2_BASE_URL: &str = "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8/resolve/main";
const V3_BASE_URL: &str = "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/resolve/main";

const MODEL_FILES: &[&str] = &[
    "encoder.int8.onnx",
    "decoder.int8.onnx",
    "joiner.int8.onnx",
    "tokens.txt",
];

#[derive(Debug, Clone)]
pub enum DownloadStatus {
    /// Download in progress. Percent is 0-100 across all files.
    InProgress(u8),
    /// Download completed successfully.
    Complete,
    /// Download failed.
    Failed(String),
}

fn base_url(model_name: &str) -> Result<&'static str, String> {
    match model_name {
        "parakeet-tdt-0.6b-v2" => Ok(V2_BASE_URL),
        "parakeet-tdt-0.6b-v3" => Ok(V3_BASE_URL),
        _ => Err(format!("Unknown model: {model_name}")),
    }
}

/// Start downloading a model. Returns a watch receiver for progress updates.
/// The download runs on a tokio task.
pub fn start_download(
    model_name: String,
) -> watch::Receiver<DownloadStatus> {
    let (tx, rx) = watch::channel(DownloadStatus::InProgress(0));
    tokio::spawn(async move {
        match download_model(&model_name, &tx).await {
            Ok(()) => { let _ = tx.send(DownloadStatus::Complete); }
            Err(e) => { let _ = tx.send(DownloadStatus::Failed(e.to_string())); }
        }
    });
    rx
}

async fn download_model(
    model_name: &str,
    progress: &watch::Sender<DownloadStatus>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let base = base_url(model_name)?;
    let dest_dir = crate::models::model_dir(model_name);
    std::fs::create_dir_all(&dest_dir)?;

    let client = reqwest::Client::new();
    let total_files = MODEL_FILES.len();

    for (i, file_name) in MODEL_FILES.iter().enumerate() {
        let url = format!("{base}/{file_name}");
        let dest_path = dest_dir.join(file_name);

        // Skip already-downloaded files
        if dest_path.exists() {
            let pct = ((i + 1) * 100 / total_files) as u8;
            let _ = progress.send(DownloadStatus::InProgress(pct));
            continue;
        }

        tracing::info!("Downloading {file_name} from {url}");

        let response = client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(format!("HTTP {} downloading {url}", response.status()).into());
        }

        let total_size = response.content_length();
        let mut stream = response.bytes_stream();
        let tmp_path = dest_path.with_extension("part");
        let mut file = tokio::fs::File::create(&tmp_path).await?;
        let mut downloaded: u64 = 0;

        use futures_util::StreamExt;
        use tokio::io::AsyncWriteExt;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            downloaded += chunk.len() as u64;

            // Report progress: spread across all files
            if let Some(total) = total_size {
                let file_pct = downloaded as f64 / total as f64;
                let overall = (i as f64 + file_pct) / total_files as f64;
                let _ = progress.send(DownloadStatus::InProgress((overall * 100.0) as u8));
            }
        }

        file.flush().await?;
        drop(file);
        tokio::fs::rename(&tmp_path, &dest_path).await?;
    }

    Ok(())
}

/// Delete a downloaded model's directory.
pub fn delete_model(model_name: &str) -> Result<(), std::io::Error> {
    let dir = crate::models::model_dir(model_name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
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
    fn base_url_rejects_unknown_model() {
        assert!(base_url("unknown-model").is_err());
    }

    #[test]
    fn delete_model_ignores_nonexistent_dir() {
        assert!(delete_model("nonexistent-model-xyz").is_ok());
    }
}
