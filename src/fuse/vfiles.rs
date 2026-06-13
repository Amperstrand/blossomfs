//! Virtual file content generators.
//!
//! Generates content for pseudo-files that exist in the FUSE mount
//! but have no corresponding Blossom blob:
//! - `README.txt`: Static description of the mount
//! - `STATUS.txt`: Live status (server count, blob count, cache info)

#![allow(dead_code)]

use std::time::{SystemTime, UNIX_EPOCH};

/// Information used to generate virtual file content.
pub struct MountInfo {
    /// The mountpoint path
    pub mountpoint: String,
    /// The npub (bech32) being served, or "all" for multi-pubkey
    pub npub: String,
    /// Number of blossom servers configured
    pub server_count: usize,
    /// Total blob count across all servers
    pub blob_count: usize,
    /// Cache directory path
    pub cache_dir: String,
}

/// Generate README.txt content.
/// This is a static informational file explaining what the mount is,
/// how to navigate it, and key concepts (hash-first layout, views).
pub fn generate_readme(info: &MountInfo) -> Vec<u8> {
    let content = format!(
        "BlossomFS — Read-only Blossom/Nostr Media Mount
=============================================

Mountpoint: {}
Pubkey (npub): {}

Directory Structure
-------------------
This mount provides a hash-first view of Blossom media with three orthogonal
navigation views:

by-sha256/     Browse files by their SHA-256 hash
by-type/       Browse files by MIME type (e.g., image/png, video/mp4)
by-date/       Browse files by upload date (YYYY/MM/DD)

Key Concepts
------------
• Files are content-addressed by SHA-256 hash
• Each file's filename is the SHA-256 hash with an optional extension
• The same blob from different servers or dates maps to the same filename
• This is a read-only filesystem — all write operations are rejected

Mounting
--------
You are currently browsing the Blossom content for pubkey: {}
",
        info.mountpoint, info.npub, info.npub
    );

    content.into_bytes()
}

