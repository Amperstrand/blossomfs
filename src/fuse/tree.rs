//! Virtual directory tree model.
//!
//! Represents the filesystem tree projected from Blossom blob descriptors.
//! The tree is built once at mount time and is immutable during the session.
//!
//! Layout:
//! ```text
//! /
//!   README.txt
//!   STATUS.txt
//!   public/
//!     <npub>/
//!       servers/
//!         <host>/
//!           by-sha256/<sha256>[.<ext>]
//!           by-type/<mime>/<sha256>[.<ext>]
//!           by-date/YYYY/MM/DD/<sha256>[.<ext>]
//!       all-servers/
//!         by-sha256/<sha256>[.<ext>]
//! ```

#![allow(dead_code)]

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::blossom::descriptor::BlobDescriptor;
use crate::util::mime::extension_for_descriptor;
use crate::util::path::{sanitize_mime_for_path, sanitize_path_component, sanitize_sha256};

/// Kind of node in the virtual tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Directory,
    File,
}

/// What backs a file's content.
#[derive(Debug, Clone)]
pub enum FileContent {
    /// Inline static content (e.g. README text).
    Static(Vec<u8>),
    /// Content fetched from a remote blob server on demand.
    Remote {
        url: String,
        sha256: String,
        mime_type: Option<String>,
        expires: Option<u64>,
    },
    /// Content served from a local file on disk (e.g. cloned git repo files).
    Local { path: PathBuf },
}

/// Information needed to lazily populate a directory on first access.
#[derive(Debug, Clone)]
pub enum LazyDir {
    /// Clone a git repository and populate the directory with its file tree.
    GitRepo {
        clone_url: String,
        cache_path: PathBuf,
    },
}

/// A node in the virtual filesystem tree.
#[derive(Debug, Clone)]
pub enum TreeNode {
    Directory {
        ino: u64,
        name: String,
        children: Vec<u64>,
        parent: u64,
        lazy: Option<LazyDir>,
    },
    File {
        ino: u64,
        name: String,
        parent: u64,
        size: u64,
        content: FileContent,
        uploaded: u64,
    },
}

/// The virtual filesystem tree.
///
/// Stores all nodes in a `Vec` indexed by inode (1-based). Root inode is
/// always 1. The tree is built once at mount time and not mutated during
/// the session (except during initial construction).
#[derive(Debug)]
pub struct Tree {
    nodes: Vec<TreeNode>,
    root: u64,
}

impl Tree {
    /// Create a new tree containing only the root directory (inode 1).
    pub fn new() -> Self {
        Tree {
            nodes: vec![TreeNode::Directory {
                ino: 1,
                name: String::from("/"),
                children: Vec::new(),
                parent: 0,
                lazy: None,
            }],
            root: 1,
        }
    }

