//! NIP-34 git collaboration event parsing and directory tree builder.
//!
//! Implements a read-only browser for Nostr-native git repositories using
//! the NIP-34 specification (https://github.com/nostr-protocol/nips/blob/master/34.md).
//!
//! The sparse-fetch approach is inspired by gitworkshop.dev
//! (https://gitworkshop.dev), which fetches relay events once and caches
//! locally. We replicate this: a one-time relay query at mount time, then
//! all data is baked into the tree as static files. No repeated queries.

use std::collections::{HashMap, HashSet};
use std::time::Duration;
use thiserror::Error;

use nostr_sdk::prelude::*;

use crate::fuse::tree::{LazyDir, Tree};

#[derive(Error, Debug)]
pub enum Nip34Error {
    #[error("nostr client error: {0}")]
    Client(#[from] nostr_sdk::client::Error),
    #[error("invalid public key: {0}")]
    InvalidPubkey(String),
    #[error("no nip-34 repos found for this pubkey")]
    NoRepos,
}

#[derive(Debug, Clone)]
pub struct Nip34Repo {
    pub repo_id: String,
    pub name: String,
    pub description: String,
    pub clone_urls: Vec<String>,
    pub web_urls: Vec<String>,
    pub relays: Vec<String>,
    pub maintainers: Vec<String>,
    pub created_at: u64,
    pub event_id: String,
    pub addr: String,
}

#[derive(Debug, Clone)]
pub struct Nip34Issue {
    pub repo_ref: String,
    pub subject: String,
    pub content: String,
    pub event_id: String,
    pub author: String,
    pub created_at: u64,
}

#[derive(Debug, Clone)]
pub struct Nip34Patch {
    pub repo_ref: String,
    pub branch_name: String,
    pub content: String,
    pub is_cover_letter: bool,
    pub event_id: String,
    pub author: String,
    pub created_at: u64,
}

pub struct Nip34Data {
    pub repos: Vec<Nip34Repo>,
    pub issues: Vec<Nip34Issue>,
    pub patches: Vec<Nip34Patch>,
}

pub async fn fetch_nip34_events(
    relays: &[String],
    pubkey_hex: &str,
) -> Result<Nip34Data, Nip34Error> {
    let public_key =
        PublicKey::from_hex(pubkey_hex).map_err(|e| Nip34Error::InvalidPubkey(e.to_string()))?;

    let client = Client::default();
    for relay_url in relays {
        client.add_relay(relay_url).await?;
    }
    client.connect().await;

    let repo_filter = Filter::new()
        .kind(Kind::Custom(30617))
        .author(public_key)
        .limit(500);

    let repo_events = client
        .fetch_events(repo_filter)
        .timeout(Duration::from_secs(15))
        .await?;

    let repos: Vec<Nip34Repo> = repo_events
        .iter()
        .filter_map(|e| {
            let tags: Vec<Vec<&str>> = e
                .tags
                .iter()
                .map(|t| t.as_slice().iter().map(|s| s.as_str()).collect())
                .collect();
            parse_repo_from_tags(&tags, pubkey_hex, &e.id.to_hex(), e.created_at.as_secs())
        })
        .collect();

    if repos.is_empty() {
        client.disconnect().await;
        return Err(Nip34Error::NoRepos);
    }

    let repo_addrs: HashSet<String> = repos.iter().map(|r| r.addr.clone()).collect();

    let event_filter = Filter::new()
        .kinds([Kind::Custom(1617), Kind::Custom(1621)])
        .limit(1000);

    let events = client
        .fetch_events(event_filter)
        .timeout(Duration::from_secs(15))
        .await?;

    client.disconnect().await;

    let mut issues = Vec::new();
    let mut patches = Vec::new();

    for event in events.iter() {
        let tags: Vec<Vec<&str>> = event
            .tags
            .iter()
            .map(|t| t.as_slice().iter().map(|s| s.as_str()).collect())
            .collect();
        let id = event.id.to_hex();
        let author = event.pubkey.to_hex();
        let ct = event.created_at.as_secs();

        match event.kind {
            Kind::Custom(1621) => {
                if let Some(issue) = parse_issue_from_tags(&tags, &event.content, &id, &author, ct)
                    && repo_addrs.contains(&issue.repo_ref)
                {
                    issues.push(issue);
                }
            }
            Kind::Custom(1617) => {
                if let Some(patch) = parse_patch_from_tags(&tags, &event.content, &id, &author, ct)
                    && repo_addrs.contains(&patch.repo_ref)
                {
                    patches.push(patch);
                }
            }
            _ => {}
        }
    }

    tracing::info!(
        "NIP-34: {} repos, {} issues, {} patches",
        repos.len(),
        issues.len(),
        patches.len()
    );

    Ok(Nip34Data {
        repos,
        issues,
        patches,
    })
}

pub fn parse_repo_from_tags(
    tags: &[Vec<&str>],
    owner_pubkey: &str,
    event_id: &str,
    created_at: u64,
) -> Option<Nip34Repo> {
    let mut repo_id = None;
    let mut name = String::new();
    let mut description = String::new();
    let mut clone_urls = Vec::new();
    let mut web_urls = Vec::new();
    let mut relays = Vec::new();
    let mut maintainers = Vec::new();

    for tag in tags {
        if tag.len() < 2 {
            continue;
        }
        match tag[0] {
            "d" => repo_id = Some(tag[1].to_string()),
            "name" => name = tag[1].to_string(),
            "description" => description = tag[1].to_string(),
            "clone" => clone_urls.push(tag[1].to_string()),
            "web" => web_urls.push(tag[1].to_string()),
            "relays" => relays.push(tag[1].to_string()),
            "maintainers" => maintainers.push(tag[1].to_string()),
            _ => {}
        }
    }

    let repo_id = repo_id?;
    let name = if name.is_empty() {
        repo_id.clone()
    } else {
        name
    };
    let addr = format!("30617:{}:{}", owner_pubkey, repo_id);

    Some(Nip34Repo {
        repo_id,
        name,
        description,
        clone_urls,
        web_urls,
        relays,
        maintainers,
        created_at,
        event_id: event_id.to_string(),
        addr,
    })
}

pub fn parse_issue_from_tags(
    tags: &[Vec<&str>],
    content: &str,
    event_id: &str,
    author: &str,
    created_at: u64,
) -> Option<Nip34Issue> {
    let mut repo_ref = None;
    let mut subject = String::new();

    for tag in tags {
        if tag.len() < 2 {
            continue;
        }
        match tag[0] {
            "a" => repo_ref = Some(tag[1].to_string()),
            "subject" => subject = tag[1].to_string(),
            _ => {}
        }
    }

    let repo_ref = repo_ref?;
    if subject.is_empty() {
        subject = "(no subject)".to_string();
    }

    Some(Nip34Issue {
        repo_ref,
        subject,
        content: content.to_string(),
        event_id: event_id.to_string(),
        author: author.to_string(),
        created_at,
    })
}

pub fn parse_patch_from_tags(
    tags: &[Vec<&str>],
    content: &str,
    event_id: &str,
    author: &str,
    created_at: u64,
) -> Option<Nip34Patch> {
    let mut repo_ref = None;
    let mut branch_name = String::new();
    let mut is_cover_letter = false;

    for tag in tags {
        if tag.len() < 2 {
            continue;
        }
        match tag[0] {
            "a" => repo_ref = Some(tag[1].to_string()),
            "branch-name" => branch_name = tag[1].to_string(),
            "t" if tag[1] == "cover-letter" => is_cover_letter = true,
            _ => {}
        }
    }

    let repo_ref = repo_ref?;
    if branch_name.is_empty() {
        branch_name = "unknown".to_string();
    }

    Some(Nip34Patch {
        repo_ref,
        branch_name,
        content: content.to_string(),
        is_cover_letter,
        event_id: event_id.to_string(),
        author: author.to_string(),
        created_at,
    })
}

pub fn build_nip34_tree(
    tree: &mut Tree,
    pubkey_hex: &str,
    data: &Nip34Data,
    clone_enabled: bool,
    cache_dir: &std::path::Path,
) -> usize {
    if data.repos.is_empty() {
        return 0;
    }

    let git_root = tree.add_directory(tree.root(), "git");
    let pk_dir = tree.get_or_create_dir(git_root, pubkey_hex);

    let mut issues_by_repo: HashMap<&str, Vec<&Nip34Issue>> = HashMap::new();
    for issue in &data.issues {
        issues_by_repo
            .entry(issue.repo_ref.as_str())
            .or_default()
            .push(issue);
    }

    let mut patches_by_repo: HashMap<&str, Vec<&Nip34Patch>> = HashMap::new();
    for patch in &data.patches {
        patches_by_repo
            .entry(patch.repo_ref.as_str())
            .or_default()
            .push(patch);
    }

    let mut file_count = 0;

    for repo in &data.repos {
        let clone_url = repo.clone_urls.first().cloned();

        let repo_dir = if clone_enabled {
            if let Some(ref url) = clone_url {
                let cache_path = cache_dir
                    .join("ngit-clones")
                    .join(pubkey_hex)
                    .join(&repo.repo_id);
                tree.add_lazy_dir(
                    pk_dir,
                    &repo.repo_id,
                    LazyDir::GitRepo {
                        clone_url: url.clone(),
                        cache_path,
                    },
                )
            } else {
                tree.get_or_create_dir(pk_dir, &repo.repo_id)
            }
        } else {
            tree.get_or_create_dir(pk_dir, &repo.repo_id)
        };

        let mut info = String::new();
        info.push_str(&format!("# {}\n\n", repo.name));
        if !repo.description.is_empty() {
            info.push_str(&format!("{}\n\n", repo.description));
        }
        info.push_str(&format!("**Repository ID:** `{}`\n\n", repo.repo_id));
        info.push_str(&format!(
            "**Published:** {}\n\n",
            format_unix_ts(repo.created_at)
        ));

        if !repo.clone_urls.is_empty() {
            info.push_str("## Clone URLs\n\n```\n");
            for url in &repo.clone_urls {
                info.push_str(&format!("git clone {}\n", url));
            }
            info.push_str("```\n\n");
        }

        if !repo.web_urls.is_empty() {
            info.push_str("## Web\n\n");
            for url in &repo.web_urls {
                info.push_str(&format!("- {}\n", url));
            }
            info.push('\n');
        }

        if !repo.relays.is_empty() {
            info.push_str("## Relays\n\n");
            for r in &repo.relays {
                info.push_str(&format!("- {}\n", r));
            }
            info.push('\n');
        }

        if !repo.maintainers.is_empty() {
            info.push_str("## Maintainers\n\n");
            for m in &repo.maintainers {
                info.push_str(&format!("- `{}`\n", m));
            }
            info.push('\n');
        }

        let issue_count = issues_by_repo
            .get(repo.addr.as_str())
            .map_or(0, |v| v.len());
        let patch_count = patches_by_repo
            .get(repo.addr.as_str())
            .map_or(0, |v| v.len());
        info.push_str(&format!(
            "## Activity\n\n- {} issue(s)\n- {} patch(es)\n",
            issue_count, patch_count
        ));

        tree.add_static_file(repo_dir, "INFO.md", info.into_bytes());
        file_count += 1;

        if !repo.clone_urls.is_empty() {
            let urls = repo
                .clone_urls
                .iter()
                .map(|u| format!("git clone {}", u))
                .collect::<Vec<_>>()
                .join("\n");
            tree.add_static_file(repo_dir, "CLONE_URLS.txt", urls.into_bytes());
            file_count += 1;
        }

        if issue_count > 0 {
            let issues_dir = tree.get_or_create_dir(repo_dir, "issues");
            for issue in issues_by_repo.get(repo.addr.as_str()).unwrap() {
                let short_id = &issue.event_id[..12.min(issue.event_id.len())];
                let filename = format!("{}.md", short_id);

                let mut content = String::new();
                content.push_str(&format!("# {}\n\n", issue.subject));
                content.push_str(&format!(
                    "**Author:** `{}...`\n\n",
                    &issue.author[..16.min(issue.author.len())]
                ));
                content.push_str(&format!(
                    "**Date:** {}\n\n",
                    format_unix_ts(issue.created_at)
                ));
                content.push_str("---\n\n");
                content.push_str(&issue.content);

                tree.add_static_file(issues_dir, &filename, content.into_bytes());
                file_count += 1;
            }
        }

        if patch_count > 0 {
            let patches_dir = tree.get_or_create_dir(repo_dir, "patches");
            for patch in patches_by_repo.get(repo.addr.as_str()).unwrap() {
                let prefix = if patch.is_cover_letter {
                    "0000-cover-letter"
                } else {
                    &patch.event_id[..12.min(patch.event_id.len())]
                };
                let filename = format!("{}.patch", prefix);

                tree.add_static_file(patches_dir, &filename, patch.content.clone().into_bytes());
                file_count += 1;
            }
        }
    }

    file_count
}

fn format_unix_ts(ts: u64) -> String {
    let days = ts / 86400;
    let hours = (ts % 86400) / 3600;
    let mins = (ts % 3600) / 60;
    format!("{}d {}h {}m", days, hours, mins)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_tags() -> Vec<Vec<&'static str>> {
        vec![
            vec!["d", "my-project"],
            vec!["name", "My Project"],
            vec!["description", "A test project"],
            vec!["clone", "https://relay.ngit.dev/npub1.../my-project.git"],
            vec!["clone", "https://gitnostr.com/npub1.../my-project.git"],
            vec!["web", "https://gitworkshop.dev/npub1.../my-project"],
            vec!["relays", "wss://relay.ngit.dev"],
            vec!["maintainers", "npub1abc..."],
        ]
    }

