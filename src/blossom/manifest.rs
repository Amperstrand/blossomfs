//! Local manifest file parser.
//!
//! Reads a JSON file containing an array of BUD-02 blob descriptors
//! and parses them into `BlobDescriptor` structs.

#![allow(dead_code)]

use crate::blossom::descriptor::BlobDescriptor;
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ManifestError {
    #[error("failed to read manifest file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse manifest JSON: {0}")]
    Parse(#[from] serde_json::Error),
}

/// Parse a manifest JSON string into a vector of blob descriptors.
pub fn parse_manifest(json: &str) -> Result<Vec<BlobDescriptor>, ManifestError> {
    let descriptors: Vec<BlobDescriptor> = serde_json::from_str(json)?;
    Ok(descriptors)
}

/// Load and parse a manifest file from disk.
pub fn load_manifest(path: &Path) -> Result<Vec<BlobDescriptor>, ManifestError> {
    let content = std::fs::read_to_string(path)?;
    parse_manifest(&content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Scenario 1: Happy path - parse_manifest with valid JSON array of 4 descriptors → returns 4 items
    #[test]
    fn test_parse_manifest_four_descriptors() {
        let json = r#"[
            {
                "url": "https://example.com/blob1.png",
                "sha256": "a9f5c7af6fe6b706d1b7604beb47a62c55968406e27aacbb7fe65e3aa67fcc56",
                "size": 512000,
                "type": "image/png",
                "uploaded": 1725105921
            },
            {
                "url": "https://example.com/blob2.mp4",
                "sha256": "99896c743d6e5236dc581141ce5f57501b483100af4f60e203df4de1408b5018",
                "size": 5242880,
                "type": "video/mp4",
                "uploaded": 1725106832
            },
            {
                "url": "https://example.com/blob3.pdf",
                "sha256": "156920e70a9dc50fe5f6982ad7153e8d09308b59cdcb8857be6356b5b814f798",
                "size": 204800,
                "type": "application/pdf",
                "uploaded": 1725107743
            },
            {
                "url": "https://example.com/blob4.txt",
                "sha256": "d217c7dc22c8adf3d4287c0b668a31909dbf607a3c10ded0bb44f99ecb35f02c",
                "size": 1024,
                "type": "text/plain",
                "uploaded": 1725108654
            }
        ]"#;

        let result = parse_manifest(json);
        assert!(result.is_ok(), "Should parse 4 descriptors");
        let descriptors = result.unwrap();
        assert_eq!(descriptors.len(), 4);
    }

    /// Scenario 2: Happy path - parse_manifest with empty array `[]` → returns empty Vec
    #[test]
    fn test_parse_manifest_empty_array() {
        let json = "[]";

        let result = parse_manifest(json);
        assert!(result.is_ok(), "Should parse empty array");
        let descriptors = result.unwrap();
        assert_eq!(descriptors.len(), 0);
        assert!(descriptors.is_empty());
    }

    /// Scenario 3: Happy path - parse_manifest single descriptor → returns 1 item with correct fields
    #[test]
    fn test_parse_manifest_single_descriptor() {
        let json = r#"[
            {
                "url": "https://example.com/blob.png",
                "sha256": "abc123def456789",
                "size": 1000,
                "type": "image/png",
                "uploaded": 1700000000
            }
        ]"#;

        let result = parse_manifest(json);
        assert!(result.is_ok(), "Should parse single descriptor");
        let descriptors = result.unwrap();
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptors[0].url, "https://example.com/blob.png");
        assert_eq!(descriptors[0].sha256, "abc123def456789");
        assert_eq!(descriptors[0].size, 1000);
        assert_eq!(descriptors[0].mime_type, Some("image/png".to_string()));
        assert_eq!(descriptors[0].uploaded, 1700000000);
    }

    /// Scenario 4: Happy path - parse_manifest descriptor with missing `type` field → returns 1 item, mime_type=None
    #[test]
    fn test_parse_manifest_missing_type_field() {
        let json = r#"[
            {
                "url": "https://example.com/blob.dat",
                "sha256": "abc123def456789",
                "size": 2000,
                "uploaded": 1700000000
            }
        ]"#;

        let result = parse_manifest(json);
        assert!(result.is_ok(), "Should parse descriptor without type field");
        let descriptors = result.unwrap();
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptors[0].url, "https://example.com/blob.dat");
        assert_eq!(descriptors[0].sha256, "abc123def456789");
        assert_eq!(descriptors[0].size, 2000);
        assert_eq!(descriptors[0].mime_type, None);
        assert_eq!(descriptors[0].uploaded, 1700000000);
    }

    /// Scenario 5: Edge - parse_manifest with malformed JSON → returns Err(Parse)
    #[test]
    fn test_parse_manifest_malformed_json() {
        let json = r#"[
            {
                "url": "https://example.com/blob.png",
                "sha256": "abc123",
                "size": 1000,
                "type": "image/png",
                "uploaded": 1700000000
            ,
        ]"#;

        let result = parse_manifest(json);
        assert!(result.is_err(), "Should fail with malformed JSON");
        match result {
            Err(ManifestError::Parse(_)) => (),
            _ => panic!("Should return ManifestError::Parse"),
        }
    }

    /// Scenario 6: Edge - parse_manifest with JSON object instead of array → returns Err(Parse)
    #[test]
    fn test_parse_manifest_object_not_array() {
        let json = r#"{
            "url": "https://example.com/blob.png",
            "sha256": "abc123",
            "size": 1000,
            "type": "image/png",
            "uploaded": 1700000000
        }"#;

        let result = parse_manifest(json);
        assert!(result.is_err(), "Should fail with object instead of array");
        match result {
            Err(ManifestError::Parse(_)) => (),
            _ => panic!("Should return ManifestError::Parse"),
        }
    }

    /// Scenario 7: Edge - parse_manifest empty string → returns Err(Parse)
    #[test]
    fn test_parse_manifest_empty_string() {
        let json = "";

        let result = parse_manifest(json);
        assert!(result.is_err(), "Should fail with empty string");
        match result {
            Err(ManifestError::Parse(_)) => (),
            _ => panic!("Should return ManifestError::Parse"),
        }
    }

    /// Scenario 8: Edge - parse_manifest with extra unknown fields → succeeds (serde ignores unknown by default)
    #[test]
    fn test_parse_manifest_extra_unknown_fields() {
        let json = r#"[
            {
                "url": "https://example.com/blob.png",
                "sha256": "abc123",
                "size": 1000,
                "type": "image/png",
                "uploaded": 1700000000,
                "extra_field": "should be ignored",
                "another_unknown": 12345,
                "metadata": {"key": "value"}
            }
        ]"#;

        let result = parse_manifest(json);
        assert!(result.is_ok(), "Should parse despite unknown fields");
        let descriptors = result.unwrap();
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptors[0].url, "https://example.com/blob.png");
        assert_eq!(descriptors[0].sha256, "abc123");
        assert_eq!(descriptors[0].size, 1000);
    }

    /// Scenario 9: Happy path - load_manifest reads examples/manifest.json → returns 4 descriptors
    #[test]
    fn test_load_manifest_from_examples() {
        let path = PathBuf::from("examples/manifest.json");

        let result = load_manifest(&path);
        assert!(result.is_ok(), "Should load examples/manifest.json");
        let descriptors = result.unwrap();
        assert_eq!(descriptors.len(), 4);
    }

    /// Scenario 10: Edge - load_manifest nonexistent file → returns Err(Io)
    #[test]
    fn test_load_manifest_nonexistent_file() {
        let path = PathBuf::from("examples/nonexistent.json");

        let result = load_manifest(&path);
        assert!(result.is_err(), "Should fail with nonexistent file");
        match result {
            Err(ManifestError::Io(_)) => (),
            _ => panic!("Should return ManifestError::Io"),
        }
    }

    /// Scenario 11: Happy path - loaded descriptors have correct sha256 values
    #[test]
    fn test_loaded_descriptors_have_correct_sha256() {
        let path = PathBuf::from("examples/manifest.json");

        let result = load_manifest(&path);
        assert!(result.is_ok());
        let descriptors = result.unwrap();

        assert_eq!(
            descriptors[0].sha256,
            "a9f5c7af6fe6b706d1b7604beb47a62c55968406e27aacbb7fe65e3aa67fcc56"
        );
        assert_eq!(
            descriptors[1].sha256,
            "99896c743d6e5236dc581141ce5f57501b483100af4f60e203df4de1408b5018"
        );
        assert_eq!(
            descriptors[2].sha256,
            "156920e70a9dc50fe5f6982ad7153e8d09308b59cdcb8857be6356b5b814f798"
        );
        assert_eq!(
            descriptors[3].sha256,
            "d217c7dc22c8adf3d4287c0b668a31909dbf607a3c10ded0bb44f99ecb35f02c"
        );
    }

    /// Scenario 12: Happy path - loaded descriptors have correct size values
    #[test]
    fn test_loaded_descriptors_have_correct_size() {
        let path = PathBuf::from("examples/manifest.json");

        let result = load_manifest(&path);
        assert!(result.is_ok());
        let descriptors = result.unwrap();

        assert_eq!(descriptors[0].size, 512000);
        assert_eq!(descriptors[1].size, 5242880);
        assert_eq!(descriptors[2].size, 204800);
        assert_eq!(descriptors[3].size, 1024);
    }
}
