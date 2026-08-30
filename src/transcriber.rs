// ABOUTME: Dispatches transcription to local engines, cloud APIs, or model-specific HTTP servers.
// ABOUTME: Captures transcript text from either stdout or JSON response.

use futures_util::StreamExt;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::OnceCell;

use voxkey_ipc::{TranscriberConfig, TranscriberProvider};

use crate::agreement::TimedWord;

type DynError = Box<dyn std::error::Error + Send + Sync>;

/// Removes captured audio when transcription completes or its future is
/// cancelled. `Drop` is essential here: code after an `.await` does not run
/// when SIGTERM or a screen-lock transition aborts the owning task.
struct AudioFileCleanup<'a> {
    path: &'a Path,
}

impl Drop for AudioFileCleanup<'_> {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("Failed to remove temp audio file: {error}");
        }
    }
}

/// Why a transcription is running. Previews repeat every few seconds for the
/// length of a recording, so their routine progress belongs at debug level
/// while the transcription the user is actually waiting for stays at info.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Final,
    Preview,
}

impl Purpose {
    fn label(self) -> &'static str {
        match self {
            Self::Final => "transcription",
            Self::Preview => "preview",
        }
    }
}

/// Emit a routine progress line at the level that suits `purpose`.
macro_rules! progress {
    ($purpose:expr, $($arg:tt)+) => {
        match $purpose {
            Purpose::Final => tracing::info!($($arg)+),
            Purpose::Preview => tracing::debug!($($arg)+),
        }
    };
}

/// Give up on connecting to the transcription endpoint after this long. An
/// unreachable or firewalled endpoint otherwise leaves the connect() hanging
/// indefinitely, which strands the daemon in the Transcribing state with no
/// way back to Idle. Kept short so the user learns the endpoint is down
/// quickly, but above a realistic connect time on a slow link.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound on a whole transcription request, so a stalled endpoint that
/// accepts the connection but never responds cannot hang the daemon either.
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_HTTP_SUCCESS_BODY_BYTES: usize = 1024 * 1024;
const MAX_BATCH_AUDIO_BYTES: u64 = 64 * 1024 * 1024;
/// The deployed Parakeet HTTP service accepts at most 120 seconds per WAV.
/// Stay below that boundary and overlap adjacent chunks so a word crossing a
/// split is heard in full by at least one request.
const PARAKEET_HTTP_CHUNK_DURATION: Duration = Duration::from_secs(115);
const PARAKEET_HTTP_CHUNK_OVERLAP: Duration = Duration::from_secs(5);
const MAX_TRANSCRIPT_OVERLAP_WORDS: usize = 64;
/// Maximum transcript bytes retained from a whisper.cpp subprocess.
const MAX_WHISPER_STDOUT_BYTES: usize = 1024 * 1024;
/// Maximum diagnostic bytes retained from a failed whisper.cpp subprocess.
const MAX_WHISPER_STDERR_BYTES: usize = 16 * 1024;
/// Context-graph boost used for each local Parakeet vocabulary entry. This is
/// sherpa-onnx's documented per-token hotword score and matches its Parakeet
/// hotword example.
const PARAKEET_HOTWORDS_SCORE: f32 = 2.0;
/// Upper bound on any single transcription, including on-device engines. A
/// wedged whisper.cpp process or a stuck model load otherwise strands the
/// daemon in Transcribing with no route back to Idle. Generous enough that a
/// long recording on slow CPU finishes normally.
pub const TRANSCRIBE_TIMEOUT: Duration = Duration::from_secs(600);

/// Build the HTTP client used for cloud transcription. Always bounded by
/// connect and total timeouts so a dead endpoint fails instead of hanging.
fn transcription_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .build()
        .expect("failed to build transcription HTTP client")
}
type SharedRecognizer = Arc<Mutex<sherpa_onnx::OfflineRecognizer>>;
type RecognizerCache = Arc<OnceCell<SharedRecognizer>>;
type SharedOnlineRecognizer = Arc<sherpa_onnx::OnlineRecognizer>;
type OnlineRecognizerCache = Arc<OnceCell<SharedOnlineRecognizer>>;
type ModelVerificationCache = Arc<OnceCell<()>>;
type NativeJob = Box<dyn FnOnce() + Send + 'static>;

static PARAKEET_WORKER: OnceLock<Result<std::sync::mpsc::SyncSender<NativeJob>, String>> =
    OnceLock::new();

/// Text plus word timing metadata used by the preview agreement layer. File
/// and HTTP providers may return no timings; the caller then estimates them
/// from the snapshot duration.
#[derive(Debug)]
pub(crate) struct DecodedTranscript {
    pub text: String,
    pub words: Vec<TimedWord>,
}

/// Everything the local streaming flow needs, without exposing the rest of
/// the transcriber enum or rebuilding model knowledge in the event loop.
#[derive(Clone)]
pub(crate) struct LocalStreamingModel {
    pub model_name: String,
    pub execution_provider: voxkey_ipc::ExecutionProviderChoice,
    recognizer: OnlineRecognizerCache,
    model_verification: ModelVerificationCache,
}

impl LocalStreamingModel {
    pub async fn recognizer(&self) -> Result<SharedOnlineRecognizer, DynError> {
        ensure_parakeet_model_available_cached(
            self.model_verification.clone(),
            self.model_name.clone(),
        )
        .await?;
        initialize_online_model_recognizer(
            self.recognizer.clone(),
            self.model_name.clone(),
            self.execution_provider,
        )
        .await
    }
}

/// Transcription backend selected by provider configuration.
pub enum Transcriber {
    WhisperCpp {
        command: String,
        args: Vec<String>,
    },
    Mistral {
        client: reqwest::Client,
        api_key: String,
        model: String,
        endpoint: String,
        prompt: Option<String>,
    },
    MistralRealtime,
    ParakeetHttp {
        client: reqwest::Client,
        api_key: String,
        model: String,
        endpoint: String,
        allow_insecure_http: bool,
        prompt: Option<String>,
    },
    Parakeet {
        model_name: String,
        execution_provider: voxkey_ipc::ExecutionProviderChoice,
        sample_rate: u32,
        recognizer: RecognizerCache,
        model_verification: ModelVerificationCache,
        hotwords: Vec<String>,
    },
    LocalStreaming(LocalStreamingModel),
}

/// Add `--prompt <vocabulary>` to whisper.cpp args unless the user's own args
/// already provide a prompt value. A trailing prompt flag is completed so the
/// subsequently appended audio path cannot be consumed as its value.
fn whisper_args_with_prompt(args: &[String], prompt: Option<&str>) -> Vec<String> {
    let is_prompt_option = |arg: &str| matches!(arg, "--prompt" | "-p");
    let mut resolved = args.to_vec();
    if args.last().is_some_and(|arg| is_prompt_option(arg)) {
        resolved.push(prompt.unwrap_or_default().to_string());
    } else if let Some(prompt) = prompt
        && !args.iter().any(|arg| is_prompt_option(arg))
    {
        resolved.push("--prompt".to_string());
        resolved.push(prompt.to_string());
    }
    resolved
}

fn is_standard_whisper_cli(command: &str) -> bool {
    Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "whisper-cpp" | "whisper-cli"))
}

fn has_whisper_option(args: &[String], options: &[&str]) -> bool {
    args.iter()
        .any(|argument| options.contains(&argument.as_str()))
}

fn configured_whisper_model(args: &[String]) -> Option<&Path> {
    args.windows(2)
        .find_map(|pair| matches!(pair[0].as_str(), "-m" | "--model").then(|| Path::new(&pair[1])))
}

fn configured_whisper_language(args: &[String]) -> Option<&str> {
    args.windows(2)
        .find_map(|pair| {
            matches!(pair[0].as_str(), "-l" | "--language").then_some(pair[1].as_str())
        })
        .or_else(|| {
            args.iter()
                .find_map(|argument| argument.strip_prefix("--language="))
        })
}

fn discover_whisper_vad_model(args: &[String]) -> Option<std::path::PathBuf> {
    let mut candidates = Vec::new();
    if let Some(model) = configured_whisper_model(args)
        && let Some(parent) = model.parent()
    {
        candidates.push(parent.join("ggml-silero-v6.2.0.bin"));
        candidates.push(parent.join("for-tests-silero-v6.2.0-ggml.bin"));
    }
    candidates.push(
        crate::models::models_dir()
            .join("whisper-vad")
            .join("ggml-silero-v6.2.0.bin"),
    );
    candidates.push(std::path::PathBuf::from(
        "/usr/share/voxkey/models/ggml-silero-v6.2.0.bin",
    ));
    candidates.into_iter().find(|path| {
        std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.file_type().is_file() && metadata.len() > 0)
    })
}

/// Apply Voxkey's vetted whisper.cpp CLI policy while respecting every user
/// override. Each preview launches a fresh process, which inherently provides
/// VoiceInk's no-context behavior; current whisper-cli has no equivalent flag.
fn whisper_args_with_defaults(command: &str, args: &[String], prompt: Option<&str>) -> Vec<String> {
    let mut resolved = whisper_args_with_prompt(args, prompt);
    if !is_standard_whisper_cli(command) {
        return resolved;
    }

    if !has_whisper_option(&resolved, &["-nt", "--no-timestamps"]) {
        resolved.push("--no-timestamps".to_string());
    }
    if !has_whisper_option(
        &resolved,
        &["-fa", "--flash-attn", "-nfa", "--no-flash-attn"],
    ) {
        resolved.push("--flash-attn".to_string());
    }
    if !has_whisper_option(&resolved, &["-tp", "--temperature"]) {
        resolved.extend(["--temperature".to_string(), "0.2".to_string()]);
    }
    if !has_whisper_option(&resolved, &["-nf", "--no-fallback"]) {
        resolved.push("--no-fallback".to_string());
    }
    if !has_whisper_option(&resolved, &["-sns", "--suppress-nst"]) {
        resolved.push("--suppress-nst".to_string());
    }

    let has_vad_model = has_whisper_option(&resolved, &["-vm", "--vad-model"]);
    let discovered_vad = (!has_vad_model)
        .then(|| discover_whisper_vad_model(&resolved))
        .flatten();
    if let Some(path) = discovered_vad {
        resolved.extend([
            "--vad-model".to_string(),
            path.to_string_lossy().into_owned(),
        ]);
    }
    if has_vad_model || has_whisper_option(&resolved, &["-vm", "--vad-model"]) {
        if !has_whisper_option(&resolved, &["--vad"]) {
            resolved.push("--vad".to_string());
        }
        if !has_whisper_option(&resolved, &["-vt", "--vad-threshold"]) {
            resolved.extend(["--vad-threshold".to_string(), "0.5".to_string()]);
        }
        if !has_whisper_option(&resolved, &["-vspd", "--vad-min-speech-duration-ms"]) {
            resolved.extend([
                "--vad-min-speech-duration-ms".to_string(),
                "250".to_string(),
            ]);
        }
        if !has_whisper_option(&resolved, &["-vsd", "--vad-min-silence-duration-ms"]) {
            resolved.extend([
                "--vad-min-silence-duration-ms".to_string(),
                "100".to_string(),
            ]);
        }
        if !has_whisper_option(&resolved, &["-vp", "--vad-speech-pad-ms"]) {
            resolved.extend(["--vad-speech-pad-ms".to_string(), "30".to_string()]);
        }
    }
    resolved
}

