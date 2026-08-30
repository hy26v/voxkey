// ABOUTME: Curated metadata for speech models Voxkey can install and run itself.
// ABOUTME: Shared by the daemon and settings UI so model IDs and presentation never drift.

/// How Voxkey executes a downloaded model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalModelRuntime {
    /// Decode a completed recording with sherpa-onnx's offline recognizer.
    OfflineTransducer,
    /// Decode microphone chunks with sherpa-onnx's online recognizer.
    OnlineTransducer,
}

impl LocalModelRuntime {
    pub const fn label(self) -> &'static str {
        match self {
            Self::OfflineTransducer => "runs when recording ends",
            Self::OnlineTransducer => "runs live while you speak",
        }
    }
}

/// Stable, user-facing facts for one installable model.
///
/// Download locations and checksums deliberately remain private to the daemon.
/// The settings process only needs this small interface to render and select
/// models; it never needs to understand how artifacts are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalModel {
    pub id: &'static str,
    pub name: &'static str,
    pub family: &'static str,
    pub description: &'static str,
    pub language_summary: &'static str,
    pub download_bytes: u64,
    pub approximate_ram_bytes: u64,
    pub license: &'static str,
    pub license_url: &'static str,
    pub source_url: &'static str,
    pub released: &'static str,
    pub runtime: LocalModelRuntime,
    pub badge: Option<&'static str>,
}

impl LocalModel {
    pub fn download_size(&self) -> String {
        readable_size(self.download_bytes)
    }

    pub fn approximate_ram(&self) -> String {
        readable_size(self.approximate_ram_bytes)
    }

    pub fn facts(&self) -> String {
        format!(
            "{} · {} download · {} RAM · {}",
            self.language_summary,
            self.download_size(),
            self.approximate_ram(),
            self.license
        )
    }
}

fn readable_size(bytes: u64) -> String {
    const MB: f64 = 1_000_000.0;
    const GB: f64 = 1_000_000_000.0;
    if bytes >= GB as u64 {
        format!("{:.1} GB", bytes as f64 / GB)
    } else {
        format!("{} MB", (bytes as f64 / MB).round() as u64)
    }
}

/// Models are ordered for selection: the current best live multilingual
/// choice first, followed by the live English model and established offline
/// models. Every entry has an immutable, checksum-verified artifact manifest
/// in the daemon.
pub const LOCAL_MODELS: &[LocalModel] = &[
    LocalModel {
        id: "nemotron-3.5-asr-streaming-0.6b",
        name: "Nemotron 3.5",
        family: "NVIDIA Nemotron",
        description: "Current multilingual model with automatic language detection and text that appears while you speak.",
        language_summary: "35 languages / 40 locales",
        download_bytes: 682_215_356,
        approximate_ram_bytes: 1_500_000_000,
        license: "OpenMDW 1.1",
        license_url: "https://openmdw.ai/license/1-1/",
        source_url: "https://huggingface.co/nvidia/nemotron-3.5-asr-streaming-0.6b",
        released: "June 2026",
        runtime: LocalModelRuntime::OnlineTransducer,
        badge: Some("Newest"),
    },
    LocalModel {
        id: "parakeet-unified-en-0.6b",
        name: "Parakeet Unified",
        family: "NVIDIA Parakeet",
        description: "New English model designed for accurate low-latency dictation with punctuation and capitalization.",
        language_summary: "English",
        download_bytes: 663_048_978,
        approximate_ram_bytes: 1_200_000_000,
        license: "NVIDIA Open Model",
        license_url: "https://www.nvidia.com/en-us/agreements/enterprise-software/nvidia-open-model-license/",
        source_url: "https://huggingface.co/nvidia/parakeet-unified-en-0.6b",
        released: "April 2026",
        runtime: LocalModelRuntime::OnlineTransducer,
        badge: Some("Best English"),
    },
    LocalModel {
        id: "parakeet-tdt-0.6b-v3",
        name: "Parakeet v3",
        family: "NVIDIA Parakeet",
        description: "Fast, established multilingual transcription with automatic punctuation, capitalization, and language detection.",
        language_summary: "25 European languages",
        download_bytes: 670_478_772,
        approximate_ram_bytes: 1_100_000_000,
        license: "CC BY 4.0",
        license_url: "https://creativecommons.org/licenses/by/4.0/",
        source_url: "https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3",
        released: "May 2025",
        runtime: LocalModelRuntime::OfflineTransducer,
        badge: Some("Reliable"),
    },
    LocalModel {
        id: "parakeet-tdt-0.6b-v2",
        name: "Parakeet v2",
        family: "NVIDIA Parakeet",
        description: "Mature English-only transcription with strong accuracy and low processor requirements.",
        language_summary: "English",
        download_bytes: 661_190_513,
        approximate_ram_bytes: 1_100_000_000,
        license: "CC BY 4.0",
        license_url: "https://creativecommons.org/licenses/by/4.0/",
        source_url: "https://huggingface.co/nvidia/parakeet-tdt-0.6b-v2",
        released: "April 2025",
        runtime: LocalModelRuntime::OfflineTransducer,
        badge: None,
    },
];

pub fn local_model(id: &str) -> Option<&'static LocalModel> {
    LOCAL_MODELS.iter().find(|model| model.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_are_unique_and_safe_directory_names() {
        for (index, model) in LOCAL_MODELS.iter().enumerate() {
            assert!(!model.id.is_empty());
            assert!(model.id.bytes().all(|byte| byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'.')));
            assert!(
                !LOCAL_MODELS[..index]
                    .iter()
                    .any(|candidate| candidate.id == model.id)
            );
        }
    }

    #[test]
    fn every_model_has_actionable_user_facing_metadata() {
        for model in LOCAL_MODELS {
            assert!(!model.name.is_empty());
            assert!(!model.description.is_empty());
            assert!(model.download_bytes > 0);
            assert!(model.approximate_ram_bytes > 0);
            assert!(model.license_url.starts_with("https://"));
            assert!(model.source_url.starts_with("https://"));
        }
    }

    #[test]
    fn sizes_are_readable_without_false_precision() {
        assert_eq!(readable_size(670_478_772), "670 MB");
        assert_eq!(readable_size(1_500_000_000), "1.5 GB");
    }
}
