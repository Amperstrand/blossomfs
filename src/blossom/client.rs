//! Blossom HTTP client for BUD-12 listing and BUD-01 retrieval.
//!
//! Uses a hybrid approach:
//! - `nostr-blossom` crate for type definitions and basic operations
//! - Raw `reqwest` for cursor pagination (BUD-12) and streaming downloads (BUD-01)
//!
//! See Wave 4 (T4.2) for implementation.

#![allow(dead_code)]

use thiserror::Error;

use crate::blossom::descriptor::BlobDescriptor;

#[derive(Error, Debug)]
pub enum BlossomClientError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("server returned {status}: {body}")]
    ServerError { status: u16, body: String },
    #[error("sha256 mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },
}

/// Response from BUD-12 list endpoint.
#[derive(Debug)]
pub struct ListResponse {
    pub descriptors: Vec<BlobDescriptor>,
    /// Cursor for pagination (None if no more results).
    pub cursor: Option<String>,
}

/// HTTP client for a single Blossom server.
pub struct BlossomClient {
    client: reqwest::Client,
    base_url: String,
}

impl BlossomClient {
    /// Default page size used by `list_all_blobs` for automatic pagination.
    const PAGE_SIZE: u32 = 2;

    /// Maximum number of pages to fetch in `list_all_blobs` (safety guard).
    const MAX_PAGES: usize = 10_000;

    /// Create a new client for the given base URL (e.g. "https://cdn.example.com").
    /// Trailing slashes are stripped.
    pub fn new(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Self {
            client: reqwest::Client::new(),
            base_url,
        }
    }

    /// List blobs for a pubkey using BUD-12.
    ///
    /// `GET /list/<pubkey_hex>`
    ///
    /// Optionally pass cursor and limit query parameters.
    /// The response body is a JSON array of blob descriptors.
    pub async fn list_blobs(
        &self,
        pubkey_hex: &str,
        cursor: Option<&str>,
        limit: Option<u32>,
    ) -> Result<ListResponse, BlossomClientError> {
        let base = format!("{}/list/{}", self.base_url, pubkey_hex);

        let mut query_parts = Vec::new();
        if let Some(c) = cursor {
            query_parts.push(format!("cursor={c}"));
        }
        if let Some(l) = limit {
            query_parts.push(format!("limit={l}"));
        }

        let url = if query_parts.is_empty() {
            base
        } else {
            format!("{base}?{}", query_parts.join("&"))
        };

        let response = self.client.get(&url).send().await?;
        let status = response.status();

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(BlossomClientError::ServerError {
                status: status.as_u16(),
                body,
            });
        }

        let body = response.text().await?;
        let descriptors: Vec<BlobDescriptor> = serde_json::from_str(&body)?;

        // Determine next cursor: if we filled the page (results >= limit),
        // use the last descriptor's uploaded timestamp as the cursor.
        let next_cursor = match (descriptors.last(), limit) {
            (Some(last), Some(l)) if (descriptors.len() as u32) >= l => {
                Some(last.uploaded.to_string())
            }
            _ => None,
        };

