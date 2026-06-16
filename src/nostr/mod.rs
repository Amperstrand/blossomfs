//! Nostr protocol modules.
//!
//! - `auth`: BUD-11 blossom server authentication events (kind 24242)
//! - `keys`: npub/nsec parsing and key management
//! - `discovery`: BUD-03 kind 10063 server list discovery (M4)
//! - `legacy_drive`: Kind 30563 old Blossom Drive parsing (M5)
//! - `nip94`: Kind 1063 file metadata enrichment (M6)
//! - `nip34`: NIP-34 git collaboration browser (repos, issues, patches)
//! - `tollgate`: Tollgate release directory builder

pub mod auth;
pub mod discovery;
pub mod keys;
pub mod legacy_drive;
pub mod nip34;
pub mod nip94;
pub mod persist;
pub mod tollgate;
