//! Content-addressed object cache.
//!
//! Layout: `<cache-dir>/objects/<aa>/<bb>/<sha256>`
//!
//! Flow:
//! 1. Check cache hit → serve from disk
//! 2. Cache miss → stream HTTP response to temp file while computing SHA-256
//! 3. Hash match → atomic rename to final path
//! 4. Hash mismatch → delete temp, return error (never cache bad data)
//! 5. Subsequent reads → cache hit