/// Generate STATUS.txt content.
/// Contains live status information: server count, blob count,
/// npub being served, cache directory path, and a timestamp.
pub fn generate_status(info: &MountInfo) -> Vec<u8> {
    // Get current timestamp
    let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs(),
        Err(_) => 0,
    };

    let content = format!(
        "BlossomFS Status
===============

Servers configured: {}
Total blobs: {}
Pubkey: {}
Cache directory: {}
Last updated: {}

Note: This file is regenerated on each read
",
        info.server_count, info.blob_count, info.npub, info.cache_dir, now
    );

    content.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_mountinfo() -> MountInfo {
        MountInfo {
            mountpoint: "/mnt/blossomfs".to_string(),
            npub: "npub1testnpub123456789".to_string(),
            server_count: 3,
            blob_count: 42,
            cache_dir: "/tmp/blossomfs_cache".to_string(),
        }
    }

    // S1: generate_readme returns non-empty Vec<u8>
    #[test]
    fn test_generate_readme_non_empty() {
        let info = create_test_mountinfo();
        let content = generate_readme(&info);
        assert!(!content.is_empty(), "README content should not be empty");
    }

    // S2: generate_readme contains "BlossomFS"
    #[test]
    fn test_generate_readme_contains_blossomfs() {
        let info = create_test_mountinfo();
        let content = generate_readme(&info);
        let text = String::from_utf8_lossy(&content);
        assert!(
            text.contains("BlossomFS"),
            "README should contain 'BlossomFS'"
        );
    }

    // S3: generate_readme contains mountpoint path
    #[test]
    fn test_generate_readme_contains_mountpoint() {
        let info = create_test_mountinfo();
        let content = generate_readme(&info);
        let text = String::from_utf8_lossy(&content);
        assert!(
            text.contains(&info.mountpoint),
            "README should contain mountpoint path '{}'",
            info.mountpoint
        );
    }

    // S4: generate_readme contains "by-sha256"
    #[test]
    fn test_generate_readme_contains_by_sha256() {
        let info = create_test_mountinfo();
        let content = generate_readme(&info);
        let text = String::from_utf8_lossy(&content);
        assert!(
            text.contains("by-sha256"),
            "README should mention 'by-sha256' directory structure"
        );
    }

    // S5: generate_readme contains "read-only"
    #[test]
    fn test_generate_readme_contains_read_only() {
        let info = create_test_mountinfo();
        let content = generate_readme(&info);
        let text = String::from_utf8_lossy(&content);
        assert!(
            text.contains("read-only"),
            "README should mention read-only nature"
        );
    }

    // S6: generate_status returns non-empty Vec<u8>
    #[test]
    fn test_generate_status_non_empty() {
        let info = create_test_mountinfo();
        let content = generate_status(&info);
        assert!(!content.is_empty(), "STATUS content should not be empty");
    }

    // S7: generate_status contains the npub
    #[test]
    fn test_generate_status_contains_npub() {
        let info = create_test_mountinfo();
        let content = generate_status(&info);
        let text = String::from_utf8_lossy(&content);
        assert!(
            text.contains(&info.npub),
            "STATUS should contain npub '{}'",
            info.npub
        );
    }

    // S8: generate_status contains server count
    #[test]
    fn test_generate_status_contains_server_count() {
        let info = create_test_mountinfo();
        let content = generate_status(&info);
        let text = String::from_utf8_lossy(&content);
        assert!(
            text.contains(&info.server_count.to_string()),
            "STATUS should contain server count '{}'",
            info.server_count
        );
    }

    // S9: generate_status contains blob count
    #[test]
    fn test_generate_status_contains_blob_count() {
        let info = create_test_mountinfo();
        let content = generate_status(&info);
        let text = String::from_utf8_lossy(&content);
        assert!(
            text.contains(&info.blob_count.to_string()),
            "STATUS should contain blob count '{}'",
            info.blob_count
        );
    }

    // S10: generate_status contains cache_dir path
    #[test]
    fn test_generate_status_contains_cache_dir() {
        let info = create_test_mountinfo();
        let content = generate_status(&info);
        let text = String::from_utf8_lossy(&content);
        assert!(
            text.contains(&info.cache_dir),
            "STATUS should contain cache directory '{}'",
            info.cache_dir
        );
    }

    // S11: generate_status content changes when MountInfo changes
    #[test]
    fn test_generate_status_changes_with_blob_count() {
        let info1 = MountInfo {
            mountpoint: "/mnt/blossomfs".to_string(),
            npub: "npub1testnpub123456789".to_string(),
            server_count: 3,
            blob_count: 42,
            cache_dir: "/tmp/blossomfs_cache".to_string(),
        };

        let info2 = MountInfo {
            mountpoint: "/mnt/blossomfs".to_string(),
            npub: "npub1testnpub123456789".to_string(),
            server_count: 3,
            blob_count: 99, // Different blob count
            cache_dir: "/tmp/blossomfs_cache".to_string(),
        };

        let content1 = generate_status(&info1);
        let content2 = generate_status(&info2);

        // Note: The timestamp will always be different, but we also check blob count
        assert_ne!(
            content1, content2,
            "STATUS content should change when blob_count changes"
        );
    }

    // S12: generate_readme with zero servers
    #[test]
    fn test_generate_readme_zero_servers() {
        let info = MountInfo {
            mountpoint: "/mnt/blossomfs".to_string(),
            npub: "npub1testnpub123456789".to_string(),
            server_count: 0,
            blob_count: 42,
            cache_dir: "/tmp/blossomfs_cache".to_string(),
        };

        let content = generate_readme(&info);
        assert!(!content.is_empty(), "README should work with zero servers");

        let text = String::from_utf8_lossy(&content);
        assert!(
            text.contains("BlossomFS"),
            "Should still contain 'BlossomFS'"
        );
    }

    // S13: generate_status with zero blobs
    #[test]
    fn test_generate_status_zero_blobs() {
        let info = MountInfo {
            mountpoint: "/mnt/blossomfs".to_string(),
            npub: "npub1testnpub123456789".to_string(),
            server_count: 3,
            blob_count: 0,
            cache_dir: "/tmp/blossomfs_cache".to_string(),
        };

        let content = generate_status(&info);
        assert!(!content.is_empty(), "STATUS should work with zero blobs");

        let text = String::from_utf8_lossy(&content);
        assert!(text.contains("0"), "Should contain '0' for blob count");
    }
}
