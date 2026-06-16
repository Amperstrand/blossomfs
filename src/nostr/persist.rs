//! Directory structure persistence via Nostr (kind 30078).
//!
//! On mount, fetches the latest kind 30078 replaceable event for the given
//! drive name and rebuilds user-created directories/files. On unmount,
//! serializes the current tree and publishes a new kind 30078 event.
//!
//! Event format (NIP-33 replaceable):
//! - kind: 30078 (application-specific)
//! - `["d", "<drive-name>"]` — NIP-33 identifier
//! - `["l", "blossomfs"]` — label for filtering
//! - `["folder", "<path>"]` — empty directory
//! - `["x", "<sha256>", "<path>", "<size>", "<mime>"]` — file entry

#![allow(dead_code)]

use std::time::Duration;
use thiserror::Error;

use nostr_sdk::prelude::*;

/// Kind 30078 — application-specific replaceable event (NIP-33).
const PERSIST_KIND: u16 = 30078;

/// Label tag value identifying BlossomFS persistence events.
const LABEL: &str = "blossomfs";

#[derive(Error, Debug)]
pub enum PersistError {
    #[error("nostr client error: {0}")]
    Client(#[from] nostr_sdk::client::Error),
    #[error("nostr sign error: {0}")]
    Sign(String),
    #[error("no persisted event found for drive '{0}'")]
    NotFound(String),
}

/// Fetch the latest persistence event for a drive from the given relays.
///
/// Queries for kind 30078 events by the given pubkey, filters by
/// `["d", drive_name]` and `["l", "blossomfs"]`, and returns the tags
/// from the newest event.
pub async fn fetch_persist_event(
    relays: &[String],
    keys: &Keys,
    drive_name: &str,
) -> Result<Vec<Vec<String>>, PersistError> {
    let pubkey = keys.public_key();

    let client = Client::default();
    for relay_url in relays {
        client.add_relay(relay_url).await?;
    }
    client.connect().await;

    let filter = Filter::new()
        .kind(Kind::Custom(PERSIST_KIND))
        .author(pubkey)
        .limit(10);

    let events = client
        .fetch_events(filter)
        .timeout(Duration::from_secs(10))
        .await?;

    client.disconnect().await;

    // Filter client-side for matching d-tag and l-tag
    let event = events.iter().find(|e| {
        let raw: Vec<Vec<&str>> = e
            .tags
            .iter()
            .map(|t| t.as_slice().iter().map(|s| s.as_str()).collect())
            .collect();

        let has_d = raw
            .iter()
            .any(|t: &Vec<&str>| t.len() >= 2 && t[0] == "d" && t[1] == drive_name);
        let has_l = raw
            .iter()
            .any(|t: &Vec<&str>| t.len() >= 2 && t[0] == "l" && t[1] == LABEL);

        has_d && has_l
    });

    let event = event.ok_or_else(|| PersistError::NotFound(drive_name.to_string()))?;

    Ok(event_to_tags(event))
}

/// Publish a persistence event with the given drive name and tags.
///
/// Signs a kind 30078 event with `["d", drive_name]`, `["l", "blossomfs"]`,
/// and the provided folder/file tags, then publishes to the given relays.
pub async fn publish_persist_event(
    relays: &[String],
    keys: &Keys,
    drive_name: &str,
    tags: &[Vec<String>],
) -> Result<(), PersistError> {
    // Build Nostr tags: metadata tags first, then content tags
    let mut nostr_tags: Vec<Tag> = vec![
        Tag::custom("d", [drive_name.to_string()]),
        Tag::custom("l", [LABEL.to_string()]),
    ];

    for tag in tags {
        match tag[0].as_str() {
            "folder" if tag.len() >= 2 => {
                nostr_tags.push(Tag::custom("folder", [tag[1].clone()]));
            }
            "x" if tag.len() >= 5 => {
                nostr_tags.push(Tag::custom(
                    "x",
                    [
                        tag[1].clone(),
                        tag[2].clone(),
                        tag[3].clone(),
                        tag[4].clone(),
                    ],
                ));
            }
            _ => {}
        }
    }

    let event = EventBuilder::new(Kind::Custom(PERSIST_KIND), "")
        .tags(nostr_tags)
        .finalize(keys)
        .map_err(|e| PersistError::Sign(e.to_string()))?;

    let client = Client::default();
    for relay_url in relays {
        client.add_relay(relay_url).await?;
    }
    client.connect().await;
    client.send_event(&event).await?;
    client.disconnect().await;

    Ok(())
}

/// Extract tag strings from a Nostr event.
///
/// Filters out metadata tags (d, l), keeping only folder/x tags.
fn event_to_tags(event: &Event) -> Vec<Vec<String>> {
    event
        .tags
        .iter()
        .map(|t| {
            t.as_slice()
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<String>>()
        })
        .filter(|v: &Vec<String>| {
            v.first()
                .is_some_and(|kind| kind == "folder" || kind == "x")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── S39: event_to_tags filters out metadata tags ──

    #[test]
    fn s39_event_to_tags_keeps_folder_and_x_only() {
        let keys = Keys::generate();

        let nostr_tags: Vec<Tag> = vec![
            Tag::custom("d", ["test-drive".to_string()]),
            Tag::custom("l", [LABEL.to_string()]),
            Tag::custom("folder", ["/docs".to_string()]),
            Tag::custom(
                "x",
                [
                    "abc123".to_string(),
                    "/docs/readme.md".to_string(),
                    "100".to_string(),
                    "text/markdown".to_string(),
                ],
            ),
        ];

        let event = EventBuilder::new(Kind::Custom(PERSIST_KIND), "")
            .tags(nostr_tags)
            .finalize(&keys)
            .expect("should finalize event");

        let extracted = event_to_tags(&event);

        assert_eq!(extracted.len(), 2);
        assert_eq!(extracted[0][0], "folder");
        assert_eq!(extracted[0][1], "/docs");
        assert_eq!(extracted[1][0], "x");
        assert_eq!(extracted[1][1], "abc123");
        assert_eq!(extracted[1][2], "/docs/readme.md");
    }

    // ── S40: event_to_tags on empty event returns empty ──

    #[test]
    fn s40_event_to_tags_empty() {
        let keys = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(PERSIST_KIND), "")
            .tags(vec![Tag::custom("d", ["empty".to_string()])])
            .finalize(&keys)
            .expect("should finalize event");

        let extracted = event_to_tags(&event);
        assert!(extracted.is_empty());
    }

    // ── S41: round-trip sign → extract ──

    #[test]
    fn s41_round_trip_sign_and_extract() {
        let keys = Keys::generate();

        let nostr_tags: Vec<Tag> = vec![
            Tag::custom("d", ["rt-drive".to_string()]),
            Tag::custom("l", [LABEL.to_string()]),
            Tag::custom("folder", ["/projects".to_string()]),
            Tag::custom("folder", ["/projects/src".to_string()]),
            Tag::custom(
                "x",
                [
                    "deadbeef".to_string(),
                    "/projects/src/main.rs".to_string(),
                    "42".to_string(),
                    "text/rust".to_string(),
                ],
            ),
        ];

        let event = EventBuilder::new(Kind::Custom(PERSIST_KIND), "")
            .tags(nostr_tags)
            .finalize(&keys)
            .expect("should finalize event");

        assert_eq!(event.kind, Kind::Custom(PERSIST_KIND));

        let extracted = event_to_tags(&event);
        assert_eq!(extracted.len(), 3);
        assert_eq!(extracted[0], vec!["folder", "/projects"]);
        assert_eq!(extracted[1], vec!["folder", "/projects/src"]);
        assert_eq!(
            extracted[2],
            vec!["x", "deadbeef", "/projects/src/main.rs", "42", "text/rust"]
        );
    }

    // ── S42: event has correct kind and d-tag ──

    #[test]
    fn s42_event_kind_and_d_tag() {
        let keys = Keys::generate();

        let nostr_tags: Vec<Tag> = vec![
            Tag::custom("d", ["my-drive".to_string()]),
            Tag::custom("l", [LABEL.to_string()]),
        ];

        let event = EventBuilder::new(Kind::Custom(PERSIST_KIND), "")
            .tags(nostr_tags)
            .finalize(&keys)
            .expect("should finalize event");

        assert_eq!(event.kind, Kind::Custom(PERSIST_KIND));

        // Verify d-tag via raw extraction
        let raw: Vec<Vec<String>> = event
            .tags
            .iter()
            .map(|t| {
                t.as_slice()
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<String>>()
            })
            .collect();

        let d = raw
            .iter()
            .find(|t: &&Vec<String>| t.len() >= 2 && t[0] == "d")
            .expect("must have d tag");
        assert_eq!(d[1], "my-drive");

        let l = raw
            .iter()
            .find(|t: &&Vec<String>| t.len() >= 2 && t[0] == "l")
            .expect("must have l tag");
        assert_eq!(l[1], LABEL);
    }
}
