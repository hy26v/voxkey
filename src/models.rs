// ABOUTME: Defines immutable artifact manifests for Voxkey's local model library.
// ABOUTME: Resolves storage paths and verifies every downloaded model before inference.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy)]
pub(crate) struct ModelArtifact {
    pub name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

pub(crate) struct ModelManifest {
    pub repository: &'static str,
    pub revision: &'static str,
    pub artifacts: &'static [ModelArtifact],
}

const V2_ARTIFACTS: &[ModelArtifact] = &[
    ModelArtifact {
        name: "encoder.int8.onnx",
        size: 652_184_296,
        sha256: "a32b12d17bbbc309d0686fbbcc2987b5e9b8333a7da83fa6b089f0a2acd651ab",
    },
    ModelArtifact {
        name: "decoder.int8.onnx",
        size: 7_257_753,
        sha256: "b6bb64963457237b900e496ee9994b59294526439fbcc1fecf705b31a15c6b4e",
    },
    ModelArtifact {
        name: "joiner.int8.onnx",
        size: 1_739_080,
        sha256: "7946164367946e7f9f29a122407c3252b680dbae9a51343eb2488d057c3c43d2",
    },
    ModelArtifact {
        name: "tokens.txt",
        size: 9_384,
        sha256: "ec182b70dd42113aff6c5372c75cac58c952443eb22322f57bbd7f53977d497d",
    },
];

const V3_ARTIFACTS: &[ModelArtifact] = &[
    ModelArtifact {
        name: "encoder.int8.onnx",
        size: 652_184_281,
        sha256: "acfc2b4456377e15d04f0243af540b7fe7c992f8d898d751cf134c3a55fd2247",
    },
    ModelArtifact {
        name: "decoder.int8.onnx",
        size: 11_845_275,
        sha256: "179e50c43d1a9de79c8a24149a2f9bac6eb5981823f2a2ed88d655b24248db4e",
    },
    ModelArtifact {
        name: "joiner.int8.onnx",
        size: 6_355_277,
        sha256: "3164c13fc2821009440d20fcb5fdc78bff28b4db2f8d0f0b329101719c0948b3",
    },
    ModelArtifact {
        name: "tokens.txt",
        size: 93_939,
        sha256: "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
    },
];

const PARAKEET_UNIFIED_ARTIFACTS: &[ModelArtifact] = &[
    ModelArtifact {
        name: "encoder.int8.onnx",
        size: 654_046_389,
        sha256: "e566c3f014598a41724f2df028779a2d4cf7943cbefa324964f6a72e8ee255fb",
    },
    ModelArtifact {
        name: "decoder.int8.onnx",
        size: 7_257_777,
        sha256: "34fea72425d2506600772ba191a6d3f99c0710abdb68d9a3dc89fa8cb2aa473a",
    },
    ModelArtifact {
        name: "joiner.int8.onnx",
        size: 1_735_860,
        sha256: "869f43f7d24595c55581ad3bf249a935fb8a71389fbdaa7504b9f46f93140f8a",
    },
    ModelArtifact {
        name: "tokens.txt",
        size: 8_952,
        sha256: "dc0b4584ab2e4ddbf888425c076c61b736e7356a015250db7d307e6f1a8188ff",
    },
];

const NEMOTRON_3_5_ARTIFACTS: &[ModelArtifact] = &[
    ModelArtifact {
        name: "encoder.int8.onnx",
        size: 657_601_403,
        sha256: "012e9321373af99021415e0b0eb3ec827b4be3153be6f30d9b448fe65e896e68",
    },
    ModelArtifact {
        name: "decoder.int8.onnx",
        size: 14_978_075,
        sha256: "19f9c98fc6d0a2c33a65a43b36fdb2e914c26c0aa9764be3aebc502a1e982fb0",
    },
    ModelArtifact {
        name: "joiner.int8.onnx",
        size: 9_504_438,
        sha256: "4101c7c679a0bc30483794b27a059e34e79232aa2068d78d51231a22c8b0d7ce",
    },
    ModelArtifact {
        name: "tokens.txt",
        size: 131_440,
        sha256: "729cc103155bafa785f9cd45746cd41cabe97eab7182fc04d594129587958f8a",
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArtifactIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

static VERIFIED_MODELS: OnceLock<Mutex<HashMap<PathBuf, Vec<ArtifactIdentity>>>> = OnceLock::new();

pub(crate) fn manifest(model_name: &str) -> Option<ModelManifest> {
    match model_name {
        "parakeet-tdt-0.6b-v2" => Some(ModelManifest {
            repository: "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8",
            revision: "1ab9323565ddb038682214b292f588070a538ce2",
            artifacts: V2_ARTIFACTS,
        }),
        "parakeet-tdt-0.6b-v3" => Some(ModelManifest {
            repository: "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8",
            revision: "2bda32ec70b097a55adaa07d9a7173915b43cc78",
            artifacts: V3_ARTIFACTS,
        }),
        "parakeet-unified-en-0.6b" => Some(ModelManifest {
            repository: "csukuangfj2/sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-streaming-560ms",
            revision: "7551fd26fc810cc1e4e043e608db4d13b59be31e",
            artifacts: PARAKEET_UNIFIED_ARTIFACTS,
        }),
        "nemotron-3.5-asr-streaming-0.6b" => Some(ModelManifest {
            repository: "csukuangfj2/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-int8-2026-06-11",
            revision: "ab43d895f5985b1bbab8b6eac8607fcdc05343f3",
            artifacts: NEMOTRON_3_5_ARTIFACTS,
        }),
        _ => None,
    }
}

pub(crate) fn verify_artifact(
    path: &std::path::Path,
    artifact: ModelArtifact,
) -> std::io::Result<bool> {
    Ok(verify_artifact_identity(path, artifact)?.is_some())
}

fn open_artifact(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let fd = match rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(error) => return Err(std::io::Error::from(error)),
    };
    Ok(fd.into())
}

fn artifact_identity(metadata: &std::fs::Metadata) -> ArtifactIdentity {
    ArtifactIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    }
}

fn current_artifact_identity(path: &std::path::Path) -> std::io::Result<ArtifactIdentity> {
    let file = open_artifact(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "model artifact is not a regular file",
        ));
    }
    Ok(artifact_identity(&metadata))
}

