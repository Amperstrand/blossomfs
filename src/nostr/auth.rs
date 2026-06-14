//! BUD-11 Blossom Server Authentication.
//!
//! Creates and signs NIP-01 events of kind 24242 for authenticating
//! with Blossom servers for upload, delete, and list operations.
//!
//! See: <https://github.com/hzrd149/blossom/blob/master/buds/11.md>

#![allow(dead_code)]

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use thiserror::Error;

use nostr_sdk::prelude::*;

/// Errors that can occur when creating BUD-11 auth events.
#[derive(Error, Debug)]
pub enum AuthError {
    #[error("failed to sign event: {0}")]
    Sign(String),
    #[error("failed to serialize event: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("failed to encode: {0}")]
    Encode(String),
}

/// BUD-11 event kind for Blossom server authentication.
const BLOSSOM_AUTH_KIND: u16 = 24242;

/// Auth token validity window in seconds (1 hour).
const AUTH_EXPIRY_SECONDS: u64 = 3600;

/// Create a signed BUD-11 auth event for blob upload and return it as
/// base64url-encoded JSON (without padding) for use in the
/// `Authorization: Nostr <token>` HTTP header.
///
/// # Arguments
///
/// * `keys` - The Nostr keys to sign the event with.
/// * `sha256_hex` - The SHA-256 hash of the file being uploaded (hex string).
/// * `file_size` - The size of the file in bytes.
///
/// # Returns
///
/// A base64url-encoded (no padding) JSON string of the signed event,
/// suitable for placing directly after `Authorization: Nostr ` in an HTTP
/// request header.
///
/// # BUD-11 Tags
///
/// The event contains:
/// - `["t", "upload"]` — operation type
/// - `["expiration", "<now + 1 hour>"]` — token expiry
/// - `["x", "<sha256_hex>"]` — content hash
/// - `["size", "<file_size>"]` — file size in bytes
pub fn create_upload_auth_header(
    keys: &Keys,
    sha256_hex: &str,
    file_size: u64,
) -> Result<String, AuthError> {
    let now = Timestamp::now();
    let expiry = Timestamp::from_secs(now.as_secs() + AUTH_EXPIRY_SECONDS);

    let tags = vec![
        Tag::custom("t", ["upload"]),
        Tag::expiration(expiry),
        Tag::custom("x", [sha256_hex]),
        Tag::custom("size", [file_size.to_string()]),
    ];

    let event = EventBuilder::new(Kind::Custom(BLOSSOM_AUTH_KIND), "")
        .tags(tags)
        .finalize(keys)
        .map_err(|e| AuthError::Sign(e.to_string()))?;

    let json = event.as_json();
    let encoded = URL_SAFE_NO_PAD.encode(json.as_bytes());

    Ok(encoded)
}

/// Create a signed BUD-11 auth event for blob deletion and return it as
/// base64url-encoded JSON (without padding) for use in the
/// `Authorization: Nostr <token>` HTTP header.
///
/// # Arguments
///
/// * `keys` - The Nostr keys to sign the event with.
/// * `sha256_hex` - The SHA-256 hash of the blob to delete (hex string).
///
/// # BUD-11 Tags
///
/// The event contains:
/// - `["t", "delete"]` — operation type
/// - `["expiration", "<now + 1 hour>"]` — token expiry
/// - `["x", "<sha256_hex>"]` — content hash
///
/// Note: no `["size"]` tag is included for delete operations.
pub fn create_delete_auth_header(keys: &Keys, sha256_hex: &str) -> Result<String, AuthError> {
    let now = Timestamp::now();
    let expiry = Timestamp::from_secs(now.as_secs() + AUTH_EXPIRY_SECONDS);

    let tags = vec![
        Tag::custom("t", ["delete"]),
        Tag::expiration(expiry),
        Tag::custom("x", [sha256_hex]),
    ];

    let event = EventBuilder::new(Kind::Custom(BLOSSOM_AUTH_KIND), "")
        .tags(tags)
        .finalize(keys)
        .map_err(|e| AuthError::Sign(e.to_string()))?;

    let json = event.as_json();
    let encoded = URL_SAFE_NO_PAD.encode(json.as_bytes());

    Ok(encoded)
}

