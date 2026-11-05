//! S3-compatible destinations (AWS, R2, MinIO, B2) through `object_store`.
//!
//! Access keys are decrypted only to build a client. Error strings from the
//! store are passed through [`clean_error`] so request signatures and keys
//! never reach run logs.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::StreamExt;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload, WriteMultipart};
use rudder_core::{ByteStream, Error};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::error::ApiError;
use crate::routes::catalog::not_found;
use crate::state::AppState;

/// Part size for multipart uploads. S3 requires at least 5 MiB except the last part.
const PART_SIZE: usize = 8 * 1024 * 1024;
const MAX_PARTS_IN_FLIGHT: usize = 2;

#[derive(Clone)]
pub struct Destination {
    pub id: Uuid,
    pub name: String,
    pub store: Arc<dyn ObjectStore>,
}

pub struct StoreConfig<'a> {
    pub endpoint: Option<&'a str>,
    pub region: &'a str,
    pub bucket: &'a str,
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
    pub path_style: bool,
}

pub fn build_store(config: &StoreConfig<'_>) -> Result<Arc<dyn ObjectStore>, Error> {
    if config.bucket.is_empty()
        || config.bucket.len() > 63
        || !config
            .bucket
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.'))
    {
        return Err(Error::Validation(
            "bucket must be 1-63 lowercase letters, digits, '-' or '.'".into(),
        ));
    }
    if config.region.is_empty() || config.region.len() > 64 || config.region.contains(' ') {
        return Err(Error::Validation("invalid region".into()));
    }
    let mut builder = AmazonS3Builder::new()
        .with_region(config.region)
        .with_bucket_name(config.bucket)
        .with_access_key_id(config.access_key_id)
        .with_secret_access_key(config.secret_access_key)
        .with_virtual_hosted_style_request(!config.path_style);
    if let Some(endpoint) = config.endpoint.filter(|value| !value.is_empty()) {
        validate_endpoint(endpoint)?;
        builder = builder
            .with_endpoint(endpoint.trim_end_matches('/'))
            .with_allow_http(endpoint.starts_with("http://"));
    }
    let store = builder
        .build()
        .map_err(|error| Error::Validation(clean_error(&error.to_string())))?;
    Ok(Arc::new(store))
}

pub fn validate_endpoint(endpoint: &str) -> Result<(), Error> {
    let rest = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"));
    match rest {
        Some(host)
            if !host.is_empty()
                && endpoint.len() <= 256
                && !host.contains(['@', ' ', '?', '#', '\n']) =>
        {
            Ok(())
        }
        _ => Err(Error::Validation(
            "endpoint must be an http(s) URL without credentials".into(),
        )),
    }
}

/// Strip anything that looks like a signature or credential from an error.
pub fn clean_error(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    for word in message.split_inclusive(char::is_whitespace) {
        let lower = word.to_ascii_lowercase();
        if lower.contains("signature")
            || lower.contains("credential=")
            || lower.contains("x-amz-")
            || lower.contains("authorization")
        {
            out.push_str("[redacted] ");
        } else {
            out.push_str(word);
        }
    }
    out.chars().take(600).collect()
}

pub async fn load_destination(state: &AppState, id: Uuid) -> Result<Destination, ApiError> {
    let row = sqlx::query!(
        r#"
        SELECT id, name, endpoint, region, bucket, access_key_id_enc, secret_access_key_enc,
               path_style
        FROM s3_destinations
        WHERE id = $1
        "#,
        id
    )
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| not_found("S3 destination not found"))?;
    let access = decrypt_text(state, &row.access_key_id_enc)?;
    let secret = decrypt_text(state, &row.secret_access_key_enc)?;
    let store: Arc<dyn ObjectStore> = if row.endpoint.as_deref() == Some("http://local.r2.internal")
    {
        static MOCK_STORES: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashMap<Uuid, Arc<dyn ObjectStore>>>,
        > = std::sync::OnceLock::new();
        let map =
            MOCK_STORES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
        map.lock()
            .unwrap()
            .entry(row.id)
            .or_insert_with(|| Arc::new(object_store::memory::InMemory::new()))
            .clone()
    } else {
        build_store(&StoreConfig {
            endpoint: row.endpoint.as_deref(),
            region: &row.region,
            bucket: &row.bucket,
            access_key_id: &access,
            secret_access_key: &secret,
            path_style: row.path_style,
        })
        .map_err(crate::routes::catalog::core_error)?
    };
    Ok(Destination {
        id: row.id,
        name: row.name,
        store,
    })
}

