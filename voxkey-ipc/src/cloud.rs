// ABOUTME: Catalog of remote speech-to-text services Voxkey can connect.
// ABOUTME: Each entry names the live protocol, default endpoint, model, and keyring.

use crate::TranscriberProvider;

/// How Voxkey talks to a remote speech-to-text service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudProtocol {
    /// OpenAI Audio Transcriptions (`POST` multipart `file` + `model`).
    OpenAiTranscriptions,
    /// Mistral Audio Transcriptions (`POST` multipart `file` + `model` + `context_bias`).
    MistralTranscriptions,
    /// Deepgram Listen (`POST` raw WAV, `Authorization: Token`).
    DeepgramListen,
    /// AssemblyAI pre-recorded (upload, create transcript, poll).
    AssemblyAiPreRecorded,
    /// ElevenLabs Scribe (`POST` multipart `file` + `model_id`).
    ElevenLabsScribe,
    /// Mistral realtime WebSocket (`Authorization: Bearer` on the handshake).
    MistralRealtime,
}

/// Shared non-secret settings for one cloud speech-to-text service.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
pub struct CloudSttConfig {
    /// Legacy plaintext API key. New values live in the desktop keyring.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub endpoint: String,
    /// Permit plaintext HTTP to literal private-network IP addresses.
    #[serde(default)]
    pub allow_insecure_http: bool,
}

/// User-facing facts for one connectable cloud speech-to-text service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloudProvider {
    pub provider: TranscriberProvider,
    pub id: &'static str,
    pub name: &'static str,
    pub combo_label: &'static str,
    pub setup_title: &'static str,
    pub setup_description: &'static str,
    pub location_subtitle: &'static str,
    pub protocol: CloudProtocol,
    pub default_endpoint: &'static str,
    pub default_model: &'static str,
    pub keyring_service: &'static str,
    pub endpoint_required: bool,
    pub allows_insecure_http: bool,
    pub streaming: bool,
}

pub const API_KEY_SERVICE_OPENAI: &str = "openai";
pub const API_KEY_SERVICE_GROQ: &str = "groq";
pub const API_KEY_SERVICE_DEEPGRAM: &str = "deepgram";
pub const API_KEY_SERVICE_ASSEMBLYAI: &str = "assemblyai";
pub const API_KEY_SERVICE_ELEVENLABS: &str = "elevenlabs";

