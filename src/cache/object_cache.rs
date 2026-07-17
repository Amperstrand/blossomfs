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

/// Evict oldest cached blobs (FIFO by modification time) until total cache
/// size is at or below `max_bytes`. The file named `keep` is never evicted.
/// Pass `max_bytes = 0` to disable eviction (no-op).
pub fn evict_oldest(cache_base: &Path, max_bytes: u64, keep: &str) -> Result<(), CacheError> {
    if max_bytes == 0 {
        return Ok(());
    }

    let objects_dir = cache_base.join("objects");
    if !objects_dir.exists() {
        return Ok(());
    }

    let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total_size: u64 = 0;

    for entry in walkdir::WalkDir::new(&objects_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = metadata
            .modified()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let size = metadata.len();
        total_size += size;
        entries.push((entry.path().to_path_buf(), size, mtime));
    }

    if total_size <= max_bytes {
        return Ok(());
    }

    entries.sort_by_key(|(_, _, mtime)| *mtime);

    for (path, size, _) in &entries {
        if total_size <= max_bytes {
            break;
        }

        if path.file_name().map(|n| n.to_string_lossy().into_owned()) == Some(keep.to_string()) {
            continue;
        }

        match fs::remove_file(path) {
            Ok(()) => {
                total_size -= size;
                tracing::debug!(
                    "evicted {:?} ({} bytes, {} remaining)",
                    path,
                    size,
                    total_size
                );
            }
            Err(e) => {
                tracing::warn!("failed to evict {:?}: {}", path, e);
            }
        }
    }

    Ok(())
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

    // --- evict_oldest tests ---

    use std::fs::FileTimes;
    use std::time::{Duration, SystemTime};

    fn write_cache_file(base: &Path, sha: &str, content: &[u8], mtime_offset_secs: u64) {
        ensure_cache_dir(base, sha).unwrap();
        let path = cache_path(base, sha).unwrap();
        fs::write(&path, content).unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let times = FileTimes::new()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1000 + mtime_offset_secs));
        file.set_times(times).unwrap();
    }

    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const SHA_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    #[test]
    fn test_evict_under_limit_no_eviction() {
        let dir = tempfile::tempdir().unwrap();
        write_cache_file(dir.path(), SHA_A, b"AAA", 0);

        evict_oldest(dir.path(), 1024, "none").unwrap();

        assert!(cache_exists(dir.path(), SHA_A));
    }

    #[test]
    fn test_evict_over_limit_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        write_cache_file(dir.path(), SHA_A, b"AAAA", 0);
        write_cache_file(dir.path(), SHA_B, b"BBBB", 10);
        write_cache_file(dir.path(), SHA_C, b"CCCC", 20);

        // Total is 12 bytes, limit to 8 → evict SHA_A (oldest, 4 bytes)
        evict_oldest(dir.path(), 8, "none").unwrap();

        assert!(!cache_exists(dir.path(), SHA_A));
        assert!(cache_exists(dir.path(), SHA_B));
        assert!(cache_exists(dir.path(), SHA_C));
    }

    #[test]
    fn test_evict_keep_file_never_evicted() {
        let dir = tempfile::tempdir().unwrap();
        write_cache_file(dir.path(), SHA_A, b"AAAA", 0);
        write_cache_file(dir.path(), SHA_B, b"BBBB", 10);

        // Limit to 4 bytes, keep SHA_A → should evict SHA_B instead
        evict_oldest(dir.path(), 4, SHA_A).unwrap();

        assert!(cache_exists(dir.path(), SHA_A));
        assert!(!cache_exists(dir.path(), SHA_B));
    }

    #[test]
    fn test_evict_zero_max_bytes_noop() {
        let dir = tempfile::tempdir().unwrap();
        write_cache_file(dir.path(), SHA_A, b"AAAA", 0);

        evict_oldest(dir.path(), 0, "none").unwrap();

        assert!(cache_exists(dir.path(), SHA_A));
    }

    #[test]
    fn test_evict_empty_cache_no_error() {
        let dir = tempfile::tempdir().unwrap();

        evict_oldest(dir.path(), 1024, "none").unwrap();
    }

    #[test]
    fn test_concurrent_eviction_no_panic() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();

        for i in 0..10 {
            let sha = format!("{i:064x}");
            write_cache_file(&base, &sha, &[0xAB; 1024], i as u64);
        }

        let keep_sha = format!("{:064x}", 0xCAFE_u128);
        write_cache_file(&base, &keep_sha, b"KEEP_ME", 99);

        let base_arc = std::sync::Arc::new(base);
        let keep_arc = std::sync::Arc::new(keep_sha.clone());

        let mut handles = Vec::new();
        for t in 0..4u8 {
            let base = base_arc.clone();
            let keep = keep_arc.clone();
            handles.push(std::thread::spawn(move || {
                for j in 0..5u8 {
                    let sha = format!("{t:032x}{j:032x}");
                    write_cache_file(&base, &sha, &[0xCD; 512], 100 + t as u64 * 10 + j as u64);
                    evict_oldest(&base, 4096, &keep).unwrap();
                }
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }

        assert!(cache_exists(&base_arc, &keep_sha));
    }

    #[test]
    fn test_concurrent_eviction_respects_limit() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();

        for i in 0..20 {
            let sha = format!("{i:064x}");
            write_cache_file(&base, &sha, &[0xAA; 2048], i as u64);
        }

        let base_arc = std::sync::Arc::new(base);
        let max_bytes = 8192u64;

        let mut handles = Vec::new();
        for _ in 0..4 {
            let base = base_arc.clone();
            handles.push(std::thread::spawn(move || {
                evict_oldest(&base, max_bytes, "nonexistent").unwrap();
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }

        let objects_dir = base_arc.join("objects");
        let mut total: u64 = 0;
        for entry in walkdir::WalkDir::new(&objects_dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
        assert!(
            total <= max_bytes + 2048,
            "cache size {total} exceeds limit {max_bytes} + tolerance"
        );
    }
}
