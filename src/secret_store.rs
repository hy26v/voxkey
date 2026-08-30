// ABOUTME: Stores transcription provider API keys in the system Secret Service via libsecret.
// ABOUTME: Replaces plaintext storage in config.toml; auto-migration runs once on daemon startup.

use std::collections::HashMap;

use secret_service::{EncryptionType, SecretService};

const APP_ATTR: &str = "voxkey";

/// Service name attribute for the Mistral batch API key.
pub const SERVICE_MISTRAL: &str = voxkey_ipc::API_KEY_SERVICE_MISTRAL;
/// Service name attribute for the Mistral Realtime API key.
pub const SERVICE_MISTRAL_REALTIME: &str = voxkey_ipc::API_KEY_SERVICE_MISTRAL_REALTIME;
/// Service name attribute for an optional self-hosted transcription token.
pub const SERVICE_MODEL_SERVER: &str = voxkey_ipc::API_KEY_SERVICE_MODEL_SERVER;

fn attributes(service: &str) -> HashMap<&str, &str> {
    HashMap::from([("app", APP_ATTR), ("service", service)])
}

fn decode_stored_key(bytes: &[u8]) -> Option<String> {
    let key = std::str::from_utf8(bytes).ok()?.trim();
    (!key.is_empty()).then(|| key.to_string())
}

fn first_usable_stored_key<I, B>(candidates: I) -> Option<String>
where
    I: IntoIterator<Item = B>,
    B: AsRef<[u8]>,
{
    candidates
        .into_iter()
        .find_map(|bytes| decode_stored_key(bytes.as_ref()))
}

async fn open_default_collection<'a>(
    ss: &'a SecretService<'_>,
) -> Result<secret_service::Collection<'a>, secret_service::Error> {
    let collection = ss.get_default_collection().await?;
    if collection.is_locked().await? {
        collection.unlock().await?;
    }
    Ok(collection)
}

fn remember_deletion_error<E>(first_error: &mut Option<E>, result: Result<(), E>) {
    if let Err(error) = result
        && first_error.is_none()
    {
        *first_error = Some(error);
    }
}

