//! fuser::Filesystem trait implementation.
//!
//! BlossomFS implements the FUSE filesystem protocol. All callbacks use
//! `&self` (fuser 0.17.0), so mutable state (content cache) uses interior
//! mutability via `Arc<Mutex<CacheState>>`.
//!
//! Stage 1 operations: lookup, getattr, readdir, open, read, statfs.
//! Write operations return EROFS.