    #[test]
    fn t01_parse_repo_basic() {
        let repo = parse_repo_from_tags(&repo_tags(), "abc123pubkey", "evt123", 1700000000)
            .expect("should parse");

        assert_eq!(repo.repo_id, "my-project");
        assert_eq!(repo.name, "My Project");
        assert_eq!(repo.description, "A test project");
        assert_eq!(repo.clone_urls.len(), 2);
        assert!(repo.clone_urls[0].contains("relay.ngit.dev"));
        assert_eq!(repo.web_urls.len(), 1);
        assert_eq!(repo.relays.len(), 1);
        assert_eq!(repo.maintainers.len(), 1);
        assert_eq!(repo.addr, "30617:abc123pubkey:my-project");
    }

    #[test]
    fn t02_parse_repo_missing_d_returns_none() {
        let tags = vec![vec!["name", "No ID"]];
        assert!(parse_repo_from_tags(&tags, "pk", "e", 0).is_none());
    }

    #[test]
    fn t03_parse_repo_name_defaults_to_id() {
        let tags = vec![vec!["d", "just-id"]];
        let repo = parse_repo_from_tags(&tags, "pk", "e", 0).unwrap();
        assert_eq!(repo.name, "just-id");
    }

    #[test]
    fn t04_parse_issue_basic() {
        let tags = vec![
            vec!["a", "30617:abc:my-project"],
            vec!["subject", "Bug: crash on startup"],
        ];
        let issue = parse_issue_from_tags(&tags, "This is the issue body", "evt1", "author1", 0)
            .expect("should parse");

        assert_eq!(issue.repo_ref, "30617:abc:my-project");
        assert_eq!(issue.subject, "Bug: crash on startup");
        assert_eq!(issue.content, "This is the issue body");
    }