fn resolve_whisper_args(args: &[String], audio_path: &Path) -> Vec<String> {
    let audio_path = audio_path.to_string_lossy();
    let mut has_audio_placeholder = false;
    let mut resolved = args
        .iter()
        .map(|arg| {
            if arg.contains("{audio_file}") {
                has_audio_placeholder = true;
                arg.replace("{audio_file}", &audio_path)
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>();

    if !has_audio_placeholder {
        resolved.push(audio_path.into_owned());
    }

    resolved
}

pub(crate) fn resolved_mistral_endpoint(endpoint: &str) -> &str {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        voxkey_ipc::MistralConfig::DEFAULT_ENDPOINT
    } else {
        endpoint
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BatchEndpointPolicy {
    Authenticated,
    Unauthenticated { allow_insecure_http: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointHostScope {
    Loopback,
    PrivateNetwork,
    Other,
}

fn endpoint_host_scope(url: &reqwest::Url) -> EndpointHostScope {
    let Some(host) = url.host_str() else {
        return EndpointHostScope::Other;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return EndpointHostScope::Loopback;
    }
    let Ok(address) = host.parse::<std::net::IpAddr>() else {
        return EndpointHostScope::Other;
    };
    if address.is_loopback() {
        EndpointHostScope::Loopback
    } else if match address {
        std::net::IpAddr::V4(address) => address.is_private(),
        std::net::IpAddr::V6(address) => address.is_unique_local(),
    } {
        EndpointHostScope::PrivateNetwork
    } else {
        EndpointHostScope::Other
    }
}

pub(crate) fn endpoint_uses_unencrypted_private_network(endpoint: &str) -> bool {
    reqwest::Url::parse(endpoint.trim()).is_ok_and(|url| {
        url.scheme() == "http" && endpoint_host_scope(&url) == EndpointHostScope::PrivateNetwork
    })
}

pub(crate) fn batch_endpoint(
    endpoint: &str,
    policy: BatchEndpointPolicy,
) -> Result<reqwest::Url, DynError> {
    let url = reqwest::Url::parse(endpoint.trim())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("Server address must use http:// or https://".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Server address must not contain embedded credentials".into());
    }
    if url.scheme() == "http" {
        match policy {
            BatchEndpointPolicy::Authenticated => {
                return Err("Cloud transcription requires an https:// address".into());
            }
            BatchEndpointPolicy::Unauthenticated {
                allow_insecure_http,
            } => match endpoint_host_scope(&url) {
                EndpointHostScope::Loopback => {}
                EndpointHostScope::PrivateNetwork if allow_insecure_http => {}
                EndpointHostScope::PrivateNetwork => {
                    return Err(
                        "Turn on ‘Allow unencrypted LAN audio’ to use this private HTTP address."
                            .into(),
                    );
                }
                EndpointHostScope::Other => {
                    return Err(
                        "Remote batch transcription requires HTTPS; unencrypted HTTP is limited to loopback and explicitly allowed private IP addresses."
                            .into(),
                    );
                }
            },
        }
    }
    Ok(url)
}

fn batch_authorization_value(api_key: &str) -> std::io::Result<String> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Add a Mistral API key in Voxkey settings before dictating",
        ));
    }
    Ok(format!("Bearer {api_key}"))
}

fn endpoint_for_log(endpoint: &str, policy: BatchEndpointPolicy) -> String {
    let Ok(mut url) = batch_endpoint(endpoint, policy) else {
        return "<invalid endpoint>".to_string();
    };
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

impl Transcriber {
    /// Whether this transcriber uses the streaming (real-time) flow rather than batch.
    pub fn is_streaming(&self) -> bool {
        matches!(self, Self::MistralRealtime { .. } | Self::LocalStreaming(_))
    }

    /// Whether transcription runs on this machine. Network-backed providers
    /// bill and rate-limit per request, which decides whether repeated preview
    /// transcriptions are affordable.
    pub fn runs_locally(&self) -> bool {
        matches!(
            self,
            Self::WhisperCpp { .. } | Self::Parakeet { .. } | Self::LocalStreaming(_)
        )
    }

    pub(crate) fn local_streaming_model(&self) -> Option<LocalStreamingModel> {
        match self {
            Self::LocalStreaming(model) => Some(model.clone()),
            _ => None,
        }
    }

    pub fn from_config(
        config: &TranscriberConfig,
        sample_rate: u32,
        vocabulary: &[String],
    ) -> Self {
        match config.provider {
            TranscriberProvider::WhisperCpp => {
                let prompt = crate::dictionary::vocabulary_prompt_for_language(
                    vocabulary,
                    configured_whisper_language(&config.whisper_cpp.args),
                );
                Self::WhisperCpp {
                    command: config.whisper_cpp.command.clone(),
                    args: whisper_args_with_defaults(
                        &config.whisper_cpp.command,
                        &config.whisper_cpp.args,
                        prompt.as_deref(),
                    ),
                }
            }
            TranscriberProvider::Mistral => Self::Mistral {
                client: transcription_http_client(),
                api_key: config.mistral.api_key.clone(),
                model: config.mistral.model.clone(),
                endpoint: config.mistral.endpoint.clone(),
                prompt: crate::dictionary::vocabulary_prompt(vocabulary),
            },
            TranscriberProvider::MistralRealtime => Self::MistralRealtime,
            TranscriberProvider::Parakeet => match config.parakeet.backend {
                voxkey_ipc::ParakeetBackend::Http => Self::ParakeetHttp {
                    client: transcription_http_client(),
                    api_key: config.parakeet.api_key.clone(),
                    model: config.parakeet.model.clone(),
                    endpoint: config.parakeet.endpoint.clone(),
                    allow_insecure_http: config.parakeet.allow_insecure_http,
                    prompt: crate::dictionary::vocabulary_prompt(vocabulary),
                },
                voxkey_ipc::ParakeetBackend::Local => {
                    if voxkey_ipc::model_library::local_model(&config.parakeet.model).is_some_and(
                        |model| {
                            model.runtime
                                == voxkey_ipc::model_library::LocalModelRuntime::OnlineTransducer
                        },
                    ) {
                        return Self::LocalStreaming(LocalStreamingModel {
                            model_name: config.parakeet.model.clone(),
                            execution_provider: config.parakeet.execution_provider,
                            recognizer: Arc::new(OnceCell::new()),
                            model_verification: Arc::new(OnceCell::new()),
                        });
                    }
                    let recognizer = Arc::new(OnceCell::new());
                    let hotwords = vocabulary
                        .iter()
                        .map(|word| word.trim())
                        .filter(|word| !word.is_empty())
                        .map(str::to_string)
                        .collect::<Vec<_>>();
                    Self::Parakeet {
                        model_name: config.parakeet.model.clone(),
                        execution_provider: config.parakeet.execution_provider,
                        sample_rate,
                        recognizer,
                        model_verification: Arc::new(OnceCell::new()),
                        hotwords,
                    }
                }
            },
        }
    }

    /// One-line description of the resolved backend, logged once per session.
    /// Setup problems are diagnosed from this rather than from a line repeated
    /// on every transcription.
    pub fn describe(&self) -> String {
        match self {
            Self::WhisperCpp { command, args } => {
                format!(
                    "whisper.cpp via `{command}` ({} configured arguments)",
                    args.len()
                )
            }
            Self::Mistral {
                model, endpoint, ..
            } => {
                let endpoint = resolved_mistral_endpoint(endpoint);
                let endpoint = endpoint_for_log(endpoint, BatchEndpointPolicy::Authenticated);
                format!("Mistral batch (model {model}, endpoint {endpoint})")
            }
            Self::MistralRealtime => "Mistral realtime streaming".to_string(),
            Self::ParakeetHttp {
                model,
                endpoint,
                allow_insecure_http,
                ..
            } => {
                let endpoint = endpoint_for_log(
                    endpoint,
                    BatchEndpointPolicy::Unauthenticated {
                        allow_insecure_http: *allow_insecure_http,
                    },
                );
                format!("Transcription server (model {model}, endpoint {endpoint})")
            }
            Self::Parakeet {
                model_name,
                execution_provider,
                ..
            } => format!("Parakeet on device (model {model_name}, {execution_provider:?})"),
            Self::LocalStreaming(model) => format!(
                "Local streaming model (model {}, {:?})",
                model.model_name, model.execution_provider
            ),
        }
    }

    /// Whether this backend can transcribe PCM the daemon already holds.
    /// Callers with the samples in memory can then skip writing a WAV and
    /// having the backend read it straight back.
    pub fn accepts_pcm(&self) -> bool {
        matches!(self, Self::Parakeet { .. })
    }

    /// Transcribe in-memory PCM, giving up after `deadline`. Returns an error
    /// for backends that only consume files; check `accepts_pcm()` first.
    #[cfg(test)]
    pub async fn transcribe_pcm(
        &self,
        deadline: Duration,
        purpose: Purpose,
        pcm_sample_rate: u32,
        pcm_channels: u16,
        chunks: &[Arc<[i16]>],
    ) -> Result<String, DynError> {
        Ok(self
            .transcribe_pcm_detailed(deadline, purpose, pcm_sample_rate, pcm_channels, chunks)
            .await?
            .text)
    }

    /// PCM transcription with token-derived word timestamps when the active
    /// in-process recognizer exposes them.
    pub(crate) async fn transcribe_pcm_detailed(
        &self,
        deadline: Duration,
        purpose: Purpose,
        pcm_sample_rate: u32,
        pcm_channels: u16,
        chunks: &[Arc<[i16]>],
    ) -> Result<DecodedTranscript, DynError> {
        let Self::Parakeet {
            model_name,
            execution_provider,
            sample_rate,
            recognizer,
            model_verification,
            hotwords,
        } = self
        else {
            return Err("This transcription engine needs recorded audio".into());
        };

        if pcm_sample_rate != *sample_rate {
            return Err(format!(
                "Recorded audio sample rate ({pcm_sample_rate}Hz) does not match the configured \
                 Parakeet sample rate ({sample_rate}Hz)"
            )
            .into());
        }

        match tokio::time::timeout(deadline, async {
            ensure_parakeet_model_available_cached(model_verification.clone(), model_name.clone())
                .await?;
            let samples = to_normalized_samples(chunks, pcm_channels)?;
            transcribe_parakeet_samples_detailed(
                model_name,
                *execution_provider,
                *sample_rate,
                recognizer.clone(),
                hotwords,
                purpose,
                samples,
            )
            .await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(format!(
                "Transcription timed out after {:.1}s",
                deadline.as_secs_f32()
            )
            .into()),
        }
    }

    /// Run transcription on the given audio file.
    /// Returns the transcript text, trimmed.
    #[cfg(test)]
    pub async fn transcribe(
        &self,
        audio_path: &Path,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        self.transcribe_within(TRANSCRIBE_TIMEOUT, Purpose::Final, audio_path)
            .await
    }

    /// Transcribe a finalized Voxkey recording. The recorder already appends
    /// the punctuation silence in-place, avoiding a second full-size WAV. The
    /// caller retains ownership so it can preserve the recording on failure.
    pub async fn transcribe_recording(
        &self,
        audio_path: &Path,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        self.transcribe_within_padding(TRANSCRIBE_TIMEOUT, Purpose::Final, audio_path, true, false)
            .await
    }

    /// Run transcription, giving up after `deadline`. The temp audio file is
    /// removed whether the run succeeds, fails, or times out.
    #[cfg(test)]
    pub async fn transcribe_within(
        &self,
        deadline: Duration,
        purpose: Purpose,
        audio_path: &Path,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        self.transcribe_within_padding(deadline, purpose, audio_path, false, true)
            .await
    }

    /// Transcribe a preview WAV whose writer already appended the punctuation
    /// silence. Keeping this separate prevents doubling the artificial tail.
    pub(crate) async fn transcribe_padded_within(
        &self,
        deadline: Duration,
        purpose: Purpose,
        audio_path: &Path,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        self.transcribe_within_padding(deadline, purpose, audio_path, true, true)
            .await
    }

    async fn transcribe_within_padding(
        &self,
        deadline: Duration,
        purpose: Purpose,
        audio_path: &Path,
        input_is_padded: bool,
        cleanup_input: bool,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let _audio_cleanup = cleanup_input.then(|| AudioFileCleanup { path: audio_path });
        let started = std::time::Instant::now();
        let padded_audio;
        let decode_path = if input_is_padded {
            audio_path
        } else {
            let source = audio_path.to_path_buf();
            padded_audio = tokio::task::spawn_blocking(move || {
                padded_wav_copy(&source, crate::preview::TRAILING_SILENCE)
            })
            .await??;
            padded_audio.as_ref()
        };
        let result = match tokio::time::timeout(
            deadline,
            self.run_transcription(purpose, decode_path),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(format!(
                "Transcription timed out after {:.1}s",
                deadline.as_secs_f32()
            )
            .into()),
        };

        match &result {
            Ok(transcript) => progress!(
                purpose,
                "Finished {} in {:.1}s ({} chars)",
                purpose.label(),
                started.elapsed().as_secs_f32(),
                transcript.len()
            ),
            Err(error) => progress!(
                purpose,
                "Failed {} after {:.1}s: {error}",
                purpose.label(),
                started.elapsed().as_secs_f32()
            ),
        }

        result
    }

    async fn run_transcription(
        &self,
        purpose: Purpose,
        audio_path: &Path,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        match self {
            Self::WhisperCpp { command, args, .. } => {
                transcribe_whisper_cpp(command, args, purpose, audio_path).await
            }
            Self::Mistral {
                client,
                api_key,
                model,
                endpoint,
                prompt,
            } => {
                transcribe_mistral(
                    client,
                    api_key,
                    model,
                    endpoint,
                    prompt.as_deref(),
                    purpose,
                    audio_path,
                )
                .await
            }
            Self::MistralRealtime { .. } => {
                Err("Live streaming cannot transcribe a saved audio file".into())
            }
            Self::LocalStreaming(_) => {
                Err("A local streaming model cannot transcribe a saved audio file".into())
            }
            Self::ParakeetHttp {
                client,
                api_key,
                model,
                endpoint,
                allow_insecure_http,
                prompt,
            } => {
                transcribe_parakeet_http(ModelServerTranscription {
                    server: ModelServer {
                        client,
                        api_key,
                        model,
                        endpoint,
                        allow_insecure_http: *allow_insecure_http,
                        prompt: prompt.as_deref(),
                    },
                    purpose,
                    audio_path,
                })
                .await
            }
            Self::Parakeet {
                model_name,
                execution_provider,
                sample_rate,
                recognizer,
                model_verification,
                hotwords,
            } => {
                transcribe_parakeet(
                    ParakeetTranscription {
                        model_name,
                        execution_provider: *execution_provider,
                        sample_rate: *sample_rate,
                        recognizer: recognizer.clone(),
                        model_verification: model_verification.clone(),
                        hotwords,
                        purpose,
                    },
                    audio_path,
                )
                .await
            }
        }
    }
}

async fn transcribe_whisper_cpp(
    command: &str,
    args: &[String],
    purpose: Purpose,
    audio_path: &Path,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let resolved_args = resolve_whisper_args(args, audio_path);

    progress!(
        purpose,
        "Running {command} with {} configured arguments",
        resolved_args.len()
    );

    let mut child = Command::new(command)
        .args(&resolved_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("whisper.cpp stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("whisper.cpp stderr was not captured"))?;
    let (stdout, stderr, status) = tokio::join!(
        read_bounded_process_output(stdout, MAX_WHISPER_STDOUT_BYTES),
        read_bounded_process_output(stderr, MAX_WHISPER_STDERR_BYTES),
        child.wait(),
    );
    let (stdout, stdout_exceeded) = stdout?;
    let (stderr, stderr_exceeded) = stderr?;
    let status = status?;

    if !status.success() {
        let mut stderr = String::from_utf8_lossy(&stderr).trim().to_string();
        if stderr_exceeded {
            stderr.push_str("… [truncated]");
        }
        return Err(format!("Transcription command failed (exit {}): {}", status, stderr).into());
    }
    if stdout_exceeded {
        return Err(format!(
            "Transcription command produced too much stdout (limit {} bytes)",
            MAX_WHISPER_STDOUT_BYTES
        )
        .into());
    }

    let transcript = decode_whisper_transcript(stdout)?;
    Ok(transcript)
}

fn decode_whisper_transcript(stdout: Vec<u8>) -> Result<String, DynError> {
    let transcript = String::from_utf8(stdout)
        .map_err(|error| format!("whisper.cpp stdout was not valid UTF-8: {error}"))?;
    Ok(transcript.trim().to_string())
}

/// Copy a captured PCM WAV and append deterministic digital silence. The
/// original stays owned by `AudioFileCleanup`; the returned path cleans up the
/// padded decode input on every success, error, timeout, and cancellation.
fn padded_wav_copy(source: &Path, silence: Duration) -> Result<tempfile::TempPath, DynError> {
    let mut reader = hound::WavReader::open(source)?;
    let spec = reader.spec();
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        return Err("Voxkey's captured WAV must be 16-bit integer PCM".into());
    }
    let silence_frames = (u128::from(spec.sample_rate) * silence.as_millis()) / 1000;
    let silence_samples = silence_frames
        .checked_mul(u128::from(spec.channels))
        .and_then(|samples| u32::try_from(samples).ok())
        .ok_or("trailing silence is too large for a WAV file")?;
    let total_samples = reader
        .len()
        .checked_add(silence_samples)
        .ok_or("padded WAV is too large")?;

    let path = tempfile::Builder::new()
        .prefix("voxkey_padded_")
        .suffix(".wav")
        .tempfile()?
        .into_temp_path();
    let mut writer = hound::WavWriter::create(&path, spec)?;
    {
        let mut output = writer.get_i16_writer(total_samples);
        for sample in reader.samples::<i16>() {
            output.write_sample(sample?);
        }
        for _ in 0..silence_samples {
            output.write_sample(0);
        }
        output.flush()?;
    }
    writer.finalize()?;
    Ok(path)
}

async fn read_bounded_process_output<R>(
    mut reader: R,
    limit: usize,
) -> std::io::Result<(Vec<u8>, bool)>
where
    R: AsyncRead + Unpin,
{
    let mut retained = Vec::with_capacity(limit.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    let mut exceeded = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let retain = read.min(limit.saturating_sub(retained.len()));
        retained.extend_from_slice(&buffer[..retain]);
        exceeded |= retain < read;
    }
    Ok((retained, exceeded))
}

/// Mistral audio transcription API response.
#[derive(serde::Deserialize)]
struct MistralTranscriptionResponse {
    text: String,
}

async fn transcribe_mistral(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    endpoint: &str,
    prompt: Option<&str>,
    purpose: Purpose,
    audio_path: &Path,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let url = resolved_mistral_endpoint(endpoint);
    transcribe_http_batch(HttpBatchRequest {
        client,
        backend_name: "Mistral",
        api_key: Some(api_key),
        endpoint_policy: BatchEndpointPolicy::Authenticated,
        model,
        url,
        prompt,
        purpose,
        audio_path,
    })
    .await
}

#[derive(Clone, Copy)]
struct ModelServer<'a> {
    client: &'a reqwest::Client,
    api_key: &'a str,
    model: &'a str,
    endpoint: &'a str,
    allow_insecure_http: bool,
    prompt: Option<&'a str>,
}

struct ModelServerTranscription<'server, 'audio> {
    server: ModelServer<'server>,
    purpose: Purpose,
    audio_path: &'audio Path,
}

async fn transcribe_parakeet_http(
    request: ModelServerTranscription<'_, '_>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let ModelServerTranscription {
        server,
        purpose,
        audio_path,
    } = request;
    let endpoint = server.endpoint.trim();
    if endpoint.is_empty() {
        return Err(
            "Set a transcription server address in Voxkey settings before dictating".into(),
        );
    }
    let server = ModelServer { endpoint, ..server };
    if tokio::fs::metadata(audio_path).await?.len() > MAX_BATCH_AUDIO_BYTES {
        return Err(format!(
            "Recorded audio exceeds the {} MiB upload limit",
            MAX_BATCH_AUDIO_BYTES / (1024 * 1024)
        )
        .into());
    }

    let source = audio_path.to_path_buf();
    let chunks = tokio::task::spawn_blocking(move || split_parakeet_http_wav(&source))
        .await
        .map_err(|error| {
            std::io::Error::other(format!("Server audio preparation task failed: {error}"))
        })??;
    let Some(chunks) = chunks else {
        return transcribe_parakeet_http_chunk(server, purpose, audio_path).await;
    };

    progress!(
        purpose,
        "Transcribing a long recording as {} overlapping server chunks",
        chunks.len()
    );
    let mut transcript = String::new();
    for chunk in &chunks {
        let next = transcribe_parakeet_http_chunk(server, purpose, chunk.path()).await?;
        transcript = merge_overlapping_transcripts(&transcript, &next);
    }
    Ok(transcript)
}

async fn transcribe_parakeet_http_chunk(
    server: ModelServer<'_>,
    purpose: Purpose,
    audio_path: &Path,
) -> Result<String, DynError> {
    transcribe_http_batch(HttpBatchRequest {
        client: server.client,
        backend_name: "Transcription server",
        api_key: (!server.api_key.trim().is_empty()).then_some(server.api_key),
        endpoint_policy: BatchEndpointPolicy::Unauthenticated {
            allow_insecure_http: server.allow_insecure_http,
        },
        model: server.model,
        url: server.endpoint,
        prompt: server.prompt,
        purpose,
        audio_path,
    })
    .await
}

/// Return private overlapping WAV chunks when a valid recorder WAV is longer
/// than the Parakeet server's request budget. Invalid WAVs stay untouched so
/// the configured server remains responsible for reporting format errors.
fn split_parakeet_http_wav(
    audio_path: &Path,
) -> Result<Option<Vec<tempfile::NamedTempFile>>, DynError> {
    let reader = match hound::WavReader::open(audio_path) {
        Ok(reader) => reader,
        Err(_) => return Ok(None),
    };
    let spec = reader.spec();
    let duration_frames = u64::from(reader.duration());
    let max_frames =
        u64::from(spec.sample_rate).saturating_mul(PARAKEET_HTTP_CHUNK_DURATION.as_secs());
    if duration_frames <= max_frames {
        return Ok(None);
    }
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        return Err("Long server recordings must be 16-bit PCM WAV files".into());
    }

    let overlap_frames =
        u64::from(spec.sample_rate).saturating_mul(PARAKEET_HTTP_CHUNK_OVERLAP.as_secs());
    if max_frames == 0 || overlap_frames >= max_frames {
        return Err("Invalid server audio chunk configuration".into());
    }

    let mut chunks = Vec::new();
    let mut start_frame = 0_u64;
    while start_frame < duration_frames {
        let end_frame = start_frame.saturating_add(max_frames).min(duration_frames);
        let mut source = hound::WavReader::open(audio_path)?;
        source.seek(u32::try_from(start_frame).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "WAV chunk start exceeds the supported range",
            )
        })?)?;

        let chunk = tempfile::Builder::new()
            .prefix("voxkey_parakeet_chunk_")
            .suffix(".wav")
            .tempfile()?;
        let mut writer = hound::WavWriter::create(chunk.path(), spec)?;
        let values = end_frame
            .saturating_sub(start_frame)
            .saturating_mul(u64::from(spec.channels));
        let values = usize::try_from(values).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "WAV chunk exceeds the supported memory range",
            )
        })?;
        for sample in source.samples::<i16>().take(values) {
            writer.write_sample(sample?)?;
        }
        writer.finalize()?;
        chunks.push(chunk);

        if end_frame == duration_frames {
            break;
        }
        start_frame = end_frame - overlap_frames;
    }
    Ok(Some(chunks))
}

