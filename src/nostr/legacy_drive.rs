#![allow(dead_code)]

use std::time::Duration;
use thiserror::Error;

use nostr_sdk::prelude::*;

#[derive(Error, Debug)]
pub enum DriveError {
    #[error("nostr client error: {0}")]
    Client(#[from] nostr_sdk::client::Error),
    #[error("invalid public key: {0}")]
    InvalidPubkey(String),
    #[error("no drive events found")]
    NoDrives,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DriveFile {
    pub sha256: String,
    pub path: String,
    pub size: Option<u64>,
    pub mime: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DriveFolder {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DriveEntry {
    File(DriveFile),
    Folder(DriveFolder),
}

#[derive(Debug, Clone)]
pub struct ParsedDrive {
    pub drive_id: String,
    pub entries: Vec<DriveEntry>,
}

pub async fn fetch_drive_events(
    relays: &[String],
    pubkey_hex: &str,
) -> Result<Vec<ParsedDrive>, DriveError> {
    let public_key =
        PublicKey::from_hex(pubkey_hex).map_err(|e| DriveError::InvalidPubkey(e.to_string()))?;

    let client = Client::default();
    for relay_url in relays {
        client.add_relay(relay_url).await?;
    }
    client.connect().await;

    let filter = Filter::new().kind(Kind::Custom(30563)).author(public_key);

    let events = client
        .fetch_events(filter)
        .timeout(Duration::from_secs(10))
        .await?;

    client.disconnect().await;

    let drives: Vec<ParsedDrive> = events
        .iter()
        .map(|event| {
            let raw_tags: Vec<Vec<&str>> = event
                .tags
                .iter()
                .map(|t| t.as_slice().iter().map(|s| s.as_str()).collect())
                .collect();
            parse_drive_from_tags(&raw_tags)
        })
        .collect();

    if drives.is_empty() {
        Err(DriveError::NoDrives)
    } else {
        Ok(drives)
    }
}

fn parse_drive_from_tags(tags: &[Vec<&str>]) -> ParsedDrive {
    let mut drive_id = String::from("default");
    let mut entries = Vec::new();

    for tag in tags {
        if tag.is_empty() {
            continue;
        }
        match tag[0] {
            "d" if tag.len() >= 2 => {
                drive_id = tag[1].to_string();
            }
            "x" => {
                if let Some(file) = parse_x_tag(tag) {
                    entries.push(DriveEntry::File(file));
                }
            }
            "folder" if tag.len() >= 2 => {
                entries.push(DriveEntry::Folder(DriveFolder {
                    path: tag[1].to_string(),
                }));
            }
            _ => {}
        }
    }

    ParsedDrive { drive_id, entries }
}

fn parse_x_tag(tag: &[&str]) -> Option<DriveFile> {
    if tag.len() < 3 {
        return None;
    }
    let sha256 = tag[1].to_string();
    let path = tag[2].to_string();

    if sha256.is_empty() || path.is_empty() {
        return None;
    }

    let size = tag.get(3).and_then(|s| s.parse::<u64>().ok());
    let mime = tag.get(4).map(|s| s.to_string());

    Some(DriveFile {
        sha256,
        path,
        size,
        mime,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_single_file_drive() {
        let tags = vec![
            vec!["d", "my-drive"],
            vec!["x", "abc123", "/photos/img.png", "1024", "image/png"],
        ];
        let drive = parse_drive_from_tags(&tags);

        assert_eq!(drive.drive_id, "my-drive");
        assert_eq!(drive.entries.len(), 1);
        match &drive.entries[0] {
            DriveEntry::File(f) => {
                assert_eq!(f.sha256, "abc123");
                assert_eq!(f.path, "/photos/img.png");
                assert_eq!(f.size, Some(1024));
                assert_eq!(f.mime.as_deref(), Some("image/png"));
            }
            _ => panic!("expected file entry"),
        }
    }

    #[test]
    fn test_parse_multiple_files() {
        let tags = vec![
            vec!["d", "media"],
            vec!["x", "hash1", "/a.txt", "100", "text/plain"],
            vec!["x", "hash2", "/b.jpg", "200", "image/jpeg"],
            vec!["x", "hash3", "/sub/c.pdf", "300", "application/pdf"],
        ];
        let drive = parse_drive_from_tags(&tags);
        assert_eq!(drive.drive_id, "media");
        assert_eq!(drive.entries.len(), 3);
    }

    #[test]
    fn test_parse_folder_entries() {
        let tags = vec![
            vec!["d", "drive"],
            vec!["folder", "/empty-dir"],
            vec!["folder", "/another/empty"],
        ];
        let drive = parse_drive_from_tags(&tags);
        assert_eq!(drive.entries.len(), 2);
        assert!(matches!(&drive.entries[0], DriveEntry::Folder(f) if f.path == "/empty-dir"));
        assert!(matches!(&drive.entries[1], DriveEntry::Folder(f) if f.path == "/another/empty"));
    }

    #[test]
    fn test_parse_mixed_files_and_folders() {
        let tags = vec![
            vec!["d", "mixed"],
            vec!["folder", "/archive"],
            vec!["x", "h1", "/archive/doc.pdf", "500", "application/pdf"],
            vec!["x", "h2", "/readme.txt", "10", "text/plain"],
        ];
        let drive = parse_drive_from_tags(&tags);
        assert_eq!(drive.entries.len(), 3);
    }

    #[test]
    fn test_parse_default_drive_id_when_missing() {
        let tags = vec![vec!["x", "h1", "/file.txt", "10", "text/plain"]];
        let drive = parse_drive_from_tags(&tags);
        assert_eq!(drive.drive_id, "default");
    }

    #[test]
    fn test_parse_x_tag_missing_sha256() {
        let tags = vec![
            vec!["d", "d"],
            vec!["x", "", "/file.txt", "10", "text/plain"],
        ];
        let drive = parse_drive_from_tags(&tags);
        assert!(drive.entries.is_empty());
    }

    #[test]
    fn test_parse_x_tag_missing_path() {
        let tags = vec![vec!["d", "d"], vec!["x", "hash123", "", "10"]];
        let drive = parse_drive_from_tags(&tags);
        assert!(drive.entries.is_empty());
    }

    #[test]
    fn test_parse_x_tag_too_short() {
        let tags = vec![vec!["d", "d"], vec!["x", "only-sha"]];
        let drive = parse_drive_from_tags(&tags);
        assert!(drive.entries.is_empty());
    }

    #[test]
    fn test_parse_x_tag_no_size_no_mime() {
        let tags = vec![vec!["d", "d"], vec!["x", "sha", "/file.bin"]];
        let drive = parse_drive_from_tags(&tags);
        assert_eq!(drive.entries.len(), 1);
        match &drive.entries[0] {
            DriveEntry::File(f) => {
                assert_eq!(f.size, None);
                assert_eq!(f.mime, None);
            }
            _ => panic!("expected file"),
        }
    }

    #[test]
    fn test_parse_empty_tags() {
        let tags: Vec<Vec<&str>> = vec![];
        let drive = parse_drive_from_tags(&tags);
        assert_eq!(drive.drive_id, "default");
        assert!(drive.entries.is_empty());
    }

    #[test]
    fn test_parse_ignores_unknown_tags() {
        let tags = vec![
            vec!["d", "d"],
            vec!["name", "My Drive"],
            vec!["x", "h1", "/f.txt", "1", "text/plain"],
            vec!["summary", "test drive"],
        ];
        let drive = parse_drive_from_tags(&tags);
        assert_eq!(drive.entries.len(), 1);
    }

    #[test]
    fn test_parse_invalid_size_treated_as_none() {
        let tags = vec![
            vec!["d", "d"],
            vec!["x", "h1", "/f.txt", "not-a-number", "text/plain"],
        ];
        let drive = parse_drive_from_tags(&tags);
        assert_eq!(drive.entries.len(), 1);
        match &drive.entries[0] {
            DriveEntry::File(f) => assert_eq!(f.size, None),
            _ => panic!("expected file"),
        }
    }

    #[test]
    fn test_parse_multiple_drives_separate() {
        let tags1 = vec![vec!["d", "drive-a"], vec!["x", "h1", "/a.txt", "1"]];
        let tags2 = vec![vec!["d", "drive-b"], vec!["x", "h2", "/b.txt", "2"]];
        let d1 = parse_drive_from_tags(&tags1);
        let d2 = parse_drive_from_tags(&tags2);
        assert_eq!(d1.drive_id, "drive-a");
        assert_eq!(d2.drive_id, "drive-b");
        assert_eq!(d1.entries.len(), 1);
        assert_eq!(d2.entries.len(), 1);
    }
}