    #[test]
    fn t05_parse_issue_missing_a_returns_none() {
        let tags = vec![vec!["subject", "No repo ref"]];
        assert!(parse_issue_from_tags(&tags, "content", "e", "a", 0).is_none());
    }

    #[test]
    fn t06_parse_issue_no_subject_defaults() {
        let tags = vec![vec!["a", "30617:abc:repo"]];
        let issue = parse_issue_from_tags(&tags, "body", "e", "a", 0).unwrap();
        assert_eq!(issue.subject, "(no subject)");
    }

    #[test]
    fn t07_parse_patch_basic() {
        let tags = vec![
            vec!["a", "30617:abc:my-project"],
            vec!["branch-name", "fix/readme"],
        ];
        let patch = parse_patch_from_tags(
            &tags,
            "From abc123...\nSubject: [PATCH] fix readme\n---\ndiff",
            "evt1",
            "auth1",
            0,
        )
        .expect("should parse");

        assert_eq!(patch.repo_ref, "30617:abc:my-project");
        assert_eq!(patch.branch_name, "fix/readme");
        assert!(!patch.is_cover_letter);
        assert!(patch.content.contains("[PATCH]"));
    }

    #[test]
    fn t08_parse_patch_cover_letter() {
        let tags = vec![
            vec!["a", "30617:abc:repo"],
            vec!["branch-name", "feature"],
            vec!["t", "cover-letter"],
        ];
        let patch = parse_patch_from_tags(&tags, "cover letter content", "e", "a", 0).unwrap();
        assert!(patch.is_cover_letter);
    }