fn transcript_word_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut spans = Vec::new();
    let mut start = None;
    for (offset, character) in text.char_indices() {
        if character.is_whitespace() {
            if let Some(start) = start.take() {
                spans.push(start..offset);
            }
        } else if start.is_none() {
            start = Some(offset);
        }
    }
    if let Some(start) = start {
        spans.push(start..text.len());
    }
    spans
}

fn normalized_overlap_word(word: &str) -> String {
    word.chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn merge_overlapping_transcripts(previous: &str, next: &str) -> String {
    let previous = previous.trim();
    let next = next.trim();
    if previous.is_empty() {
        return next.to_string();
    }
    if next.is_empty() {
        return previous.to_string();
    }

    let previous_spans = transcript_word_spans(previous);
    let next_spans = transcript_word_spans(next);
    let maximum = previous_spans
        .len()
        .min(next_spans.len())
        .min(MAX_TRANSCRIPT_OVERLAP_WORDS);
    let overlap = (1..=maximum).rev().find(|count| {
        let previous_words = &previous_spans[previous_spans.len() - count..];
        let next_words = &next_spans[..*count];
        previous_words.iter().zip(next_words).all(|(left, right)| {
            let left = normalized_overlap_word(&previous[left.clone()]);
            let right = normalized_overlap_word(&next[right.clone()]);
            !left.is_empty() && left == right
        })
    });

    let remainder = match overlap {
        Some(count) if count == next_spans.len() => "",
        Some(count) => next[next_spans[count].start..].trim_start(),
        None => next,
    };
    if remainder.is_empty() {
        previous.to_string()
    } else {
        format!("{previous} {remainder}")
    }
}

/// One OpenAI-compatible batch transcription request.
struct HttpBatchRequest<'a> {
    client: &'a reqwest::Client,
    backend_name: &'a str,
    api_key: Option<&'a str>,
    endpoint_policy: BatchEndpointPolicy,
    model: &'a str,
    url: &'a str,
    prompt: Option<&'a str>,
    purpose: Purpose,
    audio_path: &'a Path,
}

async fn transcribe_http_batch(
    request: HttpBatchRequest<'_>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let HttpBatchRequest {
        client,
        backend_name,
        api_key,
        endpoint_policy,
        model,
        url,
        prompt,
        purpose,
        audio_path,
    } = request;
    let url = batch_endpoint(url, endpoint_policy)?;
    let endpoint = endpoint_for_log(url.as_str(), endpoint_policy);
    progress!(
        purpose,
        "Sending audio to {backend_name} (model: {model}, endpoint: {endpoint})"
    );
    let authorization = api_key.map(batch_authorization_value).transpose()?;

    let file = tokio::fs::File::open(audio_path).await?;
    let file_length = file.metadata().await?.len();
    if file_length > MAX_BATCH_AUDIO_BYTES {
        return Err(format!(
            "Recorded audio exceeds the {} MiB upload limit",
            MAX_BATCH_AUDIO_BYTES / (1024 * 1024)
        )
        .into());
    }
    let file_name = audio_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "audio.wav".to_string());

    let file_stream = tokio_util::io::ReaderStream::new(file);
    let file_part = reqwest::multipart::Part::stream_with_length(
        reqwest::Body::wrap_stream(file_stream),
        file_length,
    )
    .file_name(file_name)
    .mime_str("audio/wav")?;

    let mut form = reqwest::multipart::Form::new()
        .text("model", model.to_string())
        .part("file", file_part);
    if let Some(p) = prompt {
        form = form.text("prompt", p.to_string());
    }

    let mut request = client.post(url).multipart(form);
    if let Some(authorization) = authorization {
        request = request.header("Authorization", authorization);
    }
    let response = request.send().await.map_err(|error| {
        let error = error.without_url();
        tracing::debug!("{backend_name} transport diagnostic: {error}");
        let public = if error.is_timeout() {
            format!("{backend_name} request timed out")
        } else if error.is_connect() {
            format!("Could not connect to {backend_name}")
        } else {
            format!("{backend_name} request failed")
        };
        std::io::Error::other(public)
    })?;

    if !response.status().is_success() {
        let status = response.status();
        return Err(format!("{backend_name} rejected the request ({status})").into());
    }

    let advertised_length = response.content_length();
    let body = collect_bounded_body(
        response.bytes_stream(),
        advertised_length,
        MAX_HTTP_SUCCESS_BODY_BYTES,
    )
    .await?;
    let parsed: MistralTranscriptionResponse = serde_json::from_slice(&body)?;
    let transcript = parsed.text.trim().to_string();
    tracing::info!("Transcription complete ({} chars)", transcript.len());
    Ok(transcript)
}

async fn collect_bounded_body<S, B, E>(
    chunks: S,
    advertised_length: Option<u64>,
    limit: usize,
) -> Result<Vec<u8>, DynError>
where
    S: futures_util::Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
    E: std::error::Error + Send + Sync + 'static,
{
    if advertised_length.is_some_and(|length| length > limit as u64) {
        return Err(format!("transcription response exceeds the {limit}-byte limit").into());
    }
    futures_util::pin_mut!(chunks);
    let mut body = Vec::with_capacity(
        advertised_length
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or_default(),
    );
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        let bytes = chunk.as_ref();
        if bytes.len() > limit.saturating_sub(body.len()) {
            return Err(format!("transcription response exceeds the {limit}-byte limit").into());
        }
        body.extend_from_slice(bytes);
    }
    Ok(body)
}

/// Per-user directory for the short-lived hotwords files fed to sherpa-onnx.
/// Every recognizer build gets its own file so concurrent old/new sessions
/// cannot read each other's vocabulary.
fn hotwords_cache_dir() -> std::path::PathBuf {
    hotwords_cache_dir_from(
        std::env::var_os("XDG_CACHE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

fn hotwords_cache_dir_from(
    xdg_cache_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> std::path::PathBuf {
    let cache_dir = xdg_cache_home
        .filter(|path| !path.is_empty() && Path::new(path).is_absolute())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            home.map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from("~"))
                .join(".cache")
        });
    cache_dir.join("voxkey")
}

/// Remove the fixed filename used by older releases. Current recognizers only
/// use automatically-cleaned unique files.
pub(crate) fn remove_legacy_hotwords_file() {
    let path = hotwords_cache_dir().join("hotwords.txt");
    remove_legacy_hotwords_file_at(&path);
}

fn remove_legacy_hotwords_file_at(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => tracing::info!("Removed legacy Parakeet hotwords file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!("Failed to remove legacy Parakeet hotwords: {error}"),
    }
}

fn create_hotwords_file(hotwords: &[String]) -> Result<tempfile::NamedTempFile, std::io::Error> {
    create_hotwords_file_in(&hotwords_cache_dir(), hotwords)
}

fn create_hotwords_file_in(
    cache_dir: &Path,
    hotwords: &[String],
) -> Result<tempfile::NamedTempFile, std::io::Error> {
    create_private_cache_file(cache_dir, "hotwords-", ".txt", &hotwords.join("\n"))
}

fn create_private_cache_file(
    cache_dir: &Path,
    prefix: &str,
    suffix: &str,
    contents: &str,
) -> Result<tempfile::NamedTempFile, std::io::Error> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(cache_dir)?;
    let mut file = tempfile::Builder::new()
        .prefix(prefix)
        .suffix(suffix)
        .permissions(std::fs::Permissions::from_mode(0o600))
        .tempfile_in(cache_dir)?;
    file.write_all(contents.as_bytes())?;
    file.flush()?;
    Ok(file)
}

