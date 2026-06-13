//! Cache fetch module — download, verify SHA-256, and cache blobs.
//!
//! Flow:
//! 1. Check cache hit → serve from disk
//! 2. Cache miss → download bytes, compute SHA-256
//! 3. Hash match → atomic rename to final cache path
//! 4. Hash mismatch → return error (never cache bad data)

#![allow(dead_code)]

use std::path::Path;

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::blossom::descriptor::MAX_BLOB_SIZE;
use crate::cache::object_cache::{
    CacheError, cache_exists, cache_path, ensure_cache_dir, read_cached, temp_path,
};

#[derive(Error, Debug)]
pub enum FetchError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("cache error: {0}")]
    Cache(#[from] CacheError),
    #[error("sha256 mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("response too large: {size} bytes (max {MAX_BLOB_SIZE})")]
    ResponseTooLarge { size: u64 },
}

/// Download a blob from `url`, verify its SHA-256, and cache it.
///
/// Flow:
/// 1. If cache hit (file exists at cache path), read and return cached bytes.
/// 2. If cache miss:
///    a. Download all bytes from URL
///    b. Compute SHA-256 hash
///    c. Verify hash matches `expected_sha256`
///    d. On match: ensure cache dir exists, write temp file, atomic rename → cache path
///    e. On mismatch: return `HashMismatch` error (no file written)
/// 3. Return the content bytes.
///
/// # PoC Limitation
///
/// This implementation loads the entire blob into memory via `resp.bytes()`
/// before computing the hash. For large blobs, a streaming approach that
/// writes chunks to a temp file while hashing would be more memory-efficient.
pub async fn fetch_and_cache(
    url: &str,
    expected_sha256: &str,
    cache_base: &Path,
) -> Result<Vec<u8>, FetchError> {
    // 1. Check cache first — serve from disk if available
    if cache_exists(cache_base, expected_sha256) {
        return read_cached(cache_base, expected_sha256).map_err(FetchError::Cache);
    }

    // 2a. Download — error_for_status converts 4xx/5xx into Err
    //     Redirect policy: none (security — never follow redirects from blob servers)
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let resp = client.get(url).send().await?.error_for_status()?;

    // Size guard: reject responses larger than MAX_BLOB_SIZE to prevent OOM
    if let Some(len) = resp.content_length()
        && len > MAX_BLOB_SIZE
    {
        return Err(FetchError::ResponseTooLarge { size: len });
    }

    let bytes = resp.bytes().await?;

    // Double-check actual byte count (Content-Length can be absent or lie)
    if bytes.len() as u64 > MAX_BLOB_SIZE {
        return Err(FetchError::ResponseTooLarge {
            size: bytes.len() as u64,
        });
    }

    // 2b. Compute SHA-256
    let mut hasher = Sha256::new();
    hasher.update(bytes.as_ref());
    let computed = hex::encode(hasher.finalize());

    // 2c. Verify hash (case-insensitive — expected may be uppercase)
    if computed != expected_sha256.to_lowercase() {
        return Err(FetchError::HashMismatch {
            expected: expected_sha256.to_string(),
            actual: computed,
        });
    }

    // 2d. Ensure cache directory exists for this hash
    ensure_cache_dir(cache_base, expected_sha256)?;

    // Write to temp file, then atomic rename (POSIX rename is atomic on same filesystem)
    let cache_file = cache_path(cache_base, expected_sha256)?;
    let temp = temp_path(cache_base, expected_sha256);

    // Ensure .tmp directory exists
    if let Some(parent) = temp.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(&temp, bytes.as_ref())?;

    // Atomic rename — if it fails, clean up the temp file
    if let Err(e) = std::fs::rename(&temp, &cache_file) {
        let _ = std::fs::remove_file(&temp); // Best-effort cleanup
        return Err(e.into());
    }

    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Helper: compute SHA-256 hex digest of content.
    fn sha256_hex(content: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content);
        hex::encode(hasher.finalize())
    }

    // --- TDD Scenario 1: Happy path — download, verify hash, cache ---

    #[tokio::test]
    async fn test_fetch_happy_download_and_cache() {
        let mock_server = MockServer::start().await;
        let content = b"hello world";
        let hash = sha256_hex(content);

        Mock::given(method("GET"))
            .and(path("/blob1"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
            .mount(&mock_server)
            .await;

        let url = format!("{}/blob1", mock_server.uri());
        let cache_base = tempfile::tempdir().unwrap();

        let result = fetch_and_cache(&url, &hash, cache_base.path()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), content.to_vec());
    }

    // --- TDD Scenario 2: Second call serves from cache (no HTTP) ---

    #[tokio::test]
    async fn test_second_call_serves_from_cache() {
        let mock_server = MockServer::start().await;
        let content = b"cached content";
        let hash = sha256_hex(content);

        Mock::given(method("GET"))
            .and(path("/blob2"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
            .expect(1) // Only the first call should hit the server
            .mount(&mock_server)
            .await;

        let url = format!("{}/blob2", mock_server.uri());
        let cache_base = tempfile::tempdir().unwrap();

        // First call: downloads
        let result1 = fetch_and_cache(&url, &hash, cache_base.path()).await;
        assert!(result1.is_ok());
        assert_eq!(result1.unwrap(), content.to_vec());

        // Second call: served from cache, no HTTP
        let result2 = fetch_and_cache(&url, &hash, cache_base.path()).await;
        assert!(result2.is_ok());
        assert_eq!(result2.unwrap(), content.to_vec());
    }

    // --- TDD Scenario 3: Large content (1MB+) ---

    #[tokio::test]
    async fn test_large_content() {
        let mock_server = MockServer::start().await;
        let content = vec![0xABu8; 1_000_000]; // 1 MB
        let hash = sha256_hex(&content);

        Mock::given(method("GET"))
            .and(path("/large"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.clone()))
            .mount(&mock_server)
            .await;

        let url = format!("{}/large", mock_server.uri());
        let cache_base = tempfile::tempdir().unwrap();

        let result = fetch_and_cache(&url, &hash, cache_base.path()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), content);
    }

    // --- TDD Scenario 4: SHA-256 mismatch ---

    #[tokio::test]
    async fn test_hash_mismatch() {
        let mock_server = MockServer::start().await;
        let content = b"actual content";
        let wrong_hash = sha256_hex(b"different content");

        Mock::given(method("GET"))
            .and(path("/mismatch"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
            .mount(&mock_server)
            .await;

        let url = format!("{}/mismatch", mock_server.uri());
        let cache_base = tempfile::tempdir().unwrap();

        let result = fetch_and_cache(&url, &wrong_hash, cache_base.path()).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            FetchError::HashMismatch { expected, actual } => {
                assert_eq!(expected, wrong_hash);
                assert_ne!(actual, wrong_hash);
            }
            other => panic!("Expected HashMismatch, got {other:?}"),
        }

        // Verify no cache file was created
        assert!(!cache_exists(cache_base.path(), &wrong_hash));
    }

    // --- TDD Scenario 5: Server returns 404 ---

    #[tokio::test]
    async fn test_server_404() {
        let mock_server = MockServer::start().await;
        let hash = sha256_hex(b"some content");

        Mock::given(method("GET"))
            .and(path("/notfound"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let url = format!("{}/notfound", mock_server.uri());
        let cache_base = tempfile::tempdir().unwrap();

        let result = fetch_and_cache(&url, &hash, cache_base.path()).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            FetchError::Http(_) => {}
            other => panic!("Expected FetchError::Http, got {other:?}"),
        }
    }

    // --- TDD Scenario 6: Server returns 500 ---

    #[tokio::test]
    async fn test_server_500() {
        let mock_server = MockServer::start().await;
        let hash = sha256_hex(b"some content");

        Mock::given(method("GET"))
            .and(path("/error"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let url = format!("{}/error", mock_server.uri());
        let cache_base = tempfile::tempdir().unwrap();

        let result = fetch_and_cache(&url, &hash, cache_base.path()).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            FetchError::Http(_) => {}
            other => panic!("Expected FetchError::Http, got {other:?}"),
        }
    }

    // --- TDD Scenario 7: Cache file exists from previous run ---

    #[tokio::test]
    async fn test_cache_file_exists_no_http() {
        let content = b"pre-cached content";
        let hash = sha256_hex(content);
        let cache_base = tempfile::tempdir().unwrap();

        // Pre-populate cache
        ensure_cache_dir(cache_base.path(), &hash).unwrap();
        let cache_file = cache_path(cache_base.path(), &hash).unwrap();
        std::fs::write(&cache_file, content).unwrap();

        // Invalid URL — if it tries to connect, it will fail
        let url = "http://127.0.0.1:1/should-not-be-called";

        let result = fetch_and_cache(url, &hash, cache_base.path()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), content.to_vec());
    }

    // --- TDD Scenario 8: Uppercase hash normalized to lowercase ---

    #[tokio::test]
    async fn test_uppercase_hash_normalized() {
        let mock_server = MockServer::start().await;
        let content = b"normalize test";
        let hash_lower = sha256_hex(content);
        let hash_upper = hash_lower.to_uppercase();

        Mock::given(method("GET"))
            .and(path("/upper"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
            .mount(&mock_server)
            .await;

        let url = format!("{}/upper", mock_server.uri());
        let cache_base = tempfile::tempdir().unwrap();

        let result = fetch_and_cache(&url, &hash_upper, cache_base.path()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), content.to_vec());

        // Verify cache file exists at lowercase path
        assert!(cache_exists(cache_base.path(), &hash_lower));
    }

    // --- TDD Scenario 9: Empty content (0 bytes) ---

    #[tokio::test]
    async fn test_empty_content() {
        let mock_server = MockServer::start().await;
        let content: Vec<u8> = vec![];
        let hash = sha256_hex(&content); // SHA-256 of empty string

        Mock::given(method("GET"))
            .and(path("/empty"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.clone()))
            .mount(&mock_server)
            .await;

        let url = format!("{}/empty", mock_server.uri());
        let cache_base = tempfile::tempdir().unwrap();

        let result = fetch_and_cache(&url, &hash, cache_base.path()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), content);
    }

    // --- TDD Scenario 10: Binary content (non-UTF8) preserved ---

    #[tokio::test]
    async fn test_binary_content() {
        let mock_server = MockServer::start().await;
        let content: Vec<u8> = (0..=255u8).collect();
        let hash = sha256_hex(&content);

        Mock::given(method("GET"))
            .and(path("/binary"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.clone()))
            .mount(&mock_server)
            .await;

        let url = format!("{}/binary", mock_server.uri());
        let cache_base = tempfile::tempdir().unwrap();

        let result = fetch_and_cache(&url, &hash, cache_base.path()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), content);
    }

    // --- TDD Scenario 11: Cache directory structure created correctly ---

    #[tokio::test]
    async fn test_cache_directory_structure() {
        let mock_server = MockServer::start().await;
        let content = b"structure test";
        let hash = sha256_hex(content);

        Mock::given(method("GET"))
            .and(path("/struct"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
            .mount(&mock_server)
            .await;

        let url = format!("{}/struct", mock_server.uri());
        let cache_base = tempfile::tempdir().unwrap();

        let result = fetch_and_cache(&url, &hash, cache_base.path()).await;
        assert!(result.is_ok());

        // Verify the cache file exists at the expected path: <base>/objects/<aa>/<bb>/<sha256>
        let expected_path = cache_path(cache_base.path(), &hash).unwrap();
        assert!(expected_path.exists());
        assert!(expected_path.is_file());

        let first2 = &hash[0..2];
        let next2 = &hash[2..4];

        // File name is the full hash
        assert_eq!(
            expected_path.file_name().unwrap(),
            std::ffi::OsStr::new(&hash)
        );
        // Parent dir is next2
        assert_eq!(
            expected_path.parent().unwrap().file_name().unwrap(),
            std::ffi::OsStr::new(next2)
        );
        // Grandparent dir is first2
        assert_eq!(
            expected_path
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .file_name()
                .unwrap(),
            std::ffi::OsStr::new(first2)
        );
    }

    // --- TDD Scenario 12: Network error during download ---

    #[tokio::test]
    async fn test_network_error() {
        let hash = sha256_hex(b"content");
        let cache_base = tempfile::tempdir().unwrap();

        // Port 1 — connection refused
        let url = "http://127.0.0.1:1/network-error";

        let result = fetch_and_cache(url, &hash, cache_base.path()).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            FetchError::Http(_) => {}
            other => panic!("Expected FetchError::Http, got {other:?}"),
        }
    }
}