    #[test]
    fn t09_parse_patch_missing_a_returns_none() {
        let tags = vec![vec!["branch-name", "main"]];
        assert!(parse_patch_from_tags(&tags, "content", "e", "a", 0).is_none());
    }

    #[test]
    fn t10_parse_patch_no_branch_name_defaults() {
        let tags = vec![vec!["a", "30617:abc:repo"]];
        let patch = parse_patch_from_tags(&tags, "content", "e", "a", 0).unwrap();
        assert_eq!(patch.branch_name, "unknown");
    }

    #[test]
    fn t11_build_tree_creates_hierarchy() {
        let mut tree = Tree::new();
        let pubkey = "abc123";

        let repo = Nip34Repo {
            repo_id: "my-project".to_string(),
            name: "My Project".to_string(),
            description: "Test".to_string(),
            clone_urls: vec!["https://example.com/repo.git".to_string()],
            web_urls: vec![],
            relays: vec![],
            maintainers: vec![],
            created_at: 1700000000,
            event_id: "abc".to_string(),
            addr: "30617:abc123:my-project".to_string(),
        };

        let data = Nip34Data {
            repos: vec![repo],
            issues: vec![],
            patches: vec![],
        };

        let count = build_nip34_tree(
            &mut tree,
            pubkey,
            &data,
            false,
            std::path::Path::new("/tmp/test-cache"),
        );
        assert!(count >= 2, "should create INFO.md + CLONE_URLS.txt");

        let git = tree.lookup(tree.root(), "git");
        assert!(git.is_some(), "/git should exist");

        let git = git.unwrap();
        let pk = tree.lookup(git, pubkey);
        assert!(pk.is_some(), "/git/<pubkey>/ should exist");

        let pk = pk.unwrap();
        let repo_dir = tree.lookup(pk, "my-project");
        assert!(repo_dir.is_some(), "/git/<pubkey>/my-project/ should exist");

        let repo_dir = repo_dir.unwrap();
        assert!(
            tree.lookup(repo_dir, "INFO.md").is_some(),
            "INFO.md should exist"
        );
        assert!(
            tree.lookup(repo_dir, "CLONE_URLS.txt").is_some(),
            "CLONE_URLS.txt should exist"
        );
    }