/// Reconstruct the BPE vocabulary embedded in NVIDIA's Parakeet NeMo models.
///
/// The exported ONNX bundles contain the tokens in their original tokenizer
/// order but omit `bpe.vocab`, which sherpa-onnx needs to encode normal-word
/// hotwords. NeMo's tokenizer vocab assigns score 0 to its reserved preamble,
/// then uses the negative merge rank (`-0`, `-1`, ...) for every BPE piece.
/// ParakeetV3's preamble includes language/control symbols, digits, and a final
/// `<|spltokenN|>` run; V2 has only a leading `<unk>`. `<blk>` belongs to the
/// transducer output vocabulary, not the BPE tokenizer, so it is omitted.
fn parakeet_bpe_vocab_from_tokens(tokens: &str) -> Result<String, std::io::Error> {
    use std::fmt::Write as _;

    let invalid = |message: String| std::io::Error::new(std::io::ErrorKind::InvalidData, message);
    let mut pieces = Vec::<String>::new();
    let mut expected_id = 0_usize;
    let mut saw_blank = false;

    for (line_index, line) in tokens.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if saw_blank {
            return Err(invalid(format!(
                "tokens.txt has a token after <blk> on line {}",
                line_index + 1
            )));
        }

        let mut fields = line.split_whitespace();
        let token = fields
            .next()
            .ok_or_else(|| invalid(format!("tokens.txt line {} is empty", line_index + 1)))?;
        let id = fields
            .next()
            .ok_or_else(|| {
                invalid(format!(
                    "tokens.txt line {} does not contain a token ID",
                    line_index + 1
                ))
            })?
            .parse::<usize>()
            .map_err(|error| {
                invalid(format!(
                    "tokens.txt line {} has an invalid token ID: {error}",
                    line_index + 1
                ))
            })?;
        if fields.next().is_some() {
            return Err(invalid(format!(
                "tokens.txt line {} has more than two fields",
                line_index + 1
            )));
        }
        if id != expected_id {
            return Err(invalid(format!(
                "tokens.txt line {} has ID {id}, expected {expected_id}",
                line_index + 1
            )));
        }
        expected_id += 1;

        if token == "<blk>" {
            saw_blank = true;
            continue;
        }
        pieces.push(token.to_string());
    }

    if expected_id == 0 {
        return Err(invalid("tokens.txt is empty".to_string()));
    }
    if !saw_blank {
        return Err(invalid("tokens.txt does not end with <blk>".to_string()));
    }
    let final_special_token = pieces.iter().rposition(|token| {
        token
            .strip_prefix("<|spltoken")
            .and_then(|value| value.strip_suffix("|>"))
            .is_some_and(|value| !value.is_empty() && value.chars().all(|c| c.is_ascii_digit()))
    });
    let merge_start = final_special_token.map_or_else(
        || {
            pieces
                .iter()
                .take_while(|token| token.starts_with('<') && token.ends_with('>'))
                .count()
        },
        |index| index + 1,
    );
    if merge_start == 0 || merge_start == pieces.len() {
        return Err(invalid("tokens.txt contains no BPE pieces".to_string()));
    }

    let mut vocab = String::new();
    for (index, token) in pieces.iter().enumerate() {
        if index >= merge_start && token.starts_with('<') && token.ends_with('>') {
            return Err(invalid(format!(
                "tokens.txt control token {token:?} appears after BPE pieces"
            )));
        }
        let score = if index < merge_start {
            "0".to_string()
        } else {
            format!("-{}", index - merge_start)
        };
        writeln!(vocab, "{token}\t{score}").expect("writing to a String cannot fail");
    }
    Ok(vocab)
}

struct ParakeetVocabularyFiles {
    hotwords: tempfile::NamedTempFile,
    bpe_vocab: tempfile::NamedTempFile,
}

fn create_parakeet_vocabulary_files(
    model_dir: &Path,
    hotwords: &[String],
) -> Result<Option<ParakeetVocabularyFiles>, DynError> {
    if hotwords.is_empty() {
        return Ok(None);
    }

    let tokens_path = model_dir.join("tokens.txt");
    let tokens = std::fs::read_to_string(&tokens_path).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!(
                "failed to read Parakeet tokens from {}: {error}",
                tokens_path.display()
            ),
        )
    })?;
    let bpe_vocab_contents = parakeet_bpe_vocab_from_tokens(&tokens)?;
    let hotwords = create_hotwords_file(hotwords).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("failed to create the Parakeet hotwords file: {error}"),
        )
    })?;
    let bpe_vocab = create_private_cache_file(
        &hotwords_cache_dir(),
        "parakeet-bpe-",
        ".vocab",
        &bpe_vocab_contents,
    )
    .map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("failed to create the Parakeet BPE vocabulary file: {error}"),
        )
    })?;

    Ok(Some(ParakeetVocabularyFiles {
        hotwords,
        bpe_vocab,
    }))
}

fn parakeet_sample_rate(sample_rate: u32) -> Result<i32, DynError> {
    i32::try_from(sample_rate).map_err(|_| {
        format!("Parakeet sample rate {sample_rate}Hz exceeds the supported range").into()
    })
}

fn ensure_parakeet_model_available(model_name: &str) -> Result<(), DynError> {
    if crate::models::is_model_available(model_name) {
        return Ok(());
    }
    Err(format!(
        "Parakeet model '{}' not found at {}. Download it from Voxkey settings.",
        model_name,
        crate::models::model_dir(model_name).display()
    )
    .into())
}

async fn ensure_parakeet_model_available_cached(
    verification: ModelVerificationCache,
    model_name: String,
) -> Result<(), DynError> {
    verification
        .get_or_try_init(|| async move {
            let checked_model = model_name.clone();
            run_parakeet_native("Parakeet model integrity verification", move || {
                ensure_parakeet_model_available(&checked_model)
            })
            .await
        })
        .await?;
    Ok(())
}

fn parakeet_recognizer_config(
    model_dir: &Path,
    execution_provider: voxkey_ipc::ExecutionProviderChoice,
    sample_rate: i32,
    vocabulary_files: Option<(&Path, &Path)>,
) -> sherpa_onnx::OfflineRecognizerConfig {
    let mut config = sherpa_onnx::OfflineRecognizerConfig::default();
    config.feat_config.sample_rate = sample_rate;
    config.feat_config.feature_dim = 80;
    config.model_config.transducer = sherpa_onnx::OfflineTransducerModelConfig {
        encoder: Some(
            model_dir
                .join("encoder.int8.onnx")
                .to_string_lossy()
                .into_owned(),
        ),
        decoder: Some(
            model_dir
                .join("decoder.int8.onnx")
                .to_string_lossy()
                .into_owned(),
        ),
        joiner: Some(
            model_dir
                .join("joiner.int8.onnx")
                .to_string_lossy()
                .into_owned(),
        ),
    };
    config.model_config.tokens = Some(model_dir.join("tokens.txt").to_string_lossy().into_owned());
    config.model_config.model_type = Some("nemo_transducer".to_string());
    config.model_config.num_threads = 4;
    config.model_config.provider = match execution_provider {
        voxkey_ipc::ExecutionProviderChoice::Cuda => Some("cuda".to_string()),
        voxkey_ipc::ExecutionProviderChoice::Cpu => Some("cpu".to_string()),
        // sherpa-onnx's documented default provider is CPU.
        voxkey_ipc::ExecutionProviderChoice::Auto => None,
    };
    config.decoding_method = Some("greedy_search".to_string());

    if let Some((hotwords_file, bpe_vocab)) = vocabulary_files {
        config.hotwords_file = Some(hotwords_file.to_string_lossy().into_owned());
        config.hotwords_score = PARAKEET_HOTWORDS_SCORE;
        config.decoding_method = Some("modified_beam_search".to_string());
        config.model_config.modeling_unit = Some("bpe".to_string());
        config.model_config.bpe_vocab = Some(bpe_vocab.to_string_lossy().into_owned());
    }
    config
}

fn online_model_recognizer_config(
    model_dir: &Path,
    execution_provider: voxkey_ipc::ExecutionProviderChoice,
) -> sherpa_onnx::OnlineRecognizerConfig {
    let mut config = sherpa_onnx::OnlineRecognizerConfig::default();
    config.feat_config.sample_rate = 16_000;
    config.feat_config.feature_dim = 80;
    config.model_config.transducer = sherpa_onnx::OnlineTransducerModelConfig {
        encoder: Some(
            model_dir
                .join("encoder.int8.onnx")
                .to_string_lossy()
                .into_owned(),
        ),
        decoder: Some(
            model_dir
                .join("decoder.int8.onnx")
                .to_string_lossy()
                .into_owned(),
        ),
        joiner: Some(
            model_dir
                .join("joiner.int8.onnx")
                .to_string_lossy()
                .into_owned(),
        ),
    };
    config.model_config.tokens = Some(model_dir.join("tokens.txt").to_string_lossy().into_owned());
    config.model_config.num_threads = 4;
    config.model_config.provider = match execution_provider {
        voxkey_ipc::ExecutionProviderChoice::Cuda => Some("cuda".to_string()),
        voxkey_ipc::ExecutionProviderChoice::Cpu => Some("cpu".to_string()),
        voxkey_ipc::ExecutionProviderChoice::Auto => None,
    };
    config.decoding_method = Some("greedy_search".to_string());
    config
}

fn build_online_model_recognizer(
    model_name: &str,
    execution_provider: voxkey_ipc::ExecutionProviderChoice,
) -> Result<sherpa_onnx::OnlineRecognizer, DynError> {
    let model_dir = crate::models::model_dir(model_name);
    let config = online_model_recognizer_config(&model_dir, execution_provider);
    tracing::info!(model = model_name, "Creating local streaming recognizer");
    sherpa_onnx::OnlineRecognizer::create(&config).ok_or_else(|| {
        std::io::Error::other(format!(
            "failed to create the local streaming recognizer for {model_name}"
        ))
        .into()
    })
}

/// Build a Parakeet ONNX recognizer for the given model and execution provider.
/// This is the only function allowed to call `OfflineRecognizer::create`.
fn build_parakeet_recognizer(
    model_name: &str,
    execution_provider: voxkey_ipc::ExecutionProviderChoice,
    sample_rate: u32,
    hotwords: &[String],
) -> Result<sherpa_onnx::OfflineRecognizer, DynError> {
    let model_dir = crate::models::model_dir(model_name);
    build_parakeet_recognizer_in(&model_dir, execution_provider, sample_rate, hotwords)
}

fn build_parakeet_recognizer_in(
    model_dir: &Path,
    execution_provider: voxkey_ipc::ExecutionProviderChoice,
    sample_rate: u32,
    hotwords: &[String],
) -> Result<sherpa_onnx::OfflineRecognizer, DynError> {
    let sample_rate = parakeet_sample_rate(sample_rate)?;
    let vocabulary_files = create_parakeet_vocabulary_files(model_dir, hotwords)?;
    let vocabulary_paths = vocabulary_files
        .as_ref()
        .map(|files| (files.hotwords.path(), files.bpe_vocab.path()));
    let config =
        parakeet_recognizer_config(model_dir, execution_provider, sample_rate, vocabulary_paths);

    tracing::info!("Creating Parakeet recognizer");
    let recognizer = sherpa_onnx::OfflineRecognizer::create(&config).ok_or_else(|| {
        let detail = if vocabulary_files.is_some() {
            " with vocabulary-aware modified beam search"
        } else {
            ""
        };
        std::io::Error::other(format!("failed to create Parakeet recognizer{detail}"))
    })?;
    tracing::info!("Recognizer created");
    Ok(recognizer)
}

fn spawn_native_worker(name: &str) -> Result<std::sync::mpsc::SyncSender<NativeJob>, String> {
    // One running operation plus one queued operation is the global admission
    // budget. The OS thread is deliberately detached: Rust's Tokio runtime
    // must never wait forever for an uncooperative native engine during exit.
    let (sender, receiver) = std::sync::mpsc::sync_channel::<NativeJob>(1);
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            while let Ok(job) = receiver.recv() {
                job();
            }
        })
        .map_err(|error| format!("Failed to start {name}: {error}"))?;
    Ok(sender)
}

fn parakeet_worker() -> Result<std::sync::mpsc::SyncSender<NativeJob>, DynError> {
    match PARAKEET_WORKER.get_or_init(|| spawn_native_worker("voxkey-parakeet")) {
        Ok(sender) => Ok(sender.clone()),
        Err(error) => Err(std::io::Error::other(error.clone()).into()),
    }
}

async fn run_native_operation<T, F>(
    sender: std::sync::mpsc::SyncSender<NativeJob>,
    name: &'static str,
    deadline: Duration,
    operation: F,
) -> Result<T, DynError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, DynError> + Send + 'static,
{
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let job = Box::new(move || {
        let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => Err(format!("{name} panicked")),
        };
        let _ = result_tx.send(result);
    });
    sender.try_send(job).map_err(|error| {
        let message = match error {
            std::sync::mpsc::TrySendError::Full(_) => {
                format!("{name} is busy with another native operation; try again")
            }
            std::sync::mpsc::TrySendError::Disconnected(_) => {
                format!("{name} worker stopped unexpectedly")
            }
        };
        std::io::Error::other(message)
    })?;

    match tokio::time::timeout(deadline, result_rx).await {
        Ok(Ok(Ok(value))) => Ok(value),
        Ok(Ok(Err(error))) => Err(std::io::Error::other(error).into()),
        Ok(Err(_)) => Err(std::io::Error::other(format!(
            "{name} worker stopped before returning a result"
        ))
        .into()),
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("{name} exceeded its {}s deadline", deadline.as_secs()),
        )
        .into()),
    }
}

async fn run_parakeet_native<T, F>(name: &'static str, operation: F) -> Result<T, DynError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, DynError> + Send + 'static,
{
    run_native_operation(parakeet_worker()?, name, TRANSCRIBE_TIMEOUT, operation).await
}

/// Build (or await an in-progress build of) the shared recognizer for `cache`.
/// Both warm-up and first use call this so at most one recognizer is built.
async fn initialize_parakeet_recognizer(
    cache: RecognizerCache,
    model_name: String,
    execution_provider: voxkey_ipc::ExecutionProviderChoice,
    sample_rate: u32,
    hotwords: Vec<String>,
) -> Result<SharedRecognizer, DynError> {
    let recognizer = cache
        .get_or_try_init(|| async move {
            run_parakeet_native("Parakeet model initialization", move || {
                build_parakeet_recognizer(&model_name, execution_provider, sample_rate, &hotwords)
                    .map(|value| Arc::new(Mutex::new(value)))
            })
            .await
        })
        .await?;

    Ok(recognizer.clone())
}

async fn initialize_online_model_recognizer(
    cache: OnlineRecognizerCache,
    model_name: String,
    execution_provider: voxkey_ipc::ExecutionProviderChoice,
) -> Result<SharedOnlineRecognizer, DynError> {
    let recognizer = cache
        .get_or_try_init(|| async move {
            run_parakeet_native("Local streaming model initialization", move || {
                build_online_model_recognizer(&model_name, execution_provider).map(Arc::new)
            })
            .await
        })
        .await?;
    Ok(recognizer.clone())
}

/// Decode a WAV file into its sample rate and normalized f32 samples.
fn read_wav_samples(audio_path: &Path) -> Result<(u32, Vec<f32>), DynError> {
    let mut reader = hound::WavReader::open(audio_path)?;
    let spec = reader.spec();
    let samples = match spec.sample_format {
        hound::SampleFormat::Int => {
            let max_value = (1_i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|sample| sample.map(|value| value as f32 / max_value))
                .collect::<Result<Vec<_>, _>>()?
        }
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?,
    };
    Ok((
        spec.sample_rate,
        downmix_interleaved(samples, spec.channels)?,
    ))
}

struct ParakeetTranscription<'a> {
    model_name: &'a str,
    execution_provider: voxkey_ipc::ExecutionProviderChoice,
    sample_rate: u32,
    recognizer: RecognizerCache,
    model_verification: ModelVerificationCache,
    hotwords: &'a [String],
    purpose: Purpose,
}

