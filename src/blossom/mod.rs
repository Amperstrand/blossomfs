//! Blossom protocol client modules.
//!
//! Handles BUD-01 through BUD-12 protocol interactions:
//! - `descriptor`: BUD-02 blob descriptor types
//! - `client`: HTTP client for BUD-12 listing and BUD-01 retrieval
//! - `auth`: BUD-11 authorization tokens (stage 1: stub)

pub mod auth;
pub mod client;
pub mod descriptor;
