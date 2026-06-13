//! fuser::Filesystem trait implementation.
//!
//! BlossomFS implements the FUSE filesystem protocol. All callbacks use
//! `&self` (fuser 0.17.0), so mutable state (content cache) uses interior
//! mutability via `Arc<Mutex<CacheState>>`.
//!
//! Stage 1 operations: lookup, getattr, readdir, open, read, statfs.
//! Write operations return EROFS.

#![allow(dead_code)]

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::cache::fetch::fetch_and_cache;

use fuser::{
    BsdFileFlags, CopyFileRangeFlags, Errno, FileAttr, FileHandle, FileType, Filesystem,
    FopenFlags, Generation, INodeNo, KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, Request, TimeOrNow, WriteFlags,
};

use crate::fuse::tree::{FileContent, NodeKind, Tree, TreeNode};

/// FUSE attribute/entry cache TTL for a read-only filesystem.
const TTL: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// BlossomFS struct
// ---------------------------------------------------------------------------

/// Read-only FUSE filesystem backed by a pre-built [`Tree`].
///
/// The tree is immutable during a mount session. All mutating FUSE operations
/// return `EROFS`.
pub struct BlossomFS {
    tree: Tree,
    cache_base: Option<PathBuf>,
    runtime_handle: Option<tokio::runtime::Handle>,
}

impl BlossomFS {
    /// Create a new filesystem wrapper around the given tree.
    pub fn new(tree: Tree) -> Self {
        Self {
            tree,
            cache_base: None,
            runtime_handle: None,
        }
    }

