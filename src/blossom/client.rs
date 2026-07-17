//! Blossom HTTP client for BUD-12 listing and BUD-01 retrieval.
//!
//! Uses a hybrid approach:
//! - `nostr-blossom` crate for type definitions and basic operations
//! - Raw `reqwest` for cursor pagination (BUD-12) and streaming downloads (BUD-01)
//!
//! See Wave 4 (T4.2) for implementation.

#![allow(dead_code)]

use serde::Deserialize;
use thiserror::Error;

use crate::blossom::descriptor::BlobDescriptor;
use crate::payment::PaymentStrategy;

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
    #[error("payment required: X-Cashu={x_cashu}")]
    PaymentRequired { x_cashu: String },
    #[error("payment error: {0}")]
    Payment(#[from] crate::payment::PaymentError),
}

/// Response from BUD-12 list endpoint.
#[derive(Debug)]
pub struct ListResponse {
    pub descriptors: Vec<BlobDescriptor>,
    /// Cursor for pagination (None if no more results).
    pub cursor: Option<String>,
}

/// Response from POST /upload/multipart (create session).
#[derive(Debug, Deserialize)]
pub struct MultipartUploadInit {
    pub upload_id: String,
    pub sha256: String,
    pub chunk_size: u64,
    pub total_chunks: u32,
    pub expires_in: u64,
}

/// Response from PUT /upload/multipart/:id (upload chunk).
#[derive(Debug, Deserialize)]
pub struct MultipartPartResponse {
    pub part_number: u32,
    pub etag: String,
}

/// Result of HEAD /<sha256> existence + expiry check.
#[derive(Debug)]
pub struct BlobHeadInfo {
    pub exists: bool,
    pub sunset: Option<u64>,
}

/// HTTP client for a single Blossom server.
pub struct BlossomClient {
    client: reqwest::Client,
    base_url: String,
}

impl BlossomClient {
    /// Default page size used by `list_all_blobs` for automatic pagination.
    pub const PAGE_SIZE: u32 = 500;

    const MAX_PAGES: usize = 10_000;

