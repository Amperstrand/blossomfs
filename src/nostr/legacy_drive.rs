//! Legacy Blossom Drive (kind 30563) parsing.
//!
//! Milestone 5: Parse old Blossom Drive events read-only.
//! `x` tag format: `["x", "<sha256>", "<absolute-path>", "<size>", "<mime>"]`
//! `folder` tag format: `["folder", "<path>"]` for empty directories.
//!
//! Security: All paths are untrusted — reject path traversal, normalize,
//! handle collisions deterministically.
