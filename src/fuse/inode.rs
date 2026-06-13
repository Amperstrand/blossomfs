//! Stable inode allocation.
//!
//! Root inode = `FUSE_ROOT_ID` (1). Sequential allocation during
//! tree construction with a bidirectional map for O(1) lookup.
//!
//! Inode numbers are stable for the lifetime of a mount session.
//! The root inode is always 1; subsequent inodes are assigned
//! sequentially (2, 3, 4, …).

use std::collections::HashMap;

/// FUSE root inode number (always 1).
pub const FUSE_ROOT_ID: u64 = 1;

/// A node kind for inode tracking.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum InodeKind {
    Directory,
    RegularFile,
}

/// Information about an allocated inode.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct InodeInfo {
    /// The inode number.
    pub ino: u64,
    /// Parent inode number (root's parent is itself).
    pub parent: u64,
    /// Entry name within the parent directory (root uses "/").
    pub name: String,
    /// File or directory kind.
    pub kind: InodeKind,
}

/// Allocator for stable inode numbers during a mount session.
///
/// Root inode (1) is reserved. Inodes are assigned sequentially
/// starting from 2. The internal `Vec` serves as a direct
/// `ino → index` lookup (inode *N* lives at index *N − 1*), and a
/// `HashMap<(parent, name), ino>` provides O(1) name resolution.
#[derive(Debug)]
#[allow(dead_code)]
pub struct InodeAllocator {
    nodes: Vec<InodeInfo>,
    by_name: HashMap<(u64, String), u64>,
}

