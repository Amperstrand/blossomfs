#![allow(dead_code)]

use std::time::Duration;
use thiserror::Error;

use nostr_sdk::prelude::*;

#[derive(Error, Debug)]
pub enum DiscoveryError {
    #[error("nostr client error: {0}")]
    Client(#[from] nostr_sdk::client::Error),
    #[error("no servers found in kind 10063 event")]
    NoServers,
    #[error("invalid public key: {0}")]
    InvalidPubkey(String),
}

pub async fn discover_servers(
    relays: &[String],
    pubkey_hex: &str,
) -> Result<Vec<String>, DiscoveryError> {
    let public_key = PublicKey::from_hex(pubkey_hex)
        .map_err(|e| DiscoveryError::InvalidPubkey(e.to_string()))?;

    let client = Client::default();
    for relay_url in relays {
        client.add_relay(relay_url).await?;
    }
    client.connect().await;

    let filter = Filter::new()
        .kind(Kind::Custom(10063))
        .author(public_key);

    let events = client
        .fetch_events(filter)
        .timeout(Duration::from_secs(10))
        .await?;

    client.disconnect().await;

    let raw_tags: Vec<(&str, Option<&str>)> = events
        .first()
        .map(|event| {
            event
                .tags
                .iter()
                .map(|t| (t.kind(), t.content()))
                .collect()
        })
        .unwrap_or_default();

    let servers = extract_server_urls(&raw_tags);

    if servers.is_empty() {
        Err(DiscoveryError::NoServers)
    } else {
        Ok(servers)
    }
}

fn extract_server_urls(tags: &[(&str, Option<&str>)]) -> Vec<String> {
    tags.iter()
        .filter(|(kind, _)| *kind == "server")
        .filter_map(|(_, content)| content.map(|c| c.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_single_server() {
        let tags = vec![("server", Some("https://cdn.example.com"))];
        let result = extract_server_urls(&tags);
        assert_eq!(result, vec!["https://cdn.example.com"]);
    }

    #[test]
    fn test_extract_multiple_servers() {
        let tags = vec![
            ("server", Some("https://a.example.com")),
            ("server", Some("https://b.example.com")),
            ("server", Some("https://c.example.com")),
        ];
        let result = extract_server_urls(&tags);
        assert_eq!(
            result,
            vec![
                "https://a.example.com",
                "https://b.example.com",
                "https://c.example.com",
            ]
        );
    }

    #[test]
    fn test_extract_no_server_tags() {
        let tags = vec![
            ("name", Some("my-server")),
            ("about", Some("media server")),
        ];
        let result = extract_server_urls(&tags);
        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_server_without_content_skipped() {
        let tags = vec![
            ("server", None),
            ("server", Some("https://cdn.example.com")),
        ];
        let result = extract_server_urls(&tags);
        assert_eq!(result, vec!["https://cdn.example.com"]);
    }

    #[test]
    fn test_extract_preserves_duplicates() {
        let tags = vec![
            ("server", Some("https://dup.example.com")),
            ("server", Some("https://dup.example.com")),
        ];
        let result = extract_server_urls(&tags);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_extract_empty_tags() {
        let tags: Vec<(&str, Option<&str>)> = vec![];
        let result = extract_server_urls(&tags);
        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_mixed_tags() {
        let tags = vec![
            ("name", Some("My Server")),
            ("server", Some("https://real.example.com")),
            ("nip05", Some("_@example.com")),
            ("server", Some("https://backup.example.com")),
            ("server", None),
        ];
        let result = extract_server_urls(&tags);
        assert_eq!(
            result,
            vec![
                "https://real.example.com",
                "https://backup.example.com",
            ]
        );
    }
}
