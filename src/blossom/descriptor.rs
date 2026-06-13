//! BUD-02 Blob Descriptor types.
//!
//! See: https://github.com/hzrd149/blossom/blob/master/buds/02.md
//!
//! A blob descriptor describes a single Blossom blob with:
//! - `url`: public GET endpoint URL
//! - `sha256`: content hash (64 lowercase hex chars)
//! - `size`: size in bytes
//! - `type`: MIME type (defaults to application/octet-stream)
//! - `uploaded`: Unix timestamp of upload