    #[test]
    fn t12_build_tree_with_issues_and_patches() {
        let mut tree = Tree::new();
        let pubkey = "pk";

        let addr = "30617:pk:test-repo".to_string();
        let repo = Nip34Repo {
            repo_id: "test-repo".to_string(),
            name: "Test".to_string(),
            description: "".to_string(),
            clone_urls: vec![],
            web_urls: vec![],
            relays: vec![],
            maintainers: vec![],
            created_at: 0,
            event_id: "r1".to_string(),
            addr: addr.clone(),
        };

        let issue = Nip34Issue {
            repo_ref: addr.clone(),
            subject: "Bug report".to_string(),
            content: "Something is broken".to_string(),
            event_id: "aaaa1111bbbb2222".to_string(),
            author: "feac2b853fe21fb7".to_string(),
            created_at: 1700000000,
        };

        let patch = Nip34Patch {
            repo_ref: addr.clone(),
            branch_name: "fix".to_string(),
            content: "diff --git a/foo b/foo".to_string(),
            is_cover_letter: false,
            event_id: "cccc3333dddd4444".to_string(),
            author: "feac2b853fe21fb7".to_string(),
            created_at: 1700000001,
        };

        let data = Nip34Data {
            repos: vec![repo],
            issues: vec![issue],
            patches: vec![patch],
        };

        let count = build_nip34_tree(
            &mut tree,
            pubkey,
            &data,
            false,
            std::path::Path::new("/tmp/test-cache"),
        );
        assert!(count >= 3, "should create INFO.md + issue + patch");

        let repo_dir = tree
            .lookup(tree.root(), "git")
            .and_then(|g| tree.lookup(g, pubkey))
            .and_then(|p| tree.lookup(p, "test-repo"))
            .expect("repo dir should exist");

        let issues_dir = tree.lookup(repo_dir, "issues");
        assert!(issues_dir.is_some(), "issues/ should exist");
        assert!(
            tree.lookup(issues_dir.unwrap(), "aaaa1111bbbb.md")
                .is_some(),
            "issue file should exist"
        );

        let patches_dir = tree.lookup(repo_dir, "patches");
        assert!(patches_dir.is_some(), "patches/ should exist");
    }

