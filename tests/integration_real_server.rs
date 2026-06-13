//! Integration tests against real Blossom servers.
//!
//! These tests hit live servers and are #[ignore]'d by default.
//! Run with: cargo test --test integration_real_server -- --ignored
//!
//! Configure via env vars:
//!   BLOSSOM_TEST_SERVER  (default: https://blossom.psbt.me)
//!   BLOSSOM_TEST_PUBKEY  (hex, default: all-zeros placeholder)

use reqwest::StatusCode;
use serde::Deserialize;

const DEFAULT_SERVER: &str = "https://blossom.psbt.me";
const DUMMY_PUBKEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";

fn server_url() -> String {
    std::env::var("BLOSSOM_TEST_SERVER")
        .unwrap_or_else(|_| DEFAULT_SERVER.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn test_pubkey() -> String {
    std::env::var("BLOSSOM_TEST_PUBKEY").unwrap_or_else(|_| DUMMY_PUBKEY.to_string())
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct BlobDescriptor {
    sha256: String,
    size: u64,
    url: String,
    #[serde(default)]
    r#type: Option<String>,
}

fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build HTTP client")
}

// ---------------------------------------------------------------------------
// BUD-12: List endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn list_endpoint_returns_200() {
    let url = format!("{}/list/{}", server_url(), test_pubkey());
    let resp = build_client()
        .get(&url)
        .send()
        .await
        .expect("request failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "list endpoint should return 200 for {}",
        url
    );
}

#[tokio::test]
#[ignore]
async fn list_returns_valid_blob_descriptors() {
    let url = format!("{}/list/{}", server_url(), test_pubkey());
    let resp = build_client()
        .get(&url)
        .send()
        .await
        .expect("request failed");
    let descriptors: Vec<BlobDescriptor> =
        serde_json::from_str(&resp.text().await.expect("body read failed"))
            .expect("response must be a JSON array of blob descriptors");

    for desc in &descriptors {
        assert_eq!(desc.sha256.len(), 64, "sha256 must be 64 hex chars");
        assert!(
            desc.sha256.chars().all(|c| c.is_ascii_hexdigit()),
            "sha256 must be hex: {}",
            desc.sha256
        );
        assert!(desc.size > 0, "size must be positive");
        assert!(
            desc.url.starts_with("http://") || desc.url.starts_with("https://"),
            "url must be http/https: {}",
            desc.url
        );
    }
}

#[tokio::test]
#[ignore]
async fn list_pagination_cursor_is_sha256_hex() {
    let url = format!("{}/list/{}?limit=1", server_url(), test_pubkey());
    let resp = build_client()
        .get(&url)
        .send()
        .await
        .expect("request failed");

    if resp.status() != StatusCode::OK {
        eprintln!("Skipping: server returned {}", resp.status());
        return;
    }

    let descriptors: Vec<BlobDescriptor> =
        serde_json::from_str(&resp.text().await.expect("body read failed"))
            .expect("valid JSON array");

    if let Some(last) = descriptors.last() {
        let cursor = last.sha256.clone();
        assert_eq!(
            cursor.len(),
            64,
            "cursor (sha256) must be 64 hex chars, got {}",
            cursor
        );
        assert!(
            cursor.chars().all(|c| c.is_ascii_hexdigit()),
            "cursor must be hex: {}",
            cursor
        );

        let url2 = format!(
            "{}/list/{}?limit=1&cursor={}",
            server_url(),
            test_pubkey(),
            cursor
        );
        let resp2 = build_client()
            .get(&url2)
            .send()
            .await
            .expect("page 2 request failed");
        assert_eq!(
            resp2.status(),
            StatusCode::OK,
            "page 2 with sha256 cursor should return 200"
        );

        let page2: Vec<BlobDescriptor> =
            serde_json::from_str(&resp2.text().await.expect("body read failed"))
                .expect("page 2 valid JSON");

        let cursor_present = page2.iter().any(|d| d.sha256 == cursor);
        assert!(!cursor_present, "page 2 should not contain the cursor blob");
    }
}

// ---------------------------------------------------------------------------
// BUD-01: GET /<sha256>
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn get_blob_for_listed_descriptor() {
    let pubkey = test_pubkey();
    if pubkey == DUMMY_PUBKEY {
        eprintln!("Skipping: set BLOSSOM_TEST_PUBKEY to a real pubkey with blobs");
        return;
    }

    let url = format!("{}/list/{}", server_url(), pubkey);
    let resp = build_client()
        .get(&url)
        .send()
        .await
        .expect("list request failed");
    let descriptors: Vec<BlobDescriptor> =
        serde_json::from_str(&resp.text().await.expect("body read failed"))
            .expect("valid JSON array");

    let Some(desc) = descriptors.first() else {
        eprintln!("Skipping: no blobs found for pubkey");
        return;
    };

    let blob_url = format!("{}/{}", server_url(), desc.sha256);
    let blob_resp = build_client()
        .get(&blob_url)
        .send()
        .await
        .expect("blob GET failed");

    assert_eq!(
        blob_resp.status(),
        StatusCode::OK,
        "blob GET should return 200"
    );

    let bytes = blob_resp.bytes().await.expect("blob body read failed");
    assert_eq!(
        bytes.len() as u64,
        desc.size,
        "downloaded size must match descriptor size"
    );
}

#[tokio::test]
#[ignore]
async fn get_nonexistent_blob_returns_404() {
    let fake_hash = "0000000000000000000000000000000000000000000000000000000000000000";
    let url = format!("{}/{}", server_url(), fake_hash);
    let resp = build_client()
        .get(&url)
        .send()
        .await
        .expect("request failed");
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "nonexistent blob should return 404"
    );
}

// ---------------------------------------------------------------------------
// BUD-02: Server descriptor ( /.well-known/nostrjson)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn server_descriptor_returns_200() {
    let url = format!("{}/.well-known/nostrjson", server_url());
    let resp = build_client()
        .get(&url)
        .send()
        .await
        .expect("request failed");
    match resp.status() {
        StatusCode::OK => {}
        StatusCode::NOT_FOUND => {
            eprintln!("Note: server does not implement BUD-02 server descriptors (404)");
        }
        other => panic!("unexpected status {other} for BUD-02 endpoint"),
    }
}
