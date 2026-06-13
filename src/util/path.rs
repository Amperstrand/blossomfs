//! Path and hostname sanitization.
//!
//! Security: All remote metadata is untrusted. This module ensures:
//! - No path traversal (`..`)
//! - No null bytes
//! - No control characters
//! - No absolute path escapes
//! - Safe hostname and MIME type components for filesystem paths
