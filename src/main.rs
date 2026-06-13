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
use crate::blossom::manifest::load_manifest;
use crate::cli::{Cli, Command};
use crate::fuse::fs::BlossomFS;
use crate::fuse::tree::Tree;
use crate::fuse::vfiles::{MountInfo, generate_readme, generate_status};

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

    // Create blobs directory if we have manifest or servers
    let need_blobs = args.manifest.is_some() || !args.server.is_empty();
    let blobs_dir = if need_blobs {
        Some(tree.add_directory(tree.root(), "blobs"))
    } else {
        None
    };

    // Load manifest if provided
    if let Some(ref manifest_path) = args.manifest {
        let descriptors = load_manifest(manifest_path)?;
        blob_count += descriptors.len();
        tracing::info!("loaded {} descriptors from manifest", descriptors.len());
        if let Some(bd) = blobs_dir {
            tree.build_from_descriptors(bd, &descriptors);
        }
    }

    // List blobs from Blossom servers (BUD-12)
    if !args.server.is_empty() {
        let pubkey_hex = args.pubkey.as_deref().unwrap_or("all");

        for server_url in &args.server {
            let client = BlossomClient::new(server_url);
            tracing::info!(
                "listing blobs from {} for pubkey={}",
                server_url,
                pubkey_hex
            );
            match rt.block_on(client.list_all_blobs(pubkey_hex)) {
                Ok(descriptors) => {
                    tracing::info!("listed {} blobs from {}", descriptors.len(), server_url);
                    blob_count += descriptors.len();
                    if let Some(bd) = blobs_dir {
                        tree.build_from_descriptors(bd, &descriptors);
                    }
                }
                Err(e) => {
                    tracing::warn!("failed to list blobs from {}: {}", server_url, e);
                }
            }
        }
    }

    // Add virtual files
    let npub_display = args.npub.as_deref().unwrap_or("all");
    let server_count = args.server.len();
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