/// Ordered cloud choices shown after whisper.cpp and before local models.
pub const CLOUD_PROVIDERS: &[CloudProvider] = &[
    CloudProvider {
        provider: TranscriberProvider::OpenAi,
        id: "openai",
        name: "OpenAI",
        combo_label: "OpenAI (cloud)",
        setup_title: "OpenAI setup",
        setup_description: "Store your API key and choose a transcription model",
        location_subtitle: "Sends each finished recording to OpenAI",
        protocol: CloudProtocol::OpenAiTranscriptions,
        default_endpoint: "https://api.openai.com/v1/audio/transcriptions",
        default_model: "whisper-1",
        keyring_service: API_KEY_SERVICE_OPENAI,
        endpoint_required: false,
        allows_insecure_http: false,
        streaming: false,
    },
    CloudProvider {
        provider: TranscriberProvider::Groq,
        id: "groq",
        name: "Groq",
        combo_label: "Groq (cloud)",
        setup_title: "Groq setup",
        setup_description: "Store your API key and choose a Groq Whisper model",
        location_subtitle: "Sends each finished recording to Groq",
        protocol: CloudProtocol::OpenAiTranscriptions,
        default_endpoint: "https://api.groq.com/openai/v1/audio/transcriptions",
        default_model: "whisper-large-v3-turbo",
        keyring_service: API_KEY_SERVICE_GROQ,
        endpoint_required: false,
        allows_insecure_http: false,
        streaming: false,
    },
    CloudProvider {
        provider: TranscriberProvider::Mistral,
        id: "mistral",
        name: "Mistral",
        combo_label: "Mistral (cloud)",
        setup_title: "Mistral setup",
        setup_description: "Store your API key and choose the batch transcription model",
        location_subtitle: "Sends each finished recording to Mistral",
        protocol: CloudProtocol::MistralTranscriptions,
        default_endpoint: "https://api.mistral.ai/v1/audio/transcriptions",
        default_model: "voxtral-mini-2602",
        keyring_service: crate::API_KEY_SERVICE_MISTRAL,
        endpoint_required: false,
        allows_insecure_http: false,
        streaming: false,
    },
    CloudProvider {
        provider: TranscriberProvider::MistralRealtime,
        id: "mistral-realtime",
        name: "Mistral Realtime",
        combo_label: "Mistral Realtime (cloud)",
        setup_title: "Mistral Realtime setup",
        setup_description: "Store your API key and choose the realtime transcription model",
        location_subtitle: "Streams audio to Mistral while you speak",
        protocol: CloudProtocol::MistralRealtime,
        default_endpoint: "wss://api.mistral.ai/v1/audio/transcriptions/realtime",
        default_model: "voxtral-mini-transcribe-realtime-2602",
        keyring_service: crate::API_KEY_SERVICE_MISTRAL_REALTIME,
        endpoint_required: false,
        allows_insecure_http: false,
        streaming: true,
    },
    CloudProvider {
        provider: TranscriberProvider::Deepgram,
        id: "deepgram",
        name: "Deepgram",
        combo_label: "Deepgram (cloud)",
        setup_title: "Deepgram setup",
        setup_description: "Store your API key and choose a Deepgram Listen model",
        location_subtitle: "Sends each finished recording to Deepgram",
        protocol: CloudProtocol::DeepgramListen,
        default_endpoint: "https://api.deepgram.com/v1/listen",
        default_model: "nova-3",
        keyring_service: API_KEY_SERVICE_DEEPGRAM,
        endpoint_required: false,
        allows_insecure_http: false,
        streaming: false,
    },
    CloudProvider {
        provider: TranscriberProvider::AssemblyAi,
        id: "assemblyai",
        name: "AssemblyAI",
        combo_label: "AssemblyAI (cloud)",
        setup_title: "AssemblyAI setup",
        setup_description: "Store your API key and choose an AssemblyAI speech model",
        location_subtitle: "Sends each finished recording to AssemblyAI",
        protocol: CloudProtocol::AssemblyAiPreRecorded,
        default_endpoint: "https://api.assemblyai.com",
        default_model: "universal-3-5-pro",
        keyring_service: API_KEY_SERVICE_ASSEMBLYAI,
        endpoint_required: false,
        allows_insecure_http: false,
        streaming: false,
    },
    CloudProvider {
        provider: TranscriberProvider::ElevenLabs,
        id: "elevenlabs",
        name: "ElevenLabs",
        combo_label: "ElevenLabs (cloud)",
        setup_title: "ElevenLabs setup",
        setup_description: "Store your API key and choose an ElevenLabs Scribe model",
        location_subtitle: "Sends each finished recording to ElevenLabs",
        protocol: CloudProtocol::ElevenLabsScribe,
        default_endpoint: "https://api.elevenlabs.io/v1/speech-to-text",
        default_model: "scribe_v2",
        keyring_service: API_KEY_SERVICE_ELEVENLABS,
        endpoint_required: false,
        allows_insecure_http: false,
        streaming: false,
    },
    CloudProvider {
        provider: TranscriberProvider::OpenAiCompatible,
        id: "openai-compatible",
        name: "OpenAI-compatible server",
        combo_label: "OpenAI-compatible server",
        setup_title: "Transcription server setup",
        setup_description: "Connect any server that uses the OpenAI speech-to-text HTTP format",
        location_subtitle: "Sends each finished recording to your transcription server",
        protocol: CloudProtocol::OpenAiTranscriptions,
        default_endpoint: "",
        default_model: "whisper-1",
        keyring_service: crate::API_KEY_SERVICE_MODEL_SERVER,
        endpoint_required: true,
        allows_insecure_http: true,
        streaming: false,
    },
];

pub fn cloud_provider(provider: TranscriberProvider) -> Option<&'static CloudProvider> {
    CLOUD_PROVIDERS
        .iter()
        .find(|candidate| candidate.provider == provider)
}

pub fn cloud_provider_by_service(service: &str) -> Option<&'static CloudProvider> {
    CLOUD_PROVIDERS
        .iter()
        .find(|candidate| candidate.keyring_service == service)
}

pub fn is_api_key_service(service: &str) -> bool {
    cloud_provider_by_service(service).is_some()
}

impl CloudProvider {
    pub fn resolved_model(self, configured: &str) -> String {
        let trimmed = configured.trim();
        if trimmed.is_empty() {
            self.default_model.to_string()
        } else {
            trimmed.to_string()
        }
    }

    pub fn resolved_endpoint(self, configured: &str) -> String {
        let trimmed = configured.trim();
        if trimmed.is_empty() {
            self.default_endpoint.to_string()
        } else {
            trimmed.to_string()
        }
    }

