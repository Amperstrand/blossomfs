//! Stable inode allocation.
//!
//! Root inode = `INodeNo(1)` (FUSE_ROOT_ID). Sequential allocation during
//! tree construction with a bidirectional map for O(1) lookup.
