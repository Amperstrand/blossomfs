//! Nostr key parsing and management.
//!
//! Handles parsing npub (bech32 public key) and nsec (bech32 secret key)
//! using nostr-sdk. Security: never log nsec/private keys.

#![allow(dead_code)]

use nostr_sdk::prelude::*;
use thiserror::Error;

/// Error type for key parsing operations.
#[derive(Error, Debug)]
pub enum KeyError {
    #[error("invalid public key format: expected npub (bech32) or 64-char hex")]
    InvalidPublicKey,
    #[error("invalid secret key format: expected nsec (bech32)")]
    InvalidSecretKey,
    #[error("failed to read key file: {0}")]
    FileRead(String),
    #[error("nostr error: {0}")]
    Nostr(#[from] nostr_sdk::client::Error),
}

/// Parse a bech32 npub string into a nostr PublicKey.
///
/// # Arguments
///
/// * `npub` - A bech32-encoded npub string (e.g., "npub1...")
///
/// # Returns
///
/// Returns `Ok(PublicKey)` if the npub is valid, `Err(KeyError::InvalidPublicKey)` otherwise.
pub fn parse_npub(npub: &str) -> Result<PublicKey, KeyError> {
    PublicKey::from_bech32(npub).map_err(|_| KeyError::InvalidPublicKey)
}

/// Parse a hex public key string (64 hex chars) into a nostr PublicKey.
///
/// # Arguments
///
/// * `hex` - A hex-encoded public key string (64 hex characters)
///
/// # Returns
///
/// Returns `Ok(PublicKey)` if the hex is valid, `Err(KeyError::InvalidPublicKey)` otherwise.
pub fn parse_pubkey_hex(hex: &str) -> Result<PublicKey, KeyError> {
    PublicKey::from_hex(hex).map_err(|_| KeyError::InvalidPublicKey)
}

/// Parse either npub or hex pubkey. Try npub first, fall back to hex.
///
/// # Arguments
///
/// * `input` - Either a bech32 npub string or a hex-encoded public key (64 hex characters)
///
/// # Returns
///
/// Returns `Ok(PublicKey)` if the input is valid, `Err(KeyError::InvalidPublicKey)` otherwise.
pub fn parse_pubkey(input: &str) -> Result<PublicKey, KeyError> {
    // Try npub first
    if let Ok(pk) = parse_npub(input) {
        return Ok(pk);
    }
    // Fall back to hex
    parse_pubkey_hex(input)
}

/// Convert a PublicKey to lowercase hex string (64 chars).
///
/// # Arguments
///
/// * `pk` - A nostr PublicKey
///
/// # Returns
///
/// Returns the 64-character lowercase hex representation of the public key.
pub fn pubkey_to_hex(pk: &PublicKey) -> String {
    pk.to_hex()
}

/// Convert a PublicKey to bech32 npub string.
///
/// # Arguments
///
/// * `pk` - A nostr PublicKey
///
/// # Returns
///
/// Returns `Ok(String)` with the npub bech32 encoding, or `Err` if encoding fails.
pub fn pubkey_to_npub(pk: &PublicKey) -> Result<String, KeyError> {
    pk.to_bech32().map_err(|_| KeyError::InvalidPublicKey)
}

/// Parse a bech32 nsec string into nostr Keys (contains secret key).
///
/// # Security
///
/// The caller must ensure the returned Keys are never logged or printed.
/// The nostr-sdk Keys type redacts the secret key in Debug output.
///
/// # Arguments
///
/// * `nsec` - A bech32-encoded nsec string (e.g., "nsec1...")
///
/// # Returns
///
/// Returns `Ok(Keys)` if the nsec is valid, `Err(KeyError::InvalidSecretKey)` otherwise.
pub fn parse_nsec(nsec: &str) -> Result<Keys, KeyError> {
    Keys::parse(nsec).map_err(|_| KeyError::InvalidSecretKey)
}

/// Read an nsec from a file path. The file should contain just the nsec string.
///
/// # Security
///
/// The caller must ensure the returned Keys are never logged or printed.
///
/// # Arguments
///
/// * `path` - Path to a file containing an nsec string
///
/// # Returns
///
/// Returns `Ok(Keys)` if the file exists and contains a valid nsec, `Err(KeyError::FileRead)` otherwise.
pub fn read_nsec_file(path: &std::path::Path) -> Result<Keys, KeyError> {
    let content = std::fs::read_to_string(path).map_err(|e| KeyError::FileRead(e.to_string()))?;
    let nsec = content.trim();
    parse_nsec(nsec)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate test keys for use in tests
    fn generate_test_keys() -> Keys {
        Keys::generate()
    }

    // SCENARIO 1: Happy - parse_pubkey with valid hex (64 chars) -> Ok
    #[test]
    fn scenario_1_parse_pubkey_valid_hex() {
        let keys = generate_test_keys();
        let hex = keys.public_key().to_hex();

        let result = parse_pubkey(&hex);
        assert!(result.is_ok(), "Should parse valid hex pubkey");
        let parsed = result.unwrap();
        assert_eq!(parsed.to_hex(), hex);
    }

    // SCENARIO 2: Happy - parse_pubkey with npub1... -> Ok
    #[test]
    fn scenario_2_parse_pubkey_valid_npub() {
        let keys = generate_test_keys();
        let npub = keys.public_key().to_bech32().unwrap();

        let result = parse_pubkey(&npub);
        assert!(result.is_ok(), "Should parse valid npub");
        let parsed = result.unwrap();
        assert_eq!(parsed.to_bech32().unwrap(), npub);
    }

    // SCENARIO 3: Edge - parse_pubkey with "invalid" -> Err(InvalidPublicKey)
    #[test]
    fn scenario_3_parse_pubkey_invalid() {
        let result = parse_pubkey("invalid");
        assert!(result.is_err());
        match result {
            Err(KeyError::InvalidPublicKey) => (),
            Err(e) => panic!("Expected InvalidPublicKey, got: {:?}", e),
            Ok(_) => panic!("Should have failed with InvalidPublicKey"),
        }
    }

    // SCENARIO 4: Edge - parse_pubkey with empty string -> Err(InvalidPublicKey)
    #[test]
    fn scenario_4_parse_pubkey_empty() {
        let result = parse_pubkey("");
        assert!(result.is_err());
        match result {
            Err(KeyError::InvalidPublicKey) => (),
            Err(e) => panic!("Expected InvalidPublicKey, got: {:?}", e),
            Ok(_) => panic!("Should have failed with InvalidPublicKey"),
        }
    }

    // SCENARIO 5: Happy - pubkey_to_hex returns 64-char lowercase hex
    #[test]
    fn scenario_5_pubkey_to_hex_lowercase_64_chars() {
        let keys = generate_test_keys();
        let hex = pubkey_to_hex(&keys.public_key());

        assert_eq!(hex.len(), 64, "Hex should be 64 characters");
        assert!(
            hex.chars().all(|c| c.is_ascii_hexdigit()),
            "All chars should be hex digits"
        );
    }

    // SCENARIO 6: Happy - pubkey_to_npub returns string starting with "npub1"
    #[test]
    fn scenario_6_pubkey_to_npub_starts_with_npub1() {
        let keys = generate_test_keys();
        let npub = pubkey_to_npub(&keys.public_key()).unwrap();

        assert!(npub.starts_with("npub1"), "npub should start with 'npub1'");
    }

    // SCENARIO 7: Happy - roundtrip: hex -> PublicKey -> hex preserves value
    #[test]
    fn scenario_7_roundtrip_hex_preserves_value() {
        let keys = generate_test_keys();
        let original_hex = keys.public_key().to_hex();

        let parsed = parse_pubkey_hex(&original_hex).unwrap();
        let roundtrip_hex = pubkey_to_hex(&parsed);

        assert_eq!(
            original_hex, roundtrip_hex,
            "Hex should be preserved in roundtrip"
        );
    }

    // SCENARIO 8: Happy - roundtrip: PublicKey -> npub -> PublicKey preserves key
    #[test]
    fn scenario_8_roundtrip_npub_preserves_key() {
        let keys = generate_test_keys();
        let original_hex = keys.public_key().to_hex();

        let npub = pubkey_to_npub(&keys.public_key()).unwrap();
        let parsed = parse_npub(&npub).unwrap();
        let roundtrip_hex = parsed.to_hex();

        assert_eq!(
            original_hex, roundtrip_hex,
            "PublicKey should be preserved in roundtrip"
        );
    }

    // SCENARIO 9: Edge - parse_nsec with "invalid" -> Err(InvalidSecretKey)
    #[test]
    fn scenario_9_parse_nsec_invalid() {
        let result = parse_nsec("invalid");
        assert!(result.is_err());
        match result {
            Err(KeyError::InvalidSecretKey) => (),
            Err(e) => panic!("Expected InvalidSecretKey, got: {:?}", e),
            Ok(_) => panic!("Should have failed with InvalidSecretKey"),
        }
    }

    // SCENARIO 10: Security - Keys Debug output does NOT contain secret key hex
    #[test]
    fn scenario_10_security_keys_debug_redacts_secret() {
        let keys = generate_test_keys();
        let debug_output = format!("{:?}", keys);

        // The nostr-sdk Keys type should redact the secret key
        // Check that output does not expose raw secret key data
        assert!(
            !debug_output.contains("secret_key:")
                || debug_output.contains("REDACTED")
                || debug_output.contains("*****"),
            "Debug output should redact or hide secret key, got: {}",
            debug_output
        );
    }

    // SCENARIO 11: Happy - read_nsec_file reads file with nsec -> Ok(Keys)
    #[test]
    fn scenario_11_read_nsec_file_valid() {
        let keys = generate_test_keys();
        let nsec = keys.secret_key().to_bech32().unwrap();

        let temp_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(temp_file.path(), &nsec).unwrap();

        let result = read_nsec_file(temp_file.path());
        assert!(result.is_ok(), "Should read valid nsec file");
        let parsed_keys = result.unwrap();
        assert_eq!(
            parsed_keys.public_key(),
            keys.public_key(),
            "Public key should match"
        );
    }

    // SCENARIO 12: Edge - read_nsec_file with nonexistent file -> Err(FileRead)
    #[test]
    fn scenario_12_read_nsec_file_nonexistent() {
        let nonexistent = std::path::Path::new("/nonexistent/path/to/nsec.txt");

        let result = read_nsec_file(nonexistent);
        assert!(result.is_err());
        match result {
            Err(KeyError::FileRead(_)) => (),
            Err(e) => panic!("Expected FileRead, got: {:?}", e),
            Ok(_) => panic!("Should have failed with FileRead"),
        }
    }
}
