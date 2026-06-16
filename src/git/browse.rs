//! Lazy git clone and file-tree population for FUSE browsing.
//!
//! At mount time, NIP-34 repo directories are marked as lazy.
//! On first `readdir` into a repo directory, [`clone_repo`] fetches
//! the repo via `git2` (native Rust — no external git binary needed).
//! Then [`walk_repo_tree`] uses `walkdir` to add all files and
//! directories to the virtual [`Tree`].

use std::path::Path;

use crate::fuse::tree::Tree;

#[derive(Debug)]
pub enum CloneError {
    GitFailed(String),
    AlreadyExists,
}

impl std::fmt::Display for CloneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CloneError::GitFailed(msg) => write!(f, "git error: {}", msg),
            CloneError::AlreadyExists => write!(f, "destination already exists"),
        }
    }
}

impl std::error::Error for CloneError {}

/// Clone a git repository using `git2` (native, no external binary).
///
/// Skips the clone if `dest` already exists with a `.git` dir (cached).
pub fn clone_repo(url: &str, dest: &Path) -> Result<(), CloneError> {
    if dest.exists() && dest.join(".git").exists() {
        tracing::info!("repo already cached at {:?}", dest);
        return Ok(());
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    tracing::info!("cloning {} → {:?}", url, dest);

    git2::build::RepoBuilder::new()
        .clone(url, dest)
        .map_err(|e| CloneError::GitFailed(e.message().to_string()))?;

    Ok(())
}

/// Walk a cloned repo directory and add all files/dirs to the FUSE tree
/// under `parent_ino`. Skips the `.git` directory. Uses `walkdir` for
/// robust traversal with sorted output.
pub fn walk_repo_tree(repo_path: &Path, tree: &mut Tree, parent_ino: u64) -> usize {
    use std::collections::HashMap;

    let mut path_to_ino: HashMap<std::path::PathBuf, u64> = HashMap::new();
    path_to_ino.insert(repo_path.to_path_buf(), parent_ino);
    let mut count = 0;

    for entry in walkdir::WalkDir::new(repo_path)
        .min_depth(1)
        .sort_by(|a, b| a.file_name().cmp(b.file_name()))
        .into_iter()
        .filter_entry(|e| {
            let rel = e.path().strip_prefix(repo_path).unwrap_or(e.path());
            !rel.starts_with(".git")
        })
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("walkdir error: {}", e);
                continue;
            }
        };

        let name = match entry.file_name().to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };

        let parent_path = entry.path().parent().unwrap();
        let p_ino = match path_to_ino.get(parent_path) {
            Some(&ino) => ino,
            None => continue,
        };

        if entry.file_type().is_dir() {
            let dir_ino = tree.add_directory(p_ino, &name);
            path_to_ino.insert(entry.path().to_path_buf(), dir_ino);
            count += 1;
        } else if entry.file_type().is_file() {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            tree.add_local_file(p_ino, &name, entry.path().to_path_buf(), size);
            count += 1;
        }
    }

    count
}
