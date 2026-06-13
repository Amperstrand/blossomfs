//! Blossom HTTP client for BUD-12 listing and BUD-01 retrieval.
//!
//! Uses a hybrid approach:
//! - `nostr-blossom` crate for type definitions and basic operations
//! - Raw `reqwest` for cursor pagination (BUD-12) and streaming downloads (BUD-01)
//!
//! See Wave 4 (T4.2) for implementation.
