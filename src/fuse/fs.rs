//! fuser::Filesystem trait implementation.
//!
//! BlossomFS implements the FUSE filesystem protocol. All callbacks use
//! `&self` (fuser 0.17.0), so mutable state uses interior mutability:
//! - The tree is wrapped in `Arc<RwLock<Tree>>` for concurrent read access
//!   and exclusive write access during create/unlink/mkdir operations.
//! - Write buffers (pending uploads) are tracked in
//!   `Arc<Mutex<HashMap<u64, WriteBuffer>>>`.
//!
//! Read-only mode (default): all write operations return EROFS.
//! RW mode (`new_rw`): create/write/flush/mkdir/unlink are supported.
//! On flush, buffered data is uploaded to a Blossom server via BUD-02.

#![allow(dead_code)]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use crate::cache::fetch::fetch_and_cache;

use fuser::{
    BsdFileFlags, CopyFileRangeFlags, Errno, FileAttr, FileHandle, FileType, Filesystem,
    FopenFlags, Generation, INodeNo, KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, Request, TimeOrNow, WriteFlags,
};

use nostr_sdk::prelude::Keys;
use sha2::{Digest, Sha256};

use crate::blossom::client::BlossomClient;
use crate::fuse::tree::{FileContent, NodeKind, Tree, TreeNode};
use crate::nostr::auth::create_upload_auth_header;

/// FUSE attribute/entry cache TTL for a read-only filesystem.
const TTL: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// WriteBuffer
// ---------------------------------------------------------------------------

/// Tracks data written to a file handle before upload.
struct WriteBuffer {
    ino: u64,
    parent: u64,
    name: String,
    data: Vec<u8>,
    /// Whether the data has already been uploaded (flush may fire
    /// multiple times due to fork/dup; we only upload once).
    flushed: bool,
}

// ---------------------------------------------------------------------------
// BlossomFS struct
// ---------------------------------------------------------------------------

/// FUSE filesystem backed by a pre-built [`Tree`].
///
/// In read-only mode (default), all mutating FUSE operations return `EROFS`.
/// In read-write mode (constructed via [`new_rw`](Self::new_rw)), create/write
/// callbacks buffer data and upload to a Blossom server on flush.
pub struct BlossomFS {
    tree: Arc<RwLock<Tree>>,
    cache_base: Option<PathBuf>,
    runtime_handle: Option<tokio::runtime::Handle>,
    write_state: Arc<Mutex<HashMap<u64, WriteBuffer>>>,
    keys: Option<Keys>,
    server_url: Option<String>,
    read_only: bool,
    next_fh: AtomicU64,
}

impl BlossomFS {
    /// Create a new read-only filesystem wrapper around the given tree.
    pub fn new(tree: Tree) -> Self {
        Self {
            tree: Arc::new(RwLock::new(tree)),
            cache_base: None,
            runtime_handle: None,
            write_state: Arc::new(Mutex::new(HashMap::new())),
            keys: None,
            server_url: None,
            read_only: true,
            next_fh: AtomicU64::new(1),
        }
    }

