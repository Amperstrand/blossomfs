//! MIME type to file extension inference.
//!
//! Uses `mime_guess` crate to map MIME types (e.g. "image/png") to
//! file extensions (e.g. "png"). Extensions are UX-only — they are
//! not canonical names in Blossom.
