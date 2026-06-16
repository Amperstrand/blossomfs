//! CLI argument parsing for blossomfs.
//!
//! This module defines the clap-based CLI structure for the
//! `blossomfs mount` command.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to parse CLI args and extract MountArgs if successful
    fn parse_mount_args(args: &[&str]) -> Result<MountArgs, clap::Error> {
        let cli = Cli::try_parse_from(args)?;
        match cli.command {
            Command::Mount(mount_args) => Ok(*mount_args),
            _ => panic!("expected Mount command"),
        }
    }

    #[test]
    fn s1_happy_path_with_manifest() {
        // S1: Happy path - mountpoint and manifest parsed
        let args = vec![
            "blossomfs",
            "mount",
            "--mountpoint",
            "/mnt/test",
            "--manifest",
            "manifest.json",
        ];
        let result = parse_mount_args(&args);

        assert!(result.is_ok(), "Should parse successfully");
        let mount_args = result.unwrap();
        assert_eq!(mount_args.mountpoint, PathBuf::from("/mnt/test"));
        assert_eq!(mount_args.manifest, Some(PathBuf::from("manifest.json")));
    }

    #[test]
    fn s2_happy_path_with_npub() {
        // S2: Happy path - npub parsed
        let args = vec![
            "blossomfs",
            "mount",
            "--mountpoint",
            "/mnt/test",
            "--npub",
            "npub1xyz",
        ];
        let result = parse_mount_args(&args);

        assert!(result.is_ok(), "Should parse successfully");
        let mount_args = result.unwrap();
        assert_eq!(mount_args.mountpoint, PathBuf::from("/mnt/test"));
        assert_eq!(mount_args.npub, vec![String::from("npub1xyz")]);
    }

    #[test]
    fn s3_happy_path_multiple_servers() {
        // S3: Happy path - multiple servers parsed
        let args = vec![
            "blossomfs",
            "mount",
            "--mountpoint",
            "/mnt/test",
            "--server",
            "https://a.com",
            "--server",
            "https://b.com",
        ];
        let result = parse_mount_args(&args);

        assert!(result.is_ok(), "Should parse successfully");
        let mount_args = result.unwrap();
        assert_eq!(mount_args.server.len(), 2);
        assert_eq!(mount_args.server[0], String::from("https://a.com"));
        assert_eq!(mount_args.server[1], String::from("https://b.com"));
    }

    #[test]
    fn s4_happy_path_with_read_only_flag() {
        // S4: Happy path - read-only flag parsed as true
        let args = vec![
            "blossomfs",
            "mount",
            "--mountpoint",
            "/mnt/test",
            "--read-only",
            "true",
        ];
        let result = parse_mount_args(&args);

        assert!(result.is_ok(), "Should parse successfully");
        let mount_args = result.unwrap();
        assert!(mount_args.read_only);
    }

    #[test]
    fn s5_edge_missing_mountpoint() {
        // S5: Edge - missing --mountpoint should error
        let args = vec!["blossomfs", "mount"];
        let result = parse_mount_args(&args);

        assert!(result.is_err(), "Should error when mountpoint is missing");
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("required"),
            "Error should mention required argument"
        );
    }

    #[test]
    fn s6_edge_read_only_default() {
        // S6: Edge - --read-only absent should default to true
        let args = vec!["blossomfs", "mount", "--mountpoint", "/mnt/test"];
        let result = parse_mount_args(&args);

        assert!(result.is_ok(), "Should parse successfully");
        let mount_args = result.unwrap();
        assert!(mount_args.read_only, "read_only should default to true");
    }

    #[test]
    fn s7_edge_dangerous_nsec_arg() {
        // S7: Edge - --dangerous-nsec-arg value captured
        let args = vec![
            "blossomfs",
            "mount",
            "--mountpoint",
            "/mnt/test",
            "--dangerous-nsec-arg",
            "nsec1abc123",
        ];
        let result = parse_mount_args(&args);

        assert!(result.is_ok(), "Should parse successfully");
        let mount_args = result.unwrap();
        assert_eq!(
            mount_args.dangerous_nsec_arg,
            Some(String::from("nsec1abc123"))
        );
    }

    #[test]
    fn s8_edge_multiple_relays() {
        // S8: Edge - --relay repeatable
        let args = vec![
            "blossomfs",
            "mount",
            "--mountpoint",
            "/mnt/test",
            "--relay",
            "wss://relay1.example.com",
            "--relay",
            "wss://relay2.example.com",
        ];
        let result = parse_mount_args(&args);

        assert!(result.is_ok(), "Should parse successfully");
        let mount_args = result.unwrap();
        assert_eq!(mount_args.relay.len(), 2);
        assert_eq!(
            mount_args.relay[0],
            String::from("wss://relay1.example.com")
        );
        assert_eq!(
            mount_args.relay[1],
            String::from("wss://relay2.example.com")
        );
    }

    #[test]
    fn s9_happy_path_with_pubkey() {
        // S9: Happy path - --pubkey hex string parsed correctly
        let args = vec![
            "blossomfs",
            "mount",
            "--mountpoint",
            "/mnt/test",
            "--pubkey",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ];
        let result = parse_mount_args(&args);

        assert!(result.is_ok(), "Should parse successfully");
        let mount_args = result.unwrap();
        assert_eq!(
            mount_args.pubkey,
            vec![String::from(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            )]
        );
    }

    #[test]
    fn s10_edge_cache_dir_default() {
        // S10: Edge - --cache-dir defaults correctly
        let args = vec!["blossomfs", "mount", "--mountpoint", "/mnt/test"];
        let result = parse_mount_args(&args);

        assert!(result.is_ok(), "Should parse successfully");
        let mount_args = result.unwrap();
        // Default is /tmp/blossomfs (can be refined in main.rs using directories crate)
        let expected_cache_dir = PathBuf::from("/tmp/blossomfs");
        assert_eq!(mount_args.cache_dir, expected_cache_dir);
    }

    #[test]
    fn s11_ttl_secs_default_is_one_year() {
        let args = vec!["blossomfs", "mount", "--mountpoint", "/mnt/test"];
        let mount_args = parse_mount_args(&args).unwrap();
        assert_eq!(mount_args.ttl_secs, 31536000);
    }

    #[test]
    fn s12_ttl_secs_custom_value() {
        let args = vec![
            "blossomfs",
            "mount",
            "--mountpoint",
            "/mnt/test",
            "--ttl-secs",
            "60",
        ];
        let mount_args = parse_mount_args(&args).unwrap();
        assert_eq!(mount_args.ttl_secs, 60);
    }
}