        Ok(ListResponse {
            descriptors,
            cursor: next_cursor,
        })
    }

    /// List ALL blobs for a pubkey, handling pagination automatically.
    ///
    /// Calls `list_blobs` repeatedly until no more cursor is returned.
    pub async fn list_all_blobs(
        &self,
        pubkey_hex: &str,
    ) -> Result<Vec<BlobDescriptor>, BlossomClientError> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;

        for _ in 0..Self::MAX_PAGES {
            let response = self
                .list_blobs(pubkey_hex, cursor.as_deref(), Some(Self::PAGE_SIZE))
                .await?;

            all.extend(response.descriptors);

            match response.cursor {
                Some(c) => cursor = Some(c),
                None => return Ok(all),
            }
        }

        Ok(all)
    }

    /// Download blob content by SHA-256 hash using BUD-01.
    ///
    /// `GET /<sha256>`
    ///
    /// Returns the raw bytes. Does NOT verify hash (caller's responsibility).
    pub async fn get_blob(&self, sha256: &str) -> Result<Vec<u8>, BlossomClientError> {
        let url = format!("{}/{}", self.base_url, sha256);
        let response = self.client.get(&url).send().await?;
        let status = response.status();

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(BlossomClientError::ServerError {
                status: status.as_u16(),
                body,
            });
        }

        let bytes = response.bytes().await?;
        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ── Helpers ────────────────────────────────────────────────────────────

    /// Build a single blob-descriptor JSON value.
    fn desc_json(sha: &str, uploaded: u64) -> serde_json::Value {
        serde_json::json!({
            "url": format!("https://cdn.example.com/{}", sha),
            "sha256": sha,
            "size": 100,
            "type": "image/png",
            "uploaded": uploaded
        })
    }

    // ── Scenario 1: Happy — list_blobs returns 3 descriptors ──────────────

    #[tokio::test]
    async fn test_list_blobs_returns_descriptors() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/list/pubkey123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                desc_json("aaa1", 1000),
                desc_json("aaa2", 2000),
                desc_json("aaa3", 3000),
            ])))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.list_blobs("pubkey123", None, None).await;

        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
        let resp = result.unwrap();
        assert_eq!(resp.descriptors.len(), 3);
        assert_eq!(resp.descriptors[0].sha256, "aaa1");
        assert_eq!(resp.descriptors[0].size, 100);
        assert_eq!(resp.descriptors[0].mime_type.as_deref(), Some("image/png"));
        assert_eq!(resp.descriptors[0].uploaded, 1000);
        assert_eq!(resp.descriptors[1].sha256, "aaa2");
        assert_eq!(resp.descriptors[2].sha256, "aaa3");
    }

    // ── Scenario 2: Happy — list_blobs empty array → empty Vec ─────────────

    #[tokio::test]
    async fn test_list_blobs_empty_response() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/list/emptypk"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.list_blobs("emptypk", None, None).await;

        assert!(result.is_ok());
        let resp = result.unwrap();
        assert!(resp.descriptors.is_empty());
        assert!(resp.cursor.is_none());
    }

    // ── Scenario 3: Happy — list_blobs with limit=10 query param ───────────

    #[tokio::test]
    async fn test_list_blobs_with_limit() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/list/limitpk"))
            .and(query_param("limit", "10"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([desc_json("lim1", 1000)])),
            )
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.list_blobs("limitpk", None, Some(10)).await;

        // If limit=10 wasn't sent, the mock wouldn't match and we'd get a
        // non-200 response (wiremock returns 404 for unmatched requests).
        assert!(result.is_ok(), "mock did not match — limit param missing?");
        let resp = result.unwrap();
        assert_eq!(resp.descriptors.len(), 1);
    }

    // ── Scenario 4: Happy — list_blobs with cursor="abc" ───────────────────

    #[tokio::test]
    async fn test_list_blobs_with_cursor() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/list/curpk"))
            .and(query_param("cursor", "abc"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([desc_json("cur1", 5000)])),
            )
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.list_blobs("curpk", Some("abc"), None).await;

        assert!(result.is_ok(), "mock did not match — cursor param missing?");
        let resp = result.unwrap();
        assert_eq!(resp.descriptors.len(), 1);
        assert_eq!(resp.descriptors[0].sha256, "cur1");
    }

    // ── Scenario 5: Happy — list_all_blobs paginates ───────────────────────

    #[tokio::test]
    async fn test_list_all_blobs_paginates() {
        let mock_server = MockServer::start().await;

        // Mock A (default priority=5): matches any GET /list/pagepk
        // Returns 2 items — the first page.
        Mock::given(method("GET"))
            .and(path("/list/pagepk"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                desc_json("p1a", 1000),
                desc_json("p1b", 2000),
            ])))
            .mount(&mock_server)
            .await;

        // Mock B (highest priority=1): matches GET /list/pagepk with cursor=2000
        // (the uploaded timestamp of the last item in page 1).
        // Returns 1 item — the second (final) page.
        Mock::given(method("GET"))
            .and(path("/list/pagepk"))
            .and(query_param("cursor", "2000"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([desc_json("p2a", 3000)])),
            )
            .with_priority(1)
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.list_all_blobs("pagepk").await;

        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
        let blobs = result.unwrap();
        assert_eq!(blobs.len(), 3, "expected 3 total blobs across 2 pages");
        assert_eq!(blobs[0].sha256, "p1a");
        assert_eq!(blobs[1].sha256, "p1b");
        assert_eq!(blobs[2].sha256, "p2a");
    }

    // ── Scenario 6: Happy — list_all_blobs single page ─────────────────────

    #[tokio::test]
    async fn test_list_all_blobs_single_page() {
        let mock_server = MockServer::start().await;

        // Single item — fewer than PAGE_SIZE (2), so no cursor, no pagination.
        Mock::given(method("GET"))
            .and(path("/list/singlepk"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([desc_json("only1", 9999)])),
            )
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.list_all_blobs("singlepk").await;

        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
        let blobs = result.unwrap();
        assert_eq!(blobs.len(), 1, "expected 1 blob from single page");
        assert_eq!(blobs[0].sha256, "only1");
    }

    // ── Scenario 7: Edge — list_blobs 404 → ServerError ───────────────────

    #[tokio::test]
    async fn test_list_blobs_404() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/list/notfound"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.list_blobs("notfound", None, None).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            BlossomClientError::ServerError { status, .. } => {
                assert_eq!(status, 404);
            }
            other => panic!("expected ServerError, got {other:?}"),
        }
    }

    // ── Scenario 8: Edge — list_blobs 500 → ServerError ───────────────────

    #[tokio::test]
    async fn test_list_blobs_500() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/list/servererr"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.list_blobs("servererr", None, None).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            BlossomClientError::ServerError { status, .. } => {
                assert_eq!(status, 500);
            }
            other => panic!("expected ServerError, got {other:?}"),
        }
    }

    // ── Scenario 9: Edge — list_blobs malformed JSON → Json error ─────────

    #[tokio::test]
    async fn test_list_blobs_malformed_json() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/list/badjson"))
            .respond_with(ResponseTemplate::new(200).set_body_string("this is not valid json {{{"))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.list_blobs("badjson", None, None).await;

        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), BlossomClientError::Json(_)),
            "expected Json error"
        );
    }

    // ── Scenario 10: Happy — get_blob returns correct bytes ───────────────

    #[tokio::test]
    async fn test_get_blob_returns_bytes() {
        let mock_server = MockServer::start().await;
        let content = b"Hello, Blossom!";

        Mock::given(method("GET"))
            .and(path("/abcdef0123456789"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.get_blob("abcdef0123456789").await;

        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
        let bytes = result.unwrap();
        assert_eq!(bytes, content);
    }

    // ── Scenario 11: Happy — get_blob binary data (non-UTF8) ──────────────

    #[tokio::test]
    async fn test_get_blob_binary_data() {
        let mock_server = MockServer::start().await;
        let binary: Vec<u8> = vec![0xFF, 0xFE, 0x00, 0x01, 0x80, 0xC0, 0xBF];

        Mock::given(method("GET"))
            .and(path("/binaryhash"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(binary.clone()))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.get_blob("binaryhash").await;

        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
        let bytes = result.unwrap();
        assert_eq!(bytes, binary);
    }

    // ── Scenario 12: Edge — get_blob 404 → ServerError ───────────────────

    #[tokio::test]
    async fn test_get_blob_404() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/missinghash"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.get_blob("missinghash").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            BlossomClientError::ServerError { status, .. } => {
                assert_eq!(status, 404);
            }
            other => panic!("expected ServerError, got {other:?}"),
        }
    }

    // ── Scenario 13: Edge — get_blob 500 → ServerError ───────────────────

    #[tokio::test]
    async fn test_get_blob_500() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/errorhash"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.get_blob("errorhash").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            BlossomClientError::ServerError { status, .. } => {
                assert_eq!(status, 500);
            }
            other => panic!("expected ServerError, got {other:?}"),
        }
    }
}
