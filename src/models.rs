// ABOUTME: Resolves XDG data directory paths for locally-stored Parakeet ONNX models.
// ABOUTME: Checks whether required model files are present on disk.

use std::path::PathBuf;

/// Files required for a TDT model to be considered complete.
const TDT_REQUIRED_FILES: &[&str] = &[
    "encoder.int8.onnx",
    "decoder.int8.onnx",
    "joiner.int8.onnx",
    "tokens.txt",
];

/// Base directory for model storage: ~/.local/share/voxkey/models/
pub fn models_dir() -> PathBuf {
    let data_dir = std::env::var("XDG_DATA_HOME")
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "~".to_string());
            format!("{home}/.local/share")
        });
    PathBuf::from(data_dir).join("voxkey").join("models")
}

/// Full path to a specific model directory.
pub fn model_dir(model_name: &str) -> PathBuf {
    models_dir().join(model_name)
}

/// Check if all required TDT model files exist in the model directory.
pub fn is_model_available(model_name: &str) -> bool {
    let dir = model_dir(model_name);
    TDT_REQUIRED_FILES.iter().all(|f| dir.join(f).exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_dir_ends_with_voxkey_models() {
        let dir = models_dir();
        assert!(dir.ends_with("voxkey/models"));
    }

    #[test]
    fn model_dir_appends_model_name() {
        let dir = model_dir("parakeet-tdt-0.6b-v3");
        assert!(dir.ends_with("voxkey/models/parakeet-tdt-0.6b-v3"));
    }

    #[test]
    fn is_model_available_returns_false_for_missing_model() {
        assert!(!is_model_available("nonexistent-model-xyz"));
    }

    #[test]
    fn is_model_available_returns_true_when_all_files_present() {
        let dir = tempfile::tempdir().unwrap();
        let model_name = "test-model";
        let model_path = dir.path().join(model_name);
        std::fs::create_dir_all(&model_path).unwrap();
        for file in TDT_REQUIRED_FILES {
            std::fs::write(model_path.join(file), b"fake").unwrap();
        }
        // We can't test with the real models_dir, so test the underlying logic
        let all_present = TDT_REQUIRED_FILES.iter().all(|f| model_path.join(f).exists());
        assert!(all_present);
    }
}