    /// Create a filesystem with lazy-fetch cache support.
    ///
    /// When `cache_base` and `runtime_handle` are both set, reading a
    /// `FileContent::Remote` file triggers a lazy fetch via
    /// [`fetch_and_cache`], verifies SHA-256, caches to disk, and returns
    /// the bytes. Subsequent reads of the same blob serve from cache.
    pub fn new_with_cache(
        tree: Tree,
        cache_base: PathBuf,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            tree,
            cache_base: Some(cache_base),
            runtime_handle: Some(runtime_handle),
        }
    }

    // ======================== Internal testable helpers ========================

    /// Build a [`FileAttr`] for the given inode.
    ///
    /// Returns `None` if the inode does not exist in the tree.
    fn make_fileattr(&self, ino: u64) -> Option<FileAttr> {
        let node = self.tree.get(ino)?;
        let now = SystemTime::now();
        match node {
            TreeNode::Directory { children, .. } => {
                // Count child directories for root nlink calculation.
                let child_dirs = children
                    .iter()
                    .filter_map(|&c| self.tree.get(c))
                    .filter(|n| matches!(n, TreeNode::Directory { .. }))
                    .count() as u32;

                // Root (ino 1): nlink = 2 + number of sub-directories.
                // Other dirs: nlink = 2 (. and ..).
                let nlink = if ino == self.tree.root() {
                    2 + child_dirs
                } else {
                    2
                };

                Some(FileAttr {
                    ino: INodeNo(ino),
                    size: 0,
                    blocks: 0,
                    atime: now,
                    mtime: now,
                    ctime: now,
                    crtime: now,
                    kind: FileType::Directory,
                    perm: 0o755,
                    nlink,
                    uid: 0,
                    gid: 0,
                    rdev: 0,
                    blksize: 512,
                    flags: 0,
                })
            }
            TreeNode::File { size, .. } => Some(FileAttr {
                ino: INodeNo(ino),
                size: *size,
                blocks: (*size).div_ceil(512),
                atime: now,
                mtime: now,
                ctime: now,
                crtime: now,
                kind: FileType::RegularFile,
                perm: 0o444,
                nlink: 1,
                uid: 0,
                gid: 0,
                rdev: 0,
                blksize: 512,
                flags: 0,
            }),
        }
    }

    /// Read bytes from a file at the given offset.
    ///
    /// - Static files: returns the requested byte slice (clamped to EOF).
    /// - Remote files: fetched and cached if cache_base + runtime_handle are set;
    ///   otherwise returns [`ReadError::Remote`].
    /// - Directories: returns [`ReadError::IsDir`].
    /// - Missing inode: returns [`ReadError::NotFound`].
    fn read_content(&self, ino: u64, offset: usize, size: usize) -> Result<Vec<u8>, ReadError> {
        let node = self.tree.get(ino).ok_or(ReadError::NotFound)?;
        match node {
            TreeNode::Directory { .. } => Err(ReadError::IsDir),
            TreeNode::File { content, .. } => match content {
                FileContent::Static(data) => {
                    if offset >= data.len() {
                        Ok(Vec::new())
                    } else {
                        let end = (offset + size).min(data.len());
                        Ok(data[offset..end].to_vec())
                    }
                }
                FileContent::Remote { url, sha256, .. } => {
                    let (cache_base, handle) = match (&self.cache_base, &self.runtime_handle) {
                        (Some(cb), Some(h)) => (cb, h),
                        _ => return Err(ReadError::Remote),
                    };

                    tracing::debug!("fetching blob {} from {}", sha256, url);
                    let full_data = handle
                        .block_on(fetch_and_cache(url, sha256, cache_base))
                        .map_err(|e| {
                            tracing::error!("fetch failed for {}: {}", sha256, e);
                            ReadError::Fetch
                        })?;

                    if offset >= full_data.len() {
                        Ok(Vec::new())
                    } else {
                        let end = (offset + size).min(full_data.len());
                        Ok(full_data[offset..end].to_vec())
                    }
                }
            },
        }
    }

    /// List child entries of a directory, converting [`NodeKind`] to FUSE
    /// [`FileType`].
    ///
    /// Returns `None` if the inode is not a directory.
    fn list_directory(&self, ino: u64) -> Option<Vec<(u64, String, FileType)>> {
        let entries = self.tree.readdir(ino)?;
        Some(
            entries
                .into_iter()
                .map(|(ino, name, kind)| {
                    let ft = match kind {
                        NodeKind::Directory => FileType::Directory,
                        NodeKind::File => FileType::RegularFile,
                    };
                    (ino, name, ft)
                })
                .collect(),
        )
    }

    /// Total number of nodes in the tree (used for statfs `files` count).
    fn node_count(&self) -> u64 {
        let mut count = 0u64;
        let mut ino = 1u64;
        while self.tree.get(ino).is_some() {
            count += 1;
            ino += 1;
        }
        count
    }

    /// Look up a child inode by name within a parent directory.
    fn lookup_child(&self, parent: u64, name: &str) -> Option<u64> {
        self.tree.lookup(parent, name)
    }
}

// ---------------------------------------------------------------------------

/// Error outcome when reading file content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadError {
    /// Inode does not exist.
    NotFound,
    /// Inode is a directory.
    IsDir,
    /// Content is remote and no cache/runtime is configured.
    Remote,
    /// Fetch-and-cache operation failed (network, hash mismatch, IO).
    Fetch,
}

/// Map a [`ReadError`] to the corresponding FUSE errno.
impl ReadError {
    fn to_errno(self) -> Errno {
        match self {
            ReadError::NotFound => Errno::ENOENT,
            ReadError::IsDir => Errno::EISDIR,
            ReadError::Remote => Errno::EIO,
            ReadError::Fetch => Errno::EIO,
        }
    }
}

// ---------------------------------------------------------------------------
// Filesystem trait implementation
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments, unused_variables)]
impl Filesystem for BlossomFS {
    // ---- Lifecycle ----

    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> io::Result<()> {
        Ok(())
    }

    fn destroy(&mut self) {}

