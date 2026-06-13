//! BUD-11 Nostr authorization tokens.
//!
//! Stage 1: Stub — public listing only, no auth required.
//! Stage 2: Implement kind 24242 event signing with `t`, `expiration`,
//! `server`, and `x` tags for authenticated blob operations.
//!
//! Auth header format: `Authorization: Nostr <base64url-nopad-event>`
