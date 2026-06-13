//! Nostr protocol modules.
//!
//! - `keys`: npub/nsec parsing and key management
//! - `discovery`: BUD-03 kind 10063 server list discovery (M4)
//! - `legacy_drive`: Kind 30563 old Blossom Drive parsing (M5)
//! - `nip94`: Kind 1063 file metadata enrichment (M6)

pub mod discovery;
pub mod keys;
pub mod legacy_drive;
pub mod nip94;