    // ---- Read-only operations ----

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        match self.lookup_child(parent.0, name_str) {
            Some(ino) => match self.make_fileattr(ino) {
                Some(attr) => reply.entry(&TTL, &attr, Generation(0)),
                None => reply.error(Errno::ENOENT),
            },
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.make_fileattr(ino.0) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        if self.tree.get(ino.0).is_none() {
            reply.error(Errno::ENOENT);
            return;
        }
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self.read_content(ino.0, offset as usize, size as usize) {
            Ok(data) => reply.data(&data),
            Err(err) => reply.error(err.to_errno()),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let entries = match self.list_directory(ino.0) {
            Some(e) => e,
            None => {
                reply.error(Errno::ENOTDIR);
                return;
            }
        };

        // Determine parent inode for "..".
        let parent_ino = match self.tree.get(ino.0) {
            Some(TreeNode::Directory { parent, .. }) => *parent,
            _ => 0,
        };

        // "." — offset 1
        if offset < 1 && reply.add(INodeNo(ino.0), 1, FileType::Directory, ".") {
            reply.ok();
            return;
        }

        // ".." — offset 2
        if offset < 2 && reply.add(INodeNo(parent_ino), 2, FileType::Directory, "..") {
            reply.ok();
            return;
        }

        // Children — offsets 3, 4, 5, ...
        for (i, (child_ino, name, kind)) in entries.iter().enumerate() {
            let entry_offset = (3 + i) as u64;
            if offset < entry_offset
                && reply.add(INodeNo(*child_ino), entry_offset, *kind, name.as_str())
            {
                break;
            }
        }

        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let files = self.node_count();
        reply.statfs(0, 0, 0, files, 0, 4096, 255, 4096);
    }

    // ---- Write operations — all return EROFS (read-only filesystem) ----

    fn setattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        reply.error(Errno::EROFS);
    }

    fn mknod(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn mkdir(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn unlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn rmdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn symlink(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _link_name: &OsStr,
        _target: &Path,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn rename(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _newparent: INodeNo,
        _newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EROFS);
    }

    fn link(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _newparent: INodeNo,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _offset: u64,
        _data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        reply.error(Errno::EROFS);
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        // flush is called on close(); for a read-only filesystem this is a no-op.
        // Returning EROFS here would cause errors on every file close, even reads.
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        // fsync on a read-only filesystem is a no-op.
        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        reply.error(Errno::EROFS);
    }

    fn fallocate(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _offset: u64,
        _length: u64,
        _mode: i32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EROFS);
    }

    fn copy_file_range(
        &self,
        _req: &Request,
        _ino_in: INodeNo,
        _fh_in: FileHandle,
        _offset_in: u64,
        _ino_out: INodeNo,
        _fh_out: FileHandle,
        _offset_out: u64,
        _len: u64,
        _flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        reply.error(Errno::EROFS);
    }

    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EROFS);
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::tree::Tree;

    /// Build a test filesystem with known content.
    ///
    /// Tree layout:
    /// ```text
    /// /             (ino 1, dir)
    ///   README.txt  (ino 2, file, "Hello, World!")
    ///   subdir      (ino 3, dir)
    ///   remote.bin  (ino 4, file, Remote content)
    /// ```
    fn make_test_fs() -> BlossomFS {
        let mut tree = Tree::new();
        let _ = tree.add_static_file(1, "README.txt", b"Hello, World!".to_vec());
        let _ = tree.add_directory(1, "subdir");
        let _ = tree.add_remote_file(
            1,
            "remote.bin",
            "https://cdn.example.com/blob".to_string(),
            "aaaa0000bbbb1111cccc2222dddd3333eeee4444ffff5555aabb0000".to_string(),
            42,
            Some("application/octet-stream".to_string()),
        );
        BlossomFS::new(tree)
    }

    // ============== S1: Empty tree root exists ==============

    #[test]
    fn s01_root_exists() {
        let fs = BlossomFS::new(Tree::new());
        assert_eq!(fs.tree.root(), 1);
        assert!(fs.tree.get(1).is_some());
    }

    // ============== S2: getattr on root returns Directory 0o755 ==============