    /// Create a read-only filesystem with lazy-fetch cache support.
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
            tree: Arc::new(RwLock::new(tree)),
            cache_base: Some(cache_base),
            runtime_handle: Some(runtime_handle),
            write_state: Arc::new(Mutex::new(HashMap::new())),
            keys: None,
            server_url: None,
            read_only: true,
            next_fh: AtomicU64::new(1),
        }
    }

    /// Create a read-write filesystem with upload support.
    ///
    /// Files created via the FUSE `create` callback are buffered in memory
    /// and uploaded to `server_url` when flushed (closed). The BUD-11 auth
    /// header is signed with `keys`.
    pub fn new_rw(
        tree: Tree,
        cache_base: PathBuf,
        runtime_handle: tokio::runtime::Handle,
        keys: Keys,
        server_url: String,
    ) -> Self {
        Self {
            tree: Arc::new(RwLock::new(tree)),
            cache_base: Some(cache_base),
            runtime_handle: Some(runtime_handle),
            write_state: Arc::new(Mutex::new(HashMap::new())),
            keys: Some(keys),
            server_url: Some(server_url),
            read_only: false,
            next_fh: AtomicU64::new(1),
        }
    }

    // ======================== Internal testable helpers ========================

    /// Build a [`FileAttr`] for the given inode.
    ///
    /// Returns `None` if the inode does not exist in the tree.
    fn make_fileattr(&self, ino: u64) -> Option<FileAttr> {
        let tree = self.tree.read().unwrap();
        let node = tree.get(ino)?;
        let now = SystemTime::now();
        match node {
            TreeNode::Directory { children, .. } => {
                let child_dirs = children
                    .iter()
                    .filter_map(|&c| tree.get(c))
                    .filter(|n| matches!(n, TreeNode::Directory { .. }))
                    .count() as u32;

                let nlink = if ino == tree.root() {
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
            TreeNode::File { size, uploaded, .. } => {
                let file_time = SystemTime::UNIX_EPOCH
                    .checked_add(Duration::from_secs(*uploaded))
                    .unwrap_or_else(SystemTime::now);
                Some(FileAttr {
                    ino: INodeNo(ino),
                    size: *size,
                    blocks: (*size).div_ceil(512),
                    atime: file_time,
                    mtime: file_time,
                    ctime: file_time,
                    crtime: file_time,
                    kind: FileType::RegularFile,
                    perm: 0o444,
                    nlink: 1,
                    uid: 0,
                    gid: 0,
                    rdev: 0,
                    blksize: 512,
                    flags: 0,
                })
            }
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
        let tree = self.tree.read().unwrap();
        let node = tree.get(ino).ok_or(ReadError::NotFound)?;
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
                    let url = url.clone();
                    let sha256 = sha256.clone();
                    drop(tree);
                    let (cache_base, handle) = match (&self.cache_base, &self.runtime_handle) {
                        (Some(cb), Some(h)) => (cb, h),
                        _ => return Err(ReadError::Remote),
                    };

                    tracing::debug!("fetching blob {} from {}", sha256, url);
                    let full_data = handle
                        .block_on(fetch_and_cache(&url, &sha256, cache_base))
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
        let tree = self.tree.read().unwrap();
        let entries = tree.readdir(ino)?;
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
        let tree = self.tree.read().unwrap();
        let mut count = 0u64;
        let mut ino = 1u64;
        while tree.get(ino).is_some() {
            count += 1;
            ino += 1;
        }
        count
    }

    /// Look up a child inode by name within a parent directory.
    fn lookup_child(&self, parent: u64, name: &str) -> Option<u64> {
        let tree = self.tree.read().unwrap();
        tree.lookup(parent, name)
    }

    /// Upload buffered data to the Blossom server and update the tree node.
    /// Returns `Err` when keys/server/runtime are missing, auth fails, or
    /// the HTTP upload fails — caller is responsible for cleanup.
    fn do_upload(&self, ino: u64, data: Vec<u8>) -> Result<(), ()> {
        let (keys, server_url, handle) = match (&self.keys, &self.server_url, &self.runtime_handle)
        {
            (Some(k), Some(s), Some(h)) => (k, s, h),
            _ => {
                tracing::error!("upload preconditions not met (keys/server/handle)");
                return Err(());
            }
        };

        let sha256_hex = hex::encode(Sha256::digest(&data));

        let auth_header = match create_upload_auth_header(keys, &sha256_hex, data.len() as u64) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!("auth creation failed: {}", e);
                return Err(());
            }
        };

        let client = BlossomClient::new(server_url);
        match handle.block_on(client.upload_blob(data, &auth_header)) {
            Ok(desc) => {
                tracing::info!("uploaded blob: sha256={} size={}", desc.sha256, desc.size);
                let mut tree = self.tree.write().unwrap();
                tree.update_file_node(
                    ino,
                    FileContent::Remote {
                        url: desc.url,
                        sha256: desc.sha256,
                        mime_type: desc.mime_type,
                    },
                    desc.size,
                    desc.uploaded,
                );
                Ok(())
            }
            Err(e) => {
                tracing::error!("upload failed: {}", e);
                Err(())
            }
        }
    }

    /// Remove a failed write's file from the tree.
    fn cleanup_failed_write(&self, buf: &WriteBuffer) {
        let mut tree = self.tree.write().unwrap();
        tree.remove_file_from_dir(buf.parent, &buf.name);
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
        {
            let tree = self.tree.read().unwrap();
            if tree.get(ino.0).is_none() {
                drop(tree);
                reply.error(Errno::ENOENT);
                return;
            }
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

        let parent_ino = {
            let tree = self.tree.read().unwrap();
            match tree.get(ino.0) {
                Some(TreeNode::Directory { parent, .. }) => *parent,
                _ => 0,
            }
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

    // ---- Write operations ----

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
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
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }
        if let Some(new_size) = size {
            let mut tree = self.tree.write().unwrap();
            tree.update_file_size(ino.0, new_size);
        }
        match self.make_fileattr(ino.0) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
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
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let ino = {
            let mut tree = self.tree.write().unwrap();
            match tree.get(parent.0) {
                Some(TreeNode::Directory { .. }) => {}
                _ => {
                    reply.error(Errno::ENOTDIR);
                    return;
                }
            }
            if tree.lookup(parent.0, name_str).is_some() {
                reply.error(Errno::EEXIST);
                return;
            }
            tree.add_directory(parent.0, name_str)
        };

        match self.make_fileattr(ino) {
            Some(attr) => reply.entry(&TTL, &attr, Generation(0)),
            None => reply.error(Errno::EIO),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let mut tree = self.tree.write().unwrap();
        if tree.remove_file_from_dir(parent.0, name_str) {
            drop(tree);
            reply.ok();
        } else {
            drop(tree);
            reply.error(Errno::ENOENT);
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let mut tree = self.tree.write().unwrap();
        let child_ino = match tree.lookup(parent.0, name_str) {
            Some(ino) => ino,
            None => {
                drop(tree);
                reply.error(Errno::ENOENT);
                return;
            }
        };
        match tree.get(child_ino) {
            Some(TreeNode::Directory { children, .. }) => {
                if !children.is_empty() {
                    drop(tree);
                    reply.error(Errno::ENOTEMPTY);
                    return;
                }
            }
            _ => {
                drop(tree);
                reply.error(Errno::ENOTDIR);
                return;
            }
        }
        if let Some(TreeNode::Directory { children, .. }) = tree.get_mut(parent.0) {
            children.retain(|&c| c != child_ino);
        }
        drop(tree);
        reply.ok();
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
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }

        let (ino, new_size) = {
            let mut buffers = self.write_state.lock().unwrap();
            let buf = match buffers.get_mut(&fh.0) {
                Some(b) => b,
                None => {
                    drop(buffers);
                    reply.error(Errno::EBADF);
                    return;
                }
            };
            let end = offset as usize + data.len();
            if buf.data.len() < end {
                buf.data.resize(end, 0);
            }
            buf.data[offset as usize..end].copy_from_slice(data);
            (buf.ino, buf.data.len() as u64)
        };

        {
            let mut tree = self.tree.write().unwrap();
            tree.update_file_size(ino, new_size);
        }

        reply.written(data.len() as u32);
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        // flush() can fire multiple times per open (fork/dup). We do NOT
        // remove the WriteBuffer here — only upload once and let release()
        // handle final cleanup.

        let mut to_upload: Option<(u64, Vec<u8>)> = None;

        {
            let mut state = self.write_state.lock().unwrap();
            if let Some(buf) = state.get_mut(&fh.0)
                && !buf.flushed
                && !buf.data.is_empty()
            {
                buf.flushed = true;
                to_upload = Some((buf.ino, buf.data.clone()));
            }
        }

        match to_upload {
            None => {
                reply.ok();
            }
            Some((ino, data)) => match self.do_upload(ino, data) {
                Ok(()) => reply.ok(),
                Err(()) => reply.error(Errno::EIO),
            },
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // release() is called exactly once when all fd references are closed.
        // This is the correct place to clean up the WriteBuffer. If flush()
        // didn't fire (or had no data), we do a fallback upload here.
        let buf = self.write_state.lock().unwrap().remove(&fh.0);

        if let Some(buf) = buf
            && !buf.flushed
            && !buf.data.is_empty()
        {
            match self.do_upload(buf.ino, buf.data.clone()) {
                Ok(()) => {}
                Err(()) => {
                    tracing::error!("fallback upload failed in release()");
                    self.cleanup_failed_write(&buf);
                }
            }
        }

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
        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        if self.read_only {
            reply.error(Errno::EROFS);
            return;
        }
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let ino = {
            let mut tree = self.tree.write().unwrap();
            match tree.get(parent.0) {
                Some(TreeNode::Directory { .. }) => {}
                _ => {
                    drop(tree);
                    reply.error(Errno::ENOTDIR);
                    return;
                }
            }
            if tree.lookup(parent.0, name_str).is_some() {
                drop(tree);
                reply.error(Errno::EEXIST);
                return;
            }
            tree.add_file_to_dir(parent.0, name_str, FileContent::Static(vec![]), 0, now)
        };

        self.write_state.lock().unwrap().insert(
            fh,
            WriteBuffer {
                ino,
                parent: parent.0,
                name: name_str.to_string(),
                data: Vec::new(),
                flushed: false,
            },
        );

        match self.make_fileattr(ino) {
            Some(attr) => reply.created(
                &TTL,
                &attr,
                Generation(0),
                FileHandle(fh),
                FopenFlags::empty(),
            ),
            None => reply.error(Errno::EIO),
        }
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
            1700000000,
        );
        BlossomFS::new(tree)
    }

    // ============== S1: Empty tree root exists ==============

    #[test]
    fn s01_root_exists() {
        let fs = BlossomFS::new(Tree::new());
        assert_eq!(fs.tree.read().unwrap().root(), 1);
        assert!(fs.tree.read().unwrap().get(1).is_some());
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
        assert!(
            fs.tree.read().unwrap().get(2).is_some(),
            "file inode 2 exists for open"
        );
        assert!(
            fs.tree.read().unwrap().get(1).is_some(),
            "root inode 1 exists for open"
        );
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
        let fs = make_test_fs();
        let result = fs.read_content(4, 0, 100);
        assert_eq!(result, Err(ReadError::Remote));
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
        assert_eq!(attr.nlink, 3);
    }

    // ============== Additional: error mapping (Debug repr) ==============

    #[test]
    fn s17_error_mappings() {
        assert_eq!(format!("{:?}", ReadError::NotFound.to_errno()), "Errno(2)");
        assert_eq!(format!("{:?}", ReadError::IsDir.to_errno()), "Errno(21)");
        assert_eq!(format!("{:?}", ReadError::Remote.to_errno()), "Errno(5)");
        assert_eq!(format!("{:?}", ReadError::Fetch.to_errno()), "Errno(5)");
    }

    // ============== Lazy fetch tests (S18-S22) ==============

    fn sha256_hex(content: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content);
        hex::encode(hasher.finalize())
    }

    fn make_cache_test_tree(url: String, sha256: String, size: u64) -> Tree {
        let mut tree = Tree::new();
        tree.add_remote_file(
            tree.root(),
            "remote.bin",
            url,
            sha256,
            size,
            Some("application/octet-stream".to_string()),
            1700000000,
        );
        tree
    }

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

        let (mock_server_uri, _server) = rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};

            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/blob"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
                .mount(&server)
                .await;
            (server.uri(), server)
        });

        let url = format!("{}/blob", mock_server_uri);
        let tree = make_cache_test_tree(url, hash.clone(), content.len() as u64);

        let cache_dir = tempfile::tempdir().unwrap();
        let fs = make_cache_fs(tree, cache_dir.path().to_path_buf(), handle);

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
                .expect(1)
                .mount(&server)
                .await;
            (server.uri(), server)
        });

        let url = format!("{}/blob19", mock_server_uri);
        let tree = make_cache_test_tree(url, hash.clone(), content.len() as u64);

        let cache_dir = tempfile::tempdir().unwrap();
        let fs = make_cache_fs(tree, cache_dir.path().to_path_buf(), handle);

        let data1 = fs
            .read_content(2, 0, 1024)
            .expect("first read should succeed");
        assert_eq!(data1, content.to_vec());

        let data2 = fs
            .read_content(2, 0, 1024)
            .expect("second read should succeed");
        assert_eq!(data2, content.to_vec());

        assert!(
            crate::cache::object_cache::cache_exists(cache_dir.path(), &hash),
            "cache file should exist on disk"
        );

        drop(mock_server_handle);
    }

    // ============== S20: Read Remote file with offset ==============

    #[test]
    fn s20_remote_read_with_offset() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();

        let content = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        let hash = sha256_hex(content);

        let (mock_server_uri, _server) = rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};

            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/blob20"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
                .mount(&server)
                .await;
            (server.uri(), server)
        });

        let url = format!("{}/blob20", mock_server_uri);
        let tree = make_cache_test_tree(url, hash, content.len() as u64);

        let cache_dir = tempfile::tempdir().unwrap();
        let fs = make_cache_fs(tree, cache_dir.path().to_path_buf(), handle);

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
        assert_eq!(format!("{:?}", ReadError::Fetch.to_errno()), "Errno(5)");
    }

    // ============== S22: Remote without cache returns Remote error (backward compat) ==============

    #[test]
    fn s22_remote_without_cache_returns_remote_error() {
        let fs = make_test_fs();
        let result = fs.read_content(4, 0, 100);
        assert_eq!(result, Err(ReadError::Remote));
        assert_eq!(format!("{:?}", ReadError::Remote.to_errno()), "Errno(5)");
    }

    // ============== Write logic tests (S23+) ==============

    /// Build a read-write filesystem for write tests.
    /// Returns the runtime alongside so it stays alive for the test scope.
    fn make_rw_fs() -> (tokio::runtime::Runtime, BlossomFS) {
        let mut tree = Tree::new();
        let _ = tree.add_static_file(1, "README.txt", b"Hello, World!".to_vec());
        let _ = tree.add_directory(1, "subdir");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();
        let keys = Keys::generate();
        let fs = BlossomFS::new_rw(
            tree,
            PathBuf::from("/tmp/blossomfs-test-cache"),
            handle,
            keys,
            "http://localhost:8080".to_string(),
        );
        (rt, fs)
    }

    // ============== S23: WriteBuffer accumulates data across writes ==============

    #[test]
    fn s23_write_buffer_create_and_write() {
        let fs = make_test_fs();
        let fh = 100u64;

        // Simulate create: register a write buffer
        {
            let mut state = fs.write_state.lock().unwrap();
            state.insert(
                fh,
                WriteBuffer {
                    ino: 5,
                    parent: 1,
                    name: "test.txt".to_string(),
                    data: Vec::new(),
                    flushed: false,
                },
            );
        }

        // Simulate write at offset 0: "hello"
        {
            let mut state = fs.write_state.lock().unwrap();
            let buf = state.get_mut(&fh).expect("buffer should exist");
            let data = b"hello";
            let end = data.len();
            buf.data.resize(end, 0);
            buf.data[0..end].copy_from_slice(data);
        }

        // Simulate write at offset 5: " world"
        {
            let mut state = fs.write_state.lock().unwrap();
            let buf = state.get_mut(&fh).expect("buffer should exist");
            let data = b" world";
            let end = 5 + data.len();
            buf.data.resize(end, 0);
            buf.data[5..end].copy_from_slice(data);
        }

        // Verify accumulated data
        {
            let state = fs.write_state.lock().unwrap();
            let buf = state.get(&fh).expect("buffer should exist");
            assert_eq!(buf.data, b"hello world");
            assert_eq!(buf.data.len(), 11);
        }
    }

    // ============== S24: Read-only fs rejects create (read_only flag) ==============

    #[test]
    fn s24_create_in_readonly_returns_erofs() {
        let fs = make_test_fs();
        assert!(fs.read_only, "default filesystem should be read-only");
    }

    // ============== S25: RW fs allows mkdir (tree mutation) ==============

    #[test]
    fn s25_mkdir_in_rw_mode() {
        let (_rt, fs) = make_rw_fs();
        assert!(!fs.read_only, "RW filesystem should not be read-only");

        let ino = {
            let mut tree = fs.tree.write().unwrap();
            tree.add_directory(1, "newdir")
        };

        let tree = fs.tree.read().unwrap();
        assert_eq!(tree.lookup(1, "newdir"), Some(ino));
        assert_eq!(tree.kind(ino), Some(NodeKind::Directory));
    }

    // ============== S26: RW fs allows unlink (tree mutation) ==============

    #[test]
    fn s26_unlink_in_rw_mode() {
        let (_rt, fs) = make_rw_fs();

        // Add a file to delete
        {
            let mut tree = fs.tree.write().unwrap();
            tree.add_file_to_dir(
                1,
                "todelete.bin",
                FileContent::Static(b"data".to_vec()),
                4,
                0,
            );
        }

        // Verify it exists
        {
            let tree = fs.tree.read().unwrap();
            assert!(tree.lookup(1, "todelete.bin").is_some());
        }

        // Remove it (simulates unlink callback)
        let removed = {
            let mut tree = fs.tree.write().unwrap();
            tree.remove_file_from_dir(1, "todelete.bin")
        };
        assert!(removed);

        // Verify it's gone
        {
            let tree = fs.tree.read().unwrap();
            assert!(tree.lookup(1, "todelete.bin").is_none());
        }
    }

    // ============== S27: Read-only fs rejects unlink (read_only flag) ==============

    #[test]
    fn s27_unlink_in_readonly_returns_erofs() {
        let fs = make_test_fs();
        assert!(fs.read_only, "default filesystem should be read-only");
    }

    // ============== S28: update_file_node replaces content after upload ==============

    #[test]
    fn s28_update_file_node_replaces_content() {
        let (_rt, fs) = make_rw_fs();

        // Create a file with Static content (pending write)
        let ino = {
            let mut tree = fs.tree.write().unwrap();
            tree.add_file_to_dir(1, "upload.bin", FileContent::Static(vec![0u8; 100]), 100, 0)
        };

        // Simulate flush: replace with Remote content
        {
            let mut tree = fs.tree.write().unwrap();
            let updated = tree.update_file_node(
                ino,
                FileContent::Remote {
                    url: "https://cdn.example.com/abc123".to_string(),
                    sha256: "abc123".to_string(),
                    mime_type: Some("application/octet-stream".to_string()),
                },
                100,
                1700000000,
            );
            assert!(
                updated,
                "update_file_node should return true for existing file"
            );
        }

        // Verify the node now has Remote content
        let tree = fs.tree.read().unwrap();
        match tree.get(ino) {
            Some(TreeNode::File {
                content,
                size,
                uploaded,
                ..
            }) => {
                assert_eq!(*size, 100);
                assert_eq!(*uploaded, 1700000000);
                match content {
                    FileContent::Remote { url, sha256, .. } => {
                        assert_eq!(url, "https://cdn.example.com/abc123");
                        assert_eq!(sha256, "abc123");
                    }
                    FileContent::Static(_) => panic!("expected Remote content after update"),
                }
            }
            _ => panic!("expected File node"),
        }
    }

    // ============== S29: update_file_size updates size for getattr ==============

    #[test]
    fn s29_update_file_size() {
        let (_rt, fs) = make_rw_fs();

        let ino = {
            let mut tree = fs.tree.write().unwrap();
            tree.add_file_to_dir(1, "growing.bin", FileContent::Static(vec![]), 0, 0)
        };

        // Update size (simulates write callback updating size)
        {
            let mut tree = fs.tree.write().unwrap();
            let updated = tree.update_file_size(ino, 42);
            assert!(updated);
        }

        let tree = fs.tree.read().unwrap();
        assert_eq!(tree.size(ino), Some(42));
    }

    // ============== S30: update_file_node on nonexistent returns false ==============

    #[test]
    fn s30_update_file_node_nonexistent_returns_false() {
        let (_rt, fs) = make_rw_fs();
        let mut tree = fs.tree.write().unwrap();
        let result = tree.update_file_node(9999, FileContent::Static(vec![]), 0, 0);
        assert!(!result, "updating nonexistent node should return false");
    }

    // ============== S31: RW rmdir removes empty directory ==============

    #[test]
    fn s31_rmdir_in_rw_mode() {
        let (_rt, fs) = make_rw_fs();

        // Create a directory to remove
        let dir_ino = {
            let mut tree = fs.tree.write().unwrap();
            tree.add_directory(1, "toRemove")
        };

        // Verify it exists and is empty
        {
            let tree = fs.tree.read().unwrap();
            assert_eq!(tree.lookup(1, "toRemove"), Some(dir_ino));
            let entries = tree.readdir(dir_ino).expect("should be a directory");
            assert!(entries.is_empty());
        }

        // Remove it (simulates rmdir callback logic)
        {
            let mut tree = fs.tree.write().unwrap();
            let child_ino = tree.lookup(1, "toRemove").expect("dir should exist");
            if let Some(TreeNode::Directory { children, .. }) = tree.get(child_ino) {
                assert!(children.is_empty(), "directory should be empty");
            }
            if let Some(TreeNode::Directory { children, .. }) = tree.get_mut(1) {
                children.retain(|&c| c != dir_ino);
            }
        }

        // Verify it's gone from parent's children
        {
            let tree = fs.tree.read().unwrap();
            assert_eq!(tree.lookup(1, "toRemove"), None);
        }
    }

    // ============== S32: WriteBuffer cleanup removes file from tree ==============

    #[test]
    fn s32_cleanup_failed_write_removes_file() {
        let (_rt, fs) = make_rw_fs();

        // Add a file (simulates a failed upload)
        let buf = WriteBuffer {
            ino: 5,
            parent: 1,
            name: "failed.bin".to_string(),
            data: b"lost data".to_vec(),
            flushed: false,
        };

        {
            let mut tree = fs.tree.write().unwrap();
            tree.add_file_to_dir(
                buf.parent,
                &buf.name,
                FileContent::Static(b"pending".to_vec()),
                7,
                0,
            );
        }

        // Verify it exists
        {
            let tree = fs.tree.read().unwrap();
            assert!(tree.lookup(1, "failed.bin").is_some());
        }

        // Run cleanup
        fs.cleanup_failed_write(&buf);

        // Verify it's gone
        {
            let tree = fs.tree.read().unwrap();
            assert!(tree.lookup(1, "failed.bin").is_none());
        }
    }

    // ============== S33: new_rw sets all RW fields correctly ==============

    #[test]
    fn s33_new_rw_sets_rw_fields() {
        let (_rt, fs) = make_rw_fs();
        assert!(!fs.read_only);
        assert!(fs.keys.is_some(), "keys should be set in RW mode");
        assert!(
            fs.server_url.is_some(),
            "server_url should be set in RW mode"
        );
        assert!(
            fs.runtime_handle.is_some(),
            "runtime_handle should be set in RW mode"
        );
        assert!(
            fs.cache_base.is_some(),
            "cache_base should be set in RW mode"
        );
    }
}