    /// Returns the root inode (always 1).
    pub fn root(&self) -> u64 {
        self.root
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn get(&self, ino: u64) -> Option<&TreeNode> {
        if ino >= 1 {
            self.nodes.get((ino - 1) as usize)
        } else {
            None
        }
    }

    /// Get a mutable node reference by inode number.
    pub fn get_mut(&mut self, ino: u64) -> Option<&mut TreeNode> {
        if ino >= 1 {
            self.nodes.get_mut((ino - 1) as usize)
        } else {
            None
        }
    }

    /// Find a child inode by name within a directory.
    ///
    /// Returns `None` if `parent` is not a directory or no child with
    /// the given name exists.
    pub fn lookup(&self, parent: u64, name: &str) -> Option<u64> {
        let node = self.get(parent)?;
        let children = match node {
            TreeNode::Directory { children, .. } => children,
            TreeNode::File { .. } => return None,
        };
        for &child_ino in children {
            let child = self.get(child_ino)?;
            let child_name = match child {
                TreeNode::Directory { name, .. } => name.as_str(),
                TreeNode::File { name, .. } => name.as_str(),
            };
            if child_name == name {
                return Some(child_ino);
            }
        }
        None
    }

    /// List entries in a directory.
    ///
    /// Returns `None` if `ino` is not a directory.
    /// Otherwise returns `Some(Vec)` of `(child_ino, name, kind)`.
    pub fn readdir(&self, ino: u64) -> Option<Vec<(u64, String, NodeKind)>> {
        let node = self.get(ino)?;
        let children = match node {
            TreeNode::Directory { children, .. } => children,
            TreeNode::File { .. } => return None,
        };
        let mut result = Vec::with_capacity(children.len());
        for &child_ino in children {
            let child = self.get(child_ino)?;
            let (name, kind) = match child {
                TreeNode::Directory { name, .. } => (name.clone(), NodeKind::Directory),
                TreeNode::File { name, .. } => (name.clone(), NodeKind::File),
            };
            result.push((child_ino, name, kind));
        }
        Some(result)
    }

    /// Get the kind (Directory or File) of a node.
    pub fn kind(&self, ino: u64) -> Option<NodeKind> {
        match self.get(ino)? {
            TreeNode::Directory { .. } => Some(NodeKind::Directory),
            TreeNode::File { .. } => Some(NodeKind::File),
        }
    }

    /// Get the size of a node.
    ///
    /// Returns `Some(0)` for directories, `Some(size)` for files,
    /// or `None` if the inode does not exist.
    pub fn size(&self, ino: u64) -> Option<u64> {
        match self.get(ino)? {
            TreeNode::Directory { .. } => Some(0),
            TreeNode::File { size, .. } => Some(*size),
        }
    }

    pub fn uploaded(&self, ino: u64) -> Option<u64> {
        match self.get(ino)? {
            TreeNode::File { uploaded, .. } => Some(*uploaded),
            TreeNode::Directory { .. } => None,
        }
    }

    pub fn next_inode(&self) -> u64 {
        (self.nodes.len() + 1) as u64
    }

    /// Add a directory under `parent`, returning its new inode.
    pub fn add_directory(&mut self, parent: u64, name: &str) -> u64 {
        let sanitized = sanitize_path_component(name);
        let ino = self.next_inode();
        self.nodes.push(TreeNode::Directory {
            ino,
            name: sanitized,
            children: Vec::new(),
            parent,
            lazy: None,
        });
        if let Some(TreeNode::Directory { children, .. }) = self.get_mut(parent) {
            children.push(ino);
        }
        ino
    }

    /// Add a file with static content under `parent`.
    pub fn add_static_file(&mut self, parent: u64, name: &str, content: Vec<u8>) -> u64 {
        let sanitized = sanitize_path_component(name);
        let size = content.len() as u64;
        let uploaded = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let ino = self.next_inode();
        self.nodes.push(TreeNode::File {
            ino,
            name: sanitized,
            parent,
            size,
            content: FileContent::Static(content),
            uploaded,
        });
        if let Some(TreeNode::Directory { children, .. }) = self.get_mut(parent) {
            children.push(ino);
        }
        ino
    }

    /// Add a file backed by a remote blob.
    #[allow(clippy::too_many_arguments)]
    pub fn add_remote_file(
        &mut self,
        parent: u64,
        name: &str,
        url: String,
        sha256: String,
        size: u64,
        mime_type: Option<String>,
        uploaded: u64,
        expiration: Option<u64>,
    ) -> u64 {
        let sanitized = sanitize_path_component(name);
        let ino = self.next_inode();
        self.nodes.push(TreeNode::File {
            ino,
            name: sanitized,
            parent,
            size,
            content: FileContent::Remote {
                url,
                sha256,
                mime_type,
                expires: expiration,
            },
            uploaded,
        });
        if let Some(TreeNode::Directory { children, .. }) = self.get_mut(parent) {
            children.push(ino);
        }
        ino
    }

    /// Look up a directory by name, creating it if it does not exist.
    pub fn get_or_create_dir(&mut self, parent: u64, name: &str) -> u64 {
        if let Some(child_ino) = self.lookup(parent, name) {
            return child_ino;
        }
        self.add_directory(parent, name)
    }

    /// Add a directory that will be lazily populated on first `readdir`.
    pub fn add_lazy_dir(&mut self, parent: u64, name: &str, lazy: LazyDir) -> u64 {
        let sanitized = sanitize_path_component(name);
        let ino = self.next_inode();
        self.nodes.push(TreeNode::Directory {
            ino,
            name: sanitized,
            children: Vec::new(),
            parent,
            lazy: Some(lazy),
        });
        if let Some(TreeNode::Directory { children, .. }) = self.get_mut(parent) {
            children.push(ino);
        }
        ino
    }

    /// Add a file backed by a local path on disk.
    pub fn add_local_file(&mut self, parent: u64, name: &str, path: PathBuf, size: u64) -> u64 {
        let sanitized = sanitize_path_component(name);
        let uploaded = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let ino = self.next_inode();
        self.nodes.push(TreeNode::File {
            ino,
            name: sanitized,
            parent,
            size,
            content: FileContent::Local { path },
            uploaded,
        });
        if let Some(TreeNode::Directory { children, .. }) = self.get_mut(parent) {
            children.push(ino);
        }
        ino
    }

    /// Take the lazy-population data from a directory, leaving `lazy: None`.
    /// Returns `None` if the directory has already been populated or is not lazy.
    pub fn take_lazy(&mut self, ino: u64) -> Option<LazyDir> {
        if let Some(TreeNode::Directory { lazy, .. }) = self.get_mut(ino) {
            lazy.take()
        } else {
            None
        }
    }

    // ======================== Persistence ========================

    /// Serialize user-created directories and remote files to Nostr tags.
    ///
    /// Skips auto-generated structure (the `public/` subtree, README.txt, STATUS.txt,
    /// git-backed local files). Only user-created directories and user-placed
    /// remote blob files are persisted.
    ///
    /// Format follows Blossom Drive conventions:
    /// - `["folder", "<path>"]` for empty directories
    /// - `["x", "<sha256>", "<path>", "<size>", "<mime>"]` for files
    pub fn persist_tags(&self) -> Vec<Vec<String>> {
        let mut tags = Vec::new();
        let mut path = String::new();
        self.collect_persist(self.root, &mut path, &mut tags);
        tags
    }

    fn collect_persist(&self, ino: u64, path: &mut String, tags: &mut Vec<Vec<String>>) {
        let node = match self.get(ino) {
            Some(n) => n,
            None => return,
        };

        match node {
            TreeNode::Directory { children, .. } => {
                let is_root = ino == self.root;

                if !is_root && !path.starts_with("/public") {
                    tags.push(vec!["folder".to_string(), path.clone()]);
                }

                for &child_ino in children {
                    let child = match self.get(child_ino) {
                        Some(n) => n,
                        None => continue,
                    };
                    let child_name = match child {
                        TreeNode::Directory { name, .. } => name.as_str(),
                        TreeNode::File { name, .. } => name.as_str(),
                    };

                    let prev_len = path.len();
                    path.push('/');
                    path.push_str(child_name);

                    if !path.starts_with("/public") {
                        self.collect_persist(child_ino, path, tags);
                    }

                    path.truncate(prev_len);
                }
            }
            TreeNode::File { content, size, .. } => {
                if let FileContent::Remote {
                    sha256, mime_type, ..
                } = content
                {
                    tags.push(vec![
                        "x".to_string(),
                        sha256.clone(),
                        path.clone(),
                        size.to_string(),
                        mime_type.as_deref().unwrap_or("").to_string(),
                    ]);
                }
            }
        }
    }

    /// Apply persisted tags to rebuild user-created directory structure.
    ///
    /// Processes `folder` tags first (to create empty dirs), then `x` tags
    /// (which create parent dirs as needed). Blob URLs are derived as
    /// `{server_url}/{sha256}`.
    pub fn apply_persisted(&mut self, tags: &[Vec<String>], server_url: &str) {
        for tag in tags {
            if tag.len() >= 2 && tag[0] == "folder" {
                let path = &tag[1];
                let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
                let mut parent = self.root;
                for part in &parts {
                    if !part.is_empty() {
                        parent = self.get_or_create_dir(parent, part);
                    }
                }
            }
        }

        for tag in tags {
            if tag.len() >= 4 && tag[0] == "x" {
                let sha256 = &tag[1];
                let path = &tag[2];
                let size: u64 = tag[3].parse().unwrap_or(0);
                let mime = if tag.len() >= 5 && !tag[4].is_empty() {
                    Some(tag[4].clone())
                } else {
                    None
                };

                let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
                if parts.is_empty() || parts[0].is_empty() {
                    continue;
                }

                let mut parent = self.root;
                for dir_name in &parts[..parts.len().saturating_sub(1)] {
                    if !dir_name.is_empty() {
                        parent = self.get_or_create_dir(parent, dir_name);
                    }
                }

                let file_name = parts.last().unwrap_or(&"");
                if !file_name.is_empty() {
                    let url = format!("{}/{}", server_url.trim_end_matches('/'), sha256);
                    if self.lookup(parent, file_name).is_none() {
                        self.add_remote_file(
                            parent,
                            file_name,
                            url,
                            sha256.to_string(),
                            size,
                            mime,
                            0,
                            None,
                        );
                    }
                }
            }
        }
    }

    /// Build by-sha256, by-type, and by-date subtrees from blob descriptors.
    ///
    /// Descriptors with invalid sha256 are silently skipped.
    /// In `by-sha256/`, each unique sha256 appears exactly once.
    pub fn build_from_descriptors(&mut self, parent: u64, descriptors: &[BlobDescriptor]) {
        let by_sha = self.add_directory(parent, "by-sha256");
        let by_type = self.add_directory(parent, "by-type");
        let by_date = self.add_directory(parent, "by-date");

        let mut seen: HashSet<String> = HashSet::new();

        for desc in descriptors {
            // Validate sha256; skip invalid descriptors.
            let sha = match sanitize_sha256(&desc.sha256) {
                Ok(s) => s,
                Err(_) => continue,
            };

            // Determine file extension from MIME type or URL.
            let ext = extension_for_descriptor(desc.mime_type.as_deref(), &desc.url);
            let file_name = if ext.is_empty() {
                sha.clone()
            } else {
                format!("{}.{}", sha, ext)
            };

            // by-sha256 — deduplicate by sha256.
            if seen.insert(sha.clone()) {
                self.add_remote_file(
                    by_sha,
                    &file_name,
                    desc.url.clone(),
                    sha.clone(),
                    desc.size,
                    desc.mime_type.clone(),
                    desc.uploaded,
                    desc.expiration,
                );
            }

            // by-type — group under sanitized MIME directory.
            let mime = desc.effective_mime_type();
            let mime_dir_name = sanitize_mime_for_path(mime);
            let type_dir = self.get_or_create_dir(by_type, &mime_dir_name);
            self.add_remote_file(
                type_dir,
                &file_name,
                desc.url.clone(),
                sha.clone(),
                desc.size,
                desc.mime_type.clone(),
                desc.uploaded,
                desc.expiration,
            );

            // by-date — group under YYYY/MM/DD.
            let (year, month, day) = unix_to_ymd(desc.uploaded);
            let year_dir = self.get_or_create_dir(by_date, &format!("{year:04}"));
            let month_dir = self.get_or_create_dir(year_dir, &format!("{month:02}"));
            let day_dir = self.get_or_create_dir(month_dir, &format!("{day:02}"));
            self.add_remote_file(
                day_dir,
                &file_name,
                desc.url.clone(),
                sha,
                desc.size,
                desc.mime_type.clone(),
                desc.uploaded,
                desc.expiration,
            );
        }
    }

    /// Build only the by-sha256 subtree from blob descriptors.
    ///
    /// Deduplicates by sha256 — each unique hash appears exactly once.
    /// Used for the all-servers aggregate view.
    pub fn build_by_sha256_only(&mut self, parent: u64, descriptors: &[BlobDescriptor]) {
        let by_sha = self.add_directory(parent, "by-sha256");

        let mut seen: HashSet<String> = HashSet::new();

        for desc in descriptors {
            let sha = match sanitize_sha256(&desc.sha256) {
                Ok(s) => s,
                Err(_) => continue,
            };

            if seen.insert(sha.clone()) {
                let ext = extension_for_descriptor(desc.mime_type.as_deref(), &desc.url);
                let file_name = if ext.is_empty() {
                    sha.clone()
                } else {
                    format!("{}.{}", sha, ext)
                };

                self.add_remote_file(
                    by_sha,
                    &file_name,
                    desc.url.clone(),
                    sha,
                    desc.size,
                    desc.mime_type.clone(),
                    desc.uploaded,
                    desc.expiration,
                );
            }
        }
    }

    /// Update a file node's content, size, and uploaded timestamp.
    ///
    /// Used after a successful upload to replace pending write data with
    /// remote blob info. Returns `true` if the node was found and updated.
    pub fn update_file_node(
        &mut self,
        ino: u64,
        content: FileContent,
        size: u64,
        uploaded: u64,
    ) -> bool {
        if let Some(node) = self.get_mut(ino)
            && let TreeNode::File {
                content: c,
                size: s,
                uploaded: u,
                ..
            } = node
        {
            *c = content;
            *s = size;
            *u = uploaded;
            return true;
        }
        false
    }

    /// Update just the file size (for getattr during writes).
    ///
    /// Returns `true` if the node was found and updated.
    pub fn update_file_size(&mut self, ino: u64, size: u64) -> bool {
        if let Some(node) = self.get_mut(ino)
            && let TreeNode::File { size: s, .. } = node
        {
            *s = size;
            return true;
        }
        false
    }

    pub fn expires(&self, ino: u64) -> Option<u64> {
        if let Some(TreeNode::File {
            content: FileContent::Remote { expires, .. },
            ..
        }) = self.get(ino)
        {
            *expires
        } else {
            None
        }
    }

    pub fn set_expires(&mut self, ino: u64, expires: Option<u64>) -> bool {
        if let Some(node) = self.get_mut(ino)
            && let TreeNode::File {
                content: FileContent::Remote { expires: e, .. },
                ..
            } = node
        {
            *e = expires;
            return true;
        }
        false
    }

    pub fn add_file_to_dir(
        &mut self,
        parent: u64,
        name: &str,
        content: FileContent,
        size: u64,
        uploaded: u64,
    ) -> u64 {
        let sanitized = sanitize_path_component(name);
        let ino = self.next_inode();
        self.nodes.push(TreeNode::File {
            ino,
            name: sanitized,
            parent,
            size,
            content,
            uploaded,
        });
        if let Some(TreeNode::Directory { children, .. }) = self.get_mut(parent) {
            children.push(ino);
        }
        ino
    }

    pub fn remove_file_from_dir(&mut self, parent: u64, name: &str) -> bool {
        let child_ino = match self.lookup(parent, name) {
            Some(ino) => ino,
            None => return false,
        };

        match self.get(child_ino) {
            Some(TreeNode::File { .. }) => {}
            _ => return false,
        }

        if let Some(TreeNode::Directory { children, .. }) = self.get_mut(parent) {
            children.retain(|&c| c != child_ino);
            true
        } else {
            false
        }
    }

    /// Collect all Remote files expiring within the given time window.
    ///
    /// Returns `Vec<(name, expiry_timestamp)>` for files where:
    /// - Content is `FileContent::Remote` with `expires: Some(ts)`
    /// - `ts > now` (not already expired)
    /// - `ts <= now + within_secs` (within the window)
    pub fn collect_expiring_blobs(&self, now: u64, within_secs: u64) -> Vec<(String, u64)> {
        let threshold = now.saturating_add(within_secs);
        self.nodes
            .iter()
            .filter_map(|node| match node {
                TreeNode::File {
                    name,
                    content:
                        FileContent::Remote {
                            expires: Some(exp), ..
                        },
                    ..
                } => {
                    if *exp > now && *exp <= threshold {
                        Some((name.clone(), *exp))
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect()
    }
}

impl Default for Tree {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert a Unix timestamp to (year, month, day) in the proleptic Gregorian
/// calendar using Howard Hinnant's civil-from-days algorithm.
///
/// No external date crate required.
fn unix_to_ymd(ts: u64) -> (u32, u32, u32) {
    let days = (ts / 86400) as i64;
    let z = days + 719468;
    let era = if z >= 0 {
        z / 146097
    } else {
        (z - 146096) / 146097
    };
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year as u32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: 64-char hex sha256 strings for test data.
    const SHA_A: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    const SHA_B: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const SHA_C: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    /// Helper: build a descriptor.
    fn mk_desc(sha: &str, mime: Option<&str>, uploaded: u64) -> BlobDescriptor {
        BlobDescriptor {
            url: format!("https://cdn.example.com/{}", sha),
            sha256: sha.to_string(),
            size: 100,
            mime_type: mime.map(|s| s.to_string()),
            uploaded,
            expiration: None,
        }
    }

    // ============== Scenario 1: new() creates root at ino=1 ==============

    #[test]
    fn s01_new_has_root_inode_1() {
        let tree = Tree::new();
        assert_eq!(tree.root(), 1);
        let node = tree.get(1).expect("root should exist");
        assert_eq!(tree.kind(1), Some(NodeKind::Directory));
        match node {
            TreeNode::Directory { name, children, .. } => {
                assert!(
                    children.is_empty(),
                    "root should have no children initially"
                );
                let _ = name;
            }
            TreeNode::File { .. } => panic!("root must be a directory"),
        }
    }

    // ============== Scenario 2: add_directory returns inode, lookup finds it ==============

    #[test]
    fn s02_add_directory_and_lookup() {
        let mut tree = Tree::new();
        let ino = tree.add_directory(1, "test");
        assert_eq!(ino, 2, "first directory after root should be inode 2");
        assert_eq!(tree.lookup(1, "test"), Some(2));
        assert_eq!(tree.kind(2), Some(NodeKind::Directory));
    }

    // ============== Scenario 3: add_static_file content is readable ==============

    #[test]
    fn s03_add_static_file_readable() {
        let mut tree = Tree::new();
        let ino = tree.add_static_file(1, "README.txt", b"hello".to_vec());
        assert_eq!(tree.lookup(1, "README.txt"), Some(ino));
        match tree.get(ino).unwrap() {
            TreeNode::File { size, content, .. } => {
                assert_eq!(*size, 5);
                match content {
                    FileContent::Static(data) => assert_eq!(data, b"hello"),
                    FileContent::Remote { .. } | FileContent::Local { .. } => {
                        panic!("expected Static content")
                    }
                }
            }
            TreeNode::Directory { .. } => panic!("expected File node"),
        }
    }

    // ============== Scenario 4: add_remote_file has Remote content ==============

    #[test]
    fn s04_add_remote_file() {
        let mut tree = Tree::new();
        let ino = tree.add_remote_file(
            1,
            "abc.png",
            "https://cdn.example.com/blob".to_string(),
            SHA_A.to_string(),
            100,
            Some("image/png".to_string()),
            1700000000,
            None,
        );
        assert_eq!(tree.lookup(1, "abc.png"), Some(ino));
        match tree.get(ino).unwrap() {
            TreeNode::File { size, content, .. } => {
                assert_eq!(*size, 100);
                match content {
                    FileContent::Remote {
                        url,
                        sha256,
                        mime_type,
                        expires: _,
                    } => {
                        assert_eq!(url, "https://cdn.example.com/blob");
                        assert_eq!(sha256, SHA_A);
                        assert_eq!(mime_type.as_deref(), Some("image/png"));
                    }
                    FileContent::Static(_) | FileContent::Local { .. } => {
                        panic!("expected Remote content")
                    }
                }
            }
            TreeNode::Directory { .. } => panic!("expected File node"),
        }
    }

    // ============== Scenario 5: build_from_descriptors → by-sha256 has 3 files ==============

    #[test]
    fn s05_build_by_sha256_has_all_files() {
        let mut tree = Tree::new();
        let descs = vec![
            mk_desc(SHA_A, Some("image/png"), 1700000000),
            mk_desc(SHA_B, Some("image/png"), 1700000000),
            mk_desc(SHA_C, Some("image/png"), 1700000000),
        ];
        tree.build_from_descriptors(1, &descs);

        let by_sha = tree
            .lookup(1, "by-sha256")
            .expect("by-sha256 dir should exist");
        let entries = tree.readdir(by_sha).expect("should list by-sha256");
        assert_eq!(entries.len(), 3, "by-sha256 should have exactly 3 files");

        let names: Vec<&str> = entries.iter().map(|(_, n, _)| n.as_str()).collect();
        for sha in [SHA_A, SHA_B, SHA_C] {
            assert!(
                names.iter().any(|n| n.starts_with(sha)),
                "by-sha256 should contain entry starting with {}",
                sha
            );
        }
    }

    // ============== Scenario 6: build_from_descriptors → by-type has correct MIME dirs ==============

    #[test]
    fn s06_build_by_type_mime_dirs() {
        let mut tree = Tree::new();
        let descs = vec![
            mk_desc(SHA_A, Some("image/png"), 1700000000),
            mk_desc(SHA_B, Some("application/pdf"), 1700000000),
        ];
        tree.build_from_descriptors(1, &descs);

        let by_type = tree.lookup(1, "by-type").expect("by-type dir should exist");
        let entries = tree.readdir(by_type).expect("should list by-type");
        let names: Vec<&str> = entries.iter().map(|(_, n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"image_png"),
            "should have image_png dir, got {:?}",
            names
        );
        assert!(
            names.contains(&"application_pdf"),
            "should have application_pdf dir, got {:?}",
            names
        );
    }

    // ============== Scenario 7: build_from_descriptors → by-date has YYYY/MM/DD ==============

    #[test]
    fn s07_build_by_date_structure() {
        let mut tree = Tree::new();
        // timestamp 1700000000 = 2023-11-14
        let descs = vec![mk_desc(SHA_A, Some("image/png"), 1700000000)];
        tree.build_from_descriptors(1, &descs);

        let by_date = tree.lookup(1, "by-date").expect("by-date dir should exist");
        let year = tree.lookup(by_date, "2023").expect("2023 dir should exist");
        let month = tree
            .lookup(year, "11")
            .expect("11 (November) dir should exist");
        let day = tree.lookup(month, "14").expect("14th day dir should exist");
        let entries = tree.readdir(day).expect("should list day dir");
        assert_eq!(entries.len(), 1, "day dir should have exactly 1 file");
        assert!(
            entries[0].1.starts_with(SHA_A),
            "file should start with sha, got {}",
            entries[0].1
        );
    }

    // ============== Scenario 8: empty descriptors → dirs created but empty ==============

    #[test]
    fn s08_build_empty_descriptors() {
        let mut tree = Tree::new();
        tree.build_from_descriptors(1, &[]);

        for dir_name in &["by-sha256", "by-type", "by-date"] {
            let ino = tree
                .lookup(1, dir_name)
                .unwrap_or_else(|| panic!("{} should exist", dir_name));
            let entries = tree.readdir(ino).expect("should be a directory");
            assert!(entries.is_empty(), "{} should be empty", dir_name);
        }
    }

    // ============== Scenario 9: duplicate sha256 → by-sha256 has 1 entry ==============

    #[test]
    fn s09_build_duplicate_sha256_dedup() {
        let mut tree = Tree::new();
        let descs = vec![
            mk_desc(SHA_A, Some("image/png"), 1700000000),
            mk_desc(SHA_A, Some("image/png"), 1700001000),
        ];
        tree.build_from_descriptors(1, &descs);

        let by_sha = tree.lookup(1, "by-sha256").unwrap();
        let entries = tree.readdir(by_sha).unwrap();
        assert_eq!(entries.len(), 1, "by-sha256 should deduplicate to 1 entry");
    }

    // ============== Scenario 10: invalid sha256 → skipped ==============

    #[test]
    fn s10_build_invalid_sha256_skipped() {
        let mut tree = Tree::new();
        let descs = vec![
            mk_desc("not-a-valid-hash", Some("image/png"), 1700000000),
            mk_desc(SHA_A, Some("image/png"), 1700000000),
        ];
        tree.build_from_descriptors(1, &descs);

        let by_sha = tree.lookup(1, "by-sha256").unwrap();
        let entries = tree.readdir(by_sha).unwrap();
        assert_eq!(entries.len(), 1, "only valid sha256 should be present");
        assert!(entries[0].1.starts_with(SHA_A));
    }

    // ============== Scenario 11: None mime_type → grouped under application_octet-stream ==============

    #[test]
    fn s11_build_none_mime_type_grouped() {
        let mut tree = Tree::new();
        let descs = vec![mk_desc(SHA_A, None, 1700000000)];
        tree.build_from_descriptors(1, &descs);

        let by_type = tree.lookup(1, "by-type").unwrap();
        let entries = tree.readdir(by_type).unwrap();
        let names: Vec<&str> = entries.iter().map(|(_, n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"application_octet-stream"),
            "None mime should be grouped under application_octet-stream, got {:?}",
            names
        );
    }

    // ============== Scenario 12: readdir on File returns None ==============

    #[test]
    fn s12_readdir_on_file_returns_none() {
        let mut tree = Tree::new();
        let ino = tree.add_static_file(1, "file.txt", b"data".to_vec());
        assert_eq!(
            tree.readdir(ino),
            None,
            "readdir on a file should return None"
        );
    }

    // ============== Scenario 13: lookup on File returns None ==============

    #[test]
    fn s13_lookup_on_file_returns_none() {
        let mut tree = Tree::new();
        let ino = tree.add_static_file(1, "file.txt", b"data".to_vec());
        assert_eq!(
            tree.lookup(ino, "anything"),
            None,
            "lookup on a file should return None"
        );
    }

    // ============== Scenario 14: size() returns correct values ==============

    #[test]
    fn s14_size_correct() {
        let mut tree = Tree::new();
        let dir_ino = tree.add_directory(1, "dir");
        let file_ino = tree.add_static_file(1, "file.txt", b"four bytes".to_vec());

        assert_eq!(tree.size(1), Some(0), "root directory size should be 0");
        assert_eq!(tree.size(dir_ino), Some(0), "directory size should be 0");
        assert_eq!(tree.size(file_ino), Some(10), "file size should be 10");
        assert_eq!(tree.size(999), None, "nonexistent inode should return None");
    }

    // ============== Scenario 15: kind() returns correct NodeKind ==============

    #[test]
    fn s15_kind_correct() {
        let mut tree = Tree::new();
        let dir_ino = tree.add_directory(1, "dir");
        let file_ino = tree.add_static_file(1, "file.txt", b"x".to_vec());

        assert_eq!(tree.kind(1), Some(NodeKind::Directory));
        assert_eq!(tree.kind(dir_ino), Some(NodeKind::Directory));
        assert_eq!(tree.kind(file_ino), Some(NodeKind::File));
        assert_eq!(tree.kind(999), None);
    }

    // ============== Scenario 16: build_by_sha256_only → only by-sha256 with correct files ==============

    #[test]
    fn s16_build_by_sha256_only_creates_files() {
        let mut tree = Tree::new();
        let parent = tree.add_directory(1, "all-servers");
        let descs = vec![
            mk_desc(SHA_A, Some("image/png"), 1700000000),
            mk_desc(SHA_B, Some("image/png"), 1700000000),
        ];
        tree.build_by_sha256_only(parent, &descs);

        // Only by-sha256 should exist, not by-type or by-date
        let by_sha = tree
            .lookup(parent, "by-sha256")
            .expect("by-sha256 dir should exist");
        assert_eq!(
            tree.lookup(parent, "by-type"),
            None,
            "by-type should NOT exist in build_by_sha256_only"
        );
        assert_eq!(
            tree.lookup(parent, "by-date"),
            None,
            "by-date should NOT exist in build_by_sha256_only"
        );

        let entries = tree.readdir(by_sha).expect("should list by-sha256");
        assert_eq!(entries.len(), 2, "by-sha256 should have exactly 2 files");

        let names: Vec<&str> = entries.iter().map(|(_, n, _)| n.as_str()).collect();
        for sha in [SHA_A, SHA_B] {
            assert!(
                names.iter().any(|n| n.starts_with(sha)),
                "by-sha256 should contain entry starting with {}",
                sha
            );
        }
    }

    // ============== Scenario 17: build_by_sha256_only → deduplicates by sha256 ==============

    #[test]
    fn s17_build_by_sha256_only_dedup() {
        let mut tree = Tree::new();
        let parent = tree.add_directory(1, "all-servers");
        let descs = vec![
            mk_desc(SHA_A, Some("image/png"), 1700000000),
            mk_desc(SHA_A, Some("image/png"), 1700001000),
            mk_desc(SHA_B, Some("image/png"), 1700000000),
        ];
        tree.build_by_sha256_only(parent, &descs);

        let by_sha = tree.lookup(parent, "by-sha256").unwrap();
        let entries = tree.readdir(by_sha).unwrap();
        assert_eq!(
            entries.len(),
            2,
            "duplicated sha256 should be deduplicated to 2 unique entries"
        );
    }

    // ============== Scenario 18: build_by_sha256_only → skips invalid sha256 ==============

    #[test]
    fn s18_build_by_sha256_only_skips_invalid() {
        let mut tree = Tree::new();
        let parent = tree.add_directory(1, "all-servers");
        let descs = vec![
            mk_desc("not-a-valid-hash", Some("image/png"), 1700000000),
            mk_desc(SHA_A, Some("image/png"), 1700000000),
        ];
        tree.build_by_sha256_only(parent, &descs);

        let by_sha = tree.lookup(parent, "by-sha256").unwrap();
        let entries = tree.readdir(by_sha).unwrap();
        assert_eq!(entries.len(), 1, "only valid sha256 should be present");
        assert!(entries[0].1.starts_with(SHA_A));
    }

    // ============== Scenario 19: add_file_to_dir creates file ==============

    #[test]
    fn s19_add_file_to_dir_creates_file() {
        let mut tree = Tree::new();
        let uploaded = 1700000000u64;
        let ino = tree.add_file_to_dir(
            1,
            "newfile.bin",
            FileContent::Static(b"payload".to_vec()),
            7,
            uploaded,
        );
        assert_eq!(tree.lookup(1, "newfile.bin"), Some(ino));
        assert_eq!(tree.size(ino), Some(7));
        assert_eq!(tree.uploaded(ino), Some(uploaded));
        assert_eq!(tree.kind(ino), Some(NodeKind::File));
    }

    // ============== Scenario 20: remove_file_from_dir removes file ==============

    #[test]
    fn s20_remove_file_from_dir_removes_file() {
        let mut tree = Tree::new();
        tree.add_file_to_dir(
            1,
            "temp.bin",
            FileContent::Static(b"x".to_vec()),
            1,
            1700000000,
        );
        assert!(
            tree.lookup(1, "temp.bin").is_some(),
            "file should exist before removal"
        );

        let removed = tree.remove_file_from_dir(1, "temp.bin");
        assert!(removed, "remove should return true for existing file");
        assert_eq!(
            tree.lookup(1, "temp.bin"),
            None,
            "file should not be found after removal"
        );
    }

    // ============== Scenario 21: remove_file_from_dir on nonexistent returns false ==============

    #[test]
    fn s21_remove_file_from_dir_nonexistent_returns_false() {
        let mut tree = Tree::new();
        let removed = tree.remove_file_from_dir(1, "ghost.bin");
        assert!(!removed, "removing nonexistent file should return false");
    }

    // ============== Scenario 22: remove_file_from_dir on directory name returns false ==============

    #[test]
    fn s22_remove_file_from_dir_directory_returns_false() {
        let mut tree = Tree::new();
        tree.add_directory(1, "subdir");

        let removed = tree.remove_file_from_dir(1, "subdir");
        assert!(!removed, "removing a directory should return false");

        assert!(
            tree.lookup(1, "subdir").is_some(),
            "directory should still exist after failed remove"
        );
    }

    // ============== Scenario 23: next_inode increments correctly ==============

    #[test]
    fn s23_next_inode_increments() {
        let mut tree = Tree::new();
        assert_eq!(
            tree.next_inode(),
            2,
            "after root only, next inode should be 2"
        );

        tree.add_directory(1, "dir1");
        assert_eq!(
            tree.next_inode(),
            3,
            "after adding one dir, next inode should be 3"
        );

        tree.add_static_file(1, "file.txt", b"hi".to_vec());
        assert_eq!(
            tree.next_inode(),
            4,
            "after adding a file, next inode should be 4"
        );

        tree.add_file_to_dir(1, "f2", FileContent::Static(b"y".to_vec()), 1, 0);
        assert_eq!(
            tree.next_inode(),
            5,
            "after add_file_to_dir, next inode should be 5"
        );
    }

    // ============== Scenario 24: uploaded accessor ==============

    #[test]
    fn s24_uploaded_accessor() {
        let mut tree = Tree::new();
        let dir_ino = tree.add_directory(1, "dir");
        let file_ino = tree.add_remote_file(
            1,
            "blob.png",
            "https://cdn.example.com/blob".to_string(),
            SHA_A.to_string(),
            42,
            Some("image/png".to_string()),
            1700000123,
            None,
        );

        assert_eq!(tree.uploaded(file_ino), Some(1700000123));
        assert_eq!(tree.uploaded(dir_ino), None, "directory should return None");
        assert_eq!(tree.uploaded(1), None, "root directory should return None");
        assert_eq!(
            tree.uploaded(999),
            None,
            "nonexistent inode should return None"
        );
    }

    #[test]
    fn s25_expires_accessor() {
        let mut tree = Tree::new();
        let ino = tree.add_remote_file(
            1,
            "blob.png",
            "https://cdn.example.com/blob".to_string(),
            SHA_A.to_string(),
            42,
            Some("image/png".to_string()),
            1700000000,
            None,
        );

        assert_eq!(
            tree.expires(ino),
            None,
            "new Remote file should have no expiry"
        );

        assert!(tree.set_expires(ino, Some(1794395471)));
        assert_eq!(tree.expires(ino), Some(1794395471));

        assert!(tree.set_expires(ino, None));
        assert_eq!(tree.expires(ino), None);
    }

    #[test]
    fn s26_set_expires_returns_false_for_static() {
        let mut tree = Tree::new();
        let ino = tree.add_file_to_dir(1, "file.txt", FileContent::Static(b"hi".to_vec()), 2, 0);
        assert!(!tree.set_expires(ino, Some(123)));
        assert_eq!(tree.expires(ino), None);
    }

    #[test]
    fn s27_set_expires_returns_false_for_directory() {
        let mut tree = Tree::new();
        let dir_ino = tree.add_directory(1, "subdir");
        assert!(!tree.set_expires(dir_ino, Some(123)));
        assert_eq!(tree.expires(dir_ino), None);
    }

    #[test]
    fn s28_expires_nonexistent_inode() {
        let tree = Tree::new();
        assert_eq!(tree.expires(999), None);
    }

    // ============== S29: collect_expiring finds soon-to-expire files ==============

    #[test]
    fn s29_collect_expiring_finds_soon_expiring() {
        let mut tree = Tree::new();
        let now: u64 = 1_700_000_000;

        let ino = tree.add_remote_file(
            1,
            "expiring.png",
            "https://cdn.example.com/blob".to_string(),
            SHA_A.to_string(),
            42,
            Some("image/png".to_string()),
            now - 86400,
            None,
        );
        tree.set_expires(ino, Some(now + 3 * 86400));

        let result = tree.collect_expiring_blobs(now, 7 * 86400);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "expiring.png");
        assert_eq!(result[0].1, now + 3 * 86400);
    }

    // ============== S30: collect_expiring excludes far-future expiry ==============

    #[test]
    fn s30_collect_expiring_excludes_far_future() {
        let mut tree = Tree::new();
        let now: u64 = 1_700_000_000;

        let ino = tree.add_remote_file(
            1,
            "stable.png",
            "https://cdn.example.com/blob".to_string(),
            SHA_A.to_string(),
            42,
            Some("image/png".to_string()),
            now,
            None,
        );
        tree.set_expires(ino, Some(now + 30 * 86400));

        let result = tree.collect_expiring_blobs(now, 7 * 86400);
        assert!(
            result.is_empty(),
            "file expiring in 30 days should not be in 7-day window"
        );
    }

    // ============== S31: collect_expiring excludes no-expiry files ==============

    #[test]
    fn s31_collect_expiring_excludes_no_expiry() {
        let mut tree = Tree::new();
        let now: u64 = 1_700_000_000;

        tree.add_remote_file(
            1,
            "permanent.png",
            "https://cdn.example.com/blob".to_string(),
            SHA_A.to_string(),
            42,
            Some("image/png".to_string()),
            now,
            None,
        );

        let result = tree.collect_expiring_blobs(now, 7 * 86400);
        assert!(
            result.is_empty(),
            "file with no expiry should not be collected"
        );
    }

    // ============== S32: collect_expiring excludes Static/Local files ==============

    #[test]
    fn s32_collect_expiring_excludes_non_remote() {
        let mut tree = Tree::new();
        let now: u64 = 1_700_000_000;

        tree.add_static_file(1, "readme.txt", b"hello".to_vec());
        tree.add_file_to_dir(
            1,
            "local.txt",
            FileContent::Local {
                path: std::path::PathBuf::from("/tmp/x"),
            },
            1,
            now,
        );

        let result = tree.collect_expiring_blobs(now, 7 * 86400);
        assert!(
            result.is_empty(),
            "Static and Local files should not be collected"
        );
    }

    // ============== S33: collect_expiring finds files in nested dirs ==============

    #[test]
    fn s33_collect_expiring_nested_dirs() {
        let mut tree = Tree::new();
        let now: u64 = 1_700_000_000;

        let dir = tree.add_directory(1, "subdir");
        let ino = tree.add_remote_file(
            dir,
            "nested.png",
            "https://cdn.example.com/blob".to_string(),
            SHA_A.to_string(),
            42,
            Some("image/png".to_string()),
            now,
            None,
        );
        tree.set_expires(ino, Some(now + 2 * 86400));

        let result = tree.collect_expiring_blobs(now, 7 * 86400);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "nested.png");
    }

    // ============== S34: persist_tags serializes user dirs ==============

    #[test]
    fn s34_persist_tags_user_dirs_and_files() {
        let mut tree = Tree::new();
        let docs = tree.add_directory(1, "docs");
        let _file = tree.add_remote_file(
            docs,
            "report.pdf",
            "https://blossom.psbt.me/abc".to_string(),
            SHA_A.to_string(),
            1024,
            Some("application/pdf".to_string()),
            1700000000,
            None,
        );

        let tags = tree.persist_tags();

        let has_folder = tags.iter().any(|t| t[0] == "folder" && t[1] == "/docs");
        assert!(has_folder, "should have folder tag for /docs");

        let has_file = tags
            .iter()
            .any(|t| t[0] == "x" && t[1] == SHA_A && t[2] == "/docs/report.pdf");
        assert!(has_file, "should have file tag for /docs/report.pdf");
    }

    // ============== S35: persist_tags skips public subtree ==============

    #[test]
    fn s35_persist_tags_skips_public() {
        let mut tree = Tree::new();
        let public = tree.add_directory(1, "public");
        let _auto_file = tree.add_remote_file(
            public,
            SHA_A,
            "https://blossom.psbt.me/abc".to_string(),
            SHA_A.to_string(),
            42,
            None,
            1700000000,
            None,
        );

        let tags = tree.persist_tags();
        assert!(
            tags.is_empty(),
            "public/ subtree should not produce any tags"
        );
    }

    // ============== S36: persist_tags skips static files ==============

    #[test]
    fn s36_persist_tags_skips_static_and_local() {
        let mut tree = Tree::new();
        let _static = tree.add_static_file(1, "README.txt", b"hello".to_vec());

        let tags = tree.persist_tags();
        assert!(tags.is_empty(), "static files should not be persisted");
    }

    // ============== S37: apply_persisted rebuilds structure ==============

    #[test]
    fn s37_apply_persisted_rebuilds_dirs_and_files() {
        let mut tree = Tree::new();
        let tags = vec![
            vec!["folder".to_string(), "/projects".to_string()],
            vec![
                "x".to_string(),
                SHA_B.to_string(),
                "/projects/main.rs".to_string(),
                "2048".to_string(),
                "text/plain".to_string(),
            ],
            vec!["folder".to_string(), "/empty".to_string()],
        ];

        tree.apply_persisted(&tags, "https://blossom.psbt.me");

        let projects_ino = tree
            .lookup(1, "projects")
            .expect("projects dir should exist");
        let _empty_ino = tree.lookup(1, "empty").expect("empty dir should exist");
        let file_ino = tree
            .lookup(projects_ino, "main.rs")
            .expect("main.rs should exist");

        let node = tree.get(file_ino).unwrap();
        if let TreeNode::File { size, .. } = node {
            assert_eq!(*size, 2048);
        } else {
            panic!("expected file node");
        }
    }

    // ============== S38: round-trip persist → apply ==============

    #[test]
    fn s38_round_trip_persist_apply() {
        let mut tree1 = Tree::new();
        let docs = tree1.add_directory(1, "docs");
        let images = tree1.add_directory(docs, "images");
        let _f1 = tree1.add_remote_file(
            docs,
            "readme.md",
            "https://srv.example.com/aaa".to_string(),
            SHA_A.to_string(),
            100,
            Some("text/markdown".to_string()),
            0,
            None,
        );
        let _f2 = tree1.add_remote_file(
            images,
            "logo.png",
            "https://srv.example.com/bbb".to_string(),
            SHA_B.to_string(),
            200,
            Some("image/png".to_string()),
            0,
            None,
        );

        let tags = tree1.persist_tags();
        assert!(!tags.is_empty(), "should have tags");

        let mut tree2 = Tree::new();
        tree2.apply_persisted(&tags, "https://srv.example.com");

        let docs2 = tree2.lookup(1, "docs").expect("docs should exist");
        let images2 = tree2.lookup(docs2, "images").expect("images should exist");
        let f1 = tree2
            .lookup(docs2, "readme.md")
            .expect("readme.md should exist");
        let f2 = tree2
            .lookup(images2, "logo.png")
            .expect("logo.png should exist");

        let n1 = tree2.get(f1).unwrap();
        if let TreeNode::File { size, .. } = n1 {
            assert_eq!(*size, 100);
        } else {
            panic!("expected file");
        }

        let n2 = tree2.get(f2).unwrap();
        if let TreeNode::File { size, .. } = n2 {
            assert_eq!(*size, 200);
        } else {
            panic!("expected file");
        }
    }
}