    #[test]
    fn s02_getattr_root() {
        let fs = BlossomFS::new(Tree::new());
        let attr = fs.make_fileattr(1).expect("root attr should exist");
        assert_eq!(attr.ino, INodeNo(1));
        assert_eq!(attr.kind, FileType::Directory);
        assert_eq!(attr.perm, 0o755);
        assert_eq!(attr.nlink, 2); // root with no subdirs: . + ..
        assert_eq!(attr.size, 0);
        assert_eq!(attr.uid, 0);
        assert_eq!(attr.gid, 0);
        assert_eq!(attr.blksize, 512);
    }

    // ============== S3: getattr on nonexistent inode returns None ==============

    #[test]
    fn s03_getattr_nonexistent() {
        let fs = BlossomFS::new(Tree::new());
        assert!(fs.make_fileattr(999).is_none());
    }

    // ============== S4: lookup of nonexistent name returns None ==============

    #[test]
    fn s04_lookup_nonexistent() {
        let fs = BlossomFS::new(Tree::new());
        assert_eq!(fs.lookup_child(1, "nonexistent"), None);
    }

    // ============== S5: lookup of child after add_static_file ==============

    #[test]
    fn s05_lookup_finds_child() {
        let fs = make_test_fs();
        let ino = fs
            .lookup_child(1, "README.txt")
            .expect("README.txt should be found");
        assert_eq!(ino, 2);
    }

    // ============== S6: readdir on root returns child entries ==============

    #[test]
    fn s06_readdir_root() {
        let fs = make_test_fs();
        let entries = fs.list_directory(1).expect("root should be listable");
        assert_eq!(entries.len(), 3);

        let names: Vec<&str> = entries.iter().map(|(_, n, _)| n.as_str()).collect();
        assert!(names.contains(&"README.txt"));
        assert!(names.contains(&"subdir"));
        assert!(names.contains(&"remote.bin"));

        // Verify kind mapping.
        for (_, _, kind) in &entries {
            match () {
                _ if *kind == FileType::Directory => {}
                _ if *kind == FileType::RegularFile => {}
                _ => panic!("unexpected file type"),
            }
        }

        // subdir should be Directory, others RegularFile.
        let subdir = entries
            .iter()
            .find(|(_, n, _)| n == "subdir")
            .expect("subdir entry");
        assert_eq!(subdir.2, FileType::Directory);

        let readme = entries
            .iter()
            .find(|(_, n, _)| n == "README.txt")
            .expect("README.txt entry");
        assert_eq!(readme.2, FileType::RegularFile);
    }

    // ============== S7: readdir on nonexistent / file returns None ==============

    #[test]
    fn s07_readdir_nonexistent() {
        let fs = make_test_fs();
        assert!(fs.list_directory(999).is_none(), "nonexistent inode");
        // readdir on a file should also return None.
        assert!(
            fs.list_directory(2).is_none(),
            "file inode is not a directory"
        );
    }

    // ============== S8: read on static file returns content bytes ==============

    #[test]
    fn s08_read_static_file() {
        let fs = make_test_fs();
        let data = fs.read_content(2, 0, 100).expect("should read README.txt");
        assert_eq!(data, b"Hello, World!");
    }

    // ============== S9: read with offset returns partial content ==============

    #[test]
    fn s09_read_with_offset() {
        let fs = make_test_fs();
        let data = fs
            .read_content(2, 7, 5)
            .expect("partial read should succeed");
        assert_eq!(data, b"World");
    }

    // ============== S10: read past EOF returns empty data ==============

    #[test]
    fn s10_read_past_eof() {
        let fs = make_test_fs();
        // "Hello, World!" is 13 bytes.
        let data = fs
            .read_content(2, 13, 10)
            .expect("read at EOF should succeed with empty data");
        assert!(data.is_empty());

        // Well past EOF.
        let data = fs
            .read_content(2, 100, 10)
            .expect("read past EOF should succeed with empty data");
        assert!(data.is_empty());
    }

    // ============== S11: read on directory returns IsDir ==============

    #[test]
    fn s11_read_directory() {
        let fs = make_test_fs();
        let result = fs.read_content(1, 0, 100);
        assert_eq!(result, Err(ReadError::IsDir));
    }

    // ============== S12: open returns success (file exists) ==============

