//! BUD-02 Blob Descriptor types.
//!
//! See: https://github.com/hzrd149/blossom/blob/master/buds/02.md
//!
//! A blob descriptor describes a single Blossom blob with:
//! - `url`: public GET endpoint URL
//! - `sha256`: content hash (64 lowercase hex chars)
//! - `size`: size in bytes
//! - `type`: MIME type (defaults to application/octet-stream)
//! - `uploaded`: Unix timestamp of upload

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MAX_BLOB_SIZE: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Error, Debug)]
pub enum DescriptorError {
    #[error("invalid URL scheme: must be http or https, got {0}")]
    InvalidUrlScheme(String),
    #[error("blob size {size} exceeds max {MAX_BLOB_SIZE}")]
    SizeTooLarge { size: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct BlobDescriptor {
    pub url: String,
    pub sha256: String,
    pub size: u64,
    #[serde(rename = "type")]
    pub mime_type: Option<String>,
    pub uploaded: u64,
    #[serde(default)]
    pub expiration: Option<u64>,
}

impl BlobDescriptor {
    #[allow(dead_code)]
    pub fn effective_mime_type(&self) -> &str {
        self.mime_type
            .as_deref()
            .unwrap_or("application/octet-stream")
    }

    #[allow(dead_code)]
    pub fn sha256_lower(&self) -> String {
        self.sha256.to_lowercase()
    }

    pub fn validate(&self) -> Result<(), DescriptorError> {
        if !self.url.starts_with("http://") && !self.url.starts_with("https://") {
            return Err(DescriptorError::InvalidUrlScheme(self.url.clone()));
        }
        if self.size > MAX_BLOB_SIZE {
            return Err(DescriptorError::SizeTooLarge { size: self.size });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    /// Scenario 1: Happy path - parse full descriptor with all 5 fields
    #[test]
    fn test_parse_full_descriptor() {
        let json = r#"{
            "url": "https://x/y",
            "sha256": "abc123",
            "size": 100,
            "type": "image/png",
            "uploaded": 1700000000
        }"#;

        let desc: BlobDescriptor =
            serde_json::from_str(json).expect("Should parse full descriptor");

        assert_eq!(desc.url, "https://x/y");
        assert_eq!(desc.sha256, "abc123");
        assert_eq!(desc.size, 100);
        assert_eq!(desc.mime_type, Some("image/png".to_string()));
        assert_eq!(desc.uploaded, 1700000000);
    }

    /// Scenario 2: Edge - missing "type" field
    #[test]
    fn test_missing_type_field() {
        let json = r#"{
            "url": "https://x/y",
            "sha256": "abc123",
            "size": 100,
            "uploaded": 1700000000
        }"#;

        let desc: BlobDescriptor =
            serde_json::from_str(json).expect("Should parse without type field");

        assert_eq!(desc.mime_type, None);
    }

    /// Scenario 3: Edge - missing "uploaded" field
    #[test]
    fn test_missing_uploaded_field() {
        let json = r#"{
            "url": "https://x/y",
            "sha256": "abc123",
            "size": 100,
            "type": "image/png"
        }"#;

        let result = serde_json::from_str::<BlobDescriptor>(json);
        assert!(result.is_err(), "Should fail when 'uploaded' is missing");
    }

    /// Scenario 4: Edge - missing "url" field
    #[test]
    fn test_missing_url_field() {
        let json = r#"{
            "sha256": "abc123",
            "size": 100,
            "type": "image/png",
            "uploaded": 1700000000
        }"#;

        let result = serde_json::from_str::<BlobDescriptor>(json);
        assert!(result.is_err(), "Should fail when 'url' is missing");
    }

    /// Scenario 5: Edge - missing "sha256" field
    #[test]
    fn test_missing_sha256_field() {
        let json = r#"{
            "url": "https://x/y",
            "size": 100,
            "type": "image/png",
            "uploaded": 1700000000
        }"#;

