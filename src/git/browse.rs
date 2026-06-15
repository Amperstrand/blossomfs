//! Lazy git clone and file-tree population for FUSE browsing.
//!
//! At mount time, NIP-34 repo directories are marked as lazy.
//! On first `readdir` into a repo directory, [`clone_repo`] fetches
//! the repo with `git clone --depth 1` (only the latest commit —
//! minimal bandwidth, one HTTP request per repo). Then
//! [`walk_repo_tree`] recursively adds all files and directories
//! to the virtual [`Tree`] so they become browseable through FUSE.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::fuse::tree::Tree;

#[derive(Debug)]
pub enum CloneError {
    CommandFailed(String),
    GitFailed(String),
    AlreadyExists,
}

impl std::fmt::Display for CloneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CloneError::CommandFailed(msg) => write!(f, "git command failed: {}", msg),
            CloneError::GitFailed(msg) => write!(f, "git error: {}", msg),
            CloneError::AlreadyExists => write!(f, "destination already exists"),
        }
    }
}

impl std::error::Error for CloneError {}

/// Clone a git repository with `--depth 1` (shallow, latest commit only).
///
/// Skips the clone if `dest` already exists (cached from a previous mount).
pub fn clone_repo(url: &str, dest: &Path) -> Result<(), CloneError> {
    if dest.exists() && dest.join(".git").exists() {
        tracing::info!("repo already cached at {:?}", dest);
        return Ok(());
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    tracing::info!("cloning {} → {:?}", url, dest);

    let output = Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            "--single-branch",
            "--no-tags",
            url,
            dest.to_str().ok_or_else(|| {
                CloneError::CommandFailed("non-UTF-8 destination path".to_string())
            })?,
        ])
        .output()
        .map_err(|e| CloneError::CommandFailed(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CloneError::GitFailed(stderr.to_string()));
    }

    Ok(())
}

/// Recursively walk a cloned repo directory and add all files/dirs to the
/// FUSE tree under `parent_ino`. Skips the `.git` directory.
pub fn walk_repo_tree(repo_path: &Path, tree: &mut Tree, parent_ino: u64) -> usize {
    let mut count = 0;
    walk_dir_recursive(repo_path, tree, parent_ino, &mut count);
    count
}

fn walk_dir_recursive(dir: &Path, tree: &mut Tree, parent_ino: u64, count: &mut usize) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("failed to read dir {:?}: {}", dir, e);
            return;
        }
    };

    let mut subdirs: Vec<(String, PathBuf)> = Vec::new();
    let mut files: Vec<(String, PathBuf, u64)> = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) => s,
            None => continue,
        };

        if name_str == ".git" {
            continue;
        }

        let path = entry.path();
        let metadata = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };

        if metadata.is_dir() {
            subdirs.push((name_str.to_string(), path));
        } else if metadata.is_file() {
            files.push((name_str.to_string(), path, metadata.len()));
        }
    }

    subdirs.sort_by(|a, b| a.0.cmp(&b.0));
    files.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, path) in subdirs {
        let dir_ino = tree.add_directory(parent_ino, &name);
        *count += 1;
        walk_dir_recursive(&path, tree, dir_ino, count);
    }

    for (name, path, size) in files {
        tree.add_local_file(parent_ino, &name, path, size);
        *count += 1;
    }
}