    #[test]
    fn s12_open_existing() {
        let fs = make_test_fs();
        // The open method is verified indirectly: the tree node exists,
        // which is the only condition open checks before replying.
        assert!(fs.tree.get(2).is_some(), "file inode 2 exists for open");
        assert!(fs.tree.get(1).is_some(), "root inode 1 exists for open");
    }

    // ============== S13: statfs returns non-zero file count ==============

    #[test]
    fn s13_statfs_file_count() {
        let fs = make_test_fs();
        let count = fs.node_count();
        assert!(count > 0, "node count should be > 0");
        // Tree has root(1) + README.txt(2) + subdir(3) + remote.bin(4) = 4 nodes.
        assert_eq!(count, 4);
    }

    // ============== S14: read on remote file returns Remote error (no cache configured) ==============

    #[test]
    fn s14_read_remote_file() {
        // make_test_fs() uses BlossomFS::new(tree) — no cache/runtime configured,
        // so Remote reads return ReadError::Remote.
        let fs = make_test_fs();
        let result = fs.read_content(4, 0, 100);
        assert_eq!(result, Err(ReadError::Remote));
        // Errno doesn't impl PartialEq; verify via Debug (libc::EIO = 5).
        assert_eq!(format!("{:?}", ReadError::Remote.to_errno()), "Errno(5)");
    }

    // ============== Additional: getattr on file ==============

    #[test]
    fn s15_getattr_file() {
        let fs = make_test_fs();
        let attr = fs.make_fileattr(2).expect("README.txt attr should exist");
        assert_eq!(attr.kind, FileType::RegularFile);
        assert_eq!(attr.perm, 0o444);
        assert_eq!(attr.nlink, 1);
        assert_eq!(attr.size, 13); // "Hello, World!"
        assert_eq!(attr.blocks, 1); // ceil(13/512) = 1
    }

    // ============== Additional: root nlink with subdirectory ==============

    #[test]
    fn s16_root_nlink_with_subdir() {
        let fs = make_test_fs();
        let attr = fs.make_fileattr(1).expect("root attr should exist");
        // Root has 1 sub-directory ("subdir"), so nlink = 2 + 1 = 3.
        assert_eq!(attr.nlink, 3);
    }

    // ============== Additional: error mapping (Debug repr) ==============

    #[test]
    fn s17_error_mappings() {
        // Errno doesn't impl PartialEq; verify via Debug (libc errno numbers).
        assert_eq!(format!("{:?}", ReadError::NotFound.to_errno()), "Errno(2)"); // ENOENT
        assert_eq!(format!("{:?}", ReadError::IsDir.to_errno()), "Errno(21)"); // EISDIR
        assert_eq!(format!("{:?}", ReadError::Remote.to_errno()), "Errno(5)"); // EIO
        assert_eq!(format!("{:?}", ReadError::Fetch.to_errno()), "Errno(5)"); // EIO
    }

    // ============== Lazy fetch tests (S18-S22) ==============

    use sha2::{Digest, Sha256};