/// CLI arguments for blossomfs
#[derive(Parser, Debug)]
#[command(name = "blossomfs")]
#[command(about = "Read-only FUSE filesystem for Blossom/Nostr media", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Subcommands for blossomfs
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Mount a Blossom filesystem
    Mount(Box<MountArgs>),

    Extend(ExtendArgs),
}

#[derive(Parser, Debug)]
pub struct ExtendArgs {
    #[arg(long)]
    pub sha256: String,
    #[arg(long)]
    pub server: String,
    #[arg(long)]
    pub nsec_file: Option<PathBuf>,
    #[arg(long)]
    pub cashu_token_file: Option<PathBuf>,
    #[arg(long)]
    pub nwc_uri: Option<String>,
}

/// Arguments for the mount subcommand
#[derive(Parser, Debug)]
pub struct MountArgs {
    /// Path to mount point (REQUIRED)
    #[arg(long)]
    pub mountpoint: PathBuf,

    /// Bech32 public key (npub1..., repeatable for multiple users)
    #[arg(long)]
    pub npub: Vec<String>,

    /// Hex public key (64 hex chars, repeatable for multiple users)
    #[arg(long)]
    pub pubkey: Vec<String>,

    /// Blossom server URL (repeatable)
    #[arg(long)]
    pub server: Vec<String>,

    /// Path to manifest JSON file
    #[arg(long)]
    pub manifest: Option<PathBuf>,

    /// Cache directory (default: ~/.cache/blossomfs)
    #[arg(long, default_value = "/tmp/blossomfs")]
    pub cache_dir: PathBuf,