        let result = serde_json::from_str::<BlobDescriptor>(json);
        assert!(result.is_err(), "Should fail when 'sha256' is missing");
    }

    /// Scenario 6: Edge - missing "size" field
    #[test]
    fn test_missing_size_field() {
        let json = r#"{
            "url": "https://x/y",
            "sha256": "abc123",
            "type": "image/png",
            "uploaded": 1700000000
        }"#;

        let result = serde_json::from_str::<BlobDescriptor>(json);
        assert!(result.is_err(), "Should fail when 'size' is missing");
    }

    /// Scenario 7: Edge - extra unknown fields should be ignored
    #[test]
    fn test_extra_unknown_fields() {
        let json = r#"{
            "url": "https://x/y",
            "sha256": "abc123",
            "size": 1,
            "type": "image/png",
            "uploaded": 1,
            "extra": "ignored",
            "another": 123
        }"#;

        let desc: BlobDescriptor =
            serde_json::from_str(json).expect("Should ignore unknown fields");

        assert_eq!(desc.url, "https://x/y");
        assert_eq!(desc.sha256, "abc123");
        assert_eq!(desc.size, 1);
        assert_eq!(desc.mime_type, Some("image/png".to_string()));
        assert_eq!(desc.uploaded, 1);
    }

    /// Scenario 8: Serialize → Deserialize roundtrip
    #[test]
    fn test_serialize_deserialize_roundtrip() {
        let original = BlobDescriptor {
            url: "https://example.com/blob".to_string(),
            sha256: "def456".to_string(),
            size: 200,
            mime_type: Some("application/json".to_string()),
            uploaded: 1700000500,
            expiration: None,
        };

        let json = serde_json::to_string(&original).expect("Should serialize");
        let restored: BlobDescriptor = serde_json::from_str(&json).expect("Should deserialize");

        assert_eq!(restored.url, original.url);
        assert_eq!(restored.sha256, original.sha256);
        assert_eq!(restored.size, original.size);
        assert_eq!(restored.mime_type, original.mime_type);
        assert_eq!(restored.uploaded, original.uploaded);
    }

    /// Scenario 9: effective_mime_type() returns mime_type when Some
    #[test]
    fn test_effective_mime_type_with_value() {
        let desc = BlobDescriptor {
            url: "https://x/y".to_string(),
            sha256: "abc123".to_string(),
            size: 100,
            mime_type: Some("image/png".to_string()),
            uploaded: 1700000000,
            expiration: None,
        };

        assert_eq!(desc.effective_mime_type(), "image/png");
    }

    /// Scenario 10: effective_mime_type() returns default when None
    #[test]
    fn test_effective_mime_type_default() {
        let desc = BlobDescriptor {
            url: "https://x/y".to_string(),
            sha256: "abc123".to_string(),
            size: 100,
            mime_type: None,
            uploaded: 1700000000,
            expiration: None,
        };

        assert_eq!(desc.effective_mime_type(), "application/octet-stream");
    }

    /// Scenario 11: sha256_lower() normalizes uppercase to lowercase
    #[test]
    fn test_sha256_lower() {
        let desc = BlobDescriptor {
            url: "https://x/y".to_string(),
            sha256: "ABC123DEF456".to_string(),
            size: 100,
            mime_type: Some("image/png".to_string()),
            uploaded: 1700000000,
            expiration: None,
        };

        assert_eq!(desc.sha256_lower(), "abc123def456");
    }

    #[test]
    fn test_validate_http_url_passes() {
        let desc = BlobDescriptor {
            url: "http://example.com/blob".to_string(),
            sha256: "abc123".to_string(),
            size: 100,
            mime_type: None,
            uploaded: 1,
            expiration: None,
        };
        assert!(desc.validate().is_ok());
    }

    #[test]
    fn test_validate_https_url_passes() {
        let desc = BlobDescriptor {
            url: "https://example.com/blob".to_string(),
            sha256: "abc123".to_string(),
            size: 100,
            mime_type: None,
            uploaded: 1,
            expiration: None,
        };
        assert!(desc.validate().is_ok());
    }

    #[test]
    fn test_validate_ftp_url_rejected() {
        let desc = BlobDescriptor {
            url: "ftp://evil.com/blob".to_string(),
            sha256: "abc123".to_string(),
            size: 100,
            mime_type: None,
            uploaded: 1,
            expiration: None,
        };
        assert!(matches!(
            desc.validate(),
            Err(DescriptorError::InvalidUrlScheme(_))
        ));
    }

    #[test]
    fn test_validate_file_url_rejected() {
        let desc = BlobDescriptor {
            url: "file:///etc/passwd".to_string(),
            sha256: "abc123".to_string(),
            size: 100,
            mime_type: None,
            uploaded: 1,
            expiration: None,
        };
        assert!(matches!(
            desc.validate(),
            Err(DescriptorError::InvalidUrlScheme(_))
        ));
    }

    #[test]
    fn test_validate_size_at_limit_passes() {
        let desc = BlobDescriptor {
            url: "https://x/y".to_string(),
            sha256: "abc123".to_string(),
            size: MAX_BLOB_SIZE,
            mime_type: None,
            uploaded: 1,
            expiration: None,
        };
        assert!(desc.validate().is_ok());
    }

    #[test]
    fn test_validate_size_over_limit_rejected() {
        let desc = BlobDescriptor {
            url: "https://x/y".to_string(),
            sha256: "abc123".to_string(),
            size: MAX_BLOB_SIZE + 1,
            mime_type: None,
            uploaded: 1,
            expiration: None,
        };
        assert!(matches!(
            desc.validate(),
            Err(DescriptorError::SizeTooLarge { .. })
        ));
    }
}