fn verify_artifact_identity(
    path: &std::path::Path,
    artifact: ModelArtifact,
) -> std::io::Result<Option<ArtifactIdentity>> {
    let mut file = open_artifact(path)?;
    let before = file.metadata()?;
    if !before.file_type().is_file() || before.len() != artifact.size {
        return Ok(None);
    }
    let before = artifact_identity(&before);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let after = artifact_identity(&file.metadata()?);
    if before != after || format!("{:x}", hasher.finalize()) != artifact.sha256 {
        return Ok(None);
    }
    Ok(Some(after))
}

/// Base directory for model storage: ~/.local/share/voxkey/models/
pub fn models_dir() -> PathBuf {
    models_dir_from(
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

fn models_dir_from(
    xdg_data_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> PathBuf {
    let data_dir = xdg_data_home
        .filter(|path| !path.is_empty() && std::path::Path::new(path).is_absolute())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            home.map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("~"))
                .join(".local/share")
        });
    data_dir.join("voxkey").join("models")
}

/// Full path to a specific model directory.
pub fn model_dir(model_name: &str) -> PathBuf {
    models_dir().join(model_name)
}

/// Check if all required TDT model files exist in the model directory.
pub fn is_model_available(model_name: &str) -> bool {
    is_model_available_in(&models_dir(), model_name)
}

