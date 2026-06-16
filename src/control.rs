//! Runtime control socket — Unix-domain JSON command interface.
//!
//! Listens on `{cache_dir}/blossomfs.sock` and accepts one JSON command per
//! line. Supported commands: `status`, `freeze`, `unfreeze`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::fuse::tree::Tree;

#[derive(Deserialize)]
struct ControlRequest {
    cmd: String,
}

#[derive(Serialize)]
struct ControlResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    read_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frozen: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nodes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_bytes: Option<u64>,
}

pub async fn run_control_socket(
    socket_path: PathBuf,
    tree: Arc<RwLock<Tree>>,
    frozen: Arc<AtomicBool>,
    cache_base: Option<PathBuf>,
    read_only: bool,
) -> std::io::Result<()> {
    let _ = std::fs::remove_file(&socket_path);
    let listener = tokio::net::UnixListener::bind(&socket_path)?;
    tracing::info!("control socket listening at {}", socket_path.display());

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let tree = Arc::clone(&tree);
                let frozen = Arc::clone(&frozen);
                let cache_base = cache_base.clone();
                tokio::spawn(async move {
                    handle_connection(stream, tree, frozen, cache_base, read_only).await;
                });
            }
            Err(e) => {
                tracing::warn!("control socket accept error: {}", e);
            }
        }
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    tree: Arc<RwLock<Tree>>,
    frozen: Arc<AtomicBool>,
    cache_base: Option<PathBuf>,
    read_only: bool,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    if reader.read_line(&mut line).await.is_ok() {
        let resp = handle_command(&line, &tree, &frozen, &cache_base, read_only);
        let json = serde_json::to_string(&resp)
            .unwrap_or_else(|_| r#"{"status":"error","error":"serialize failed"}"#.to_string());
        let _ = writer.write_all(json.as_bytes()).await;
        let _ = writer.write_all(b"\n").await;
    }
}

fn handle_command(
    line: &str,
    tree: &Arc<RwLock<Tree>>,
    frozen: &Arc<AtomicBool>,
    cache_base: &Option<PathBuf>,
    read_only: bool,
) -> ControlResponse {
    let req: ControlRequest = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            return ControlResponse {
                status: "error".into(),
                error: Some(format!("invalid command: {e}")),
                read_only: None,
                frozen: None,
                nodes: None,
                cache_bytes: None,
            };
        }
    };

    match req.cmd.as_str() {
        "status" => {
            let nodes = tree.read().map(|t| t.node_count()).unwrap_or(0);
            let cache_bytes = cache_base.as_ref().and_then(|p| compute_cache_size(p).ok());
            ControlResponse {
                status: "ok".into(),
                error: None,
                read_only: Some(read_only),
                frozen: Some(frozen.load(Ordering::Relaxed)),
                nodes: Some(nodes),
                cache_bytes,
            }
        }
        "freeze" => {
            frozen.store(true, Ordering::Relaxed);
            ControlResponse {
                status: "ok".into(),
                error: None,
                read_only: Some(read_only),
                frozen: Some(true),
                nodes: None,
                cache_bytes: None,
            }
        }
        "unfreeze" => {
            frozen.store(false, Ordering::Relaxed);
            ControlResponse {
                status: "ok".into(),
                error: None,
                read_only: Some(read_only),
                frozen: Some(false),
                nodes: None,
                cache_bytes: None,
            }
        }
        other => ControlResponse {
            status: "error".into(),
            error: Some(format!("unknown command: {other}")),
            read_only: None,
            frozen: None,
            nodes: None,
            cache_bytes: None,
        },
    }
}

fn compute_cache_size(dir: &Path) -> std::io::Result<u64> {
    let mut total: u64 = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            total += entry.metadata()?.len();
        } else if path.is_dir() {
            total += compute_cache_size(&path)?;
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::tree::Tree;

    #[test]
    fn c01_status_command() {
        let tree = Arc::new(RwLock::new(Tree::new()));
        let frozen = Arc::new(AtomicBool::new(false));

        let resp = handle_command(r#"{"cmd":"status"}"#, &tree, &frozen, &None, false);

        assert_eq!(resp.status, "ok");
        assert_eq!(resp.read_only, Some(false));
        assert_eq!(resp.frozen, Some(false));
        assert_eq!(resp.nodes, Some(1));
        assert_eq!(resp.cache_bytes, None);
    }

    #[test]
    fn c02_freeze_unfreeze() {
        let tree = Arc::new(RwLock::new(Tree::new()));
        let frozen = Arc::new(AtomicBool::new(false));

        let resp = handle_command(r#"{"cmd":"freeze"}"#, &tree, &frozen, &None, false);
        assert_eq!(resp.status, "ok");
        assert_eq!(resp.frozen, Some(true));
        assert!(frozen.load(Ordering::Relaxed));

        let resp = handle_command(r#"{"cmd":"unfreeze"}"#, &tree, &frozen, &None, false);
        assert_eq!(resp.status, "ok");
        assert_eq!(resp.frozen, Some(false));
        assert!(!frozen.load(Ordering::Relaxed));
    }

    #[test]
    fn c03_unknown_command() {
        let tree = Arc::new(RwLock::new(Tree::new()));
        let frozen = Arc::new(AtomicBool::new(false));

        let resp = handle_command(r#"{"cmd":"explode"}"#, &tree, &frozen, &None, false);

        assert_eq!(resp.status, "error");
        assert!(resp.error.unwrap().contains("unknown command"));
    }

    #[test]
    fn c04_invalid_json() {
        let tree = Arc::new(RwLock::new(Tree::new()));
        let frozen = Arc::new(AtomicBool::new(false));

        let resp = handle_command("not json", &tree, &frozen, &None, false);

        assert_eq!(resp.status, "error");
        assert!(resp.error.unwrap().contains("invalid command"));
    }

    #[test]
    fn c05_cache_size_with_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        std::fs::write(dir.join("a.bin"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.join("b.bin"), vec![0u8; 200]).unwrap();

        let size = compute_cache_size(dir).unwrap();
        assert_eq!(size, 300);
    }
}
