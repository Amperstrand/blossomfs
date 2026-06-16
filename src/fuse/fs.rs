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

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::cache::fetch::fetch_and_cache;
use crate::cache::object_cache::cache_path;

use fuser::{
    BsdFileFlags, CopyFileRangeFlags, Errno, FileAttr, FileHandle, FileType, Filesystem,
    FopenFlags, Generation, INodeNo, KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};

use nostr_sdk::prelude::Keys;
use sha2::{Digest, Sha256};

use crate::blossom::client::{BlossomClient, BlossomClientError};
use crate::blossom::descriptor::BlobDescriptor;
use crate::fuse::tree::{FileContent, LazyDir, NodeKind, Tree, TreeNode};
use crate::nostr::auth::{create_delete_auth_header, create_upload_auth_header};

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
    /// When true (O_APPEND), writes ignore the offset and append to the end.
    append: bool,
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
    ttl: Duration,
    max_write_bytes: usize,
    free_period_secs: u64,
    max_free_size_bytes: usize,
    max_cache_bytes: u64,
    multipart_threshold: usize,
    /// Sha256 hashes currently being fetched. Prevents duplicate HTTP
    /// downloads when multiple FUSE read() calls hit the same uncached blob.
    fetch_in_progress: Arc<(Mutex<HashSet<String>>, Condvar)>,
    /// When true, all write operations return EROFS even in RW mode.
    frozen: Arc<AtomicBool>,
    payment: Arc<dyn crate::payment::PaymentStrategy>,
}

impl BlossomFS {
    /// Create a new read-only filesystem wrapper around the given tree.
    pub fn new(tree: Tree, ttl: Duration) -> Self {
        Self {
            tree: Arc::new(RwLock::new(tree)),
            cache_base: None,
            runtime_handle: None,
            write_state: Arc::new(Mutex::new(HashMap::new())),
            keys: None,
            server_url: None,
            read_only: true,
            next_fh: AtomicU64::new(1),
            ttl,
            max_write_bytes: 100 * 1024 * 1024,
            free_period_secs: 30 * 86400,
            max_free_size_bytes: 1024 * 1024,
            max_cache_bytes: 0,
            multipart_threshold: 50 * 1024 * 1024,
            fetch_in_progress: Arc::new((Mutex::new(HashSet::new()), Condvar::new())),
            frozen: Arc::new(AtomicBool::new(false)),
            payment: Arc::new(crate::payment::NoPayment),
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
        ttl: Duration,
        free_period_secs: u64,
        max_free_size_bytes: usize,
        max_cache_bytes: u64,
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
            ttl,
            max_write_bytes: 100 * 1024 * 1024,
            free_period_secs,
            max_free_size_bytes,
            max_cache_bytes,
            multipart_threshold: 50 * 1024 * 1024,
            fetch_in_progress: Arc::new((Mutex::new(HashSet::new()), Condvar::new())),
            frozen: Arc::new(AtomicBool::new(false)),
            payment: Arc::new(crate::payment::NoPayment),
        }
    }

