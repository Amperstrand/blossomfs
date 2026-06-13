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

    // Build the virtual tree
    let mut tree = Tree::new();

    // Determine blob count and server info for virtual files
    let mut blob_count = 0usize;
    let server_count = args.server.len();

    // Load manifest if provided
    if let Some(ref manifest_path) = args.manifest {
        let descriptors = load_manifest(manifest_path)?;
        blob_count = descriptors.len();
        tracing::info!("loaded {} descriptors from manifest", blob_count);

        // Create directory structure for blobs
        // M1 layout: /blobs/ with by-sha256/by-type/by-date
        let blobs_dir = tree.add_directory(tree.root(), "blobs");
        tree.build_from_descriptors(blobs_dir, &descriptors);
    }

    // Add virtual files
    let npub_display = args.npub.as_deref().unwrap_or("all");
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

    // Create filesystem
    let fs = BlossomFS::new(tree);

    // Mount options
    let mut options = fuser::Config::default();
    options.mount_options = vec![
        MountOption::RO,
        MountOption::FSName("blossomfs".to_string()),
        MountOption::Subtype("blossomfs".to_string()),
    ];

    tracing::info!("mounting FUSE filesystem...");
    fuser::mount2(fs, &args.mountpoint, &options)?;

    Ok(())
}
