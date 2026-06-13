#![allow(dead_code)]

use std::time::Duration;
use thiserror::Error;

use nostr_sdk::prelude::*;

#[derive(Error, Debug)]
pub enum Nip94Error {
    #[error("nostr client error: {0}")]
    Client(#[from] nostr_sdk::client::Error),
    #[error("invalid public key: {0}")]
    InvalidPubkey(String),
    #[error("no nip-94 events found")]
    NoEvents,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FileMeta {
    pub sha256: Option<String>,
    pub url: Option<String>,
    pub mime_type: Option<String>,
    pub size: Option<u64>,
    pub dim: Option<String>,
    pub blurhash: Option<String>,
    pub alt: Option<String>,
}

pub async fn fetch_nip94_events(
    relays: &[String],
    pubkey_hex: &str,
) -> Result<Vec<FileMeta>, Nip94Error> {
    let public_key = PublicKey::from_hex(pubkey_hex)
        .map_err(|e| Nip94Error::InvalidPubkey(e.to_string()))?;

    let client = Client::default();
    for relay_url in relays {
        client.add_relay(relay_url).await?;
    }
    client.connect().await;

    let filter = Filter::new()
        .kind(Kind::Custom(1063))
        .author(public_key);

    let events = client
        .fetch_events(filter)
        .timeout(Duration::from_secs(10))
        .await?;

    client.disconnect().await;

    let metas: Vec<FileMeta> = events
        .iter()
        .map(|event| {
            let raw_tags: Vec<Vec<&str>> = event
                .tags
                .iter()
                .map(|t| t.as_slice().iter().map(|s| s.as_str()).collect())
                .collect();
            parse_nip94_from_tags(&raw_tags)
        })
        .collect();

    if metas.is_empty() {
        Err(Nip94Error::NoEvents)
    } else {
        Ok(metas)
    }
}

fn parse_nip94_from_tags(tags: &[Vec<&str>]) -> FileMeta {
    let mut meta = FileMeta {
        sha256: None,
        url: None,
        mime_type: None,
        size: None,
        dim: None,
        blurhash: None,
        alt: None,
    };

    for tag in tags {
        if tag.len() < 2 {
            continue;
        }
        match tag[0] {
            "x" => {
                meta.sha256 = Some(tag[1].to_string());
            }
            "ox" => {
                if meta.sha256.is_none() {
                    meta.sha256 = Some(tag[1].to_string());
                }
            }
            "url" => {
                meta.url = Some(tag[1].to_string());
            }
            "m" => {
                meta.mime_type = Some(tag[1].to_string());
            }
            "size" => {
                meta.size = tag[1].parse::<u64>().ok();
            }
            "dim" => {
                meta.dim = Some(tag[1].to_string());
            }
            "blurhash" => {
                meta.blurhash = Some(tag[1].to_string());
            }
            "alt" => {
                meta.alt = Some(tag[1].to_string());
            }
            _ => {}
        }
    }

    meta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_full_metadata() {
        let tags = vec![
            vec!["url", "https://cdn.example.com/abc123.png"],
            vec!["x", "abc123def456"],
            vec!["m", "image/png"],
            vec!["size", "1048576"],
            vec!["dim", "1920x1080"],
            vec!["blurhash", "LEHV6nWB2yk8pyo0adR*.7kCMdnj"],
            vec!["alt", "A beautiful sunset"],
        ];
        let meta = parse_nip94_from_tags(&tags);

        assert_eq!(meta.sha256.as_deref(), Some("abc123def456"));
        assert_eq!(meta.url.as_deref(), Some("https://cdn.example.com/abc123.png"));
        assert_eq!(meta.mime_type.as_deref(), Some("image/png"));
        assert_eq!(meta.size, Some(1048576));
        assert_eq!(meta.dim.as_deref(), Some("1920x1080"));
        assert_eq!(meta.blurhash.as_deref(), Some("LEHV6nWB2yk8pyo0adR*.7kCMdnj"));
        assert_eq!(meta.alt.as_deref(), Some("A beautiful sunset"));
    }

    #[test]
    fn test_parse_minimal_only_sha256() {
        let tags = vec![vec!["x", "deadbeef"]];
        let meta = parse_nip94_from_tags(&tags);
        assert_eq!(meta.sha256.as_deref(), Some("deadbeef"));
        assert!(meta.url.is_none());
        assert!(meta.mime_type.is_none());
        assert!(meta.size.is_none());
    }

    #[test]
    fn test_parse_ox_tag_sets_sha256_if_x_missing() {
        let tags = vec![vec!["ox", "originalhash"]];
        let meta = parse_nip94_from_tags(&tags);
        assert_eq!(meta.sha256.as_deref(), Some("originalhash"));
    }

    #[test]
    fn test_parse_x_takes_priority_over_ox() {
        let tags = vec![
            vec!["ox", "originalhash"],
            vec!["x", "verifiedhash"],
        ];
        let meta = parse_nip94_from_tags(&tags);
        assert_eq!(meta.sha256.as_deref(), Some("verifiedhash"));
    }

    #[test]
    fn test_parse_empty_tags() {
        let tags: Vec<Vec<&str>> = vec![];
        let meta = parse_nip94_from_tags(&tags);
        assert!(meta.sha256.is_none());
        assert!(meta.url.is_none());
    }

    #[test]
    fn test_parse_invalid_size_treated_as_none() {
        let tags = vec![
            vec!["x", "hash"],
            vec!["size", "not-a-number"],
        ];
        let meta = parse_nip94_from_tags(&tags);
        assert_eq!(meta.sha256.as_deref(), Some("hash"));
        assert_eq!(meta.size, None);
    }

    #[test]
    fn test_parse_ignores_unknown_tags() {
        let tags = vec![
            vec!["x", "hash"],
            vec!["nonce", "12345"],
            vec!["custom", "value"],
        ];
        let meta = parse_nip94_from_tags(&tags);
        assert_eq!(meta.sha256.as_deref(), Some("hash"));
        assert!(meta.url.is_none());
    }

    #[test]
    fn test_parse_short_tag_skipped() {
        let tags = vec![
            vec!["x"],
            vec!["url"],
        ];
        let meta = parse_nip94_from_tags(&tags);
        assert!(meta.sha256.is_none());
        assert!(meta.url.is_none());
    }

    #[test]
    fn test_parse_multiple_events_independent() {
        let tags1 = vec![
            vec!["x", "hash1"],
            vec!["m", "image/png"],
        ];
        let tags2 = vec![
            vec!["x", "hash2"],
            vec!["m", "video/mp4"],
            vec!["dim", "3840x2160"],
        ];
        let m1 = parse_nip94_from_tags(&tags1);
        let m2 = parse_nip94_from_tags(&tags2);
        assert_eq!(m1.sha256.as_deref(), Some("hash1"));
        assert_eq!(m1.mime_type.as_deref(), Some("image/png"));
        assert_eq!(m2.sha256.as_deref(), Some("hash2"));
        assert_eq!(m2.mime_type.as_deref(), Some("video/mp4"));
        assert_eq!(m2.dim.as_deref(), Some("3840x2160"));
    }

    #[test]
    fn test_parse_empty_sha256_value() {
        let tags = vec![vec!["x", ""]];
        let meta = parse_nip94_from_tags(&tags);
        assert_eq!(meta.sha256, Some(String::new()));
    }
}
