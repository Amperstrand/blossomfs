// BlossomFS — Read-only FUSE filesystem for Blossom/Nostr media
//
// This is the main entry point. CLI parsing, manifest loading, virtual tree
// construction, and FUSE mount orchestration.

#![allow(dead_code)]

mod blossom;
mod cache;
mod cli;
mod fuse;
mod nostr;
mod util;

use std::path::Path;

use clap::Parser;
use fuser::MountOption;

use crate::blossom::client::BlossomClient;
use crate::blossom::descriptor::BlobDescriptor;
use crate::blossom::manifest::load_manifest;
use crate::cli::{Cli, Command};
use crate::fuse::fs::BlossomFS;
use crate::fuse::tree::Tree;
use crate::fuse::vfiles::{MountInfo, generate_readme, generate_status};
use crate::nostr::discovery::discover_servers;
use crate::nostr::keys::parse_npub;
use crate::nostr::legacy_drive::{DriveEntry, fetch_drive_events};
use crate::nostr::nip94::fetch_nip94_events;
use crate::util::path::sanitize_hostname;

fn resolve_pubkey_hex(args: &cli::MountArgs) -> Option<String> {
    if let Some(ref pk) = args.pubkey {
        return Some(pk.clone());
    }
    if let Some(ref npub) = args.npub
        && let Ok(pk) = parse_npub(npub)
    {
        return Some(pk.to_hex());
    }
    None
}

fn ensure_drive_path(tree: &mut Tree, root: u64, path: &str) -> u64 {
    let trimmed = path.trim_start_matches('/');
    let mut current = root;
    for component in trimmed.split('/') {
        if component.is_empty() {
            continue;
        }
        current = tree.get_or_create_dir(current, component);
    }
    current
}

fn add_drive_file(
    tree: &mut Tree,
    root: u64,
    path: &str,
    sha256: &str,
    size: u64,
    mime: Option<&str>,
    servers: &[String],
) -> bool {
    if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        tracing::warn!("skipping drive file with invalid sha256: {}", sha256);
        return false;
    }

    let trimmed = path.trim_start_matches('/');
    let parts: Vec<&str> = trimmed.split('/').collect();
    if parts.is_empty() {
        return false;
    }

    let mut current = root;
    for dir in &parts[..parts.len().saturating_sub(1)] {
        if dir.is_empty() {
            continue;
        }
        current = tree.get_or_create_dir(current, dir);
    }

    if let Some(&filename) = parts.last() {
        if filename.is_empty() {
            return false;
        }
        let url = servers
            .first()
            .map(|s| format!("{}/{}", s.trim_end_matches('/'), sha256))
            .unwrap_or_else(|| format!("blossom://{}", sha256));
        tree.add_remote_file(
            current,
            filename,
            url,
            sha256.to_string(),
            size,
            mime.map(|s| s.to_string()),
        );
        true
    } else {
        false
    }
}

fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Mount(args) => {
            if let Err(e) = run_mount(args) {
                eprintln!("blossomfs: error: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn run_mount(args: cli::MountArgs) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("blossomfs mounting at {:?}", args.mountpoint);

    // Verify mountpoint exists
    if !Path::new(&args.mountpoint).exists() {
        return Err(format!("mountpoint {:?} does not exist", args.mountpoint).into());
    }

    let rt = tokio::runtime::Runtime::new()?;

    // Build the virtual tree
    let mut tree = Tree::new();
    let mut blob_count = 0usize;
    let mut all_descriptors: Vec<crate::blossom::descriptor::BlobDescriptor> = Vec::new();

    // Resolve pubkey hex from --pubkey or --npub
    let pubkey_hex = resolve_pubkey_hex(&args);

    // Discover servers via NIP-B7/BUD-03 (kind 10063) if relays are provided
    let mut effective_servers = args.server.clone();
    if !args.relay.is_empty() {
        if let Some(ref pk) = pubkey_hex {
            tracing::info!(
                "querying {} relay(s) for kind 10063 server list (pubkey={})",
                args.relay.len(),
                pk
            );
            match rt.block_on(discover_servers(&args.relay, pk)) {
                Ok(servers) => {
                    tracing::info!("discovered {} server(s) via NIP-B7", servers.len());
                    for s in &servers {
                        if !effective_servers.contains(s) {
                            effective_servers.push(s.clone());
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("server discovery via relays failed: {}", e);
                }
            }
        } else {
            tracing::warn!("relays provided but no --npub or --pubkey; skipping server discovery");
        }
    }

    // Create /public/ root if we have any blob sources
    let need_public = args.manifest.is_some() || !effective_servers.is_empty();
    let public_dir = if need_public {
        Some(tree.add_directory(tree.root(), "public"))
    } else {
        None
    };

    // --- Manifest source: /public/local/servers/manifest/ ---
    if let Some(ref manifest_path) = args.manifest {
        let raw = load_manifest(manifest_path)?;
        let descriptors: Vec<BlobDescriptor> = raw
            .into_iter()
            .filter(|d| {
                if let Err(e) = d.validate() {
                    tracing::warn!("skipping invalid descriptor (sha={}): {}", d.sha256, e);
                    false
                } else {
                    true
                }
            })
            .collect();
        blob_count += descriptors.len();
        tracing::info!(
            "loaded {} valid descriptors from manifest",
            descriptors.len()
        );

        if let Some(pd) = public_dir {
            let local_dir = tree.add_directory(pd, "local");
            let servers_dir = tree.add_directory(local_dir, "servers");
            let manifest_dir = tree.add_directory(servers_dir, "manifest");
            tree.build_from_descriptors(manifest_dir, &descriptors);
        }
    }

    // --- Server sources: /public/<pubkey>/servers/<host>/ and /public/<pubkey>/all-servers/ ---
    if !effective_servers.is_empty() {
        let pk_label = pubkey_hex.as_deref().unwrap_or("all");

        if let Some(pd) = public_dir {
            let pubkey_dir = tree.add_directory(pd, pk_label);
            let servers_dir = tree.add_directory(pubkey_dir, "servers");

            for server_url in &effective_servers {
                let host = sanitize_hostname(server_url);
                let host_dir = tree.add_directory(servers_dir, &host);

                let client = BlossomClient::new(server_url);
                tracing::info!("listing blobs from {} for pubkey={}", server_url, pk_label);
                match rt.block_on(client.list_all_blobs(pk_label)) {
                    Ok(raw) => {
                        let descriptors: Vec<BlobDescriptor> = raw
                            .into_iter()
                            .filter(|d| {
                                if let Err(e) = d.validate() {
                                    tracing::warn!(
                                        "skipping invalid descriptor from {}: {}",
                                        server_url,
                                        e
                                    );
                                    false
                                } else {
                                    true
                                }
                            })
                            .collect();
                        tracing::info!("listed {} blobs from {}", descriptors.len(), server_url);
                        blob_count += descriptors.len();
                        all_descriptors.extend(descriptors.clone());
                        tree.build_from_descriptors(host_dir, &descriptors);
                    }
                    Err(e) => {
                        tracing::warn!("failed to list blobs from {}: {}", server_url, e);
                    }
                }
            }

            // All-servers aggregate: only by-sha256, deduplicated
            if !all_descriptors.is_empty() {
                let all_dir = tree.add_directory(pubkey_dir, "all-servers");
                tree.build_by_sha256_only(all_dir, &all_descriptors);
            }
        }
    }

    // --- Drive sources: /drives/<pubkey>/<drive-id>/... (kind 30563) ---
    if !args.relay.is_empty()
        && let Some(ref pk) = pubkey_hex
    {
        match rt.block_on(fetch_drive_events(&args.relay, pk)) {
            Ok(drives) => {
                tracing::info!("fetched {} drive(s) via kind 30563", drives.len());
                let drives_root = tree.add_directory(tree.root(), "drives");
                let pk_dir = tree.add_directory(drives_root, pk);

                for drive in &drives {
                    let drive_inode = tree.add_directory(pk_dir, &drive.drive_id);
                    for entry in &drive.entries {
                        match entry {
                            DriveEntry::File(f) => {
                                if add_drive_file(
                                    &mut tree,
                                    drive_inode,
                                    &f.path,
                                    &f.sha256,
                                    f.size.unwrap_or(0),
                                    f.mime.as_deref(),
                                    &effective_servers,
                                ) {
                                    blob_count += 1;
                                }
                            }
                            DriveEntry::Folder(fl) => {
                                ensure_drive_path(&mut tree, drive_inode, &fl.path);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("drive fetch via relays failed: {}", e);
            }
        }
    }

    // --- NIP-94 metadata: /metadata/<sha256>.json (kind 1063) ---
    if !args.relay.is_empty()
        && let Some(ref pk) = pubkey_hex
    {
        match rt.block_on(fetch_nip94_events(&args.relay, pk)) {
            Ok(metas) => {
                tracing::info!("fetched {} NIP-94 metadata events", metas.len());
                let meta_root = tree.add_directory(tree.root(), "metadata");
                for meta in &metas {
                    if let Some(ref sha) = meta.sha256 {
                        if sha.len() == 64 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
                            let json = serde_json::to_string_pretty(meta)
                                .unwrap_or_default()
                                .into_bytes();
                            tree.add_static_file(meta_root, &format!("{}.json", sha), json);
                        } else {
                            tracing::warn!("skipping NIP-94 metadata with invalid sha256: {}", sha);
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("NIP-94 fetch via relays failed: {}", e);
            }
        }
    }

    // Add virtual files
    let npub_display = args.npub.as_deref().unwrap_or("all");
    let server_count = effective_servers.len();
    let mount_info = MountInfo {
        mountpoint: args.mountpoint.display().to_string(),
        npub: npub_display.to_string(),
        server_count,
        blob_count,
        cache_dir: args.cache_dir.display().to_string(),
    };

    let readme_content = generate_readme(&mount_info);
    let status_content = generate_status(&mount_info);
    tree.add_static_file(tree.root(), "README.txt", readme_content);
    tree.add_static_file(tree.root(), "STATUS.txt", status_content);

    tracing::info!(
        "tree built: {} blob descriptors, mountpoint = {:?}",
        blob_count,
        args.mountpoint
    );

    // Ensure cache directory exists
    std::fs::create_dir_all(&args.cache_dir)?;

    // Create filesystem with cache and tokio runtime handle for lazy fetch
    let handle = rt.handle().clone();
    let fs = BlossomFS::new_with_cache(tree, args.cache_dir.clone(), handle);

    // Mount options
    let mut options = fuser::Config::default();
    options.mount_options = vec![
        MountOption::RO,
        MountOption::FSName("blossomfs".to_string()),
        MountOption::Subtype("blossomfs".to_string()),
    ];

    tracing::info!("mounting FUSE filesystem...");
    fuser::mount2(fs, &args.mountpoint, &options)?;

    // rt stays alive until here — dropped after mount2 returns (after unmount)
    drop(rt);

    Ok(())
}
