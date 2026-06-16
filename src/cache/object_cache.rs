//! Content-addressed object cache.
//!
//! Layout: `<cache-dir>/objects/<aa>/<bb>/<sha256>`
//!
//! Flow:
//! 1. Check cache hit → serve from disk
//! 2. Cache miss → stream HTTP response to temp file while computing SHA-256
//! 3. Hash match → atomic rename to final path
//! 4. Hash mismatch → delete temp, return error (never cache bad data)
//! 5. Subsequent reads → cache hit

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CacheError {
    #[error("invalid sha256: {0}")]
    InvalidSha256(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Compute the cache path for a SHA-256 hash.
/// Layout: <base>/objects/<first2>/<next2>/<full_sha256>
/// Example: cache_path("/cache", "abcdef1234...64chars") -> "/cache/objects/ab/cd/abcdef1234...64chars"
pub fn cache_path(base: &Path, sha256: &str) -> Result<PathBuf, CacheError> {
    let normalized =
        crate::util::path::sanitize_sha256(sha256).map_err(CacheError::InvalidSha256)?;

    let first2 = &normalized[0..2];
    let next2 = &normalized[2..4];

    let path = base
        .join("objects")
        .join(first2)
        .join(next2)
        .join(&normalized);

    Ok(path)
}

/// Compute the parent directory of a cache entry (for mkdir -p)
pub fn cache_dir(base: &Path, sha256: &str) -> Result<PathBuf, CacheError> {
    let normalized =
        crate::util::path::sanitize_sha256(sha256).map_err(CacheError::InvalidSha256)?;

    let first2 = &normalized[0..2];
    let next2 = &normalized[2..4];

    let dir = base.join("objects").join(first2).join(next2);

    Ok(dir)
}

/// Check if a cached object exists
pub fn cache_exists(base: &Path, sha256: &str) -> bool {
    match cache_path(base, sha256) {
        Ok(path) => path.exists(),
        Err(_) => false,
    }
}

/// Read a cached object's content as bytes (for small files in tests)
pub fn read_cached(base: &Path, sha256: &str) -> Result<Vec<u8>, CacheError> {
    let path = cache_path(base, sha256)?;
    let content = fs::read(&path)?;
    Ok(content)
}

/// Ensure the cache directory structure exists for a given sha256.
/// Creates <base>/objects/<aa>/<bb>/ if needed.
pub fn ensure_cache_dir(base: &Path, sha256: &str) -> Result<PathBuf, CacheError> {
    let dir = cache_dir(base, sha256)?;
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    // Valid 64-char hex hash for tests
    const VALID_HASH: &str = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";

    #[test]
    fn test_cache_path_valid() {
        let base = Path::new("/cache");
        let result = cache_path(base, VALID_HASH);
        assert!(result.is_ok());
        let path = result.unwrap();
        assert_eq!(
            path,
            PathBuf::from(
                "/cache/objects/ab/cd/abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"
            )
        );
    }

    #[test]
    fn test_cache_dir_returns_parent() {
        let base = Path::new("/cache");
        let result = cache_dir(base, VALID_HASH);
        assert!(result.is_ok());
        let dir = result.unwrap();
        assert_eq!(dir, PathBuf::from("/cache/objects/ab/cd"));
    }

    #[test]
    fn test_cache_path_short_hash() {
        let base = Path::new("/cache");
        let result = cache_path(base, "short");
        assert!(result.is_err());
        match result.unwrap_err() {
            CacheError::InvalidSha256(_) => (),
            _ => panic!("Expected InvalidSha256 error"),
        }
    }

    #[test]
    fn test_cache_path_non_hex_hash() {
        let base = Path::new("/cache");
        let result = cache_path(
            base,
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        );
        assert!(result.is_err());
        match result.unwrap_err() {
            CacheError::InvalidSha256(_) => (),
            _ => panic!("Expected InvalidSha256 error"),
        }
    }

    #[test]
    fn test_cache_exists_false() {
        let base = Path::new("/cache");
        assert!(!cache_exists(base, VALID_HASH));
    }

    #[test]
    fn test_ensure_cache_dir_creates_dirs() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base = temp_dir.path();

        let result = ensure_cache_dir(base, VALID_HASH);
        assert!(result.is_ok());
        let created_dir = result.unwrap();

        assert!(created_dir.exists());
        assert!(created_dir.is_dir());
        assert_eq!(created_dir, base.join("objects").join("ab").join("cd"));
    }

    #[test]
    fn test_cache_exists_true_after_write() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base = temp_dir.path();

        ensure_cache_dir(base, VALID_HASH).unwrap();
        let cache_path = cache_path(base, VALID_HASH).unwrap();
        fs::write(&cache_path, b"test content").unwrap();

        assert!(cache_exists(base, VALID_HASH));
    }

    #[test]
    fn test_read_cached() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base = temp_dir.path();

        ensure_cache_dir(base, VALID_HASH).unwrap();
        let cache_path = cache_path(base, VALID_HASH).unwrap();
        let content = b"test content bytes";
        fs::write(&cache_path, content).unwrap();

        let result = read_cached(base, VALID_HASH);
        assert!(result.is_ok());
        let read_content = result.unwrap();
        assert_eq!(read_content, content.to_vec());
    }

    #[test]
    fn test_cache_path_deterministic() {
        let base = Path::new("/cache");
        let result1 = cache_path(base, VALID_HASH).unwrap();
        let result2 = cache_path(base, VALID_HASH).unwrap();
        assert_eq!(result1, result2);
    }

    #[test]
    fn test_cache_path_uppercase_normalization() {
        let base = Path::new("/cache");
        let uppercase_hash = "ABCDEF1234567890ABCDEF1234567890ABCDEF1234567890ABCDEF1234567890";
        let result = cache_path(base, uppercase_hash);
        assert!(result.is_ok());
        let path = result.unwrap();
        assert_eq!(
            path,
            PathBuf::from(
                "/cache/objects/ab/cd/abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"
            )
        );
    }

    #[test]
    fn test_cache_path_empty_string() {
        let base = Path::new("/cache");
        let result = cache_path(base, "");
        assert!(result.is_err());
        match result.unwrap_err() {
            CacheError::InvalidSha256(_) => (),
            _ => panic!("Expected InvalidSha256 error"),
        }
    }

    #[test]
    fn test_cache_path_no_path_escape() {
        let base = Path::new("/cache");
        let result = cache_path(base, VALID_HASH);
        assert!(result.is_ok());
        let path = result.unwrap();
        let path_str = path.to_str().unwrap();
        assert!(!path_str.contains(".."));
    }
}