    #[test]
    fn t13_build_tree_empty_is_noop() {
        let mut tree = Tree::new();
        let data = Nip34Data {
            repos: vec![],
            issues: vec![],
            patches: vec![],
        };
        let count = build_nip34_tree(
            &mut tree,
            "pk",
            &data,
            false,
            std::path::Path::new("/tmp/test-cache"),
        );
        assert_eq!(count, 0);
        assert!(tree.lookup(tree.root(), "git").is_none());
    }

    #[test]
    fn t14_format_unix_ts() {
        let ts = 1700000000u64;
        let formatted = format_unix_ts(ts);
        assert!(formatted.contains("d"));
        assert!(formatted.contains("h"));
    }

    #[test]
    fn t15_build_tree_multiple_repos() {
        let mut tree = Tree::new();
        let pubkey = "pk";

        let repo1 = Nip34Repo {
            repo_id: "project-a".to_string(),
            name: "Project A".to_string(),
            description: "".to_string(),
            clone_urls: vec![],
            web_urls: vec![],
            relays: vec![],
            maintainers: vec![],
            created_at: 0,
            event_id: "a".to_string(),
            addr: "30617:pk:project-a".to_string(),
        };

        let repo2 = Nip34Repo {
            repo_id: "project-b".to_string(),
            name: "Project B".to_string(),
            description: "".to_string(),
            clone_urls: vec![],
            web_urls: vec![],
            relays: vec![],
            maintainers: vec![],
            created_at: 0,
            event_id: "b".to_string(),
            addr: "30617:pk:project-b".to_string(),
        };

        let data = Nip34Data {
            repos: vec![repo1, repo2],
            issues: vec![],
            patches: vec![],
        };

        build_nip34_tree(
            &mut tree,
            pubkey,
            &data,
            false,
            std::path::Path::new("/tmp/test-cache"),
        );

        let pk_dir = tree
            .lookup(tree.root(), "git")
            .and_then(|g| tree.lookup(g, pubkey))
            .unwrap();

        assert!(tree.lookup(pk_dir, "project-a").is_some());
        assert!(tree.lookup(pk_dir, "project-b").is_some());
    }
}