    fn sha256_hex(content: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content);
        hex::encode(hasher.finalize())
    }

    /// Build a tree with a single Remote file pointing at `url` with the given content hash.
    fn make_cache_test_tree(url: String, sha256: String, size: u64) -> Tree {
        let mut tree = Tree::new();
        tree.add_remote_file(
            tree.root(),
            "remote.bin",
            url,
            sha256,
            size,
            Some("application/octet-stream".to_string()),
        );
        tree
    }

    /// Build a BlossomFS with cache support for lazy-fetch tests.
    fn make_cache_fs(tree: Tree, cache_base: PathBuf, handle: tokio::runtime::Handle) -> BlossomFS {
        BlossomFS::new_with_cache(tree, cache_base, handle)
    }

    // ============== S18: Remote file with cache — fetch and return content ==============

    #[test]
    fn s18_remote_fetch_returns_content() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();

        let content = b"Hello from BlossomFS lazy fetch!";
        let hash = sha256_hex(content);

        let mock_server_uri = rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};

            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/blob"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
                .mount(&server)
                .await;
            server.uri()
        });

        let url = format!("{}/blob", mock_server_uri);
        let tree = make_cache_test_tree(url, hash.clone(), content.len() as u64);

        let cache_dir = tempfile::tempdir().unwrap();
        let fs = make_cache_fs(tree, cache_dir.path().to_path_buf(), handle);

        // remote.bin is inode 2 (root=1, file=2)
        let data = fs.read_content(2, 0, 1024).expect("should fetch and read");
        assert_eq!(data, content.to_vec());
    }

    // ============== S19: Second read serves from cache (no HTTP) ==============

    #[test]
    fn s19_second_read_serves_from_cache() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();

        let content = b"cached blob data for s19";
        let hash = sha256_hex(content);

        let (mock_server_uri, mock_server_handle) = rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};

            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/blob19"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
                .expect(1) // Only first call hits the server
                .mount(&server)
                .await;
            (server.uri(), server)
        });

        let url = format!("{}/blob19", mock_server_uri);
        let tree = make_cache_test_tree(url, hash.clone(), content.len() as u64);

        let cache_dir = tempfile::tempdir().unwrap();
        let fs = make_cache_fs(tree, cache_dir.path().to_path_buf(), handle);

        // First read: fetches from HTTP
        let data1 = fs
            .read_content(2, 0, 1024)
            .expect("first read should succeed");
        assert_eq!(data1, content.to_vec());

        // Second read: served from cache (mock expects exactly 1 request)
        let data2 = fs
            .read_content(2, 0, 1024)
            .expect("second read should succeed");
        assert_eq!(data2, content.to_vec());

        // Verify cache file exists on disk
        assert!(
            crate::cache::object_cache::cache_exists(cache_dir.path(), &hash),
            "cache file should exist on disk"
        );

        // Drop the mock server handle to verify expectations were met
        drop(mock_server_handle);
    }

    // ============== S20: Read Remote file with offset ==============

    #[test]
    fn s20_remote_read_with_offset() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();

        let content = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        let hash = sha256_hex(content);

        let mock_server_uri = rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};

            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/blob20"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
                .mount(&server)
                .await;
            server.uri()
        });

        let url = format!("{}/blob20", mock_server_uri);
        let tree = make_cache_test_tree(url, hash, content.len() as u64);

        let cache_dir = tempfile::tempdir().unwrap();
        let fs = make_cache_fs(tree, cache_dir.path().to_path_buf(), handle);

        // Read from offset 10, size 10 → should return "ABCDEFGHIJ"
        let data = fs
            .read_content(2, 10, 10)
            .expect("offset read should succeed");
        assert_eq!(data, b"ABCDEFGHIJ");
    }

    // ============== S21: Fetch failure (404) returns Fetch error ==============

    #[test]
    fn s21_fetch_failure_404_returns_fetch_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();

        let hash = sha256_hex(b"some content that won't be fetched");

        let mock_server_uri = rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};

            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/notfound"))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;
            server.uri()
        });

        let url = format!("{}/notfound", mock_server_uri);
        let tree = make_cache_test_tree(url, hash, 100);

        let cache_dir = tempfile::tempdir().unwrap();
        let fs = make_cache_fs(tree, cache_dir.path().to_path_buf(), handle);

        let result = fs.read_content(2, 0, 100);
        assert_eq!(result, Err(ReadError::Fetch));
        // to_errno() maps Fetch → EIO (5)
        assert_eq!(format!("{:?}", ReadError::Fetch.to_errno()), "Errno(5)");
    }

    // ============== S22: Remote without cache returns Remote error (backward compat) ==============

    #[test]
    fn s22_remote_without_cache_returns_remote_error() {
        // BlossomFS::new(tree) — no cache configured
        let fs = make_test_fs();
        // make_test_fs has remote.bin at inode 4
        let result = fs.read_content(4, 0, 100);
        assert_eq!(result, Err(ReadError::Remote));
        assert_eq!(format!("{:?}", ReadError::Remote.to_errno()), "Errno(5)");
    }
}