/// Decode a base64url-encoded auth token back into a Nostr `Event`.
///
/// Useful for verifying or inspecting tokens on the receiving side.
fn decode_auth_event(token: &str) -> Result<Event, AuthError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|e| AuthError::Encode(e.to_string()))?;
    let event: Event = Event::from_json(bytes).map_err(|e| AuthError::Sign(e.to_string()))?;
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers ────────────────────────────────────────────────────────────

    /// Helper: find a tag by kind and return its value (second element).
    fn find_tag<'a>(event: &'a Event, kind: &str) -> Option<&'a str> {
        event
            .tags
            .iter()
            .find(|t| t.kind() == kind)
            .and_then(|t| t.as_slice().get(1).map(|s| s.as_str()))
    }

    // ── S1: Upload auth → valid base64url decoding to JSON with kind 24242 ──

    #[test]
    fn test_upload_auth_decodes_to_valid_json_kind_24242() {
        let keys = Keys::generate();
        let sha256 = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let size: u64 = 1024;

        let token = create_upload_auth_header(&keys, sha256, size)
            .expect("should create upload auth header");

        // Must be valid base64url (no padding)
        assert!(!token.contains('='), "token must not contain padding '='");
        assert!(
            !token.contains('+'),
            "token must not contain '+' (base64url uses '-')"
        );
        assert!(
            !token.contains('/'),
            "token must not contain '/' (base64url uses '_')"
        );

        let event = decode_auth_event(&token).expect("should decode to valid event");
        assert_eq!(event.kind, Kind::Custom(24242));
    }

    // ── S2: Upload auth tags contain all required fields ────────────────────

    #[test]
    fn test_upload_auth_tags_contain_all_required_fields() {
        let keys = Keys::generate();
        let sha256 = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let size: u64 = 4096;

        let token = create_upload_auth_header(&keys, sha256, size).unwrap();
        let event = decode_auth_event(&token).unwrap();

        // ["t", "upload"]
        assert_eq!(find_tag(&event, "t"), Some("upload"));

        // ["expiration", "<unix_seconds>"]
        let exp_str = find_tag(&event, "expiration").expect("must have expiration tag");
        let exp_val: u64 = exp_str.parse().expect("expiration must be numeric");
        let now = Timestamp::now().as_secs();
        assert!(
            exp_val > now && exp_val <= now + 3600 + 5,
            "expiration should be ~1 hour from now"
        );

        // ["x", "<sha256_hex>"]
        assert_eq!(find_tag(&event, "x"), Some(sha256));

        // ["size", "<size_bytes>"]
        assert_eq!(find_tag(&event, "size"), Some("4096"));
    }

    // ── S2b: Content is empty string ────────────────────────────────────────

    #[test]
    fn test_upload_auth_content_is_empty() {
        let keys = Keys::generate();
        let token = create_upload_auth_header(&keys, "somelonghash", 100).unwrap();
        let event = decode_auth_event(&token).unwrap();

        assert_eq!(event.content, "");
    }

    // ── S3: Event signature verifies with the signer's public key ──────────

    #[test]
    fn test_upload_auth_signature_valid() {
        let keys = Keys::generate();
        let expected_pubkey = keys.public_key();

        let token = create_upload_auth_header(&keys, "deadbeefhash", 999).unwrap();
        let event = decode_auth_event(&token).unwrap();

        // Pubkey must match the keys that signed it
        assert_eq!(event.pubkey, expected_pubkey);

        // Signature must verify (checks both id and sig)
        event.verify().expect("event signature must be valid");
    }

    // ── S3b: Delete auth signature valid ────────────────────────────────────

    #[test]
    fn test_delete_auth_signature_valid() {
        let keys = Keys::generate();
        let expected_pubkey = keys.public_key();

        let token = create_delete_auth_header(&keys, "deadbeefhash").unwrap();
        let event = decode_auth_event(&token).unwrap();

        assert_eq!(event.pubkey, expected_pubkey);
        event
            .verify()
            .expect("delete event signature must be valid");
    }

    // ── S4: Delete auth → tag ["t", "delete"], no size tag ──────────────────

    #[test]
    fn test_delete_auth_has_delete_tag_and_no_size() {
        let keys = Keys::generate();
        let sha256 = "aabbccddaabbccddaabbccddaabbccddaabbccddaabbccddaabbccddaabbccdd";

        let token = create_delete_auth_header(&keys, sha256).unwrap();
        let event = decode_auth_event(&token).unwrap();

        // Must have ["t", "delete"]
        assert_eq!(find_tag(&event, "t"), Some("delete"));

        // Must have ["x", "<sha256>"]
        assert_eq!(find_tag(&event, "x"), Some(sha256));

        // Must have ["expiration", ...]
        assert!(
            find_tag(&event, "expiration").is_some(),
            "delete auth must have expiration tag"
        );

        // Must NOT have ["size", ...] tag
        assert!(
            find_tag(&event, "size").is_none(),
            "delete auth must not have size tag"
        );
    }

    // ── S4b: Delete auth kind is 24242 ──────────────────────────────────────

    #[test]
    fn test_delete_auth_kind_is_24242() {
        let keys = Keys::generate();
        let token = create_delete_auth_header(&keys, "somehash").unwrap();
        let event = decode_auth_event(&token).unwrap();

        assert_eq!(event.kind, Kind::Custom(24242));
    }

    // ── S5: Empty sha256 string still produces a valid event ────────────────

    #[test]
    fn test_upload_auth_with_empty_sha256_still_works() {
        let keys = Keys::generate();

        let token = create_upload_auth_header(&keys, "", 0);
        assert!(
            token.is_ok(),
            "empty sha256 should not cause error at this layer"
        );

        let token = token.unwrap();
        let event = decode_auth_event(&token).expect("should decode");
        assert_eq!(find_tag(&event, "x"), Some(""));
        assert_eq!(find_tag(&event, "size"), Some("0"));
    }

    // ── S5b: Large file size value ──────────────────────────────────────────

    #[test]
    fn test_upload_auth_large_size() {
        let keys = Keys::generate();
        let large_size: u64 = 1_073_741_824; // 1 GiB

        let token = create_upload_auth_header(&keys, "largefilehash", large_size).unwrap();
        let event = decode_auth_event(&token).unwrap();

        assert_eq!(find_tag(&event, "size"), Some("1073741824"));
    }

    // ── S5c: Two different tokens for different hashes are different ────────

    #[test]
    fn test_different_hashes_produce_different_tokens() {
        let keys = Keys::generate();

        let token1 = create_upload_auth_header(&keys, "hash_a", 100).unwrap();
        let token2 = create_upload_auth_header(&keys, "hash_b", 100).unwrap();

        assert_ne!(token1, token2, "tokens for different hashes must differ");
    }

    // ── S5d: Same keys/hash produce same pubkey in token ────────────────────

    #[test]
    fn test_token_pubkey_matches_signer() {
        let keys = Keys::generate();
        let expected_pk = keys.public_key().to_hex();

        let token = create_upload_auth_header(&keys, "pkcheck", 42).unwrap();
        let event = decode_auth_event(&token).unwrap();

        assert_eq!(
            event.pubkey.to_hex(),
            expected_pk,
            "token pubkey must match signer's pubkey"
        );
    }
}