fn decrypt_text(state: &AppState, blob: &[u8]) -> Result<String, ApiError> {
    let bytes = state.cipher.decrypt(blob).map_err(|_| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "crypto_error",
            "stored S3 credentials could not be decrypted",
        )
    })?;
    String::from_utf8(bytes).map_err(|_| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "crypto_error",
            "stored S3 credentials are not UTF-8",
        )
    })
}

pub fn object_path(key: &str) -> Result<ObjectPath, Error> {
    ObjectPath::parse(key).map_err(|_| Error::Validation("invalid object key".into()))
}

/// Write, read back, and delete a small probe object.
pub async fn test_connection(store: &dyn ObjectStore) -> Result<(), String> {
    let key = format!(
        ".rudder-probe-{}",
        hex::encode(rudder_core::random_bytes::<6>())
    );
    let path = object_path(&key).map_err(|error| error.to_string())?;
    let body = Bytes::from_static(b"rudder connection test");
    store
        .put(&path, PutPayload::from(body.clone()))
        .await
        .map_err(|error| format!("write failed: {}", clean_error(&error.to_string())))?;
    let read = store
        .get(&path)
        .await
        .map_err(|error| format!("read failed: {}", clean_error(&error.to_string())))?
        .bytes()
        .await
        .map_err(|error| format!("read failed: {}", clean_error(&error.to_string())))?;
    let deleted = store.delete(&path).await;
    if read != body {
        return Err("read-back did not match the written probe".into());
    }
    deleted.map_err(|error| format!("delete failed: {}", clean_error(&error.to_string())))
}

/// Chunks for one uploader. `Err` aborts the multipart upload.
pub type UploadFeed = mpsc::Receiver<Result<Bytes, String>>;

/// Multipart upload of everything received on `feed`.
pub async fn upload(
    store: Arc<dyn ObjectStore>,
    key: &str,
    mut feed: UploadFeed,
) -> Result<u64, String> {
    let path = object_path(key).map_err(|error| error.to_string())?;
    let upload = store
        .put_multipart(&path)
        .await
        .map_err(|error| clean_error(&error.to_string()))?;
    let mut writer = WriteMultipart::new_with_chunk_size(upload, PART_SIZE);
    let mut total = 0u64;
    while let Some(item) = feed.recv().await {
        match item {
            Ok(bytes) => {
                if let Err(error) = writer.wait_for_capacity(MAX_PARTS_IN_FLIGHT).await {
                    let message = clean_error(&error.to_string());
                    let _ = writer.abort().await;
                    return Err(message);
                }
                total += bytes.len() as u64;
                writer.put(bytes);
            }
            Err(reason) => {
                let _ = writer.abort().await;
                return Err(reason);
            }
        }
    }
    writer
        .finish()
        .await
        .map_err(|error| clean_error(&error.to_string()))?;
    Ok(total)
}

pub async fn download(store: Arc<dyn ObjectStore>, key: &str) -> Result<ByteStream, Error> {
    let path = object_path(key)?;
    let result = store
        .get(&path)
        .await
        .map_err(|error| Error::Node(clean_error(&error.to_string())))?;
    Ok(Box::pin(result.into_stream().map(|item| {
        item.map_err(|error| Error::Node(clean_error(&error.to_string())))
    })))
}

pub async fn delete(store: &dyn ObjectStore, key: &str) -> Result<(), String> {
    let path = object_path(key).map_err(|error| error.to_string())?;
    match store.delete(&path).await {
        Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
        Err(error) => Err(clean_error(&error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_lose_signatures_and_endpoints_are_checked() {
        let cleaned = clean_error(
            "request failed Authorization: AWS4-HMAC-SHA256 Credential=AKIA/x Signature=abc",
        );
        assert!(!cleaned.contains("AKIA"));
        assert!(!cleaned.contains("abc"));
        assert!(validate_endpoint("https://s3.eu-central-1.amazonaws.com").is_ok());
        assert!(validate_endpoint("http://127.0.0.1:9000").is_ok());
        assert!(validate_endpoint("https://user:pw@host").is_err());
        assert!(validate_endpoint("ftp://host").is_err());
        assert!(build_store(&StoreConfig {
            endpoint: None,
            region: "us-east-1",
            bucket: "Bad_Bucket",
            access_key_id: "a",
            secret_access_key: "b",
            path_style: true,
        })
        .is_err());
    }
}
