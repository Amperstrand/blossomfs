//! Virtual directory tree model.
//!
//! Represents the filesystem tree projected from Blossom blob descriptors.
//! The tree is built once at mount time and is immutable during the session.
//!
//! Layout:
//! ```text
//! /
//!   README.txt
//!   STATUS.txt
//!   public/
//!     <npub>/
//!       servers/
//!         <host>/
//!           by-sha256/<sha256>[.<ext>]
//!           by-type/<mime>/<sha256>[.<ext>]
//!           by-date/YYYY/MM/DD/<sha256>[.<ext>]
//!       all-servers/
//!         by-sha256/<sha256>[.<ext>]
//! ```