#[allow(dead_code)]
impl InodeAllocator {
    /// Creates an allocator pre-populated with the root inode
    /// (ino = 1, parent = 1, name = "/", kind = Directory).
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let root = InodeInfo {
            ino: FUSE_ROOT_ID,
            parent: FUSE_ROOT_ID,
            name: "/".to_string(),
            kind: InodeKind::Directory,
        };
        Self {
            nodes: vec![root],
            by_name: HashMap::new(),
        }
    }

    /// Allocates a new inode and returns its number.
    ///
    /// # Panics
    ///
    /// Panics if `parent` does not refer to an existing inode.
    pub fn alloc(&mut self, parent: u64, name: &str, kind: InodeKind) -> u64 {
        if self.get(parent).is_none() {
            panic!("alloc: parent inode {parent} does not exist");
        }
        let new_ino = self.nodes.len() as u64 + 1;
        self.nodes.push(InodeInfo {
            ino: new_ino,
            parent,
            name: name.to_string(),
            kind,
        });
        self.by_name.insert((parent, name.to_string()), new_ino);
        new_ino
    }

    /// Look up an inode by parent inode number and entry name.
    pub fn lookup(&self, parent: u64, name: &str) -> Option<u64> {
        self.by_name.get(&(parent, name.to_string())).copied()
    }

    /// Get `InodeInfo` for an inode number.
    pub fn get(&self, ino: u64) -> Option<&InodeInfo> {
        let idx = ino.checked_sub(1)? as usize;
        self.nodes.get(idx)
    }

    /// Returns `FUSE_ROOT_ID` (1).
    pub fn root(&self) -> u64 {
        FUSE_ROOT_ID
    }

    /// Number of allocated inodes (including root).
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Always `false` — the allocator always contains at least the root.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Resolve the full POSIX path from root to the given inode.
    ///
    /// Returns `None` if `ino` does not refer to an existing inode.
    pub fn path(&self, ino: u64) -> Option<String> {
        if ino == FUSE_ROOT_ID {
            return Some("/".to_string());
        }
        let mut parts: Vec<&str> = Vec::new();
        let mut current = ino;
        loop {
            let idx = current.checked_sub(1)? as usize;
            let node = self.nodes.get(idx)?;
            if current == FUSE_ROOT_ID {
                break;
            }
            parts.push(&node.name);
            current = node.parent;
        }
        parts.reverse();
        Some(format!("/{}", parts.join("/")))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Scenario 1: new() creates root with ino=1, parent=1, kind=Directory ---

    #[test]
    fn test_new_creates_root() {
        let alloc = InodeAllocator::new();
        let root_info = alloc.get(FUSE_ROOT_ID).expect("root must exist");
        assert_eq!(root_info.ino, 1);
        assert_eq!(root_info.parent, 1);
        assert_eq!(root_info.kind, InodeKind::Directory);
        assert_eq!(root_info.name, "/");
    }

    // --- Scenario 2: alloc(root, "test", Directory) returns 2 ---

    #[test]
    fn test_alloc_first_child_returns_2() {
        let mut alloc = InodeAllocator::new();
        let ino = alloc.alloc(FUSE_ROOT_ID, "test", InodeKind::Directory);
        assert_eq!(ino, 2);
    }

    // --- Scenario 3: alloc(root, "file", RegularFile) returns 3 (sequential) ---

    #[test]
    fn test_alloc_sequential() {
        let mut alloc = InodeAllocator::new();
        let _ = alloc.alloc(FUSE_ROOT_ID, "test", InodeKind::Directory);
        let ino = alloc.alloc(FUSE_ROOT_ID, "file", InodeKind::RegularFile);
        assert_eq!(ino, 3);
    }

    // --- Scenario 4: lookup(root, "test") returns Some(2) ---

    #[test]
    fn test_lookup_existing() {
        let mut alloc = InodeAllocator::new();
        let _ = alloc.alloc(FUSE_ROOT_ID, "test", InodeKind::Directory);
        assert_eq!(alloc.lookup(FUSE_ROOT_ID, "test"), Some(2));
    }

    // --- Scenario 5: lookup(root, "nonexistent") returns None ---

    #[test]
    fn test_lookup_nonexistent_returns_none() {
        let alloc = InodeAllocator::new();
        assert_eq!(alloc.lookup(FUSE_ROOT_ID, "nonexistent"), None);
    }

    // --- Scenario 6: get(999) returns None ---

    #[test]
    fn test_get_nonexistent_returns_none() {
        let alloc = InodeAllocator::new();
        assert!(alloc.get(999).is_none());
    }

    // --- Scenario 7: get(1) returns Some with name="/" ---

    #[test]
    fn test_get_root_name() {
        let alloc = InodeAllocator::new();
        let info = alloc.get(1).expect("root must exist");
        assert_eq!(info.name, "/");
    }

    // --- Scenario 8: alloc 10000 nodes — all unique, no panic ---

    #[test]
    fn test_alloc_10k_nodes_unique() {
        let mut alloc = InodeAllocator::new();
        let mut seen = std::collections::HashSet::new();
        for i in 0..10_000u64 {
            let name = format!("node_{i}");
            let ino = alloc.alloc(FUSE_ROOT_ID, &name, InodeKind::RegularFile);
            assert!(seen.insert(ino), "duplicate inode {ino}");
        }
        assert_eq!(alloc.len(), 10_001); // root + 10 000
    }

    // --- Scenario 9: path(1) returns "/" (root) ---

    #[test]
    fn test_path_root() {
        let alloc = InodeAllocator::new();
        assert_eq!(alloc.path(FUSE_ROOT_ID), Some("/".to_string()));
    }

    // --- Scenario 10: path(child_of_root) returns "/childname" ---

    #[test]
    fn test_path_child_of_root() {
        let mut alloc = InodeAllocator::new();
        let ino = alloc.alloc(FUSE_ROOT_ID, "childname", InodeKind::Directory);
        assert_eq!(alloc.path(ino), Some("/childname".to_string()));
    }

    // --- Scenario 11: path(deeply_nested) returns "/a/b/c/file" ---

    #[test]
    fn test_path_deeply_nested() {
        let mut alloc = InodeAllocator::new();

        // root → a → b → c → file
        let a = alloc.alloc(FUSE_ROOT_ID, "a", InodeKind::Directory);
        let b = alloc.alloc(a, "b", InodeKind::Directory);
        let c = alloc.alloc(b, "c", InodeKind::Directory);
        let file = alloc.alloc(c, "file", InodeKind::RegularFile);

        assert_eq!(alloc.path(file), Some("/a/b/c/file".to_string()));
    }

    // --- Scenario 12: path(999) returns None ---

    #[test]
    fn test_path_nonexistent_returns_none() {
        let alloc = InodeAllocator::new();
        assert_eq!(alloc.path(999), None);
    }

    // --- Scenario 13: same construction sequence produces same mapping ---

    #[test]
    fn test_deterministic_allocation() {
        fn build() -> InodeAllocator {
            let mut a = InodeAllocator::new();
            let pub_dir = a.alloc(FUSE_ROOT_ID, "public", InodeKind::Directory);
            let npub = a.alloc(pub_dir, "npub1abc", InodeKind::Directory);
            let hash = a.alloc(npub, "by-sha256", InodeKind::Directory);
            a.alloc(hash, "deadbeef", InodeKind::RegularFile);
            a
        }

        let a1 = build();
        let a2 = build();

        // Same inode count
        assert_eq!(a1.len(), a2.len());

        // Same inode numbers for the same names
        assert_eq!(
            a1.lookup(FUSE_ROOT_ID, "public"),
            a2.lookup(FUSE_ROOT_ID, "public")
        );
        assert_eq!(a1.path(5), a2.path(5));
    }

    // --- Scenario 14: alloc with nonexistent parent panics ---

    #[test]
    #[should_panic(expected = "does not exist")]
    fn test_alloc_nonexistent_parent_panics() {
        let mut alloc = InodeAllocator::new();
        alloc.alloc(999, "orphan", InodeKind::RegularFile);
    }
}