async fn transcribe_parakeet(
    request: ParakeetTranscription<'_>,
    audio_path: &Path,
) -> Result<String, DynError> {
    let ParakeetTranscription {
        model_name,
        execution_provider,
        sample_rate,
        recognizer,
        model_verification,
        hotwords,
        purpose,
    } = request;
    ensure_parakeet_model_available_cached(model_verification, model_name.to_string()).await?;

    progress!(
        purpose,
        "Parakeet transcription: model={model_name}, provider={execution_provider:?}, path={}",
        audio_path.display()
    );

    let wav_path = audio_path.to_path_buf();
    let (wav_sample_rate, samples) = run_parakeet_native("Parakeet audio decoding", move || {
        read_wav_samples(&wav_path)
    })
    .await?;

    if wav_sample_rate != sample_rate {
        return Err(format!(
            "Recorded audio sample rate ({wav_sample_rate}Hz) does not match the configured \
             Parakeet sample rate ({sample_rate}Hz)"
        )
        .into());
    }

    transcribe_parakeet_samples(
        model_name,
        execution_provider,
        sample_rate,
        recognizer,
        hotwords,
        purpose,
        samples,
    )
    .await
}

fn downmix_interleaved(samples: Vec<f32>, channels: u16) -> Result<Vec<f32>, DynError> {
    let channels = usize::from(channels);
    if channels == 0 {
        return Err("audio must have at least one channel".into());
    }
    if !samples.len().is_multiple_of(channels) {
        return Err(format!(
            "audio has {} samples, which is not a whole number of {channels}-channel frames",
            samples.len()
        )
        .into());
    }
    if channels == 1 {
        return Ok(samples);
    }

    Ok(samples
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect())
}

/// Convert interleaved 16-bit PCM to normalized mono floats for the
/// recognizer, matching what `read_wav_samples` produces for a 16-bit WAV.
fn to_normalized_samples(chunks: &[Arc<[i16]>], channels: u16) -> Result<Vec<f32>, DynError> {
    const FULL_SCALE: f32 = 32768.0;
    let total = chunks.iter().map(|chunk| chunk.len()).sum();
    let mut samples = Vec::with_capacity(total);
    for chunk in chunks {
        samples.extend(chunk.iter().map(|sample| *sample as f32 / FULL_SCALE));
    }
    downmix_interleaved(samples, channels)
}

#[cfg(test)]
fn decode_parakeet_samples(
    recognizer: &sherpa_onnx::OfflineRecognizer,
    sample_rate: i32,
    samples: &[f32],
) -> Result<String, DynError> {
    Ok(decode_parakeet_samples_detailed(recognizer, sample_rate, samples)?.text)
}

fn decode_parakeet_samples_detailed(
    recognizer: &sherpa_onnx::OfflineRecognizer,
    sample_rate: i32,
    samples: &[f32],
) -> Result<DecodedTranscript, DynError> {
    i32::try_from(samples.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Parakeet audio contains too many samples for the native API",
        )
    })?;
    let stream = recognizer.create_stream();
    stream.accept_waveform(sample_rate, samples);
    recognizer.decode(&stream);
    let result = stream
        .get_result()
        .ok_or_else(|| std::io::Error::other("Parakeet inference returned no result"))?;
    let text = result.text.trim().to_string();
    let words = merge_parakeet_tokens(
        &result.tokens,
        result.timestamps.as_deref(),
        result.durations.as_deref(),
        sample_rate as u32,
        samples.len() as u64,
    );
    Ok(DecodedTranscript { text, words })
}

/// Merge SentencePiece tokens into timed words. Parakeet marks a new word
/// with either U+2581 or an ASCII leading space; punctuation stays attached to
/// the preceding word so sentence-boundary confirmation can see it.
fn merge_parakeet_tokens(
    tokens: &[String],
    timestamps: Option<&[f32]>,
    durations: Option<&[f32]>,
    sample_rate: u32,
    audio_frames: u64,
) -> Vec<TimedWord> {
    let Some(timestamps) = timestamps.filter(|values| values.len() == tokens.len()) else {
        return Vec::new();
    };
    let durations = durations.filter(|values| values.len() == tokens.len());
    let frame_for_seconds = |seconds: f32| -> u64 {
        if !seconds.is_finite() || seconds <= 0.0 {
            return 0;
        }
        ((seconds as f64 * sample_rate as f64).round() as u64).min(audio_frames)
    };

    let mut words = Vec::new();
    let mut current_text = String::new();
    let mut current_start = 0_u64;
    let mut current_end = 0_u64;

    let push_current = |words: &mut Vec<TimedWord>, text: &mut String, start: u64, end: u64| {
        if !text.is_empty() {
            words.push(TimedWord::new(std::mem::take(text), start, end.max(start)));
        }
    };

    for (index, token) in tokens.iter().enumerate() {
        let begins_word = token.starts_with('▁') || token.starts_with(' ');
        let cleaned = token.trim_start_matches(['▁', ' ']).replace('▁', " ");
        if cleaned.is_empty() {
            continue;
        }
        let token_start = frame_for_seconds(timestamps[index]);
        let token_end = durations
            .and_then(|values| values.get(index).copied())
            .map(|duration| frame_for_seconds(timestamps[index] + duration.max(0.0)))
            .or_else(|| timestamps.get(index + 1).copied().map(frame_for_seconds))
            .unwrap_or(audio_frames)
            .max(token_start);

        if begins_word && !current_text.is_empty() {
            push_current(&mut words, &mut current_text, current_start, current_end);
        }
        if current_text.is_empty() {
            current_start = token_start;
        }
        current_text.push_str(&cleaned);
        current_end = token_end;
    }
    push_current(&mut words, &mut current_text, current_start, current_end);
    words
}

/// Run Parakeet inference over already-decoded audio. The recognizer is shared
/// and single-threaded, so inference serializes on its mutex.
async fn transcribe_parakeet_samples(
    model_name: &str,
    execution_provider: voxkey_ipc::ExecutionProviderChoice,
    sample_rate: u32,
    recognizer: RecognizerCache,
    hotwords: &[String],
    purpose: Purpose,
    samples: Vec<f32>,
) -> Result<String, DynError> {
    Ok(transcribe_parakeet_samples_detailed(
        model_name,
        execution_provider,
        sample_rate,
        recognizer,
        hotwords,
        purpose,
        samples,
    )
    .await?
    .text)
}

async fn transcribe_parakeet_samples_detailed(
    model_name: &str,
    execution_provider: voxkey_ipc::ExecutionProviderChoice,
    sample_rate: u32,
    recognizer: RecognizerCache,
    hotwords: &[String],
    purpose: Purpose,
    samples: Vec<f32>,
) -> Result<DecodedTranscript, DynError> {
    let recognizer = initialize_parakeet_recognizer(
        recognizer,
        model_name.to_string(),
        execution_provider,
        sample_rate,
        hotwords.to_vec(),
    )
    .await?;
    let sample_rate = parakeet_sample_rate(sample_rate)?;

    let transcript = run_parakeet_native("Parakeet inference", move || {
        let recognizer = recognizer
            .lock()
            .map_err(|_| std::io::Error::other("Parakeet recognizer mutex poisoned"))?;
        decode_parakeet_samples_detailed(&recognizer, sample_rate, &samples)
    })
    .await?;

    progress!(
        purpose,
        "Parakeet inference produced {} chars",
        transcript.text.len()
    );
    Ok(transcript)
}