fn deletion_result<E>(first_error: Option<E>) -> Result<(), E> {
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

async fn read_first_usable_item(
    service: &str,
    items: Vec<secret_service::Item<'_>>,
) -> Option<String> {
    let mut candidates = Vec::new();
    for item in items {
        if item.is_locked().await.unwrap_or(true)
            && let Err(e) = item.unlock().await
        {
            tracing::warn!("Could not unlock secret for '{service}': {e}");
            continue;
        }
        match item.get_secret().await {
            Ok(bytes) => {
                candidates.push(bytes);
                if let Some(key) = first_usable_stored_key(&candidates) {
                    return Some(key);
                }
            }
            Err(e) => tracing::warn!("Could not read secret for '{service}': {e}"),
        }
    }
    None
}

/// Look up an API key in the Secret Service. Returns None if no item is stored
/// or the keyring cannot be opened. Errors are logged at warn level.
pub async fn get(service: &str) -> Option<String> {
    match tokio::time::timeout(
        crate::deadline::KEYRING_OPERATION,
        get_without_deadline(service),
    )
    .await
    {
        Ok(key) => key,
        Err(_) => {
            tracing::warn!("Secret Service lookup timed out for '{service}'");
            None
        }
    }
}

async fn get_without_deadline(service: &str) -> Option<String> {
    let ss = match SecretService::connect(EncryptionType::Dh).await {
        Ok(ss) => ss,
        Err(e) => {
            tracing::warn!("Cannot connect to Secret Service for '{service}': {e}");
            return None;
        }
    };
    let attrs = attributes(service);

    // `set` always replaces the item in the default collection. Read that
    // collection first as well, so an older duplicate elsewhere can never
    // override a rotated key merely because SearchItems returned it first.
    match open_default_collection(&ss).await {
        Ok(collection) => match collection.search_items(attrs.clone()).await {
            Ok(items) => {
                if let Some(key) = read_first_usable_item(service, items).await {
                    return Some(key);
                }
            }
            Err(e) => tracing::warn!(
                "Default Secret Service collection lookup failed for '{service}': {e}"
            ),
        },
        Err(e) => tracing::warn!(
            "Could not open the default Secret Service collection for '{service}': {e}"
        ),
    }

    // Compatibility fallback for an item created outside the default
    // collection by an older/manual setup.
    let items = match ss.search_items(attrs).await {
        Ok(found) => found,
        Err(e) => {
            tracing::warn!("Secret Service lookup failed for '{service}': {e}");
            return None;
        }
    };
    read_first_usable_item(
        service,
        items.unlocked.into_iter().chain(items.locked).collect(),
    )
    .await
}

/// Store an API key in the default collection, replacing any existing entry
/// for this service.
pub async fn set(service: &str, key: &str) -> Result<(), crate::deadline::DynError> {
    crate::deadline::run(
        "Secret Service key storage",
        crate::deadline::KEYRING_OPERATION,
        async {
            let ss = SecretService::connect(EncryptionType::Dh).await?;
            let collection = open_default_collection(&ss).await?;
            let attrs = attributes(service);
            let label = format!("Voxkey {service} API key");
            collection
                .create_item(&label, attrs, key.as_bytes(), true, "text/plain")
                .await?;
            Ok::<(), secret_service::Error>(())
        },
    )
    .await
}

/// Remove the stored API key for a service. No-op if no entry exists.
pub async fn delete(service: &str) -> Result<(), crate::deadline::DynError> {
    crate::deadline::run(
        "Secret Service key deletion",
        crate::deadline::KEYRING_OPERATION,
        async {
            let ss = SecretService::connect(EncryptionType::Dh).await?;
            let attrs = attributes(service);
            let items = ss.search_items(attrs).await?;
            let mut first_error = None;
            for item in items.unlocked.into_iter().chain(items.locked) {
                let result = item.delete().await;
                if let Err(error) = &result {
                    tracing::warn!("Could not delete secret for '{service}': {error}");
                }
                remember_deletion_error(&mut first_error, result);
            }
            deletion_result(first_error)
        },
    )
    .await
}

/// Check whether an API key is stored for the given service.
pub async fn has(service: &str) -> bool {
    get(service).await.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_include_app_and_service() {
        let attrs = attributes(SERVICE_MISTRAL);
        assert_eq!(attrs.get("app"), Some(&"voxkey"));
        assert_eq!(attrs.get("service"), Some(&"mistral"));
    }

    #[test]
    fn service_constants_match_provider_kebab_case() {
        assert_eq!(SERVICE_MISTRAL, "mistral");
        assert_eq!(SERVICE_MISTRAL_REALTIME, "mistral-realtime");
        assert_eq!(SERVICE_MODEL_SERVER, "model-server");
    }

    #[test]
    fn invalid_or_blank_keyring_values_are_not_usable_api_keys() {
        assert_eq!(
            decode_stored_key(b"  sk-valid-key \n"),
            Some("sk-valid-key".to_string())
        );
        assert_eq!(decode_stored_key(b" \t\n"), None);
        assert_eq!(decode_stored_key(&[0xff, 0xfe]), None);
    }

    #[test]
    fn stale_keyring_items_do_not_hide_a_later_valid_key() {
        let candidates = vec![b" \t".to_vec(), b"sk-current".to_vec()];

        let key = first_usable_stored_key(candidates);

        assert_eq!(key.as_deref(), Some("sk-current"));
    }

    #[test]
    fn deleting_secrets_reports_failures_after_attempting_every_item() {
        let attempted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut first_error = None;
        for item in [1, 2, 3] {
            attempted.lock().unwrap().push(item);
            let result = if item == 2 { Err("locked") } else { Ok(()) };
            remember_deletion_error(&mut first_error, result);
        }
        let error = deletion_result(first_error)
            .expect_err("a failed key deletion must reach the D-Bus caller");

        assert_eq!(error, "locked");
        assert_eq!(*attempted.lock().unwrap(), [1, 2, 3]);
    }
}
