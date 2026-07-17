// BlossomFS — Read-only FUSE filesystem for Blossom/Nostr media
//
// This is the main entry point. CLI parsing, manifest loading, virtual tree
// construction, and FUSE mount orchestration.

#![allow(dead_code)]

mod blossom;
mod cache;
mod cli;
mod config;
mod control;
mod fuse;
mod git;
mod nostr;
mod payment;
mod util;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use clap::{CommandFactory, FromArgMatches};
use fuser::MountOption;

use crate::blossom::client::BlossomClient;
use crate::blossom::descriptor::BlobDescriptor;
use crate::blossom::manifest::load_manifest;
use crate::cli::{Cli, Command};
use crate::fuse::fs::BlossomFS;
use crate::fuse::tree::Tree;
use crate::fuse::vfiles::{MountInfo, generate_readme, generate_status};
use crate::nostr::discovery::discover_servers;
use crate::nostr::keys::{parse_npub, parse_nsec, read_nsec_file};
use crate::nostr::legacy_drive::{DriveEntry, fetch_drive_events};
use crate::nostr::nip34::fetch_nip34_events;
use crate::nostr::nip94::fetch_nip94_events;
use crate::nostr::persist;
use crate::payment::PaymentStrategy;
use crate::util::path::sanitize_hostname;

fn resolve_pubkey_hexes(args: &cli::MountArgs) -> Vec<String> {
    let mut result = Vec::new();
    for pk in &args.pubkey {
        result.push(pk.clone());
    }
    for npub_str in &args.npub {
        if let Ok(pk) = parse_npub(npub_str) {
            result.push(pk.to_hex());
        }
    }
    result
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
            0,
            None,
        );
        true
    } else {
        false
    }
}

fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cmd = Cli::command();
    let matches = cmd.get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(e) => e.exit(),
    };

    match cli.command {
        Command::Mount(args) => {
            let mut args = *args;
            let mount_matches = matches.subcommand_matches("mount");
            let is_explicit = |name: &str| {
                mount_matches
                    .and_then(|m| m.value_source(name))
                    .map(|s| s == clap::parser::ValueSource::CommandLine)
                    .unwrap_or(false)
            };

            let config_path = args.config.clone();
            if let Err(e) =
                config::BlossomConfig::load_merged(config_path.as_deref(), &mut args, is_explicit)
            {
                eprintln!("blossomfs: config error: {e}");
                std::process::exit(1);
            }

            if args.daemon {
                tracing::info!("forking to background (daemon mode)");
                if let Err(e) = daemonize::Daemonize::new().start() {
                    eprintln!("blossomfs: daemonize failed: {e}");
                    std::process::exit(1);
                }
            }

            if let Err(e) = run_mount(args) {
                eprintln!("blossomfs: error: {e}");
                std::process::exit(1);
            }
        }
        Command::Extend(args) => {
            if let Err(e) = run_extend(args) {
                eprintln!("blossomfs: error: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn run_extend(args: cli::ExtendArgs) -> Result<(), Box<dyn std::error::Error>> {
    let keys = if let Some(ref path) = args.nsec_file {
        crate::nostr::keys::read_nsec_file(path)?
    } else {
        return Err("extend requires --nsec-file".into());
    };

    let auth_header = crate::nostr::auth::create_upload_auth_header(&keys, &args.sha256, 0)?;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let client = BlossomClient::new(args.server.clone());

        let payment: Box<dyn PaymentStrategy> = if let Some(ref path) = args.cashu_token_file {
            let token = std::fs::read_to_string(path)?.trim().to_string();
            Box::new(crate::payment::TokenStrategy::new(token))
        } else if let Some(ref uri) = args.nwc_uri {
            match crate::payment::NwcStrategy::new(uri) {
                Ok(s) => Box::new(s),
                Err(e) => return Err(format!("invalid NWC URI: {e}").into()),
            }
        } else {
            return Err("extend requires --cashu-token-file or --nwc-uri".into());
        };

        match client
            .extend_blob_with_payment(&args.sha256, &auth_header, payment.as_ref(), None)
            .await
        {
            Ok(desc) => {
                println!("extended: {} ({} bytes)", desc.sha256, desc.size);
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    })
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
    let pubkey_hexes = resolve_pubkey_hexes(&args);
    let pubkey_hex = pubkey_hexes.first().cloned();

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
    if !effective_servers.is_empty()
        && let Some(pd) = public_dir
    {
        let pubkeys_to_list: Vec<&str> = if pubkey_hexes.is_empty() {
            vec!["all"]
        } else {
            pubkey_hexes.iter().map(|s| s.as_str()).collect()
        };
        let max = args.max_blobs as usize;

        for pk_label in &pubkeys_to_list {
            let mut pubkey_descriptors: Vec<BlobDescriptor> = Vec::new();
            let pubkey_dir = tree.add_directory(pd, pk_label);
            let servers_dir = tree.add_directory(pubkey_dir, "servers");

            for server_url in &effective_servers {
                let host = sanitize_hostname(server_url);
                let host_dir = tree.add_directory(servers_dir, &host);

                let client = BlossomClient::new(server_url);
                tracing::info!("listing blobs from {} for pubkey={}", server_url, pk_label);
                match rt.block_on(client.list_all_blobs(pk_label, max, BlossomClient::PAGE_SIZE)) {
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
                        pubkey_descriptors.extend(descriptors.clone());
                        tree.build_from_descriptors(host_dir, &descriptors);
                    }
                    Err(e) => {
                        tracing::warn!("failed to list blobs from {}: {}", server_url, e);
                    }
                }
            }

            if !pubkey_descriptors.is_empty() {
                let all_dir = tree.add_directory(pubkey_dir, "all-servers");
                tree.build_by_sha256_only(all_dir, &pubkey_descriptors);
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

    // --- NIP-94: /metadata/<sha256>.json + /nip94/<pubkey>/<filename> + /tollgate/ (kind 1063) ---
    if !args.relay.is_empty()
        && let Some(ref pk) = pubkey_hex
    {
        match rt.block_on(fetch_nip94_events(&args.relay, pk)) {
            Ok(records) => {
                tracing::info!("fetched {} NIP-94 metadata events", records.len());

                let meta_root = tree.add_directory(tree.root(), "metadata");

                let nip94_root = tree.add_directory(tree.root(), "nip94");
                let pk_dir = tree.add_directory(nip94_root, pk);
                let mut seen_filenames: std::collections::HashSet<String> =
                    std::collections::HashSet::new();

                let mut tollgate_releases: Vec<crate::nostr::tollgate::TollgateRelease> =
                    Vec::new();

                for record in &records {
                    let meta = &record.meta;
                    if let Some(ref sha) = meta.sha256
                        && sha.len() == 64
                        && sha.chars().all(|c| c.is_ascii_hexdigit())
                    {
                        let json = serde_json::to_string_pretty(meta)
                            .unwrap_or_default()
                            .into_bytes();
                        tree.add_static_file(meta_root, &format!("{}.json", sha), json);

                        if let Some(ref url) = meta.url {
                            let display_name = meta.filename.as_deref().unwrap_or(sha);
                            let unique_name = if seen_filenames.contains(display_name) {
                                format!("{}-{}", &sha[..12], display_name)
                            } else {
                                display_name.to_string()
                            };
                            seen_filenames.insert(display_name.to_string());

                            tree.add_remote_file(
                                pk_dir,
                                &unique_name,
                                url.clone(),
                                sha.clone(),
                                meta.size.unwrap_or(0),
                                meta.mime_type.clone(),
                                0,
                                None,
                            );
                            blob_count += 1;
                        }

                        let raw_tag_refs: Vec<Vec<&str>> = record
                            .raw_tags
                            .iter()
                            .map(|t| t.iter().map(|s| s.as_str()).collect())
                            .collect();
                        if let Some(rel) =
                            crate::nostr::tollgate::parse_tollgate_release(&raw_tag_refs)
                        {
                            tollgate_releases.push(rel);
                        }
                    } else if let Some(ref sha) = meta.sha256 {
                        tracing::warn!("skipping NIP-94 metadata with invalid sha256: {}", sha);
                    }
                }

                if !tollgate_releases.is_empty() {
                    let count =
                        crate::nostr::tollgate::build_tollgate_tree(&mut tree, &tollgate_releases);
                    blob_count += count;
                    tracing::info!("built tollgate directory tree with {} releases", count);
                }
            }
            Err(e) => {
                tracing::warn!("NIP-94 fetch via relays failed: {}", e);
            }
        }
    }

    // --- NIP-34: /git/<pubkey>/<repo-id>/... (kind 30617 repos, 1617 patches, 1621 issues) ---
    if !args.nip34_relay.is_empty()
        && let Some(ref nip34_pk) = args.nip34_pubkey
    {
        let resolved_pk = if let Ok(pk) = parse_npub(nip34_pk) {
            pk.to_hex()
        } else {
            nip34_pk.clone()
        };
        match rt.block_on(fetch_nip34_events(&args.nip34_relay, &resolved_pk)) {
            Ok(data) => {
                let count = crate::nostr::nip34::build_nip34_tree(
                    &mut tree,
                    &resolved_pk,
                    &data,
                    args.nip34_clone,
                    &args.cache_dir,
                );
                blob_count += count;
                tracing::info!(
                    "NIP-34: built git browser tree ({} files, {} repos)",
                    count,
                    data.repos.len()
                );
            }
            Err(e) => {
                tracing::warn!("NIP-34 fetch failed: {}", e);
            }
        }
    }

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let expiring_soon = tree.collect_expiring_blobs(now_secs, 7 * 86400);
    if !expiring_soon.is_empty() {
        tracing::info!("found {} blobs expiring within 7 days", expiring_soon.len());
    }

    // Add virtual files
    let npub_display = args
        .npub
        .first()
        .or_else(|| args.pubkey.first())
        .map(|s| s.as_str())
        .unwrap_or("all");
    let server_count = effective_servers.len();
    let mount_info = MountInfo {
        mountpoint: args.mountpoint.display().to_string(),
        npub: npub_display.to_string(),
        server_count,
        blob_count,
        max_blobs: args.max_blobs,
        cache_dir: args.cache_dir.display().to_string(),
        expiring_soon,
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

    // Resolve nsec for RW mode or persistence
    let need_keys = !args.read_only || args.persist.is_some();
    let keys = if need_keys {
        if let Some(ref path) = args.nsec_file {
            match read_nsec_file(path) {
                Ok(k) => Some(k),
                Err(e) => {
                    tracing::warn!("failed to read nsec file: {}", e);
                    None
                }
            }
        } else if let Some(ref nsec_str) = args.dangerous_nsec_arg {
            match parse_nsec(nsec_str) {
                Ok(k) => Some(k),
                Err(e) => {
                    tracing::warn!("failed to parse --dangerous-nsec-arg: {}", e);
                    None
                }
            }
        } else {
            tracing::warn!(
                "RW mode requires --nsec-file or --dangerous-nsec-arg, falling back to read-only"
            );
            None
        }
    } else {
        None
    };

    // Clone keys for persist publish (keys is moved into BlossomFS below)
    let persist_keys = keys.clone();

    // Fetch persisted directory structure if --persist is set
    if let Some(ref drive_name) = args.persist {
        if let Some(ref k) = keys {
            if !args.relay.is_empty() {
                tracing::info!("fetching persisted drive '{}' from relays", drive_name);
                match rt.block_on(persist::fetch_persist_event(&args.relay, k, drive_name)) {
                    Ok(tags) => {
                        let srv = effective_servers.first().map(|s| s.as_str()).unwrap_or("");
                        tree.apply_persisted(&tags, srv);
                        tracing::info!("applied {} persisted tags", tags.len());
                    }
                    Err(persist::PersistError::NotFound(_)) => {
                        tracing::info!("no persisted drive found for '{}'", drive_name);
                    }
                    Err(e) => {
                        tracing::warn!("failed to fetch persisted drive: {}", e);
                    }
                }
            } else {
                tracing::warn!("--persist requires --relay to fetch persisted structure");
            }
        } else {
            tracing::warn!("--persist requires --nsec-file or --dangerous-nsec-arg");
        }
    }

    let server_url = effective_servers.first().cloned();

    let payment: Arc<dyn PaymentStrategy> = if let Some(ref path) = args.cashu_token_file {
        match std::fs::read_to_string(path) {
            Ok(token) => Arc::new(crate::payment::TokenStrategy::new(token.trim().to_string())),
            Err(e) => {
                tracing::warn!("failed to read cashu token file: {}", e);
                Arc::new(crate::payment::NoPayment)
            }
        }
    } else if let Some(ref uri) = args.nwc_uri {
        match crate::payment::NwcStrategy::new(uri) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::warn!("failed to parse NWC URI: {}", e);
                Arc::new(crate::payment::NoPayment)
            }
        }
    } else {
        Arc::new(crate::payment::NoPayment)
    };

    // Create filesystem with appropriate constructor
    let handle = rt.handle().clone();
    let (mut fs, is_rw) = match (keys, server_url, args.read_only) {
        (Some(k), Some(s), false) => {
            tracing::info!("mounting in RW mode (upload server: {})", s);
            (
                BlossomFS::new_rw(
                    tree,
                    args.cache_dir.clone(),
                    handle,
                    k,
                    s,
                    Duration::from_secs(args.ttl_secs),
                    (args.max_write_mb as usize) * 1024 * 1024,
                    args.free_period_days * 86400,
                    (args.max_free_size_mb as usize) * 1024 * 1024,
                    args.max_cache_size * 1024 * 1024,
                    payment,
                    (args.multipart_threshold_mb as usize) * 1024 * 1024,
                    (args.write_buffer_mb as usize) * 1024 * 1024,
                    Arc::new(tokio::sync::Semaphore::new(
                        args.max_concurrent_uploads as usize,
                    )),
                ),
                true,
            )
        }
        _ => {
            tracing::info!("mounting in read-only mode");
            (
                BlossomFS::new_with_cache(
                    tree,
                    args.cache_dir.clone(),
                    handle,
                    Duration::from_secs(args.ttl_secs),
                    args.free_period_days * 86400,
                    (args.max_free_size_mb as usize) * 1024 * 1024,
                    args.max_cache_size * 1024 * 1024,
                ),
                false,
            )
        }
    };

    fs.set_http_timeout(Duration::from_secs(args.http_timeout_secs));

    // Mount options — RW mode omits RO flag
    let mut options = fuser::Config::default();
    let mut mount_opts = vec![
        MountOption::FSName("blossomfs".to_string()),
        MountOption::Subtype("blossomfs".to_string()),
    ];
    if !is_rw {
        mount_opts.insert(0, MountOption::RO);
    }
    options.mount_options = mount_opts;

    tracing::info!("mounting FUSE filesystem...");

    let _ = sd_notify::notify(&[sd_notify::NotifyState::Ready]);

    // Extract shared state before mount2 consumes fs
    let tree_arc = fs.tree_handle();
    let persist_tree = if args.persist.is_some() {
        Some(tree_arc.clone())
    } else {
        None
    };
    let frozen_handle = fs.frozen_handle();
    let cache_base = fs.cache_base_path();
    let fs_read_only = !is_rw;

    let socket_path = args.cache_dir.join("blossomfs.sock");
    let _control_task = rt.handle().spawn(crate::control::run_control_socket(
        socket_path.clone(),
        tree_arc,
        frozen_handle,
        cache_base,
        fs_read_only,
    ));

    fuser::mount2(fs, &args.mountpoint, &options)?;

    let _ = std::fs::remove_file(&socket_path);

    let _ = sd_notify::notify(&[sd_notify::NotifyState::Stopping]);

    // Publish persisted tree after unmount
    if let (Some(drive_name), Some(tree_arc), Some(pk)) =
        (&args.persist, persist_tree, &persist_keys)
        && !args.relay.is_empty()
    {
        let tags = tree_arc.read().unwrap().persist_tags();
        tracing::info!(
            "publishing {} persisted tags for drive '{}'",
            tags.len(),
            drive_name
        );
        if let Err(e) = rt.block_on(persist::publish_persist_event(
            &args.relay,
            pk,
            drive_name,
            &tags,
        )) {
            tracing::warn!("failed to publish persisted drive: {}", e);
        }
    }

    drop(rt);

    Ok(())
}