#[cfg(test)]
fn parse_mistral_response(json: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let parsed: MistralTranscriptionResponse = serde_json::from_str(json)?;
    Ok(parsed.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use voxkey_ipc::{MistralConfig, MistralRealtimeConfig, ParakeetConfig, WhisperCppConfig};

    fn unauthenticated_endpoint_policy(allow_insecure_http: bool) -> BatchEndpointPolicy {
        BatchEndpointPolicy::Unauthenticated {
            allow_insecure_http,
        }
    }

    fn persistent_test_wav(prefix: &str) -> std::path::PathBuf {
        let file = tempfile::Builder::new()
            .prefix(prefix)
            .suffix(".wav")
            .tempfile()
            .unwrap();
        let path = file.into_temp_path().keep().unwrap();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        writer.write_sample(123_i16).unwrap();
        writer.finalize().unwrap();
        path
    }

    #[tokio::test]
    async fn a_wedged_parakeet_worker_does_not_wedge_the_async_runtime() {
        let sender = spawn_native_worker("voxkey-parakeet-test").unwrap();
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let worker_gate = gate.clone();

        let result = run_native_operation(
            sender,
            "test Parakeet inference",
            Duration::from_millis(20),
            move || {
                let (open, wake) = &*worker_gate;
                let mut open = open.lock().unwrap();
                while !*open {
                    open = wake.wait(open).unwrap();
                }
                Ok(())
            },
        )
        .await;

        assert!(
            matches!(result, Err(ref error) if error.to_string().contains("deadline")),
            "wedged native work was not bounded: {result:?}"
        );
        tokio::time::timeout(Duration::from_millis(20), async {})
            .await
            .unwrap();

        let (open, wake) = &*gate;
        *open.lock().unwrap() = true;
        wake.notify_all();
    }

    #[test]
    fn from_config_creates_whisper_cpp_variant() {
        let config = TranscriberConfig {
            provider: TranscriberProvider::WhisperCpp,
            whisper_cpp: WhisperCppConfig {
                command: "/usr/bin/whisper".to_string(),
                args: vec!["-m".to_string(), "model.bin".to_string()],
            },
            mistral: MistralConfig::default(),
            mistral_realtime: MistralRealtimeConfig::default(),
            parakeet: ParakeetConfig::default(),
        };
        let t = Transcriber::from_config(&config, 16000, &[]);
        match t {
            Transcriber::WhisperCpp { command, args, .. } => {
                assert_eq!(command, "/usr/bin/whisper");
                assert_eq!(
                    args,
                    vec![
                        "-m",
                        "model.bin",
                        "--prompt",
                        "Hello, how are you doing? Nice to meet you."
                    ]
                );
            }
            _ => panic!("Expected WhisperCpp variant"),
        }
    }

    #[test]
    fn from_config_creates_mistral_variant() {
        let config = TranscriberConfig {
            provider: TranscriberProvider::Mistral,
            whisper_cpp: WhisperCppConfig::default(),
            mistral: MistralConfig {
                api_key: "sk-test".to_string(),
                model: "voxtral-mini-2602".to_string(),
                endpoint: String::new(),
            },
            mistral_realtime: MistralRealtimeConfig::default(),
            parakeet: ParakeetConfig::default(),
        };
        let t = Transcriber::from_config(&config, 16000, &[]);
        match t {
            Transcriber::Mistral { api_key, model, .. } => {
                assert_eq!(api_key, "sk-test");
                assert_eq!(model, "voxtral-mini-2602");
            }
            _ => panic!("Expected Mistral variant"),
        }
    }

    #[test]
    fn whitespace_only_mistral_endpoint_uses_the_default() {
        let transcriber = Transcriber::Mistral {
            client: transcription_http_client(),
            api_key: "test-key".to_string(),
            model: MistralConfig::DEFAULT_MODEL.to_string(),
            endpoint: "   \t".to_string(),
            prompt: None,
        };

        assert_eq!(
            transcriber.describe(),
            format!(
                "Mistral batch (model {}, endpoint {})",
                MistralConfig::DEFAULT_MODEL,
                MistralConfig::DEFAULT_ENDPOINT
            )
        );
    }

    #[test]
    fn from_config_creates_mistral_realtime_variant() {
        let config = TranscriberConfig {
            provider: TranscriberProvider::MistralRealtime,
            whisper_cpp: WhisperCppConfig::default(),
            mistral: MistralConfig::default(),
            mistral_realtime: MistralRealtimeConfig {
                api_key: "sk-rt".to_string(),
                model: "voxtral-mini-transcribe-realtime-2602".to_string(),
                endpoint: String::new(),
            },
            parakeet: ParakeetConfig::default(),
        };
        let t = Transcriber::from_config(&config, 16000, &[]);
        assert!(matches!(t, Transcriber::MistralRealtime));
    }

    #[test]
    fn is_streaming_returns_true_for_mistral_realtime() {
        let t = Transcriber::MistralRealtime;
        assert!(t.is_streaming());
    }

    #[test]
    fn catalog_runtime_selects_the_matching_local_transcription_flow() {
        for model in voxkey_ipc::model_library::LOCAL_MODELS {
            let config = TranscriberConfig {
                provider: TranscriberProvider::Parakeet,
                parakeet: voxkey_ipc::ParakeetConfig {
                    model: model.id.to_string(),
                    backend: voxkey_ipc::ParakeetBackend::Local,
                    ..Default::default()
                },
                ..Default::default()
            };
            let transcriber = Transcriber::from_config(&config, 16_000, &[]);
            match model.runtime {
                voxkey_ipc::model_library::LocalModelRuntime::OnlineTransducer => {
                    assert!(matches!(&transcriber, Transcriber::LocalStreaming(_)));
                    assert!(transcriber.is_streaming());
                }
                voxkey_ipc::model_library::LocalModelRuntime::OfflineTransducer => {
                    assert!(matches!(&transcriber, Transcriber::Parakeet { .. }));
                    assert!(!transcriber.is_streaming());
                }
            }
            assert!(transcriber.runs_locally());
        }
    }

    #[test]
    fn is_streaming_returns_false_for_batch_providers() {
        let whisper = Transcriber::WhisperCpp {
            command: String::new(),
            args: vec![],
        };
        assert!(!whisper.is_streaming());

        let mistral = Transcriber::Mistral {
            client: reqwest::Client::new(),
            api_key: String::new(),
            model: String::new(),
            endpoint: String::new(),
            prompt: None,
        };
        assert!(!mistral.is_streaming());

        let parakeet_http = Transcriber::ParakeetHttp {
            client: reqwest::Client::new(),
            model: String::new(),
            endpoint: String::new(),
            api_key: String::new(),
            allow_insecure_http: false,
            prompt: None,
        };
        assert!(!parakeet_http.is_streaming());
    }

    #[tokio::test]
    async fn a_wedged_local_engine_gives_up_instead_of_hanging_the_daemon() {
        let audio = persistent_test_wav("voxkey_wedged_");
        let transcriber = Transcriber::WhisperCpp {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "exec sleep 120".to_string(),
                "{audio_file}".to_string(),
            ],
        };

        let error = transcriber
            .transcribe_within(Duration::from_millis(200), Purpose::Final, &audio)
            .await
            .expect_err("a wedged engine must not resolve");

        assert!(
            error.to_string().contains("timed out"),
            "unexpected error: {error}"
        );
        assert!(!audio.exists(), "temp audio must be cleaned up on timeout");
    }

    #[tokio::test]
    async fn final_recording_failure_leaves_audio_owned_by_the_caller() {
        let audio = persistent_test_wav("voxkey_recoverable_failure_");
        let transcriber = Transcriber::WhisperCpp {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                "exit 42".to_string(),
                "voxkey-test".to_string(),
                "{audio_file}".to_string(),
            ],
        };

        transcriber
            .transcribe_recording(&audio)
            .await
            .expect_err("the deterministic backend must fail");

        assert!(
            audio.exists(),
            "the final-recording caller must be able to archive a failure"
        );
        std::fs::remove_file(audio).unwrap();
    }

    #[test]
    fn in_memory_pcm_decodes_identically_to_the_wav_round_trip() {
        let pcm: Vec<i16> = vec![-32768, -21846, -1, 0, 1, 16384, 32767];
        let wav = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(wav.path(), spec).unwrap();
        for sample in &pcm {
            writer.write_sample(*sample).unwrap();
        }
        writer.finalize().unwrap();

        let (rate, from_file) = read_wav_samples(wav.path()).unwrap();
        // Split across chunks to prove chunk boundaries change nothing.
        let chunks: Vec<Arc<[i16]>> =
            vec![Arc::from(pcm[..3].to_vec()), Arc::from(pcm[3..].to_vec())];

        assert_eq!(rate, 16_000);
        assert_eq!(to_normalized_samples(&chunks, 1).unwrap(), from_file);
    }

    #[test]
    fn stereo_audio_is_downmixed_before_parakeet_inference() {
        let pcm: Vec<i16> = vec![32767, -32768, 16384, 16384];
        let wav = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(wav.path(), spec).unwrap();
        for sample in &pcm {
            writer.write_sample(*sample).unwrap();
        }
        writer.finalize().unwrap();

        let (_, from_file) = read_wav_samples(wav.path()).unwrap();
        let chunks = vec![Arc::from(pcm)];

        assert_eq!(from_file.len(), 2);
        assert!(
            from_file[0].abs() < 0.0001,
            "opposite channels did not cancel"
        );
        assert!((from_file[1] - 0.5).abs() < 0.0001);
        assert_eq!(to_normalized_samples(&chunks, 2).unwrap(), from_file);
    }

    #[test]
    fn final_wav_padding_appends_one_second_without_changing_the_capture() {
        let audio = persistent_test_wav("voxkey_padding_");

        let padded = padded_wav_copy(&audio, Duration::from_secs(1)).unwrap();
        let original = hound::WavReader::open(&audio).unwrap();
        let mut reader = hound::WavReader::open(&padded).unwrap();
        let samples = reader
            .samples::<i16>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(original.len(), 1);
        assert_eq!(samples.len(), 16_001);
        assert_eq!(samples[0], 123);
        assert!(samples[1..].iter().all(|sample| *sample == 0));
        std::fs::remove_file(audio).unwrap();
    }

    #[tokio::test]
    async fn file_only_backends_reject_in_memory_pcm() {
        let transcriber = Transcriber::WhisperCpp {
            command: "whisper-cpp".to_string(),
            args: vec![],
        };

        let error = transcriber
            .transcribe_pcm(
                Duration::from_secs(1),
                Purpose::Final,
                16_000,
                1,
                &[Arc::from(vec![0_i16; 8])],
            )
            .await
            .expect_err("whisper.cpp cannot take PCM");

        assert!(!transcriber.accepts_pcm());
        assert!(
            error.to_string().contains("needs recorded audio"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn realtime_backend_rejects_batch_transcription_without_panicking() {
        let audio = persistent_test_wav("voxkey_realtime_reject_");

        let error = Transcriber::MistralRealtime
            .transcribe_within(Duration::from_secs(1), Purpose::Final, &audio)
            .await
            .expect_err("a streaming backend cannot accept a batch audio file");

        assert!(error.to_string().contains("streaming"), "{error}");
        assert!(!audio.exists(), "rejected audio must still be cleaned up");
    }

    #[test]
    fn backend_description_never_includes_the_api_key() {
        let mistral = Transcriber::Mistral {
            client: reqwest::Client::new(),
            api_key: "sk-super-secret".to_string(),
            model: "voxtral-mini-2602".to_string(),
            endpoint: String::new(),
            prompt: None,
        };

        let description = mistral.describe();

        assert!(!description.contains("sk-super-secret"), "{description}");
        assert!(description.contains("voxtral-mini-2602"), "{description}");
        assert!(
            description.contains(voxkey_ipc::MistralConfig::DEFAULT_ENDPOINT),
            "{description}"
        );
    }

    #[test]
    fn batch_backend_description_never_includes_endpoint_credentials() {
        let mistral = Transcriber::Mistral {
            client: reqwest::Client::new(),
            api_key: String::new(),
            model: "voxtral-mini-2602".to_string(),
            endpoint: "https://alice:secret@transcribe.example.test/v1".to_string(),
            prompt: None,
        };

        let description = mistral.describe();

        assert!(!description.contains("alice"), "{description}");
        assert!(!description.contains("secret"), "{description}");
    }

    #[test]
    fn opted_in_private_parakeet_endpoint_is_described_as_valid() {
        let parakeet = Transcriber::ParakeetHttp {
            client: reqwest::Client::new(),
            model: "parakeet-tdt-0.6b-v3".to_string(),
            endpoint: "http://192.168.1.132:8000/v1/audio/transcriptions".to_string(),
            api_key: "must-not-appear".to_string(),
            allow_insecure_http: true,
            prompt: None,
        };

        let description = parakeet.describe();

        assert!(description.contains("192.168.1.132:8000"), "{description}");
        assert!(!description.contains("<invalid endpoint>"), "{description}");
    }

    #[test]
    fn batch_endpoint_rejects_embedded_credentials() {
        let error = batch_endpoint(
            "https://alice:secret@transcribe.example.test/v1",
            unauthenticated_endpoint_policy(false),
        )
        .expect_err("credentials must not be accepted in a batch endpoint");

        assert!(error.to_string().contains("credentials"), "{error}");
    }

    #[test]
    fn private_http_requires_an_explicit_opt_in() {
        let endpoint = "http://192.168.1.132:8000/v1/audio/transcriptions";
        let error = batch_endpoint(endpoint, unauthenticated_endpoint_policy(false))
            .expect_err("LAN audio must not be sent in plaintext without permission");

        assert!(
            error.to_string().contains("Allow unencrypted LAN audio"),
            "{error}"
        );
        assert!(batch_endpoint(endpoint, unauthenticated_endpoint_policy(true)).is_ok());
        assert!(endpoint_uses_unencrypted_private_network(endpoint));
        assert!(!endpoint_uses_unencrypted_private_network(
            "https://192.168.1.132:8000/v1/audio/transcriptions"
        ));
    }

    #[test]
    fn private_http_opt_in_is_limited_to_literal_private_addresses() {
        for endpoint in [
            "http://10.8.0.4:8000/v1/audio/transcriptions",
            "http://172.31.4.9:8000/v1/audio/transcriptions",
            "http://[fd12:3456:789a::4]:8000/v1/audio/transcriptions",
        ] {
            assert!(
                batch_endpoint(endpoint, unauthenticated_endpoint_policy(true)).is_ok(),
                "private endpoint should be allowed: {endpoint}"
            );
        }
        for endpoint in [
            "http://203.0.113.10:8000/v1/audio/transcriptions",
            "http://parakeet.example.test:8000/v1/audio/transcriptions",
            "http://[fe80::1]:8000/v1/audio/transcriptions",
        ] {
            let error = batch_endpoint(endpoint, unauthenticated_endpoint_policy(true))
                .expect_err("the opt-in must not allow public, named, or link-local hosts");
            assert!(error.to_string().contains("private IP"), "{error}");
        }
    }

    #[test]
    fn loopback_http_does_not_require_an_opt_in() {
        assert!(
            batch_endpoint(
                "http://127.0.0.1:8000/v1/audio/transcriptions",
                unauthenticated_endpoint_policy(false),
            )
            .is_ok()
        );
        assert!(
            batch_endpoint(
                "http://[::1]:8000/v1/audio/transcriptions",
                unauthenticated_endpoint_policy(false),
            )
            .is_ok()
        );
    }

    #[test]
    fn authenticated_batch_endpoint_never_allows_plaintext() {
        let error = batch_endpoint(
            "http://127.0.0.1:8000/v1/audio/transcriptions",
            BatchEndpointPolicy::Authenticated,
        )
        .expect_err("a bearer credential must never cross plaintext HTTP");
        assert!(error.to_string().contains("https://"), "{error}");
        assert!(
            batch_endpoint(
                "https://transcribe.example.test/v1/audio/transcriptions",
                BatchEndpointPolicy::Authenticated,
            )
            .is_ok()
        );
    }

    #[test]
    fn mistral_batch_rejects_a_blank_api_key_before_uploading_audio() {
        for api_key in ["", "  \t\n"] {
            let error = batch_authorization_value(api_key)
                .expect_err("blank credentials must not reach a batch request");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("Add a Mistral API key"));
        }
        assert_eq!(
            batch_authorization_value("  sk-batch-key \n").unwrap(),
            "Bearer sk-batch-key"
        );
    }

    #[test]
    fn backend_description_shows_the_resolved_whisper_command() {
        let whisper = Transcriber::WhisperCpp {
            command: "/usr/bin/whisper".to_string(),
            args: vec!["-m".to_string(), "model.bin".to_string()],
        };

        assert_eq!(
            whisper.describe(),
            "whisper.cpp via `/usr/bin/whisper` (2 configured arguments)"
        );
    }

    #[test]
    fn whisper_backend_description_does_not_echo_argument_secrets() {
        let whisper = Transcriber::WhisperCpp {
            command: "/usr/bin/whisper-wrapper".to_string(),
            args: vec![
                "--api-key".to_string(),
                "sk-private-wrapper-key".to_string(),
                "--prompt".to_string(),
                "Private customer vocabulary".to_string(),
            ],
        };

        let description = whisper.describe();

        assert!(
            !description.contains("sk-private-wrapper-key"),
            "{description}"
        );
        assert!(
            !description.contains("Private customer vocabulary"),
            "{description}"
        );
    }

    #[test]
    fn only_on_device_providers_report_running_locally() {
        assert!(
            Transcriber::WhisperCpp {
                command: String::new(),
                args: vec![],
            }
            .runs_locally()
        );

        assert!(
            !Transcriber::Mistral {
                client: reqwest::Client::new(),
                api_key: String::new(),
                model: String::new(),
                endpoint: String::new(),
                prompt: None,
            }
            .runs_locally()
        );

        assert!(
            !Transcriber::ParakeetHttp {
                client: reqwest::Client::new(),
                model: String::new(),
                endpoint: String::new(),
                api_key: String::new(),
                allow_insecure_http: false,
                prompt: None,
            }
            .runs_locally()
        );

        assert!(!Transcriber::MistralRealtime.runs_locally());
    }

    #[tokio::test]
    async fn from_config_creates_parakeet_variant() {
        let config = TranscriberConfig {
            provider: TranscriberProvider::Parakeet,
            whisper_cpp: WhisperCppConfig::default(),
            mistral: MistralConfig::default(),
            mistral_realtime: MistralRealtimeConfig::default(),
            parakeet: voxkey_ipc::ParakeetConfig {
                model: "voxkey-test-model-that-does-not-exist".to_string(),
                backend: voxkey_ipc::ParakeetBackend::Local,
                endpoint: String::new(),
                api_key: String::new(),
                allow_insecure_http: false,
                execution_provider: voxkey_ipc::ExecutionProviderChoice::Cpu,
            },
        };
        let t = Transcriber::from_config(&config, 16000, &[]);
        assert!(!t.is_streaming());
        match t {
            Transcriber::Parakeet { recognizer, .. } => {
                assert!(recognizer.get().is_none());
            }
            _ => panic!("Expected Parakeet variant"),
        }
    }

    #[tokio::test]
    async fn from_config_normalizes_local_parakeet_hotwords() {
        let mut config = TranscriberConfig {
            provider: TranscriberProvider::Parakeet,
            ..TranscriberConfig::default()
        };
        config.parakeet.model = "voxkey-test-model-that-does-not-exist".to_string();
        config.parakeet.backend = voxkey_ipc::ParakeetBackend::Local;
        let vocabulary = vec![
            "  ".to_string(),
            " Voxkey ".to_string(),
            String::new(),
            "Parakeet".to_string(),
        ];

        let transcriber = Transcriber::from_config(&config, 16_000, &vocabulary);

        match transcriber {
            Transcriber::Parakeet { hotwords, .. } => {
                assert_eq!(hotwords, vec!["Voxkey", "Parakeet"]);
            }
            _ => panic!("Expected Parakeet variant"),
        }
    }

    #[test]
    fn from_config_creates_parakeet_http_variant() {
        let config = TranscriberConfig {
            provider: TranscriberProvider::Parakeet,
            parakeet: voxkey_ipc::ParakeetConfig {
                model: "parakeet-tdt-0.6b-v3".to_string(),
                backend: voxkey_ipc::ParakeetBackend::Http,
                endpoint: "http://192.168.1.132:8000/v1/audio/transcriptions".to_string(),
                api_key: "server-token".to_string(),
                allow_insecure_http: true,
                execution_provider: voxkey_ipc::ExecutionProviderChoice::Cuda,
            },
            ..Default::default()
        };

        let transcriber = Transcriber::from_config(&config, 16000, &["Voxkey".to_string()]);
        match transcriber {
            Transcriber::ParakeetHttp {
                model,
                endpoint,
                api_key,
                allow_insecure_http,
                prompt,
                ..
            } => {
                assert_eq!(model, "parakeet-tdt-0.6b-v3");
                assert_eq!(
                    endpoint,
                    "http://192.168.1.132:8000/v1/audio/transcriptions"
                );
                assert_eq!(api_key, "server-token");
                assert!(allow_insecure_http);
                assert_eq!(prompt.as_deref(), Some("Important Vocabulary: Voxkey"));
            }
            _ => panic!("Expected ParakeetHttp variant"),
        }
    }

    #[test]
    fn parakeet_http_omits_prompt_without_vocabulary() {
        let config = TranscriberConfig {
            provider: TranscriberProvider::Parakeet,
            parakeet: voxkey_ipc::ParakeetConfig {
                model: "parakeet-tdt-0.6b-v3".to_string(),
                backend: voxkey_ipc::ParakeetBackend::Http,
                endpoint: "http://192.168.1.132:8000/v1/audio/transcriptions".to_string(),
                api_key: String::new(),
                allow_insecure_http: false,
                execution_provider: voxkey_ipc::ExecutionProviderChoice::Cpu,
            },
            ..Default::default()
        };

        let transcriber = Transcriber::from_config(&config, 16_000, &[]);
        match transcriber {
            Transcriber::ParakeetHttp { prompt, .. } => assert_eq!(prompt, None),
            _ => panic!("Expected ParakeetHttp variant"),
        }
    }

    #[test]
    fn parse_mistral_response_extracts_text() {
        let json = r#"{"text": " Hello, world! "}"#;
        let text = parse_mistral_response(json).unwrap();
        assert_eq!(text, " Hello, world! ");
    }

    #[test]
    fn parse_mistral_response_rejects_invalid_json() {
        let json = r#"{"error": "unauthorized"}"#;
        assert!(parse_mistral_response(json).is_err());
    }

    #[tokio::test]
    async fn oversized_successful_http_body_is_rejected_before_parsing() {
        let chunks = futures_util::stream::iter([Ok::<_, std::io::Error>(vec![
            b'x';
            MAX_HTTP_SUCCESS_BODY_BYTES
                + 1
        ])]);

        let result = collect_bounded_body(chunks, None, MAX_HTTP_SUCCESS_BODY_BYTES).await;

        assert!(result.is_err());
    }

    /// A transcription subprocess must not outlive the daemon: if the task
    /// awaiting it is cancelled (runtime shutdown, panic path), the child has
    /// to be killed instead of orphaned.
    #[tokio::test]
    async fn whisper_cpp_child_is_killed_when_transcription_is_cancelled() {
        let pid_path = std::env::temp_dir().join(format!("voxkey_test_{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&pid_path);

        let command = "sh".to_string();
        let args = vec![
            "-c".to_string(),
            format!("echo $$ > {}; exec sleep 60", pid_path.display()),
        ];
        let audio = std::path::Path::new("/tmp/nonexistent.wav");

        let task = tokio::spawn(async move {
            let _ = transcribe_whisper_cpp(&command, &args, Purpose::Final, audio).await;
        });

        let pid: i32 = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(contents) = std::fs::read_to_string(&pid_path)
                    && let Ok(pid) = contents.trim().parse()
                {
                    break pid;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child process did not start");

        task.abort();
        let _ = task.await;

        // Allow tokio's reaper a moment to collect the killed child; a reaped
        // but uncollected child shows up as a zombie, which also counts as dead.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let dead = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat.split_whitespace().nth(2) == Some("Z"),
        };
        let _ = std::fs::remove_file(&pid_path);
        assert!(
            dead,
            "transcription child process {pid} survived task cancellation"
        );
    }

    #[tokio::test]
    async fn cancelling_batch_transcription_removes_its_private_audio() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let _ = accepted_tx.send(());
            let _socket = socket;
            std::future::pending::<()>().await;
        });

        let audio = persistent_test_wav("voxkey_cancelled_transcription_");

        let transcriber = Transcriber::ParakeetHttp {
            client: reqwest::Client::new(),
            model: "test-model".to_string(),
            endpoint: format!("http://{address}/v1/audio/transcriptions"),
            api_key: String::new(),
            allow_insecure_http: false,
            prompt: None,
        };
        let task_audio = audio.clone();
        let task = tokio::spawn(async move {
            let _ = transcriber.transcribe(&task_audio).await;
        });

        tokio::time::timeout(Duration::from_secs(5), accepted_rx)
            .await
            .expect("transcription request did not connect")
            .expect("accept notification was dropped");
        task.abort();
        let _ = task.await;

        assert!(
            !audio.exists(),
            "cancelled transcription left captured audio behind"
        );
    }

    #[tokio::test]
    async fn whisper_cpp_rejects_excessive_stdout() {
        let command = "sh";
        let args = vec![
            "-c".to_string(),
            format!("head -c {} /dev/zero", MAX_WHISPER_STDOUT_BYTES + 1),
        ];

        let error = transcribe_whisper_cpp(
            command,
            &args,
            Purpose::Final,
            Path::new("/tmp/nonexistent.wav"),
        )
        .await
        .expect_err("oversized command output must be rejected");

        assert!(error.to_string().contains("too much stdout"), "{error}");
    }

    #[tokio::test]
    async fn whisper_cpp_truncates_excessive_stderr() {
        let args = vec![
            "-c".to_string(),
            format!(
                "head -c {} /dev/zero >&2; exit 1",
                MAX_WHISPER_STDERR_BYTES + 1
            ),
        ];

        let error = transcribe_whisper_cpp(
            "sh",
            &args,
            Purpose::Final,
            Path::new("/tmp/nonexistent.wav"),
        )
        .await
        .expect_err("failed commands must surface a bounded diagnostic");
        let message = error.to_string();

        assert!(message.len() <= MAX_WHISPER_STDERR_BYTES + 128, "{message}");
        assert!(message.contains("[truncated]"), "{message}");
    }

    #[test]
    fn whisper_stdout_must_be_valid_utf8() {
        let error = decode_whisper_transcript(vec![b'h', b'i', 0xff])
            .expect_err("invalid subprocess text must not be injected as replacement characters");

        assert!(error.to_string().contains("UTF-8"), "{error}");
    }

    /// A transcription endpoint that accepts the connection but never replies
    /// must not hang the daemon forever: the request has to time out and
    /// surface an error so the state machine can return to Idle and notify the
    /// user. Uses a deliberately short client timeout to keep the test fast.
    #[tokio::test]
    async fn batch_transcription_errors_instead_of_hanging_on_dead_endpoint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept connections but never send a response, so only a client-side
        // timeout can end the request.
        tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                if let Ok((stream, _)) = listener.accept().await {
                    held.push(stream);
                }
            }
        });

        let audio = std::env::temp_dir().join(format!("voxkey_test_{}.wav", std::process::id()));
        std::fs::write(&audio, b"RIFF....WAVEfmt ").unwrap();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(300))
            .build()
            .unwrap();
        let endpoint = format!("http://{addr}/v1/audio/transcriptions");

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            transcribe_parakeet_http(ModelServerTranscription {
                server: ModelServer {
                    client: &client,
                    api_key: "",
                    model: "test-model",
                    endpoint: &endpoint,
                    allow_insecure_http: false,
                    prompt: None,
                },
                purpose: Purpose::Final,
                audio_path: &audio,
            }),
        )
        .await;

        let _ = std::fs::remove_file(&audio);
        assert!(
            outcome.is_ok(),
            "transcription hung past the client timeout instead of cancelling"
        );
        assert!(
            outcome.unwrap().is_err(),
            "an unresponsive endpoint must produce a transcription error"
        );
    }

    #[tokio::test]
    async fn batch_http_errors_do_not_echo_provider_response_bodies() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }

            let body = "provider error details ".repeat(4096);
            let headers = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        });

        let audio = std::env::temp_dir().join(format!(
            "voxkey_bounded_http_error_test_{}.wav",
            std::process::id()
        ));
        std::fs::write(&audio, b"RIFF....WAVEfmt ").unwrap();
        let client = reqwest::Client::new();
        let endpoint = format!("http://{addr}/v1/audio/transcriptions");
        let error = transcribe_parakeet_http(ModelServerTranscription {
            server: ModelServer {
                client: &client,
                api_key: "",
                model: "test-model",
                endpoint: &endpoint,
                allow_insecure_http: false,
                prompt: None,
            },
            purpose: Purpose::Final,
            audio_path: &audio,
        })
        .await
        .expect_err("the endpoint deliberately returned an error");
        let _ = std::fs::remove_file(audio);
        let message = error.to_string();

        assert!(message.contains("400 Bad Request"), "{message}");
        assert!(!message.contains("provider error details"), "{message}");
    }

    #[tokio::test]
    async fn parakeet_http_sends_its_model_prompt_and_optional_authorization() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_tx, request_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }

            let body = r#"{"text":"Parakeet server result"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            let _ = request_tx.send(String::from_utf8_lossy(&request).into_owned());
        });

        let audio = std::env::temp_dir().join(format!(
            "voxkey_parakeet_http_test_{}.wav",
            std::process::id()
        ));
        std::fs::write(&audio, b"RIFF....WAVEfmt ").unwrap();
        let endpoint = format!("http://{addr}/v1/audio/transcriptions");
        let client = reqwest::Client::new();
        let result = transcribe_parakeet_http(ModelServerTranscription {
            server: ModelServer {
                client: &client,
                api_key: "server-token",
                model: "parakeet-tdt-0.6b-v3",
                endpoint: &endpoint,
                allow_insecure_http: false,
                prompt: Some("Important Vocabulary: VoxKey, Siobhan"),
            },
            purpose: Purpose::Final,
            audio_path: &audio,
        })
        .await
        .unwrap();
        let request = request_rx.await.unwrap();
        let _ = std::fs::remove_file(audio);

        assert_eq!(result, "Parakeet server result");
        assert!(request.starts_with("POST /v1/audio/transcriptions HTTP/1.1"));
        assert!(request.contains("name=\"file\""));
        assert!(request.contains("Content-Type: audio/wav"));
        assert!(request.contains("name=\"model\""));
        assert!(request.contains("parakeet-tdt-0.6b-v3"));
        assert!(request.contains("name=\"prompt\""));
        assert!(request.contains("Important Vocabulary: VoxKey, Siobhan"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer server-token")
        );
    }

    #[tokio::test]
    async fn parakeet_http_chunks_recordings_beyond_the_server_duration_limit() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            request
        }

        fn uploaded_wav_duration(request: &[u8]) -> f64 {
            let wav_start = request
                .windows(4)
                .position(|part| part == b"RIFF")
                .expect("multipart request did not contain a WAV file");
            let reader = hound::WavReader::new(std::io::Cursor::new(&request[wav_start..]))
                .expect("uploaded file was not a readable WAV");
            f64::from(reader.duration()) / f64::from(reader.spec().sample_rate)
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let durations = Arc::new(Mutex::new(Vec::new()));
        let server_durations = durations.clone();
        tokio::spawn(async move {
            for successful_text in ["alpha boundary words", "boundary words omega"] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_request(&mut socket).await;
                let duration = uploaded_wav_duration(&request);
                server_durations.lock().unwrap().push(duration);
                let (status, body) = if duration > 120.0 {
                    (
                        "422 Unprocessable Entity",
                        r#"{"error":{"message":"The WAV duration exceeds the configured limit."}}"#
                            .to_string(),
                    )
                } else {
                    ("200 OK", format!(r#"{{"text":"{successful_text}"}}"#))
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        // A low sample rate keeps this 121-second regression fixture tiny.
        // The real server rejects anything above 120 seconds regardless of
        // byte size, which is exactly the production failure being replayed.
        let wav = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 10,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(wav.path(), spec).unwrap();
        for sample in 0..1_210_i16 {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();

        let client = reqwest::Client::new();
        let endpoint = format!("http://{address}/v1/audio/transcriptions");
        let result = transcribe_parakeet_http(ModelServerTranscription {
            server: ModelServer {
                client: &client,
                api_key: "",
                model: "parakeet-tdt-0.6b-v3",
                endpoint: &endpoint,
                allow_insecure_http: false,
                prompt: None,
            },
            purpose: Purpose::Final,
            audio_path: wav.path(),
        })
        .await;
        let transcript =
            result.unwrap_or_else(|error| panic!("long recording was not transcribed: {error}"));

        assert_eq!(transcript, "alpha boundary words omega");
        let durations = durations.lock().unwrap();
        assert_eq!(durations.len(), 2, "expected two bounded uploads");
        assert!(durations.iter().all(|duration| *duration <= 120.0));
        assert!(
            durations.iter().sum::<f64>() > 121.0,
            "chunks need overlap so words at a boundary are not cut off"
        );
    }

    #[tokio::test]
    async fn parakeet_http_requires_its_own_endpoint() {
        let audio = std::env::temp_dir().join(format!(
            "voxkey_parakeet_http_missing_endpoint_{}.wav",
            std::process::id()
        ));
        std::fs::write(&audio, b"RIFF....WAVEfmt ").unwrap();
        let client = reqwest::Client::new();
        let error = transcribe_parakeet_http(ModelServerTranscription {
            server: ModelServer {
                client: &client,
                api_key: "",
                model: "parakeet-tdt-0.6b-v3",
                endpoint: "",
                allow_insecure_http: false,
                prompt: None,
            },
            purpose: Purpose::Final,
            audio_path: &audio,
        })
        .await
        .unwrap_err();
        let _ = std::fs::remove_file(audio);

        assert!(
            error
                .to_string()
                .contains("Set a transcription server address")
        );
    }

    #[test]
    fn whisper_args_gain_prompt_from_vocabulary() {
        let args = whisper_args_with_prompt(
            &["-m".to_string(), "model.bin".to_string()],
            Some("Important Vocabulary: Voxkey"),
        );
        assert_eq!(
            args,
            vec![
                "-m",
                "model.bin",
                "--prompt",
                "Important Vocabulary: Voxkey"
            ]
        );
    }

    #[test]
    fn whisper_args_with_existing_prompt_are_untouched() {
        let existing = vec!["--prompt".to_string(), "my prompt".to_string()];
        let args = whisper_args_with_prompt(&existing, Some("Important Vocabulary: X"));
        assert_eq!(args, existing);
    }

    #[test]
    fn whisper_short_prompt_option_is_not_duplicated() {
        let existing = vec!["-p".to_string(), "my prompt".to_string()];
        let args = whisper_args_with_prompt(&existing, Some("Important Vocabulary: X"));
        assert_eq!(args, existing);
    }

    #[test]
    fn whisper_args_fill_a_trailing_prompt_option() {
        let existing = vec![
            "-m".to_string(),
            "model.bin".to_string(),
            "--prompt".to_string(),
        ];
        let args = whisper_args_with_prompt(&existing, Some("Important Vocabulary: Voxkey"));

        assert_eq!(
            args,
            vec![
                "-m",
                "model.bin",
                "--prompt",
                "Important Vocabulary: Voxkey"
            ]
        );
        assert_eq!(
            resolve_whisper_args(&args, Path::new("/tmp/recording.wav")),
            vec![
                "-m",
                "model.bin",
                "--prompt",
                "Important Vocabulary: Voxkey",
                "/tmp/recording.wav"
            ]
        );
    }

    #[test]
    fn dangling_whisper_prompt_without_vocabulary_cannot_consume_the_audio_path() {
        let existing = vec!["--prompt".to_string()];
        let args = whisper_args_with_prompt(&existing, None);

        assert_eq!(
            resolve_whisper_args(&args, Path::new("/tmp/recording.wav")),
            vec!["--prompt", "", "/tmp/recording.wav"]
        );
    }

    #[test]
    fn dangling_whisper_short_prompt_cannot_consume_the_audio_path() {
        let existing = vec!["-p".to_string()];
        let args = whisper_args_with_prompt(&existing, None);

        assert_eq!(
            resolve_whisper_args(&args, Path::new("/tmp/recording.wav")),
            vec!["-p", "", "/tmp/recording.wav"]
        );
    }

    #[test]
    fn whisper_args_without_vocabulary_are_untouched() {
        let existing = vec!["-m".to_string(), "model.bin".to_string()];
        assert_eq!(whisper_args_with_prompt(&existing, None), existing);
    }

    #[test]
    fn standard_whisper_cli_gets_stable_decode_defaults_and_discovered_vad() {
        let temp = tempfile::tempdir().unwrap();
        let model = temp.path().join("ggml-base.bin");
        let vad = temp.path().join("ggml-silero-v6.2.0.bin");
        std::fs::write(&model, b"model").unwrap();
        std::fs::write(&vad, b"vad").unwrap();
        let args = vec!["-m".to_string(), model.to_string_lossy().into_owned()];

        let resolved = whisper_args_with_defaults("/usr/bin/whisper-cli", &args, Some("Style."));

        for option in [
            "--no-timestamps",
            "--flash-attn",
            "--temperature",
            "--no-fallback",
            "--suppress-nst",
            "--vad-model",
            "--vad",
            "--vad-threshold",
            "--vad-min-speech-duration-ms",
            "--vad-speech-pad-ms",
            "--prompt",
        ] {
            assert!(
                resolved.iter().any(|argument| argument == option),
                "{resolved:?}"
            );
        }
        assert!(resolved.iter().any(|argument| argument == "0.2"));
        assert!(
            resolved
                .iter()
                .any(|argument| argument == &vad.to_string_lossy())
        );
    }

    #[test]
    fn whisper_defaults_respect_explicit_user_decode_choices() {
        let args = [
            "--no-flash-attn",
            "--temperature",
            "0.7",
            "--vad-model",
            "/custom/vad.bin",
            "--vad-threshold",
            "0.8",
        ]
        .map(str::to_string);

        let resolved = whisper_args_with_defaults("whisper-cpp", &args, None);

        assert!(!resolved.iter().any(|argument| argument == "--flash-attn"));
        assert_eq!(
            resolved
                .iter()
                .filter(|argument| *argument == "--temperature")
                .count(),
            1
        );
        assert_eq!(
            resolved
                .iter()
                .filter(|argument| *argument == "--vad-threshold")
                .count(),
            1
        );
    }

    #[test]
    fn arbitrary_transcriber_wrappers_do_not_receive_whisper_only_flags() {
        let args = ["-c", "print('ok')"].map(str::to_string);
        let resolved = whisper_args_with_defaults("python3", &args, Some("Style."));

        assert_eq!(
            resolved,
            ["-c", "print('ok')", "--prompt", "Style."].map(str::to_string)
        );
    }

    #[test]
    fn whisper_args_include_audio_without_a_placeholder() {
        let args = vec!["-m".to_string(), "model.bin".to_string()];

        assert_eq!(
            resolve_whisper_args(&args, Path::new("/tmp/recording.wav")),
            vec!["-m", "model.bin", "/tmp/recording.wav"]
        );
        assert_eq!(
            resolve_whisper_args(
                &["--file".to_string(), "{audio_file}".to_string()],
                Path::new("/tmp/recording.wav")
            ),
            vec!["--file", "/tmp/recording.wav"]
        );
    }

    #[test]
    fn concurrent_recognizer_builds_get_independent_hotwords_files() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let first = create_hotwords_file_in(temp.path(), &["first vocabulary".to_string()])
            .expect("first build should get a hotwords file");
        let second = create_hotwords_file_in(temp.path(), &["second vocabulary".to_string()])
            .expect("second build should get a hotwords file");
        let first_path = first.path().to_path_buf();
        let second_path = second.path().to_path_buf();

        assert_ne!(first_path, second_path);
        assert_eq!(
            std::fs::read_to_string(&first_path).unwrap(),
            "first vocabulary"
        );
        assert_eq!(
            std::fs::read_to_string(&second_path).unwrap(),
            "second vocabulary"
        );
        assert_eq!(
            std::fs::metadata(&first_path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        drop(first);
        assert!(!first_path.exists());
        assert!(second_path.exists());
    }

    #[test]
    fn unwritable_hotwords_cache_is_reported_instead_of_ignoring_vocabulary() {
        let temp = tempfile::tempdir().unwrap();
        let blocking_file = temp.path().join("not-a-directory");
        std::fs::write(&blocking_file, b"blocking file").unwrap();

        assert!(create_hotwords_file_in(&blocking_file, &["Voxkey".to_string()]).is_err());
    }

    #[test]
    fn reconstructs_v2_parakeet_bpe_merge_scores_from_token_order() {
        let tokens = "<unk> 0\n▁t 1\n▁th 2\nin 3\n<blk> 4\n";

        assert_eq!(
            parakeet_bpe_vocab_from_tokens(tokens).unwrap(),
            "<unk>\t0\n▁t\t-0\n▁th\t-1\nin\t-2\n"
        );
    }

    #[test]
    fn reconstructs_v3_parakeet_bpe_scores_after_the_reserved_preamble() {
        let tokens = "<unk> 0\n<|nospeech|> 1\n0 2\n<|spltoken0|> 3\nen 4\n▁s 5\n<blk> 6\n";

        assert_eq!(
            parakeet_bpe_vocab_from_tokens(tokens).unwrap(),
            "<unk>\t0\n<|nospeech|>\t0\n0\t0\n<|spltoken0|>\t0\nen\t-0\n▁s\t-1\n"
        );
    }

    #[test]
    fn malformed_token_order_cannot_create_a_wrong_bpe_vocabulary() {
        let error = parakeet_bpe_vocab_from_tokens("<unk> 0\n▁t 2\n<blk> 3\n")
            .expect_err("non-contiguous token IDs must be rejected");

        assert!(error.to_string().contains("expected 1"), "{error}");
    }

    #[test]
    fn local_vocabulary_selects_the_complete_parakeet_hotword_configuration() {
        let model_dir = Path::new("/models/parakeet-v3");
        let hotwords = Path::new("/cache/hotwords.txt");
        let bpe_vocab = Path::new("/cache/bpe.vocab");
        let config = parakeet_recognizer_config(
            model_dir,
            voxkey_ipc::ExecutionProviderChoice::Cpu,
            16_000,
            Some((hotwords, bpe_vocab)),
        );

        assert_eq!(
            config.decoding_method.as_deref(),
            Some("modified_beam_search")
        );
        assert_eq!(config.hotwords_file.as_deref(), Some("/cache/hotwords.txt"));
        assert_eq!(config.hotwords_score, PARAKEET_HOTWORDS_SCORE);
        assert_eq!(config.model_config.modeling_unit.as_deref(), Some("bpe"));
        assert_eq!(
            config.model_config.bpe_vocab.as_deref(),
            Some("/cache/bpe.vocab")
        );
        assert_eq!(
            config.model_config.model_type.as_deref(),
            Some("nemo_transducer")
        );
        assert_eq!(config.model_config.provider.as_deref(), Some("cpu"));
    }

    #[test]
    fn parakeet_without_vocabulary_stays_on_greedy_search() {
        let config = parakeet_recognizer_config(
            Path::new("/models/parakeet-v3"),
            voxkey_ipc::ExecutionProviderChoice::Auto,
            16_000,
            None,
        );

        assert_eq!(config.decoding_method.as_deref(), Some("greedy_search"));
        assert!(config.hotwords_file.is_none());
        assert!(config.model_config.modeling_unit.is_none());
        assert!(config.model_config.bpe_vocab.is_none());
        assert!(config.model_config.provider.is_none());
    }

    #[test]
    fn parakeet_sentencepiece_tokens_merge_into_timed_words() {
        let tokens = vec![
            "▁Hello".to_string(),
            ",".to_string(),
            "▁world".to_string(),
            "!".to_string(),
        ];
        let timestamps = [0.1, 0.4, 0.6, 0.9];
        let durations = [0.2, 0.1, 0.2, 0.1];

        let words =
            merge_parakeet_tokens(&tokens, Some(&timestamps), Some(&durations), 16_000, 32_000);

        assert_eq!(words.len(), 2);
        assert_eq!(words[0], TimedWord::new("Hello,", 1_600, 8_000));
        assert_eq!(words[1], TimedWord::new("world!", 9_600, 16_000));
    }

    #[test]
    fn missing_or_misaligned_token_timestamps_fall_back_to_estimation() {
        let tokens = vec!["▁hello".to_string(), "▁world".to_string()];
        assert!(merge_parakeet_tokens(&tokens, None, None, 16_000, 20_000).is_empty());
        assert!(merge_parakeet_tokens(&tokens, Some(&[0.1]), None, 16_000, 20_000).is_empty());
    }

    #[test]
    #[ignore = "requires an external Parakeet model and its NeMo tokenizer.vocab"]
    fn generated_parakeet_bpe_vocab_matches_the_nemo_tokenizer() {
        let model_dir = std::env::var_os("VOXKEY_TEST_PARAKEET_MODEL_DIR")
            .map(std::path::PathBuf::from)
            .expect("set VOXKEY_TEST_PARAKEET_MODEL_DIR to the extracted model directory");
        let reference_path = std::env::var_os("VOXKEY_TEST_PARAKEET_BPE_VOCAB")
            .map(std::path::PathBuf::from)
            .expect("set VOXKEY_TEST_PARAKEET_BPE_VOCAB to NeMo's tokenizer.vocab");
        let tokens = std::fs::read_to_string(model_dir.join("tokens.txt")).unwrap();
        let reference = std::fs::read_to_string(reference_path).unwrap();

        assert_eq!(parakeet_bpe_vocab_from_tokens(&tokens).unwrap(), reference);
    }

    #[test]
    #[ignore = "requires an external ParakeetV3 model and WAV fixture"]
    fn real_parakeet_v3_vocabulary_changes_decoding() {
        let model_dir = std::env::var_os("VOXKEY_TEST_PARAKEET_MODEL_DIR")
            .map(std::path::PathBuf::from)
            .expect("set VOXKEY_TEST_PARAKEET_MODEL_DIR to the extracted model directory");
        let wav_path = std::env::var_os("VOXKEY_TEST_PARAKEET_WAV")
            .map(std::path::PathBuf::from)
            .expect("set VOXKEY_TEST_PARAKEET_WAV to the vocabulary test WAV");
        let hotword = std::env::var("VOXKEY_TEST_PARAKEET_HOTWORD")
            .expect("set VOXKEY_TEST_PARAKEET_HOTWORD to the word spoken in the WAV");
        let (sample_rate, samples) = read_wav_samples(&wav_path).unwrap();
        let sample_rate_i32 = parakeet_sample_rate(sample_rate).unwrap();

        let greedy = build_parakeet_recognizer_in(
            &model_dir,
            voxkey_ipc::ExecutionProviderChoice::Cpu,
            sample_rate,
            &[],
        )
        .unwrap();
        let greedy_text = decode_parakeet_samples(&greedy, sample_rate_i32, &samples).unwrap();

        let biased = build_parakeet_recognizer_in(
            &model_dir,
            voxkey_ipc::ExecutionProviderChoice::Cpu,
            sample_rate,
            std::slice::from_ref(&hotword),
        )
        .unwrap();
        let biased_text = decode_parakeet_samples(&biased, sample_rate_i32, &samples).unwrap();

        eprintln!("greedy: {greedy_text}");
        eprintln!("biased: {biased_text}");
        assert_ne!(
            greedy_text, biased_text,
            "the chosen fixture/hotword did not exercise contextual biasing"
        );
        assert!(
            biased_text.to_lowercase().contains(&hotword.to_lowercase()),
            "biased transcript did not contain {hotword:?}: {biased_text}"
        );
    }

    #[test]
    fn blank_xdg_cache_home_uses_the_home_directory_for_hotwords() {
        assert_eq!(
            hotwords_cache_dir_from(
                Some(std::ffi::OsStr::new("")),
                Some(std::ffi::OsStr::new("/home/test")),
            ),
            std::path::PathBuf::from("/home/test/.cache/voxkey")
        );
        assert_eq!(
            hotwords_cache_dir_from(
                Some(std::ffi::OsStr::new("relative-cache")),
                Some(std::ffi::OsStr::new("/home/test")),
            ),
            std::path::PathBuf::from("/home/test/.cache/voxkey")
        );
    }

    #[test]
    fn legacy_shared_hotwords_file_is_removed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("hotwords.txt");
        std::fs::write(&path, "private name").unwrap();
        assert!(path.exists());

        remove_legacy_hotwords_file_at(&path);
        assert!(!path.exists());
    }

    #[test]
    fn parakeet_sample_rate_cannot_wrap_negative_at_the_ffi_boundary() {
        assert_eq!(parakeet_sample_rate(16_000).unwrap(), 16_000);
        assert!(parakeet_sample_rate(u32::MAX).is_err());
    }

    #[test]
    fn in_memory_parakeet_rejects_a_missing_model_before_inference() {
        let model_name = "voxkey-model-that-does-not-exist-for-pcm";

        let error = ensure_parakeet_model_available(model_name)
            .expect_err("missing model files must stop PCM transcription before inference");

        assert!(error.to_string().contains(model_name), "{error}");
    }

    #[test]
    fn from_config_threads_vocabulary_into_whisper_prompt() {
        let config = TranscriberConfig::default();
        let t = Transcriber::from_config(&config, 16000, &["Voxkey".to_string()]);
        match t {
            Transcriber::WhisperCpp { args, .. } => {
                assert!(args.contains(&"--prompt".to_string()));
                assert!(args.iter().any(|a| a.contains("Voxkey")));
            }
            _ => panic!("Expected WhisperCpp variant"),
        }
    }

    #[test]
    fn from_config_selects_the_style_prompt_for_whisper_language_args() {
        let mut config = TranscriberConfig::default();
        config.whisper_cpp.command = "python3".to_string();
        config.whisper_cpp.args = ["--language=es-MX", "{audio_file}"]
            .map(str::to_string)
            .to_vec();

        let transcriber = Transcriber::from_config(&config, 16_000, &[]);

        match transcriber {
            Transcriber::WhisperCpp { args, .. } => {
                let prompt_index = args.iter().position(|arg| arg == "--prompt").unwrap();
                assert_eq!(
                    args[prompt_index + 1],
                    "¡Hola, ¿cómo estás? Encantado de conocerte."
                );
            }
            _ => panic!("Expected WhisperCpp variant"),
        }
    }
}