    /// Create a new client without a timeout (for tests and CLI commands).
    pub fn new(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("failed to build HTTP client"),
            base_url,
        }
    }

    /// Create a new client with a custom HTTP timeout.
    pub fn with_timeout(base_url: impl Into<String>, timeout: std::time::Duration) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(timeout)
                .build()
                .expect("failed to build HTTP client"),
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

        // Determine next cursor per BUD-12: if we filled the page (results >= limit),
        // use the last descriptor's sha256 as the cursor.
        let next_cursor = match (descriptors.last(), limit) {
            (Some(last), Some(l)) if (descriptors.len() as u32) >= l => Some(last.sha256.clone()),
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
        max: usize,
        page_size: u32,
    ) -> Result<Vec<BlobDescriptor>, BlossomClientError> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;

        for _ in 0..Self::MAX_PAGES {
            let response = self
                .list_blobs(pubkey_hex, cursor.as_deref(), Some(page_size))
                .await?;

            all.extend(response.descriptors);

            if max > 0 && all.len() >= max {
                all.truncate(max);
                return Ok(all);
            }

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

    /// Upload a blob using BUD-02.
    ///
    /// `PUT /upload`
    ///
    /// # Headers
    ///
    /// - `Authorization: Nostr <auth_header>` (base64url-encoded signed event)
    /// - `Content-Type: application/octet-stream`
    ///
    /// # Arguments
    ///
    /// * `data` - Raw file bytes to upload.
    /// * `auth_header` - Base64url-encoded signed BUD-11 auth event (without
    ///   the `Nostr ` prefix — this method adds it).
    ///
    /// # Returns
    ///
    /// The blob descriptor from the server response on success, or
    /// `BlossomClientError::ServerError` on non-2xx responses.
    pub async fn upload_blob(
        &self,
        data: Vec<u8>,
        auth_header: &str,
    ) -> Result<BlobDescriptor, BlossomClientError> {
        let url = format!("{}/upload", self.base_url);

        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&data);
        let sha256_hex = format!("{:x}", hasher.finalize());

        let response = self
            .client
            .put(&url)
            .header("Authorization", format!("Nostr {auth_header}"))
            .header("Content-Type", "application/octet-stream")
            .header("X-SHA-256", &sha256_hex)
            .body(data)
            .send()
            .await?;

        let status = response.status();

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(BlossomClientError::ServerError {
                status: status.as_u16(),
                body,
            });
        }

        let body = response.text().await?;
        let descriptor: BlobDescriptor = serde_json::from_str(&body)?;
        Ok(descriptor)
    }

    /// Delete a blob from the server (BUD-08).
    ///
    /// `DELETE /<sha256>`
    ///
    /// # Headers
    ///
    /// - `Authorization: Nostr <auth_header>` (base64url-encoded signed event)
    pub async fn delete_blob(
        &self,
        sha256: &str,
        auth_header: &str,
    ) -> Result<(), BlossomClientError> {
        let url = format!("{}/{}", self.base_url, sha256);

        let response = self
            .client
            .delete(&url)
            .header("Authorization", format!("Nostr {auth_header}"))
            .send()
            .await?;

        let status = response.status();

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(BlossomClientError::ServerError {
                status: status.as_u16(),
                body,
            });
        }

        Ok(())
    }

    async fn upload_attempt(
        &self,
        data: &[u8],
        auth_header: &str,
        payment_token: Option<&str>,
    ) -> Result<BlobDescriptor, BlossomClientError> {
        let url = format!("{}/upload", self.base_url);
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(data);
        let sha256_hex = format!("{:x}", hasher.finalize());

        let mut req = self
            .client
            .put(&url)
            .header("Authorization", format!("Nostr {auth_header}"))
            .header("Content-Type", "application/octet-stream")
            .header("X-SHA-256", &sha256_hex);

        if let Some(token) = payment_token {
            req = req.header("X-Cashu", token);
        }

        let response = req.body(data.to_vec()).send().await?;
        let status = response.status();

        if status.is_success() {
            let body = response.text().await?;
            return Ok(serde_json::from_str(&body)?);
        }

        if status.as_u16() == 402 {
            let x_cashu = response
                .headers()
                .get("x-cashu")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            return Err(BlossomClientError::PaymentRequired { x_cashu });
        }

        let body = response.text().await.unwrap_or_default();
        Err(BlossomClientError::ServerError {
            status: status.as_u16(),
            body,
        })
    }

    pub async fn upload_blob_with_payment(
        &self,
        data: Vec<u8>,
        auth_header: &str,
        payment: &dyn PaymentStrategy,
    ) -> Result<BlobDescriptor, BlossomClientError> {
        match self.upload_attempt(&data, auth_header, None).await {
            Ok(desc) => Ok(desc),
            Err(BlossomClientError::PaymentRequired { x_cashu }) => {
                let payment_token = payment.pay(&x_cashu)?;
                self.upload_attempt(&data, auth_header, Some(&payment_token))
                    .await
            }
            Err(e) => Err(e),
        }
    }

    async fn extend_attempt(
        &self,
        sha256: &str,
        auth_header: &str,
        payment_token: Option<&str>,
        desired_expiry: Option<u64>,
    ) -> Result<BlobDescriptor, BlossomClientError> {
        let url = format!("{}/{}", self.base_url, sha256);
        let mut req = self
            .client
            .patch(&url)
            .header("Authorization", format!("Nostr {auth_header}"));

        if let Some(token) = payment_token {
            req = req.header("X-Cashu", token);
        }
        if let Some(expiry) = desired_expiry {
            req = req.header("X-Desired-Expiry", expiry.to_string());
        }

        let response = req.send().await?;
        let status = response.status();

        if status.is_success() {
            let body = response.text().await?;
            return Ok(serde_json::from_str(&body)?);
        }

        if status.as_u16() == 402 {
            let x_cashu = response
                .headers()
                .get("x-cashu")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            return Err(BlossomClientError::PaymentRequired { x_cashu });
        }

        let body = response.text().await.unwrap_or_default();
        Err(BlossomClientError::ServerError {
            status: status.as_u16(),
            body,
        })
    }

    pub async fn extend_blob_with_payment(
        &self,
        sha256: &str,
        auth_header: &str,
        payment: &dyn PaymentStrategy,
        desired_expiry: Option<u64>,
    ) -> Result<BlobDescriptor, BlossomClientError> {
        match self
            .extend_attempt(sha256, auth_header, None, desired_expiry)
            .await
        {
            Ok(desc) => Ok(desc),
            Err(BlossomClientError::PaymentRequired { x_cashu }) => {
                let payment_token = payment.pay(&x_cashu)?;
                self.extend_attempt(sha256, auth_header, Some(&payment_token), desired_expiry)
                    .await
            }
            Err(e) => Err(e),
        }
    }

    /// Initiate a multipart upload session (blossomflare).
    ///
    /// `POST /upload/multipart`
    pub async fn init_multipart_upload(
        &self,
        content_length: u64,
        content_type: &str,
        auth_header: &str,
    ) -> Result<MultipartUploadInit, BlossomClientError> {
        let url = format!("{}/upload/multipart", self.base_url);
        let body = serde_json::json!({
            "content_length": content_length,
            "content_type": content_type,
        });
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Nostr {auth_header}"))
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(BlossomClientError::ServerError {
                status: status.as_u16(),
                body,
            });
        }
        let body = response.text().await?;
        let init: MultipartUploadInit = serde_json::from_str(&body)?;
        Ok(init)
    }

    /// Upload a single chunk (blossomflare).
    ///
    /// `PUT /upload/multipart/:upload_id`
    pub async fn upload_multipart_part(
        &self,
        upload_id: &str,
        part_number: u32,
        data: Vec<u8>,
        auth_header: &str,
    ) -> Result<MultipartPartResponse, BlossomClientError> {
        let url = format!("{}/upload/multipart/{}", self.base_url, upload_id);
        let response = self
            .client
            .put(&url)
            .header("Authorization", format!("Nostr {auth_header}"))
            .header("X-Part-Number", part_number.to_string())
            .header("Content-Type", "application/octet-stream")
            .body(data)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(BlossomClientError::ServerError {
                status: status.as_u16(),
                body,
            });
        }
        let body = response.text().await?;
        let part: MultipartPartResponse = serde_json::from_str(&body)?;
        Ok(part)
    }

    /// Complete a multipart upload (blossomflare).
    ///
    /// `POST /upload/multipart/:upload_id/complete`
    pub async fn complete_multipart_upload(
        &self,
        upload_id: &str,
        auth_header: &str,
    ) -> Result<BlobDescriptor, BlossomClientError> {
        let url = format!("{}/upload/multipart/{}/complete", self.base_url, upload_id);
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Nostr {auth_header}"))
            .header("Content-Type", "application/json")
            .body("{}")
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(BlossomClientError::ServerError {
                status: status.as_u16(),
                body,
            });
        }
        let body = response.text().await?;
        let descriptor: BlobDescriptor = serde_json::from_str(&body)?;
        Ok(descriptor)
    }

    /// Upload a blob using multipart, orchestrating init → chunks → complete.
    ///
    /// Convenience method that chains the three steps.
    /// If init fails with 404/405, the error is returned (caller should fall back to single-shot).
    pub async fn upload_blob_multipart(
        &self,
        data: &[u8],
        content_type: &str,
        auth_header: &str,
    ) -> Result<BlobDescriptor, BlossomClientError> {
        let init = self
            .init_multipart_upload(data.len() as u64, content_type, auth_header)
            .await?;

        let chunk_size = init.chunk_size as usize;
        for part_num in 1..=init.total_chunks {
            let offset = (part_num as usize - 1) * chunk_size;
            let end = std::cmp::min(offset + chunk_size, data.len());
            let chunk = data[offset..end].to_vec();
            self.upload_multipart_part(&init.upload_id, part_num, chunk, auth_header)
                .await?;
        }

        self.complete_multipart_upload(&init.upload_id, auth_header)
            .await
    }

    /// Check if a blob exists on the server (BUD-01).
    ///
    /// `HEAD /<sha256>` — returns `true` if the blob exists (200/206),
    /// `false` if it does not (404).
    pub async fn head_blob(&self, sha256: &str) -> Result<bool, BlossomClientError> {
        let url = format!("{}/{}", self.base_url, sha256);
        let response = self.client.head(&url).send().await?;
        let status = response.status();
        if status.is_success() {
            Ok(true)
        } else if status.as_u16() == 404 {
            Ok(false)
        } else {
            let body = response.text().await.unwrap_or_default();
            Err(BlossomClientError::ServerError {
                status: status.as_u16(),
                body,
            })
        }
    }

    /// Check if a blob exists and return its expiry from the `Sunset` header (BUD-01).
    ///
    /// Returns `Ok(Some(unix_ts))` if the blob exists (Sunset header parsed),
    /// `Ok(None)` if the blob does not exist (404) or has no Sunset header,
    /// or `Err` on server errors.
    pub async fn head_blob_with_expiry(
        &self,
        sha256: &str,
    ) -> Result<BlobHeadInfo, BlossomClientError> {
        let url = format!("{}/{}", self.base_url, sha256);
        let response = self.client.head(&url).send().await?;
        let status = response.status();

        if status.is_success() {
            let sunset = response
                .headers()
                .get("sunset")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| httpdate::parse_http_date(s).ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            Ok(BlobHeadInfo {
                exists: true,
                sunset,
            })
        } else if status.as_u16() == 404 {
            Ok(BlobHeadInfo {
                exists: false,
                sunset: None,
            })
        } else {
            let body = response.text().await.unwrap_or_default();
            Err(BlossomClientError::ServerError {
                status: status.as_u16(),
                body,
            })
        }
    }

    /// Pre-flight check before upload (BUD-06).
    ///
    /// `HEAD /upload` with `X-SHA-256`, `X-Content-Length`, `X-Content-Type`
    /// headers. Returns `Ok(())` if the server would accept the upload (200),
    /// `Err(PaymentRequired)` if payment is needed (402),
    /// or `Err(ServerError)` for other rejections (413, 415, 429, etc.).
    pub async fn preflight_upload(
        &self,
        auth_header: &str,
        sha256_hex: &str,
        content_length: u64,
        content_type: &str,
    ) -> Result<(), BlossomClientError> {
        let url = format!("{}/upload", self.base_url);
        let response = self
            .client
            .head(&url)
            .header("Authorization", format!("Nostr {auth_header}"))
            .header("X-SHA-256", sha256_hex)
            .header("X-Content-Length", content_length.to_string())
            .header("X-Content-Type", content_type)
            .send()
            .await?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }

        if status.as_u16() == 402 {
            let x_cashu = response
                .headers()
                .get("x-cashu")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            return Err(BlossomClientError::PaymentRequired { x_cashu });
        }

        let body = response.text().await.unwrap_or_default();
        Err(BlossomClientError::ServerError {
            status: status.as_u16(),
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ── Helpers ────────────────────────────────────────────────────────────

    fn desc_json(sha: &str, uploaded: u64) -> serde_json::Value {
        serde_json::json!({
            "url": format!("https://cdn.example.com/{}", sha),
            "sha256": sha,
            "size": 100,
            "type": "image/png",
            "uploaded": uploaded
        })
    }

    struct MockPayment;
    impl crate::payment::PaymentStrategy for MockPayment {
        fn pay(&self, _req: &str) -> Result<String, crate::payment::PaymentError> {
            Ok("cashuBproof".to_string())
        }
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

        // Mock B (highest priority=1): matches GET /list/pagepk with cursor=p1b
        // (the sha256 of the last item in page 1, per BUD-12 spec).
        // Returns 1 item — the second (final) page.
        Mock::given(method("GET"))
            .and(path("/list/pagepk"))
            .and(query_param("cursor", "p1b"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([desc_json("p2a", 3000)])),
            )
            .with_priority(1)
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.list_all_blobs("pagepk", 0, 2).await;

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
        let result = client.list_all_blobs("singlepk", 0, 2).await;

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

    // ── Scenario 14: Happy — upload_blob returns descriptor ───────────────

    #[tokio::test]
    async fn test_upload_blob_returns_descriptor() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "url": "https://cdn.example.com/abc123",
                "sha256": "abc123",
                "size": 13,
                "type": "application/octet-stream",
                "uploaded": 1700000000
            })))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let data = b"Hello, World!".to_vec();
        let result = client.upload_blob(data, "dummy_auth_token").await;

        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
        let desc = result.unwrap();
        assert_eq!(desc.sha256, "abc123");
        assert_eq!(desc.size, 13);
        assert_eq!(desc.url, "https://cdn.example.com/abc123");
        assert_eq!(desc.uploaded, 1700000000);
    }

    // ── Scenario 14b: Happy — upload_blob sends Authorization header ──────

    #[tokio::test]
    async fn test_upload_blob_sends_nostr_auth_header() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .and(header("Authorization", "Nostr my_token_123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "url": "https://cdn.example.com/x",
                "sha256": "x",
                "size": 5,
                "type": "application/octet-stream",
                "uploaded": 1
            })))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.upload_blob(b"hello".to_vec(), "my_token_123").await;

        // If the Authorization header didn't match, wiremock returns 404
        assert!(
            result.is_ok(),
            "expected Ok (header matched), got {:?}",
            result.err()
        );
    }

    // ── Scenario 14c: Happy — upload_blob sends Content-Type header ───────

    #[tokio::test]
    async fn test_upload_blob_sends_content_type_header() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .and(header("Content-Type", "application/octet-stream"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "url": "https://cdn.example.com/y",
                "sha256": "y",
                "size": 1,
                "type": "application/octet-stream",
                "uploaded": 1
            })))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.upload_blob(vec![0x42], "tok").await;

        assert!(
            result.is_ok(),
            "expected Ok (content-type matched), got {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_upload_blob_sends_sha256_header() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .and(header(
                "X-SHA-256",
                "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "url": "https://cdn.example.com/sha256test",
                "sha256": "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
                "size": 5,
                "type": "application/octet-stream",
                "uploaded": 1
            })))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let data = b"hello".to_vec();
        let result = client.upload_blob(data, "tok").await;

        assert!(
            result.is_ok(),
            "expected Ok (X-SHA-256 header matched), got {:?}",
            result.err()
        );
    }

    // ── Scenario 15: Edge — upload_blob 402 Payment Required ──────────────

    #[tokio::test]
    async fn test_upload_blob_402_payment_required() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(402))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.upload_blob(b"data".to_vec(), "tok").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            BlossomClientError::ServerError { status, .. } => {
                assert_eq!(status, 402);
            }
            other => panic!("expected ServerError, got {other:?}"),
        }
    }

    // ── Scenario 16: Edge — upload_blob 500 → ServerError ────────────────

    #[tokio::test]
    async fn test_upload_blob_500() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.upload_blob(b"data".to_vec(), "tok").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            BlossomClientError::ServerError { status, .. } => {
                assert_eq!(status, 500);
            }
            other => panic!("expected ServerError, got {other:?}"),
        }
    }

    // ── Scenario 17: Happy — delete_blob sends DELETE + Nostr auth ────────

    #[tokio::test]
    async fn test_delete_blob_sends_nostr_auth_header() {
        let mock_server = MockServer::start().await;

        Mock::given(method("DELETE"))
            .and(path("/abc123"))
            .and(header("Authorization", "Nostr my_token_123"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.delete_blob("abc123", "my_token_123").await;

        assert!(
            result.is_ok(),
            "expected Ok (header matched), got {:?}",
            result.err()
        );
    }

    // ── Scenario 18: Edge — delete_blob 404 → ServerError ────────────────

    #[tokio::test]
    async fn test_delete_blob_404() {
        let mock_server = MockServer::start().await;

        Mock::given(method("DELETE"))
            .and(path("/abc123"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.delete_blob("abc123", "tok").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            BlossomClientError::ServerError { status, body } => {
                assert_eq!(status, 404);
                assert!(body.contains("not found"));
            }
            other => panic!("expected ServerError, got {other:?}"),
        }
    }

    // ── Scenario 19: Happy — init_multipart_upload returns fields ──────────

    #[tokio::test]
    async fn test_init_multipart_returns_fields() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/upload/multipart"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "upload_id": "sess123",
                "sha256": "abc",
                "chunk_size": 52428800u64,
                "total_chunks": 3u32,
                "expires_in": 3600u64
            })))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client
            .init_multipart_upload(150000000, "application/octet-stream", "tok")
            .await;

        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
        let init = result.unwrap();
        assert_eq!(init.upload_id, "sess123");
        assert_eq!(init.total_chunks, 3);
        assert_eq!(init.chunk_size, 52428800);
    }

    // ── Scenario 20: Happy — init_multipart_upload sends Nostr auth ────────

    #[tokio::test]
    async fn test_init_multipart_sends_auth_header() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/upload/multipart"))
            .and(header("Authorization", "Nostr tok"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "upload_id": "x",
                "sha256": "x",
                "chunk_size": 1u64,
                "total_chunks": 1u32,
                "expires_in": 1u64
            })))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client
            .init_multipart_upload(10, "application/octet-stream", "tok")
            .await;

        assert!(
            result.is_ok(),
            "expected Ok (auth header matched), got {:?}",
            result.err()
        );
    }

    // ── Scenario 21: Edge — init_multipart_upload 404 → ServerError ────────

    #[tokio::test]
    async fn test_init_multipart_404_server_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/upload/multipart"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client
            .init_multipart_upload(100, "application/octet-stream", "tok")
            .await;

        assert!(result.is_err());
        match result.unwrap_err() {
            BlossomClientError::ServerError { status, .. } => assert_eq!(status, 404),
            other => panic!("expected ServerError, got {other:?}"),
        }
    }

    // ── Scenario 22: Happy — upload_multipart_part sends X-Part-Number ─────

    #[tokio::test]
    async fn test_upload_multipart_part_sends_part_number() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/upload/multipart/sess123"))
            .and(header("X-Part-Number", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "part_number": 1u32,
                "etag": "etag1"
            })))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client
            .upload_multipart_part("sess123", 1, vec![1, 2, 3], "tok")
            .await;

        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
        let part = result.unwrap();
        assert_eq!(part.part_number, 1);
        assert_eq!(part.etag, "etag1");
    }

    // ── Scenario 23: Edge — upload_multipart_part 500 → ServerError ────────

    #[tokio::test]
    async fn test_upload_multipart_part_500_server_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/upload/multipart/sess123"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client
            .upload_multipart_part("sess123", 1, vec![1, 2, 3], "tok")
            .await;

        assert!(result.is_err());
        match result.unwrap_err() {
            BlossomClientError::ServerError { status, .. } => assert_eq!(status, 500),
            other => panic!("expected ServerError, got {other:?}"),
        }
    }

    // ── Scenario 24: Happy — complete_multipart_upload returns descriptor ──

    #[tokio::test]
    async fn test_complete_multipart_returns_descriptor() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/upload/multipart/sess123/complete"))
            .respond_with(ResponseTemplate::new(201).set_body_json(desc_json("comp1", 1700000000)))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.complete_multipart_upload("sess123", "tok").await;

        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
        let desc = result.unwrap();
        assert_eq!(desc.sha256, "comp1");
        assert_eq!(desc.size, 100);
        assert_eq!(desc.url, "https://cdn.example.com/comp1");
        assert_eq!(desc.uploaded, 1700000000);
    }

    // ── Scenario 25: Happy — upload_blob_multipart full flow (init→3×PUT→complete)

    #[tokio::test]
    async fn test_upload_blob_multipart_full_flow() {
        let mock_server = MockServer::start().await;

        // init: chunk_size=50, total_chunks=3 → 120B splits as 50+50+20
        Mock::given(method("POST"))
            .and(path("/upload/multipart"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "upload_id": "sess",
                "sha256": "h",
                "chunk_size": 50u64,
                "total_chunks": 3u32,
                "expires_in": 3600u64
            })))
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/upload/multipart/sess"))
            .and(header("X-Part-Number", "1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "part_number": 1u32, "etag": "e1" })),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/upload/multipart/sess"))
            .and(header("X-Part-Number", "2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "part_number": 2u32, "etag": "e2" })),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/upload/multipart/sess"))
            .and(header("X-Part-Number", "3"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "part_number": 3u32, "etag": "e3" })),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("POST"))
            .and(path("/upload/multipart/sess/complete"))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(desc_json("fullflow", 1700000000)),
            )
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let data = vec![0u8; 120];
        let result = client
            .upload_blob_multipart(&data, "application/octet-stream", "tok")
            .await;

        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
        let desc = result.unwrap();
        assert_eq!(desc.sha256, "fullflow");
        assert_eq!(desc.size, 100);
    }

    // ── Scenario 26: Edge — upload_blob_multipart init 404 short-circuits ──

    #[tokio::test]
    async fn test_upload_blob_multipart_init_404() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/upload/multipart"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        // complete must NEVER be called when init fails.
        Mock::given(method("POST"))
            .and(path("/upload/multipart/sess/complete"))
            .respond_with(ResponseTemplate::new(201).set_body_json(desc_json("never", 1)))
            .expect(0)
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client
            .upload_blob_multipart(&[0u8; 120], "application/octet-stream", "tok")
            .await;

        assert!(result.is_err(), "expected init failure to propagate");
        match result.unwrap_err() {
            BlossomClientError::ServerError { status, .. } => assert_eq!(status, 404),
            other => panic!("expected ServerError from init, got {other:?}"),
        }
    }

    // ── BUD-01: head_blob existence check ──────────────────────────────

    #[tokio::test]
    async fn test_head_blob_exists() {
        let mock_server = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/abc123def456"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let exists = client.head_blob("abc123def456").await.unwrap();

        assert!(exists);
    }

    #[tokio::test]
    async fn test_head_blob_not_found() {
        let mock_server = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/abc123def456"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let exists = client.head_blob("abc123def456").await.unwrap();

        assert!(!exists);
    }

    #[tokio::test]
    async fn test_head_blob_206_partial_counts_as_exists() {
        let mock_server = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/abc123def456"))
            .respond_with(ResponseTemplate::new(206))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let exists = client.head_blob("abc123def456").await.unwrap();

        assert!(exists);
    }

    #[tokio::test]
    async fn test_head_blob_500_server_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/abc123def456"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client.head_blob("abc123def456").await;

        assert!(result.is_err());
    }

    // ── BUD-01: head_blob_with_expiry ────────────────────────────────

    #[tokio::test]
    async fn test_head_blob_with_expiry_parses_sunset() {
        let mock_server = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/abc123"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("sunset", "Wed, 17 Jun 2026 12:00:00 GMT"),
            )
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let info = client.head_blob_with_expiry("abc123").await.unwrap();

        assert!(info.exists);
        assert!(info.sunset.is_some(), "sunset should be parsed");
    }

    #[tokio::test]
    async fn test_head_blob_with_expiry_no_sunset_header() {
        let mock_server = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/abc123"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let info = client.head_blob_with_expiry("abc123").await.unwrap();

        assert!(info.exists);
        assert!(
            info.sunset.is_none(),
            "sunset should be None when header absent"
        );
    }

    #[tokio::test]
    async fn test_head_blob_with_expiry_not_found() {
        let mock_server = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/abc123"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let info = client.head_blob_with_expiry("abc123").await.unwrap();

        assert!(!info.exists);
        assert!(info.sunset.is_none());
    }

    // ── Extend with X-Desired-Expiry ──────────────────────────────────

    #[tokio::test]
    async fn test_extend_sends_desired_expiry_header() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PATCH"))
            .and(path("/abc123"))
            .and(header("X-Desired-Expiry", "9999999999"))
            .and(header("Authorization", "Nostr tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(desc_json("abc123", 1000)))
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client
            .extend_blob_with_payment(
                "abc123",
                "tok",
                &crate::payment::NoPayment,
                Some(9999999999),
            )
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_extend_without_desired_expiry_omits_header() {
        let mock_server = MockServer::start().await;

        // Match any PATCH that does NOT have X-Desired-Expiry
        Mock::given(method("PATCH"))
            .and(path("/abc123"))
            .and(header("Authorization", "Nostr tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(desc_json("abc123", 1000)))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client
            .extend_blob_with_payment("abc123", "tok", &crate::payment::NoPayment, None)
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_extend_desired_expiry_200_already_covered() {
        let mock_server = MockServer::start().await;

        Mock::given(method("PATCH"))
            .and(path("/abc123"))
            .and(header("X-Desired-Expiry", "1000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(desc_json("abc123", 1000)))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client
            .extend_blob_with_payment("abc123", "tok", &crate::payment::NoPayment, Some(1000))
            .await;

        assert!(
            result.is_ok(),
            "server returns 200 when expiry already covered"
        );
    }

    #[tokio::test]
    async fn test_extend_desired_expiry_402_then_pay() {
        let mock_server = MockServer::start().await;

        // First attempt (no payment) → 402, only responds once
        Mock::given(method("PATCH"))
            .and(path("/abc123"))
            .and(header("X-Desired-Expiry", "999999"))
            .respond_with(
                ResponseTemplate::new(402)
                    .insert_header("X-Cashu", "creqAtest")
                    .insert_header("X-Price-Sats", "5"),
            )
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        // Second attempt (with payment) → 200
        Mock::given(method("PATCH"))
            .and(path("/abc123"))
            .and(header("X-Cashu", "cashuBproof"))
            .and(header("X-Desired-Expiry", "999999"))
            .respond_with(ResponseTemplate::new(200).set_body_json(desc_json("abc123", 1000)))
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client
            .extend_blob_with_payment("abc123", "tok", &MockPayment, Some(999999))
            .await;

        assert!(result.is_ok());
    }

    // ── BUD-06: preflight_upload ───────────────────────────────────────

    #[tokio::test]
    async fn test_preflight_upload_200_ok() {
        let mock_server = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/upload"))
            .and(header("X-SHA-256", "abc123"))
            .and(header("X-Content-Length", "1024"))
            .and(header("X-Content-Type", "image/png"))
            .and(header("Authorization", "Nostr tok"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client
            .preflight_upload("tok", "abc123", 1024, "image/png")
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_preflight_upload_402_payment() {
        let mock_server = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/upload"))
            .respond_with(
                ResponseTemplate::new(402)
                    .insert_header("X-Cashu", "creqA12345")
                    .insert_header("X-Price-Sats", "50"),
            )
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client
            .preflight_upload("tok", "abc123", 1048576, "video/mp4")
            .await;

        match result {
            Err(BlossomClientError::PaymentRequired { x_cashu }) => {
                assert_eq!(x_cashu, "creqA12345");
            }
            other => panic!("expected PaymentRequired, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_preflight_upload_413_too_large() {
        let mock_server = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(413).insert_header("X-Reason", "File too large"))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client
            .preflight_upload("tok", "abc123", 5368709120, "video/mp4")
            .await;

        match result {
            Err(BlossomClientError::ServerError { status, .. }) => assert_eq!(status, 413),
            other => panic!("expected ServerError 413, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_preflight_upload_415_unsupported_type() {
        let mock_server = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(415))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client
            .preflight_upload("tok", "abc123", 1024, "application/exe")
            .await;

        match result {
            Err(BlossomClientError::ServerError { status, .. }) => assert_eq!(status, 415),
            other => panic!("expected ServerError 415, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_preflight_upload_429_rate_limited() {
        let mock_server = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "60"))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client
            .preflight_upload("tok", "abc123", 1024, "image/png")
            .await;

        match result {
            Err(BlossomClientError::ServerError { status, .. }) => assert_eq!(status, 429),
            other => panic!("expected ServerError 429, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_preflight_upload_404_server_no_support() {
        let mock_server = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client
            .preflight_upload("tok", "abc123", 1024, "image/png")
            .await;

        match result {
            Err(BlossomClientError::ServerError { status, .. }) => assert_eq!(status, 404),
            other => panic!("expected ServerError 404, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_preflight_upload_sends_all_required_headers() {
        let mock_server = MockServer::start().await;

        Mock::given(method("HEAD"))
            .and(path("/upload"))
            .and(header("Authorization", "Nostr mytoken"))
            .and(header("X-SHA-256", "aabbccdd"))
            .and(header("X-Content-Length", "999"))
            .and(header("X-Content-Type", "application/pdf"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client
            .preflight_upload("mytoken", "aabbccdd", 999, "application/pdf")
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn p01_payment_flow_402_then_success() {
        use crate::payment::PaymentError;
        use crate::payment::PaymentStrategy;
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        struct MockPayment;
        impl PaymentStrategy for MockPayment {
            fn pay(&self, _request: &str) -> Result<String, PaymentError> {
                Ok("cashuBmocktoken123".to_string())
            }
        }

        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(402).insert_header("X-Cashu", "creqAabc123"))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .and(header("X-Cashu", "cashuBmocktoken123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "url": "https://example.com/abc",
                "sha256": "abc",
                "size": 9,
                "type": "application/octet-stream",
                "uploaded": 1700000000
            })))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client
            .upload_blob_with_payment(b"test data".to_vec(), "Nostr tok", &MockPayment)
            .await;

        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
        let desc = result.unwrap();
        assert_eq!(desc.sha256, "abc");
    }

    #[tokio::test]
    async fn p02_payment_flow_passes_xcashu_to_strategy() {
        use crate::payment::PaymentError;
        use crate::payment::PaymentStrategy;
        use std::sync::{Arc, Mutex};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        struct CapturingPayment(Arc<Mutex<String>>);
        impl PaymentStrategy for CapturingPayment {
            fn pay(&self, request: &str) -> Result<String, PaymentError> {
                *self.0.lock().unwrap() = request.to_string();
                Ok("cashuBtoken".to_string())
            }
        }

        let mock_server = MockServer::start().await;
        let captured = Arc::new(Mutex::new(String::new()));

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(402).insert_header("X-Cashu", "creqAspecific123"))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "url": "https://example.com/def",
                "sha256": "def",
                "size": 5,
                "type": "application/octet-stream",
                "uploaded": 1700000000
            })))
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let strategy = CapturingPayment(captured.clone());
        let _ = client
            .upload_blob_with_payment(b"hello".to_vec(), "Nostr tok", &strategy)
            .await;

        let got = captured.lock().unwrap().clone();
        assert_eq!(got, "creqAspecific123");
    }

    #[tokio::test]
    async fn p03_payment_not_needed_succeeds_directly() {
        use crate::payment::NoPayment;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "url": "https://example.com/ghi",
                "sha256": "ghi",
                "size": 3,
                "type": "application/octet-stream",
                "uploaded": 1700000000
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result = client
            .upload_blob_with_payment(b"abc".to_vec(), "Nostr tok", &NoPayment)
            .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap().sha256, "ghi");
    }
}