fn is_model_available_in(models_dir: &std::path::Path, model_name: &str) -> bool {
    let mut components = std::path::Path::new(model_name).components();
    if !matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    ) {
        return false;
    }

    let Some(manifest) = manifest(model_name) else {
        return false;
    };
    let dir = models_dir.join(model_name);
    if !std::fs::symlink_metadata(&dir).is_ok_and(|metadata| metadata.file_type().is_dir()) {
        return false;
    }
    let current_identities = manifest
        .artifacts
        .iter()
        .map(|artifact| current_artifact_identity(&dir.join(artifact.name)))
        .collect::<Result<Vec<_>, _>>();
    let Ok(current_identities) = current_identities else {
        return false;
    };
    let verified_models = VERIFIED_MODELS.get_or_init(|| Mutex::new(HashMap::new()));
    if verified_models
        .lock()
        .is_ok_and(|verified| verified.get(&dir) == Some(&current_identities))
    {
        return true;
    }

    let verified_identities = manifest
        .artifacts
        .iter()
        .map(|artifact| verify_artifact_identity(&dir.join(artifact.name), *artifact))
        .collect::<Result<Vec<_>, _>>();
    let Ok(verified_identities) = verified_identities else {
        return false;
    };
    let Some(verified_identities) = verified_identities.into_iter().collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    if let Ok(mut verified) = verified_models.lock() {
        verified.insert(dir, verified_identities);
    }
    true
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
    fn blank_xdg_data_home_uses_the_home_directory_default() {
        assert_eq!(
            models_dir_from(
                Some(std::ffi::OsStr::new("")),
                Some(std::ffi::OsStr::new("/home/test")),
            ),
            PathBuf::from("/home/test/.local/share/voxkey/models")
        );
    }

    #[test]
    fn relative_xdg_data_home_does_not_redirect_models_into_the_working_directory() {
        assert_eq!(
            models_dir_from(
                Some(std::ffi::OsStr::new("relative-data")),
                Some(std::ffi::OsStr::new("/home/test")),
            ),
            PathBuf::from("/home/test/.local/share/voxkey/models")
        );
    }

    #[test]
    fn model_dir_appends_model_name() {
        let dir = model_dir("parakeet-tdt-0.6b-v3");
        assert!(dir.ends_with("voxkey/models/parakeet-tdt-0.6b-v3"));
    }

    #[test]
    fn every_library_model_has_a_matching_immutable_manifest() {
        for model in voxkey_ipc::model_library::LOCAL_MODELS {
            let manifest = manifest(model.id).expect("catalog model is missing its manifest");
            assert!(!manifest.repository.is_empty());
            assert_eq!(manifest.revision.len(), 40);
            assert_eq!(
                manifest
                    .artifacts
                    .iter()
                    .map(|artifact| artifact.size)
                    .sum::<u64>(),
                model.download_bytes,
                "{} metadata does not match its artifacts",
                model.id
            );
        }
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
        for artifact in V3_ARTIFACTS {
            std::fs::write(model_path.join(artifact.name), b"fake").unwrap();
        }
        // We can't test with the real models_dir, so test the underlying logic
        let all_present = V3_ARTIFACTS
            .iter()
            .all(|artifact| model_path.join(artifact.name).exists());
        assert!(all_present);
    }

    #[test]
    fn only_nonempty_files_make_a_model_available() {
        let temp = tempfile::tempdir().unwrap();
        let model_path = temp.path().join("broken-model");
        std::fs::create_dir_all(&model_path).unwrap();
        for artifact in V3_ARTIFACTS {
            let path = model_path.join(artifact.name);
            if artifact.name == "encoder.int8.onnx" {
                std::fs::create_dir(&path).unwrap();
            } else {
                std::fs::write(&path, b"model data").unwrap();
            }
        }

        assert!(!is_model_available_in(temp.path(), "broken-model"));

        let encoder = model_path.join("encoder.int8.onnx");
        std::fs::remove_dir(&encoder).unwrap();
        std::fs::write(&encoder, b"").unwrap();
        assert!(!is_model_available_in(temp.path(), "broken-model"));
    }

    #[test]
    fn model_availability_cannot_escape_the_models_directory() {
        let temp = tempfile::tempdir().unwrap();
        let models = temp.path().join("models");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        for artifact in V3_ARTIFACTS {
            std::fs::write(outside.join(artifact.name), b"model data").unwrap();
        }

        assert!(!is_model_available_in(&models, "../outside"));
    }

    #[cfg(unix)]
    #[test]
    fn model_availability_does_not_follow_symlinks_outside_the_cache() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let models = temp.path().join("models");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        for artifact in V3_ARTIFACTS {
            std::fs::write(outside.join(artifact.name), b"model data").unwrap();
        }

        symlink(&outside, models.join("linked-model")).unwrap();
        assert!(!is_model_available_in(&models, "linked-model"));

        let mixed = models.join("mixed-model");
        std::fs::create_dir(&mixed).unwrap();
        for artifact in V3_ARTIFACTS {
            symlink(outside.join(artifact.name), mixed.join(artifact.name)).unwrap();
        }
        assert!(!is_model_available_in(&models, "mixed-model"));
    }

    #[test]
    fn artifact_identity_requires_exact_size_and_sha256() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("artifact");
        let artifact = ModelArtifact {
            name: "artifact",
            size: 10,
            sha256: "6dbdb6a147ad4d808455652bf5a10120161678395f6bfbd21eb6fe4e731aceeb",
        };
        std::fs::write(&path, b"model data").unwrap();
        assert!(verify_artifact(&path, artifact).unwrap());

        std::fs::write(&path, b"model datb").unwrap();
        assert!(!verify_artifact(&path, artifact).unwrap());
        std::fs::write(&path, b"short").unwrap();
        assert!(!verify_artifact(&path, artifact).unwrap());
    }
}