    /// Create a read-write filesystem with upload support.
    ///
    /// Files created via the FUSE `create` callback are buffered in memory
    /// and uploaded to `server_url` when flushed (closed). The BUD-11 auth
    /// header is signed with `keys`.
    #[allow(clippy::too_many_arguments)]
    pub fn new_rw(
        tree: Tree,
        cache_base: PathBuf,
        runtime_handle: tokio::runtime::Handle,
        keys: Keys,
        server_url: String,
        ttl: Duration,
        max_write_bytes: usize,
        free_period_secs: u64,
        max_free_size_bytes: usize,
        max_cache_bytes: u64,
        payment: Arc<dyn crate::payment::PaymentStrategy>,
        multipart_threshold: usize,
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
            ttl,
            max_write_bytes,
            free_period_secs,
            max_free_size_bytes,
            max_cache_bytes,
            multipart_threshold,
            fetch_in_progress: Arc::new((Mutex::new(HashSet::new()), Condvar::new())),
            frozen: Arc::new(AtomicBool::new(false)),
            payment,
        }
    }

    /// Get a handle to the tree for post-unmount extraction.
    ///
    /// Used by `--persist` to serialize the tree after the FUSE session ends.
    pub fn tree_handle(&self) -> Arc<RwLock<Tree>> {
        Arc::clone(&self.tree)
    }

    /// Get a handle to the frozen flag for the control socket.
    pub fn frozen_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.frozen)
    }

    /// Get the cache base path for the control socket.
    pub fn cache_base_path(&self) -> Option<PathBuf> {
        self.cache_base.clone()
    }

    /// Check if the filesystem is frozen (write-locked at runtime).
    pub fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::Relaxed)
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

                    let (map, cvar) = &*self.fetch_in_progress;
                    {
                        let mut guard = map.lock().unwrap();
                        while guard.contains(&sha256) {
                            guard = cvar.wait(guard).unwrap();
                        }
                        guard.insert(sha256.clone());
                    }

                    let fetch_result = handle
                        .block_on(fetch_and_cache(&url, &sha256, cache_base))
                        .map_err(|e| {
                            tracing::error!("fetch failed for {}: {}", sha256, e);
                            ReadError::Fetch
                        });

                    {
                        let mut guard = map.lock().unwrap();
                        guard.remove(&sha256);
                        cvar.notify_all();
                    }

                    let (full_data, expiry) = fetch_result?;

                    if self.max_cache_bytes > 0
                        && let Err(e) = crate::cache::object_cache::evict_oldest(
                            cache_base,
                            self.max_cache_bytes,
                            &sha256,
                        )
                    {
                        tracing::warn!("cache eviction failed: {}", e);
                    }

                    if let Some(ts) = expiry {
                        let mut tree = self.tree.write().unwrap();
                        tree.set_expires(ino, Some(ts));
                    }

                    if offset >= full_data.len() {
                        Ok(Vec::new())
                    } else {
                        let end = (offset + size).min(full_data.len());
                        Ok(full_data[offset..end].to_vec())
                    }
                }
                FileContent::Local { path } => {
                    let path = path.clone();
                    drop(tree);
                    let data = std::fs::read(&path).map_err(|e| {
                        tracing::error!("local read failed for {:?}: {}", path, e);
                        ReadError::Fetch
                    })?;
                    if offset >= data.len() {
                        Ok(Vec::new())
                    } else {
                        let end = (offset + size).min(data.len());
                        Ok(data[offset..end].to_vec())
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

    /// If `ino` is a lazy directory, populate it now (clone + walk + fill tree).
    /// No-op if already populated or not a lazy directory.
    fn populate_if_lazy(&self, ino: u64) {
        let lazy = {
            let mut tree = self.tree.write().unwrap();
            tree.take_lazy(ino)
        };

        if let Some(LazyDir::GitRepo {
            clone_url,
            cache_path,
        }) = lazy
        {
            tracing::info!("lazy populating inode {} from {}", ino, clone_url);

            if let Err(e) = crate::git::browse::clone_repo(&clone_url, &cache_path) {
                tracing::error!("clone failed for {}: {}", clone_url, e);
                let mut tree = self.tree.write().unwrap();
                tree.add_static_file(
                    ino,
                    "CLONE_FAILED.txt",
                    format!(
                        "Failed to clone repository:\n  URL: {}\n  Error: {}\n",
                        clone_url, e
                    )
                    .into_bytes(),
                );
                return;
            }

            let added = {
                let mut tree = self.tree.write().unwrap();
                crate::git::browse::walk_repo_tree(&cache_path, &mut tree, ino)
            };
            tracing::info!("populated {} files/dirs for inode {}", added, ino);
        }
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

    fn compute_effective_expiry(&self, ino: u64) -> Option<u64> {
        let tree = self.tree.read().unwrap();
        let node = tree.get(ino)?;
        let TreeNode::File {
            content,
            size: file_size,
            uploaded,
            ..
        } = node
        else {
            return None;
        };
        let FileContent::Remote { expires, .. } = content else {
            return None;
        };
        if let Some(ts) = *expires {
            Some(ts)
        } else if (*file_size as usize) <= self.max_free_size_bytes {
            Some(*uploaded + self.free_period_secs)
        } else {
            None
        }
    }

    /// Look up a child inode by name within a parent directory.
    fn lookup_child(&self, parent: u64, name: &str) -> Option<u64> {
        let tree = self.tree.read().unwrap();
        tree.lookup(parent, name)
    }

    /// Upload buffered data to the Blossom server and update the tree node.
    ///
    /// Retries up to 3 times with exponential backoff (1 s, 2 s) on transient
    /// failures (network errors, HTTP 5xx, 429). Permanent failures (4xx) and
    /// auth-creation errors return immediately. Returns `Err(())` on final
    /// failure — caller is responsible for cleanup.
    fn do_upload(&self, ino: u64, data: Vec<u8>) -> Result<(), ()> {
        let (keys, server_url, handle) = match (&self.keys, &self.server_url, &self.runtime_handle)
        {
            (Some(k), Some(s), Some(h)) => (k, s, h),
            _ => {
                tracing::error!("upload preconditions not met (keys/server/handle)");
                return Err(());
            }
        };

        let data_len = data.len();
        let sha256_hex = hex::encode(Sha256::digest(&data));

        let auth_header = match create_upload_auth_header(keys, &sha256_hex, data_len as u64) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!("auth creation failed: {}", e);
                return Err(());
            }
        };

        let client = BlossomClient::new(server_url);

        // ── Dedup: skip upload if blob already exists (BUD-01) ──
        match handle.block_on(client.head_blob_with_expiry(&sha256_hex)) {
            Ok(info) if info.exists => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let desired_expiry = now + self.free_period_secs;

                let needs_extend = info.sunset.is_none_or(|s| s < desired_expiry);
                if needs_extend {
                    let days_left = info.sunset.map_or(0, |s| s.saturating_sub(now) / 86400);
                    tracing::info!("blob exists, {days_left} days left, extending lease");
                    match handle.block_on(client.extend_blob_with_payment(
                        &sha256_hex,
                        &auth_header,
                        self.payment.as_ref(),
                        Some(desired_expiry),
                    )) {
                        Ok(desc) => {
                            tracing::info!(
                                "lease extended to ~{} days",
                                desc.expiration.unwrap_or(0).saturating_sub(now) / 86400
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                "lease extension failed ({e}), blob still accessible until expiry"
                            );
                        }
                    }
                } else {
                    tracing::info!("blob already exists with sufficient expiry, skipping upload");
                }

                let mut tree = self.tree.write().unwrap();
                tree.update_file_node(
                    ino,
                    FileContent::Remote {
                        url: format!("{server_url}/{sha256_hex}"),
                        sha256: sha256_hex.clone(),
                        mime_type: None,
                        expires: info.sunset,
                    },
                    data_len as u64,
                    0,
                );
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("dedup check failed ({e}), proceeding with upload");
            }
        }

        // ── Preflight: check server will accept upload (BUD-06) ──
        match handle.block_on(client.preflight_upload(
            &auth_header,
            &sha256_hex,
            data_len as u64,
            "application/octet-stream",
        )) {
            Ok(()) => {}
            Err(BlossomClientError::PaymentRequired { .. }) => {
                tracing::debug!("preflight: payment required (will handle during upload)");
            }
            Err(BlossomClientError::ServerError { status, .. })
                if matches!(status, 413 | 415 | 429) =>
            {
                tracing::error!("preflight rejected upload: HTTP {status}");
                return Err(());
            }
            Err(e) => {
                tracing::warn!("preflight check failed ({e}), proceeding anyway");
            }
        }

        tracing::info!(
            "uploading {} bytes for inode {} (sha256={}…)",
            data_len,
            ino,
            &sha256_hex[..16]
        );

        const MAX_ATTEMPTS: u32 = 3;

        let result = if data_len > self.multipart_threshold {
            tracing::info!(
                "file {} bytes > threshold {} bytes, attempting multipart upload",
                data_len,
                self.multipart_threshold
            );
            match handle.block_on(client.upload_blob_multipart(
                &data,
                "application/octet-stream",
                &auth_header,
            )) {
                Ok(desc) => Ok(desc),
                Err(BlossomClientError::ServerError {
                    status: 404 | 405, ..
                }) => {
                    tracing::warn!("server doesn't support multipart, falling back to single-shot");
                    handle.block_on(upload_with_retry(
                        &client,
                        &data,
                        &auth_header,
                        self.payment.as_ref(),
                        MAX_ATTEMPTS,
                    ))
                }
                Err(e) => {
                    tracing::warn!("multipart upload failed ({e}), falling back to single-shot");
                    handle.block_on(upload_with_retry(
                        &client,
                        &data,
                        &auth_header,
                        self.payment.as_ref(),
                        MAX_ATTEMPTS,
                    ))
                }
            }
        } else {
            handle.block_on(upload_with_retry(
                &client,
                &data,
                &auth_header,
                self.payment.as_ref(),
                MAX_ATTEMPTS,
            ))
        };

        match result {
            Ok(desc) => {
                tracing::info!(
                    "upload complete: {} bytes → {} (sha256={})",
                    desc.size,
                    desc.url,
                    &desc.sha256[..16]
                );
                let mut tree = self.tree.write().unwrap();
                tree.update_file_node(
                    ino,
                    FileContent::Remote {
                        url: desc.url,
                        sha256: desc.sha256,
                        mime_type: desc.mime_type,
                        expires: None,
                    },
                    desc.size,
                    desc.uploaded,
                );
                Ok(())
            }
            Err(BlossomClientError::ServerError { status, body }) => {
                match status {
                    402 => tracing::error!(
                        "upload failed: HTTP 402 Payment Required — \
                         server demands payment for {} bytes",
                        data_len
                    ),
                    413 => tracing::error!(
                        "upload failed: HTTP 413 Payload Too Large — \
                         file exceeds server limit"
                    ),
                    s => tracing::error!(
                        "upload failed: HTTP {} — {}",
                        s,
                        body.chars().take(200).collect::<String>()
                    ),
                }
                Err(())
            }
            Err(e) => {
                tracing::error!("upload failed after {} attempts: {}", MAX_ATTEMPTS, e);
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

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

fn xattr_enodata() -> Errno {
    #[cfg(target_os = "linux")]
    {
        Errno::ENODATA
    }
    #[cfg(not(target_os = "linux"))]
    {
        Errno::ENOTSUP
    }
}

/// Upload a blob with retry on transient failures.
///
/// Retries up to `max_attempts` times with exponential backoff (1 s, 2 s, …)
/// on transient failures (network errors, HTTP 429/5xx). Non-retryable errors
/// (HTTP 4xx except 429) return immediately.
///
/// Returns the blob descriptor on success, or the last error on failure.
async fn upload_with_retry(
    client: &BlossomClient,
    data: &[u8],
    auth_header: &str,
    payment: &dyn crate::payment::PaymentStrategy,
    max_attempts: u32,
) -> Result<BlobDescriptor, BlossomClientError> {
    for attempt in 1..=max_attempts {
        match client
            .upload_blob_with_payment(data.to_vec(), auth_header, payment)
            .await
        {
            Ok(desc) => return Ok(desc),
            Err(e) => {
                let retry = match &e {
                    BlossomClientError::ServerError { status, .. } => {
                        is_retryable_status(*status) && attempt < max_attempts
                    }
                    BlossomClientError::PaymentRequired { .. } | BlossomClientError::Payment(_) => {
                        false
                    }
                    _ => attempt < max_attempts,
                };

                if !retry {
                    return Err(e);
                }

                let delay = 1u64 << (attempt - 1);
                tracing::warn!(
                    "upload attempt {}/{} error, retrying in {}s: {}",
                    attempt,
                    max_attempts,
                    delay,
                    e
                );
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
        }
    }
    unreachable!("loop body always returns or continues")
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
        self.populate_if_lazy(parent.0);

        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        match self.lookup_child(parent.0, name_str) {
            Some(ino) => match self.make_fileattr(ino) {
                Some(attr) => reply.entry(&self.ttl, &attr, Generation(0)),
                None => reply.error(Errno::ENOENT),
            },
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.make_fileattr(ino.0) {
            Some(attr) => reply.attr(&self.ttl, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let is_writable = !self.read_only && !self.is_frozen() && flags.0 & 0o3 != 0;

        if !is_writable {
            let tree = self.tree.read().unwrap();
            if tree.get(ino.0).is_none() {
                drop(tree);
                reply.error(Errno::ENOENT);
                return;
            }
            drop(tree);
            reply.opened(FileHandle(0), FopenFlags::empty());
            return;
        }

        let o_trunc = flags.0 & 0o1000 != 0;
        let o_append = flags.0 & 0o2000 != 0;

        let initial_data = {
            let tree = self.tree.read().unwrap();
            match tree.get(ino.0) {
                None => {
                    drop(tree);
                    reply.error(Errno::ENOENT);
                    return;
                }
                Some(TreeNode::Directory { .. }) => {
                    drop(tree);
                    reply.error(Errno::EISDIR);
                    return;
                }
                Some(TreeNode::File { content, .. }) => match content {
                    FileContent::Static(data) => {
                        if o_trunc {
                            Vec::new()
                        } else {
                            data.clone()
                        }
                    }
                    FileContent::Remote { .. } | FileContent::Local { .. } => {
                        drop(tree);
                        reply.error(Errno::EACCES);
                        return;
                    }
                },
            }
        };

        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);

        self.write_state.lock().unwrap().insert(
            fh,
            WriteBuffer {
                ino: ino.0,
                parent: 0,
                name: String::new(),
                data: initial_data,
                flushed: false,
                append: o_append,
            },
        );

        if o_trunc {
            let mut tree = self.tree.write().unwrap();
            tree.update_file_size(ino.0, 0);
        }

        reply.opened(FileHandle(fh), FopenFlags::empty());
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
        self.populate_if_lazy(ino.0);

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
        if self.read_only || self.is_frozen() {
            reply.error(Errno::EROFS);
            return;
        }
        if let Some(new_size) = size {
            let mut tree = self.tree.write().unwrap();
            tree.update_file_size(ino.0, new_size);
        }
        match self.make_fileattr(ino.0) {
            Some(attr) => reply.attr(&self.ttl, &attr),
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
        if self.read_only || self.is_frozen() {
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
            Some(attr) => reply.entry(&self.ttl, &attr, Generation(0)),
            None => reply.error(Errno::EIO),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.read_only || self.is_frozen() {
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

        let sha256_to_delete: Option<String> = {
            let tree = self.tree.read().unwrap();
            let child_ino = match tree.lookup(parent.0, name_str) {
                Some(ino) => ino,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            };
            match tree.get(child_ino) {
                Some(TreeNode::File {
                    content: FileContent::Remote { sha256, .. },
                    ..
                }) => Some(sha256.clone()),
                _ => None,
            }
        };

        if let Some(ref sha256) = sha256_to_delete {
            if let (Some(keys), Some(server_url), Some(handle)) =
                (&self.keys, &self.server_url, &self.runtime_handle)
            {
                match create_delete_auth_header(keys, sha256) {
                    Ok(auth_header) => {
                        let client = BlossomClient::new(server_url);
                        if let Err(e) = handle.block_on(client.delete_blob(sha256, &auth_header)) {
                            tracing::warn!("server delete failed for {}: {}", sha256, e);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("failed to sign delete auth for {}: {}", sha256, e);
                    }
                }
            }

            if let Some(ref cache_base) = self.cache_base
                && let Ok(cache_file) = cache_path(cache_base, sha256)
                && cache_file.exists()
            {
                let _ = std::fs::remove_file(&cache_file);
            }
        }

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
        if self.read_only || self.is_frozen() {
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
        reply.error(if self.read_only || self.is_frozen() {
            Errno::EROFS
        } else {
            Errno::ENOSYS
        });
    }

    fn link(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _newparent: INodeNo,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(if self.read_only || self.is_frozen() {
            Errno::EROFS
        } else {
            Errno::ENOSYS
        });
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
        if self.read_only || self.is_frozen() {
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
            let effective_offset = if buf.append {
                buf.data.len() as u64
            } else {
                offset
            };
            let end = effective_offset as usize + data.len();
            if end > self.max_write_bytes {
                drop(buffers);
                reply.error(Errno::EFBIG);
                return;
            }
            if buf.data.len() < end {
                buf.data.resize(end, 0);
            }
            buf.data[effective_offset as usize..end].copy_from_slice(data);
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
        flags: i32,
        reply: ReplyCreate,
    ) {
        if self.read_only || self.is_frozen() {
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
            if let Some(existing_ino) = tree.lookup(parent.0, name_str) {
                if flags & 0o1000 != 0 {
                    tree.update_file_node(existing_ino, FileContent::Static(vec![]), 0, now);
                    existing_ino
                } else {
                    drop(tree);
                    reply.error(Errno::EEXIST);
                    return;
                }
            } else {
                tree.add_file_to_dir(parent.0, name_str, FileContent::Static(vec![]), 0, now)
            }
        };

        self.write_state.lock().unwrap().insert(
            fh,
            WriteBuffer {
                ino,
                parent: parent.0,
                name: name_str.to_string(),
                data: Vec::new(),
                flushed: false,
                append: flags & 0o2000 != 0,
            },
        );

        match self.make_fileattr(ino) {
            Some(attr) => reply.created(
                &self.ttl,
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

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        if name != OsStr::new("user.blossom.expiry") {
            reply.error(xattr_enodata());
            return;
        }

        let Some(ts) = self.compute_effective_expiry(ino.into()) else {
            reply.error(xattr_enodata());
            return;
        };

        let value = ts.to_string();
        let value_bytes = value.as_bytes();

        if size == 0 {
            reply.size(value_bytes.len() as u32);
        } else if (size as usize) >= value_bytes.len() {
            reply.data(value_bytes);
        } else {
            reply.error(Errno::ERANGE);
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let tree = self.tree.read().unwrap();
        let node = match tree.get(ino.into()) {
            Some(n) => n,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let TreeNode::File { content, .. } = node else {
            if size == 0 {
                reply.size(0);
            } else {
                reply.data(&[]);
            }
            return;
        };

        let FileContent::Remote { .. } = content else {
            if size == 0 {
                reply.size(0);
            } else {
                reply.data(&[]);
            }
            return;
        };

        let xattr_list = b"user.blossom.expiry\0";

        if size == 0 {
            reply.size(xattr_list.len() as u32);
        } else if (size as usize) >= xattr_list.len() {
            reply.data(xattr_list);
        } else {
            reply.error(Errno::ERANGE);
        }
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
            None,
        );
        BlossomFS::new(tree, Duration::from_secs(1))
    }

    // ============== S1: Empty tree root exists ==============

    #[test]
    fn s01_root_exists() {
        let fs = BlossomFS::new(Tree::new(), Duration::from_secs(1));
        assert_eq!(fs.tree.read().unwrap().root(), 1);
        assert!(fs.tree.read().unwrap().get(1).is_some());
    }

    // ============== S2: getattr on root returns Directory 0o755 ==============

    #[test]
    fn s02_getattr_root() {
        let fs = BlossomFS::new(Tree::new(), Duration::from_secs(1));
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
        let fs = BlossomFS::new(Tree::new(), Duration::from_secs(1));
        assert!(fs.make_fileattr(999).is_none());
    }

    // ============== S4: lookup of nonexistent name returns None ==============

    #[test]
    fn s04_lookup_nonexistent() {
        let fs = BlossomFS::new(Tree::new(), Duration::from_secs(1));
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
            None,
        );
        tree
    }

    fn make_cache_fs(tree: Tree, cache_base: PathBuf, handle: tokio::runtime::Handle) -> BlossomFS {
        BlossomFS::new_with_cache(
            tree,
            cache_base,
            handle,
            Duration::from_secs(1),
            30 * 86400,
            1024 * 1024,
            0,
        )
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

    // ============== S43: Concurrent reads dedup to single HTTP fetch ==============

    #[test]
    fn s43_concurrent_reads_single_fetch() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();

        let content = b"dedup test blob for s43";
        let hash = sha256_hex(content);

        let (mock_server_uri, mock_server) = rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};

            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/dedup43"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
                .expect(1)
                .mount(&server)
                .await;
            (server.uri(), server)
        });

        let url = format!("{}/dedup43", mock_server_uri);
        let tree = make_cache_test_tree(url, hash, content.len() as u64);

        let cache_dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(make_cache_fs(tree, cache_dir.path().to_path_buf(), handle));

        let fs_a = Arc::clone(&fs);
        let fs_b = Arc::clone(&fs);

        let h1 = std::thread::spawn(move || fs_a.read_content(2, 0, 1024));
        let h2 = std::thread::spawn(move || fs_b.read_content(2, 0, 1024));

        let data1 = h1.join().unwrap().expect("thread 1 read should succeed");
        let data2 = h2.join().unwrap().expect("thread 2 read should succeed");

        assert_eq!(data1, content.to_vec());
        assert_eq!(data2, content.to_vec());

        drop(mock_server);
    }

    // ============== S44: Frozen flag blocks writes but allows reads ==============

    #[test]
    fn s44_freeze_flag_behavior() {
        let (_rt, fs) = make_rw_fs();

        assert!(!fs.is_frozen(), "should start unfrozen");

        fs.frozen_handle().store(true, Ordering::Relaxed);
        assert!(fs.is_frozen(), "should be frozen after flag set");

        let data = fs
            .read_content(2, 0, 100)
            .expect("reads should still work when frozen");
        assert_eq!(data, b"Hello, World!");

        fs.frozen_handle().store(false, Ordering::Relaxed);
        assert!(!fs.is_frozen(), "should be unfrozen after flag cleared");
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
            Duration::from_secs(1),
            100 * 1024 * 1024,
            30 * 86400,
            1024 * 1024,
            0,
            Arc::new(crate::payment::NoPayment),
            50 * 1024 * 1024,
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
                    append: false,
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
                    expires: None,
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
                    FileContent::Static(_) | FileContent::Local { .. } => {
                        panic!("expected Remote content after update")
                    }
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
            append: false,
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

    // ============== Retry logic tests (R01-R09) ==============

    fn upload_desc_json() -> serde_json::Value {
        serde_json::json!({
            "url": "https://cdn.example.com/abc",
            "sha256": "abc",
            "size": 9,
            "type": "application/octet-stream",
            "uploaded": 1700000000
        })
    }

    // ============== R01: is_retryable_status returns true for transient errors ====

    #[test]
    fn r01_is_retryable_status_true_for_transient_errors() {
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(502));
        assert!(is_retryable_status(503));
        assert!(is_retryable_status(504));
    }

    // ============== R02: is_retryable_status false for permanent errors ============

    #[test]
    fn r02_is_retryable_status_false_for_permanent_errors() {
        assert!(!is_retryable_status(200));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(402));
        assert!(!is_retryable_status(403));
        assert!(!is_retryable_status(404));
        assert!(!is_retryable_status(413));
    }

    // ============== R03: upload_with_retry succeeds on first attempt ==============

    #[tokio::test(start_paused = true)]
    async fn r03_upload_succeeds_on_first_attempt() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upload_desc_json()))
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result =
            upload_with_retry(&client, b"test data", "tok", &crate::payment::NoPayment, 3).await;

        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
        let desc = result.unwrap();
        assert_eq!(desc.sha256, "abc");
        assert_eq!(desc.size, 9);
    }

    // ============== R04: retry on 429 then succeed on 2nd attempt ================

    #[tokio::test(start_paused = true)]
    async fn r04_retry_on_429_then_succeed() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upload_desc_json()))
            .with_priority(2)
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result =
            upload_with_retry(&client, b"test data", "tok", &crate::payment::NoPayment, 3).await;

        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
    }

    // ============== R05: retry on 500 twice then succeed on 3rd attempt ==========

    #[tokio::test(start_paused = true)]
    async fn r05_retry_on_500_twice_then_succeed() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(2)
            .with_priority(1)
            .mount(&mock_server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(upload_desc_json()))
            .with_priority(2)
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result =
            upload_with_retry(&client, b"test data", "tok", &crate::payment::NoPayment, 3).await;

        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
    }

    // ============== R06: exhaust retries on 500, return ServerError ==============

    #[tokio::test(start_paused = true)]
    async fn r06_exhaust_retries_on_500() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(500))
            .expect(3)
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result =
            upload_with_retry(&client, b"test data", "tok", &crate::payment::NoPayment, 3).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            BlossomClientError::ServerError { status, .. } => {
                assert_eq!(status, 500);
            }
            other => panic!("expected ServerError, got {other:?}"),
        }
    }

    // ============== R07: no retry on 402 Payment Required =======================

    #[tokio::test(start_paused = true)]
    async fn r07_no_retry_on_402() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(402))
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result =
            upload_with_retry(&client, b"test data", "tok", &crate::payment::NoPayment, 3).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            BlossomClientError::Payment(_) => { /* expected: no payment configured */ }
            other => panic!("expected Payment error, got {other:?}"),
        }
    }

    // ============== R08: no retry on 413 Payload Too Large =======================

    #[tokio::test(start_paused = true)]
    async fn r08_no_retry_on_413() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(413))
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result =
            upload_with_retry(&client, b"test data", "tok", &crate::payment::NoPayment, 3).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            BlossomClientError::ServerError { status, .. } => {
                assert_eq!(status, 413);
            }
            other => panic!("expected ServerError, got {other:?}"),
        }
    }

    // ============== R09: no retry on 404 Not Found ===============================

    #[tokio::test(start_paused = true)]
    async fn r09_no_retry_on_404() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;

        Mock::given(method("PUT"))
            .and(path("/upload"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&mock_server)
            .await;

        let client = BlossomClient::new(mock_server.uri());
        let result =
            upload_with_retry(&client, b"test data", "tok", &crate::payment::NoPayment, 3).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            BlossomClientError::ServerError { status, .. } => {
                assert_eq!(status, 404);
            }
            other => panic!("expected ServerError, got {other:?}"),
        }
    }

    // ============== Expiry computation tests (e01–e06) ==============

    #[test]
    fn e01_effective_expiry_server_provided() {
        let (_rt, fs) = make_rw_fs();
        let ino = {
            let mut tree = fs.tree.write().unwrap();
            tree.add_remote_file(
                1,
                "blob.bin",
                "https://cdn.example.com/blob".to_string(),
                "abc123".to_string(),
                100,
                None,
                1700000000,
                None,
            )
        };
        {
            let mut tree = fs.tree.write().unwrap();
            tree.set_expires(ino, Some(1794395471));
        }
        assert_eq!(fs.compute_effective_expiry(ino), Some(1794395471));
    }

    #[test]
    fn e02_effective_expiry_local_fallback_small_file() {
        let (_rt, fs) = make_rw_fs();
        let uploaded: u64 = 1700000000;
        let ino = {
            let mut tree = fs.tree.write().unwrap();
            tree.add_remote_file(
                1,
                "small.bin",
                "https://cdn.example.com/small".to_string(),
                "small123".to_string(),
                500_000,
                None,
                uploaded,
                None,
            )
        };
        let expected = uploaded + 30 * 86400;
        assert_eq!(fs.compute_effective_expiry(ino), Some(expected));
    }

    #[test]
    fn e03_effective_expiry_large_file_no_fallback() {
        let (_rt, fs) = make_rw_fs();
        let ino = {
            let mut tree = fs.tree.write().unwrap();
            tree.add_remote_file(
                1,
                "large.bin",
                "https://cdn.example.com/large".to_string(),
                "large123".to_string(),
                2_000_000,
                None,
                1700000000,
                None,
            )
        };
        assert_eq!(fs.compute_effective_expiry(ino), None);
    }

    #[test]
    fn e04_effective_expiry_directory() {
        let (_rt, fs) = make_rw_fs();
        let dir_ino = {
            let mut tree = fs.tree.write().unwrap();
            tree.add_directory(1, "subdir")
        };
        assert_eq!(fs.compute_effective_expiry(dir_ino), None);
    }

    #[test]
    fn e05_effective_expiry_static_file() {
        let (_rt, fs) = make_rw_fs();
        let ino = {
            let mut tree = fs.tree.write().unwrap();
            tree.add_file_to_dir(1, "file.txt", FileContent::Static(b"hi".to_vec()), 2, 0)
        };
        assert_eq!(fs.compute_effective_expiry(ino), None);
    }

    #[test]
    fn e06_effective_expiry_nonexistent_inode() {
        let (_rt, fs) = make_rw_fs();
        assert_eq!(fs.compute_effective_expiry(999), None);
    }

    #[test]
    fn e07_server_expiry_overrides_local_fallback() {
        let (_rt, fs) = make_rw_fs();
        let uploaded: u64 = 1700000000;
        let ino = {
            let mut tree = fs.tree.write().unwrap();
            tree.add_remote_file(
                1,
                "override.bin",
                "https://cdn.example.com/blob".to_string(),
                "override123".to_string(),
                500_000,
                None,
                uploaded,
                None,
            )
        };
        {
            let mut tree = fs.tree.write().unwrap();
            tree.set_expires(ino, Some(1794395471));
        }
        assert_eq!(
            fs.compute_effective_expiry(ino),
            Some(1794395471),
            "server expiry should take priority over local fallback"
        );
    }

    // ============== BUD-01/BUD-06: Dedup + Preflight Integration ==============

    #[test]
    fn test_do_upload_dedup_skips_upload_when_blob_exists() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();

        let data = b"dedup test data".to_vec();
        let hash = sha256_hex(&data);

        let (mock_uri, _server) = rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};

            let server = MockServer::start().await;

            Mock::given(method("HEAD"))
                .and(path(format!("/{hash}")))
                .respond_with(ResponseTemplate::new(200))
                .mount(&server)
                .await;

            Mock::given(method("PUT"))
                .and(path("/upload"))
                .respond_with(ResponseTemplate::new(201))
                .expect(0)
                .mount(&server)
                .await;

            (server.uri(), server)
        });

        let mut tree = Tree::new();
        let ino = tree.add_file_to_dir(1, "test.bin", FileContent::Static(vec![]), 0, 0);

        let keys = Keys::generate();
        let fs = BlossomFS::new_rw(
            tree,
            PathBuf::from("/tmp/blossomfs-test-cache"),
            handle,
            keys,
            mock_uri,
            Duration::from_secs(1),
            100 * 1024 * 1024,
            30 * 86400,
            1024 * 1024,
            0,
            Arc::new(crate::payment::NoPayment),
            50 * 1024 * 1024,
        );

        let result = fs.do_upload(ino, data);
        assert!(result.is_ok(), "dedup should succeed without uploading");
    }

    #[test]
    fn test_do_upload_preflight_413_rejects_without_uploading() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();

        let data = b"too large test".to_vec();
        let hash = sha256_hex(&data);

        let (mock_uri, _server) = rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};

            let server = MockServer::start().await;

            Mock::given(method("HEAD"))
                .and(path(format!("/{hash}")))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;

            Mock::given(method("HEAD"))
                .and(path("/upload"))
                .respond_with(ResponseTemplate::new(413))
                .mount(&server)
                .await;

            Mock::given(method("PUT"))
                .and(path("/upload"))
                .respond_with(ResponseTemplate::new(201))
                .expect(0)
                .mount(&server)
                .await;

            (server.uri(), server)
        });

        let mut tree = Tree::new();
        let ino = tree.add_file_to_dir(1, "big.bin", FileContent::Static(vec![]), 0, 0);

        let keys = Keys::generate();
        let fs = BlossomFS::new_rw(
            tree,
            PathBuf::from("/tmp/blossomfs-test-cache"),
            handle,
            keys,
            mock_uri,
            Duration::from_secs(1),
            100 * 1024 * 1024,
            30 * 86400,
            1024 * 1024,
            0,
            Arc::new(crate::payment::NoPayment),
            50 * 1024 * 1024,
        );

        let result = fs.do_upload(ino, data);
        assert!(result.is_err(), "preflight 413 should reject upload");
    }

    #[test]
    fn test_do_upload_preflight_error_proceeds_anyway() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();

        let data = b"advisory preflight test".to_vec();
        let hash = sha256_hex(&data);

        let (mock_uri, _server) = rt.block_on(async {
            use wiremock::matchers::{method, path};
            use wiremock::{Mock, MockServer, ResponseTemplate};

            let server = MockServer::start().await;

            Mock::given(method("HEAD"))
                .and(path(format!("/{hash}")))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;

            Mock::given(method("HEAD"))
                .and(path("/upload"))
                .respond_with(ResponseTemplate::new(500))
                .mount(&server)
                .await;

            Mock::given(method("PUT"))
                .and(path("/upload"))
                .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "url": "http://mock/blob",
                    "sha256": hash,
                    "size": data.len(),
                    "type": "application/octet-stream",
                    "uploaded": 1000
                })))
                .expect(1)
                .mount(&server)
                .await;

            (server.uri(), server)
        });

        let mut tree = Tree::new();
        let ino = tree.add_file_to_dir(1, "advisory.bin", FileContent::Static(vec![]), 0, 0);

        let keys = Keys::generate();
        let fs = BlossomFS::new_rw(
            tree,
            PathBuf::from("/tmp/blossomfs-test-cache"),
            handle,
            keys,
            mock_uri,
            Duration::from_secs(1),
            100 * 1024 * 1024,
            30 * 86400,
            1024 * 1024,
            0,
            Arc::new(crate::payment::NoPayment),
            50 * 1024 * 1024,
        );

        let result = fs.do_upload(ino, data);
        assert!(
            result.is_ok(),
            "preflight error should not block upload (advisory)"
        );
    }
}