    /// Mount in read-only mode (default: true; pass --read-only=false for RW)
    #[arg(long, action = clap::ArgAction::Set, default_value = "true")]
    pub read_only: bool,

    /// Path to file containing nsec for authenticated operations
    #[arg(long)]
    pub nsec_file: Option<PathBuf>,

    /// Raw nsec on command line
    ///
    /// WARNING: This will expose your secret key in shell history and process listings.
    /// Only use this for testing purposes. Use --nsec-file for production.
    #[arg(long)]
    pub dangerous_nsec_arg: Option<String>,

    /// Nostr relay URL for server discovery (repeatable)
    #[arg(long)]
    pub relay: Vec<String>,

    /// NIP-34 relay for git repo browsing (repeatable)
    #[arg(long)]
    pub nip34_relay: Vec<String>,

    /// NIP-34 pubkey whose repos to browse (hex or npub)
    #[arg(long)]
    pub nip34_pubkey: Option<String>,

    /// [EXPERIMENTAL] Enable lazy git clone for NIP-34 repos.
    /// When set, browsing into a repo directory triggers a shallow
    /// `git clone --depth 1` on first access, then the full file tree
    /// becomes browseable through FUSE.
    #[arg(long, default_value_t = false)]
    pub nip34_clone: bool,

    /// FUSE entry/attribute cache TTL in seconds.
    ///
    /// Since Blossom blobs are content-addressed (immutable), a long TTL is
    /// safe. Default is 31536000 (1 year). Use a lower value for debugging.
    #[arg(long, default_value_t = 31536000)]
    pub ttl_secs: u64,

    #[arg(long, default_value_t = 1024)]
    pub max_write_mb: u64,

    /// Files above this size use multipart upload (server must support it).
    /// Files below use single-shot PUT. Default: 50 MB.
    #[arg(long, default_value_t = 50)]
    pub multipart_threshold_mb: u64,

    #[arg(long, default_value_t = 30)]
    pub free_period_days: u64,

    #[arg(long, default_value_t = 1)]
    pub max_free_size_mb: u64,

    /// Maximum cache size in megabytes. 0 = unlimited (default).
    /// When exceeded, oldest cached blobs are evicted (FIFO).
    #[arg(long, default_value_t = 0)]
    pub max_cache_size: u64,

    /// Persist directory structure to Nostr (kind 30078).
    ///
    /// On mount, loads saved user-created directories from relays.
    /// On unmount, publishes the current structure so it survives remounts.
    /// Value is the drive name (used as NIP-33 d-tag identifier).
    /// Requires --nsec-file or --dangerous-nsec-arg for signing.
    #[arg(long)]
    pub persist: Option<String>,

    /// Maximum number of blobs to load per server (0 = unlimited).
    /// Caps mount time for accounts with many blobs.
    #[arg(long, default_value_t = 1000)]
    pub max_blobs: u64,

    #[arg(long)]
    pub config: Option<PathBuf>,

    #[arg(long, default_value_t = false)]
    pub daemon: bool,

    #[arg(long)]
    pub cashu_token_file: Option<PathBuf>,

    #[arg(long)]
    pub nwc_uri: Option<String>,
}

impl Default for MountArgs {
    fn default() -> Self {
        MountArgs {
            mountpoint: PathBuf::new(),
            npub: Vec::new(),
            pubkey: Vec::new(),
            server: Vec::new(),
            manifest: None,
            cache_dir: PathBuf::from("/tmp/blossomfs"),
            read_only: true,
            nsec_file: None,
            dangerous_nsec_arg: None,
            relay: Vec::new(),
            nip34_relay: Vec::new(),
            nip34_pubkey: None,
            nip34_clone: false,
            ttl_secs: 31536000,
            max_write_mb: 1024,
            multipart_threshold_mb: 50,
            free_period_days: 30,
            max_free_size_mb: 1,
            max_cache_size: 0,
            persist: None,
            max_blobs: 1000,
            config: None,
            daemon: false,
            cashu_token_file: None,
            nwc_uri: None,
        }
    }
}
