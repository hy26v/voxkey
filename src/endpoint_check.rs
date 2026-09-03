// ABOUTME: Checks configured transcription endpoints without sending audio or credentials.
// ABOUTME: Distinguishes a reachable service from invalid paths, outages, and transport failures.

use std::time::{Duration, Instant};

use tokio_tungstenite::tungstenite;
use voxkey_ipc::{EndpointCheckResult, ParakeetBackend, TranscriberConfig, TranscriberProvider};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const CHECK_TIMEOUT: Duration = Duration::from_secs(8);

pub async fn check(config: &TranscriberConfig) -> EndpointCheckResult {
    let started = Instant::now();
    let result = match config.provider.cloud() {
        Some(cloud) if cloud.streaming => {
            let model = cloud.resolved_model(config.cloud_model().unwrap_or(""));
            let endpoint = cloud.resolved_endpoint(config.cloud_endpoint().unwrap_or(""));
            check_websocket(&endpoint, &model).await
        }
        Some(cloud) => {
            let endpoint = cloud.resolved_endpoint(config.cloud_endpoint().unwrap_or(""));
            if cloud.endpoint_required && endpoint.trim().is_empty() {
                Err("Enter the transcription server address before checking it.".to_string())
            } else {
                let kind = if cloud.allows_insecure_http {
                    HttpEndpointKind::Parakeet
                } else {
                    HttpEndpointKind::Mistral
                };
                check_http(&endpoint, kind, config.allow_insecure_http()).await
            }
        }
        None if config.provider == TranscriberProvider::Parakeet
            && config.parakeet.backend == ParakeetBackend::Http =>
        {
            let endpoint = config.parakeet.endpoint.trim();
            if endpoint.is_empty() {
                Err("Enter the transcription server address before checking it.".to_string())
            } else {
                check_http(
                    endpoint,
                    HttpEndpointKind::Parakeet,
                    config.parakeet.allow_insecure_http,
                )
                .await
            }
        }
        None => Err("The selected model does not use a network server.".to_string()),
    };

    let uses_unencrypted_lan = config.allow_insecure_http()
        && crate::transcriber::endpoint_uses_unencrypted_private_network(
            config.cloud_endpoint().unwrap_or(&config.parakeet.endpoint),
        );

    match result {
        Ok(ProbeOutcome::Connected) if uses_unencrypted_lan => {
            EndpointCheckResult::reachable(format!(
                "Server responded in {}. Audio and transcripts will travel unencrypted.",
                readable_duration(started.elapsed())
            ))
        }
        Ok(ProbeOutcome::Connected) => EndpointCheckResult::reachable(format!(
            "Server responded in {}.",
            readable_duration(started.elapsed())
        )),
        Ok(ProbeOutcome::AuthenticationRequired) => EndpointCheckResult::reachable(format!(
            "Server reached in {}. Your API key will be verified when you dictate.",
            readable_duration(started.elapsed())
        )),
        Err(message) => EndpointCheckResult::failed(message),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeOutcome {
    Connected,
    AuthenticationRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpEndpointKind {
    Mistral,
    Parakeet,
}

async fn check_http(
    endpoint: &str,
    kind: HttpEndpointKind,
    allow_insecure_http: bool,
) -> Result<ProbeOutcome, String> {
    let policy = match kind {
        HttpEndpointKind::Mistral => crate::transcriber::BatchEndpointPolicy::Authenticated,
        HttpEndpointKind::Parakeet => crate::transcriber::BatchEndpointPolicy::Unauthenticated {
            allow_insecure_http,
        },
    };
    let url = crate::transcriber::batch_endpoint(endpoint, policy)
        .map_err(|error| friendly_url_error(&error.to_string(), "http:// or https://"))?;
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(CHECK_TIMEOUT)
        .user_agent(concat!("Voxkey/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| "Voxkey could not prepare the connectivity check.".to_string())?;

    // An empty POST exercises the exact transcription route without uploading
    // audio, a model name, or an API key. Compatible servers normally answer
    // with an authentication or missing-field response, both of which prove
    // that the route is reachable.
    let response = client
        .post(url)
        .header(reqwest::header::CONTENT_LENGTH, 0)
        .send()
        .await
        .map_err(friendly_http_transport_error)?;

    classify_http_status(response.status(), kind)
}

fn classify_http_status(
    status: reqwest::StatusCode,
    _kind: HttpEndpointKind,
) -> Result<ProbeOutcome, String> {
    match status {
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            Ok(ProbeOutcome::AuthenticationRequired)
        }
        reqwest::StatusCode::NOT_FOUND => Err(
            "The server is reachable, but this address was not found. Check the URL path."
                .to_string(),
        ),
        reqwest::StatusCode::METHOD_NOT_ALLOWED => Err(
            "The server is reachable, but this address does not accept transcription requests."
                .to_string(),
        ),
        status if status.is_server_error() => Err(format!(
            "The server is reachable but unavailable ({status}). Try again when it is ready."
        )),
        _ => Ok(ProbeOutcome::Connected),
    }
}

async fn check_websocket(endpoint: &str, model: &str) -> Result<ProbeOutcome, String> {
    let url = crate::streaming::streaming_url(endpoint, model)
        .map_err(|error| friendly_url_error(&error.to_string(), "ws:// or wss://"))?;
    let uri = url.parse::<http::Uri>().map_err(|_| {
        "Enter a complete realtime server address beginning with ws:// or wss://.".to_string()
    })?;
    let host = uri
        .authority()
        .ok_or_else(|| "The realtime address must include a server name or IP.".to_string())?;
    let request = http::Request::builder()
        .uri(&url)
        .header("Host", host.as_str())
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tungstenite::handshake::client::generate_key(),
        )
        .body(())
        .map_err(|_| "Voxkey could not prepare the WebSocket check.".to_string())?;

    let connection = tokio::time::timeout(CHECK_TIMEOUT, tokio_tungstenite::connect_async(request))
        .await
        .map_err(|_| {
            format!(
                "The server did not respond within {} seconds. Check the address and network.",
                CHECK_TIMEOUT.as_secs()
            )
        })?;

    match connection {
        Ok((socket, _)) => {
            drop(socket);
            Ok(ProbeOutcome::Connected)
        }
        Err(tungstenite::Error::Http(response)) => classify_websocket_status(response.status()),
        Err(tungstenite::Error::Io(error))
            if error.kind() == std::io::ErrorKind::ConnectionRefused =>
        {
            Err(
                "The server refused the connection. Check that it is running and the port is correct."
                    .to_string(),
            )
        }
        Err(tungstenite::Error::Io(error))
            if error.kind() == std::io::ErrorKind::TimedOut =>
        {
            Err(format!(
                "The server did not respond within {} seconds. Check the address and network.",
                CHECK_TIMEOUT.as_secs()
            ))
        }
        Err(tungstenite::Error::Tls(_)) => Err(
            "Could not establish a secure connection. Check the server's TLS certificate."
                .to_string(),
        ),
        Err(error) => {
            tracing::debug!("Realtime endpoint transport diagnostic: {error}");
            Err(
                "Could not reach the realtime server. Check the address and network connection."
                    .to_string(),
            )
        }
    }
}

fn classify_websocket_status(status: http::StatusCode) -> Result<ProbeOutcome, String> {
    match status {
        http::StatusCode::UNAUTHORIZED | http::StatusCode::FORBIDDEN => {
            Ok(ProbeOutcome::AuthenticationRequired)
        }
        http::StatusCode::NOT_FOUND => Err(
            "The server is reachable, but this realtime address was not found. Check the URL path."
                .to_string(),
        ),
        status if status.is_server_error() => Err(format!(
            "The realtime server is reachable but unavailable ({status}). Try again when it is ready."
        )),
        status => Err(format!(
            "The server rejected the WebSocket connection ({status}). Check that this is a Mistral-compatible realtime server."
        )),
    }
}

fn friendly_http_transport_error(error: reqwest::Error) -> String {
    let public = if error.is_timeout() {
        format!(
            "The server did not respond within {} seconds. Check the address and network.",
            CHECK_TIMEOUT.as_secs()
        )
    } else if error.is_connect() {
        "Could not reach the server. Check the address, port, network, and TLS certificate."
            .to_string()
    } else if error.is_redirect() {
        "The server redirected too many times. Enter its final transcription address.".to_string()
    } else {
        "The server did not complete the connectivity check. Try again.".to_string()
    };
    tracing::debug!(
        "HTTP endpoint transport diagnostic: {}",
        error.without_url()
    );
    public
}

fn friendly_url_error(error: &str, expected_schemes: &str) -> String {
    if error.contains("relative URL") || error.contains("empty host") || error.contains("no host") {
        format!("Enter a complete address beginning with {expected_schemes}.")
    } else {
        error.to_string()
    }
}

fn readable_duration(elapsed: Duration) -> String {
    if elapsed < Duration::from_secs(1) {
        format!("{} ms", elapsed.as_millis().max(1))
    } else {
        format!("{:.1} s", elapsed.as_secs_f32())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn serve_one_response(
        status: &'static str,
    ) -> (std::net::SocketAddr, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (request_tx, request_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            let _ = request_tx.send(String::from_utf8_lossy(&request).into_owned());
            let response =
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        (address, request_rx)
    }

    #[test]
    fn endpoint_probe_accepts_authentication_challenges_as_reachable() {
        assert_eq!(
            classify_http_status(reqwest::StatusCode::UNAUTHORIZED, HttpEndpointKind::Mistral,),
            Ok(ProbeOutcome::AuthenticationRequired)
        );
        assert_eq!(
            classify_websocket_status(http::StatusCode::FORBIDDEN),
            Ok(ProbeOutcome::AuthenticationRequired)
        );
    }

    #[test]
    fn model_server_authentication_challenge_proves_the_route_is_reachable() {
        assert_eq!(
            classify_http_status(
                reqwest::StatusCode::UNAUTHORIZED,
                HttpEndpointKind::Parakeet,
            ),
            Ok(ProbeOutcome::AuthenticationRequired)
        );
    }

    #[test]
    fn endpoint_probe_rejects_missing_routes_and_server_outages() {
        assert!(
            classify_http_status(reqwest::StatusCode::NOT_FOUND, HttpEndpointKind::Parakeet)
                .is_err()
        );
        assert!(
            classify_http_status(
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                HttpEndpointKind::Mistral,
            )
            .is_err()
        );
        assert!(classify_websocket_status(http::StatusCode::NOT_FOUND).is_err());
    }

    #[test]
    fn subsecond_connection_times_are_easy_to_scan() {
        assert_eq!(readable_duration(Duration::from_millis(0)), "1 ms");
        assert_eq!(readable_duration(Duration::from_millis(284)), "284 ms");
        assert_eq!(readable_duration(Duration::from_millis(1250)), "1.2 s");
    }

    #[tokio::test]
    async fn http_probe_reaches_the_exact_route_without_audio_or_credentials() {
        let (address, request_rx) = serve_one_response("422 Unprocessable Entity").await;
        let mut config = TranscriberConfig {
            provider: TranscriberProvider::Parakeet,
            ..Default::default()
        };
        config.parakeet.backend = ParakeetBackend::Http;
        config.parakeet.endpoint = format!("http://{address}/v1/audio/transcriptions");

        let result = check(&config).await;
        let request = request_rx.await.unwrap();
        let lowercase_request = request.to_ascii_lowercase();

        assert_eq!(result.status, voxkey_ipc::EndpointCheckStatus::Reachable);
        assert!(request.starts_with("POST /v1/audio/transcriptions "));
        assert!(lowercase_request.contains("content-length: 0"));
        assert!(!lowercase_request.contains("authorization:"));
        assert!(!lowercase_request.contains("multipart/form-data"));
    }

    #[tokio::test]
    async fn missing_http_route_fails_with_a_recovery_hint() {
        let (address, _request_rx) = serve_one_response("404 Not Found").await;
        let mut config = TranscriberConfig {
            provider: TranscriberProvider::Parakeet,
            ..Default::default()
        };
        config.parakeet.backend = ParakeetBackend::Http;
        config.parakeet.endpoint = format!("http://{address}/wrong-path");

        let result = check(&config).await;

        assert_eq!(result.status, voxkey_ipc::EndpointCheckStatus::Failed);
        assert!(result.message.contains("URL path"), "{}", result.message);
    }

    #[tokio::test]
    async fn private_http_is_blocked_before_network_without_explicit_permission() {
        let mut config = TranscriberConfig {
            provider: TranscriberProvider::Parakeet,
            ..Default::default()
        };
        config.parakeet.backend = ParakeetBackend::Http;
        config.parakeet.endpoint = "http://192.168.1.132:8000/v1/audio/transcriptions".to_string();

        let result = check(&config).await;

        assert_eq!(result.status, voxkey_ipc::EndpointCheckStatus::Failed);
        assert!(
            result.message.contains("Allow unencrypted LAN audio"),
            "{}",
            result.message
        );
    }

    #[tokio::test]
    async fn websocket_authentication_challenge_proves_reachability_without_a_key() {
        let (address, request_rx) = serve_one_response("401 Unauthorized").await;
        let mut config = TranscriberConfig {
            provider: TranscriberProvider::MistralRealtime,
            ..Default::default()
        };
        config.mistral_realtime.endpoint = format!("ws://{address}/v1/realtime");
        config.mistral_realtime.api_key = "must-not-cross-the-network".to_string();

        let result = check(&config).await;
        let request = request_rx.await.unwrap().to_ascii_lowercase();

        assert_eq!(result.status, voxkey_ipc::EndpointCheckStatus::Reachable);
        assert!(result.message.contains("API key"), "{}", result.message);
        assert!(!request.contains("authorization:"));
        assert!(!request.contains("must-not-cross-the-network"));
    }
}