    pub fn stored_model(self, entered: &str) -> String {
        let trimmed = entered.trim();
        if trimmed.is_empty() || trimmed == self.default_model {
            String::new()
        } else {
            trimmed.to_string()
        }
    }

    pub fn stored_endpoint(self, entered: &str) -> String {
        let trimmed = entered.trim();
        if trimmed.is_empty() || trimmed == self.default_endpoint {
            String::new()
        } else {
            trimmed.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_ids_match_serde_provider_names() {
        for cloud in CLOUD_PROVIDERS {
            let json = serde_json::to_string(&cloud.provider).unwrap();
            assert_eq!(json, format!("\"{}\"", cloud.id));
            assert_eq!(
                cloud_provider(cloud.provider).map(|item| item.id),
                Some(cloud.id)
            );
        }
    }

    #[test]
    fn every_cloud_provider_has_connectable_defaults() {
        for cloud in CLOUD_PROVIDERS {
            assert!(!cloud.name.is_empty());
            assert!(!cloud.default_model.is_empty());
            assert!(!cloud.keyring_service.is_empty());
            if cloud.endpoint_required {
                assert!(cloud.default_endpoint.is_empty());
            } else {
                assert!(
                    cloud.default_endpoint.starts_with("https://")
                        || cloud.default_endpoint.starts_with("wss://")
                );
            }
        }
    }

    #[test]
    fn keyring_services_are_unique() {
        for (index, cloud) in CLOUD_PROVIDERS.iter().enumerate() {
            assert!(
                !CLOUD_PROVIDERS[..index]
                    .iter()
                    .any(|candidate| candidate.keyring_service == cloud.keyring_service)
            );
        }
    }

    #[test]
    fn live_api_defaults_match_documented_contracts() {
        let openai = cloud_provider(TranscriberProvider::OpenAi).unwrap();
        assert_eq!(
            openai.default_endpoint,
            "https://api.openai.com/v1/audio/transcriptions"
        );
        assert_eq!(openai.default_model, "whisper-1");
        assert_eq!(openai.protocol, CloudProtocol::OpenAiTranscriptions);

        let groq = cloud_provider(TranscriberProvider::Groq).unwrap();
        assert_eq!(
            groq.default_endpoint,
            "https://api.groq.com/openai/v1/audio/transcriptions"
        );
        assert_eq!(groq.default_model, "whisper-large-v3-turbo");
        assert_eq!(groq.protocol, CloudProtocol::OpenAiTranscriptions);

        let mistral = cloud_provider(TranscriberProvider::Mistral).unwrap();
        assert_eq!(
            mistral.default_endpoint,
            "https://api.mistral.ai/v1/audio/transcriptions"
        );
        assert_eq!(mistral.default_model, "voxtral-mini-2602");
        assert_eq!(mistral.protocol, CloudProtocol::MistralTranscriptions);

        let realtime = cloud_provider(TranscriberProvider::MistralRealtime).unwrap();
        assert_eq!(
            realtime.default_endpoint,
            "wss://api.mistral.ai/v1/audio/transcriptions/realtime"
        );
        assert_eq!(
            realtime.default_model,
            "voxtral-mini-transcribe-realtime-2602"
        );
        assert_eq!(realtime.protocol, CloudProtocol::MistralRealtime);

        let deepgram = cloud_provider(TranscriberProvider::Deepgram).unwrap();
        assert_eq!(
            deepgram.default_endpoint,
            "https://api.deepgram.com/v1/listen"
        );
        assert_eq!(deepgram.default_model, "nova-3");
        assert_eq!(deepgram.protocol, CloudProtocol::DeepgramListen);

        let assemblyai = cloud_provider(TranscriberProvider::AssemblyAi).unwrap();
        assert_eq!(assemblyai.default_endpoint, "https://api.assemblyai.com");
        assert_eq!(assemblyai.default_model, "universal-3-5-pro");
        assert_eq!(assemblyai.protocol, CloudProtocol::AssemblyAiPreRecorded);

        let elevenlabs = cloud_provider(TranscriberProvider::ElevenLabs).unwrap();
        assert_eq!(
            elevenlabs.default_endpoint,
            "https://api.elevenlabs.io/v1/speech-to-text"
        );
        assert_eq!(elevenlabs.default_model, "scribe_v2");
        assert_eq!(elevenlabs.protocol, CloudProtocol::ElevenLabsScribe);
    }
}
