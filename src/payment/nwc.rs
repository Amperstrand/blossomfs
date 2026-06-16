#![allow(dead_code)]

use std::time::Duration;

use nostr_sdk::prelude::*;
use thiserror::Error;

use super::PaymentError;
use super::PaymentStrategy;

const NWC_REQUEST_KIND: u16 = 23194;
const NWC_RESPONSE_KIND: u16 = 23195;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Error, Debug)]
pub enum NwcParseError {
    #[error("invalid NWC URI: must start with nostr+walletconnect://")]
    InvalidScheme,
    #[error("invalid NWC URI: missing wallet pubkey")]
    MissingPubkey,
    #[error("invalid NWC URI: missing relay parameter")]
    MissingRelay,
    #[error("invalid NWC URI: missing secret parameter")]
    MissingSecret,
    #[error("invalid wallet pubkey: {0}")]
    InvalidPubkey(String),
    #[error("invalid secret key: {0}")]
    InvalidSecret(String),
}

#[derive(Debug)]
pub struct NwcConfig {
    pub wallet_pubkey: PublicKey,
    pub relay: String,
    pub secret: SecretKey,
}

impl NwcConfig {
    pub fn client_keys(&self) -> Keys {
        Keys::new(self.secret.clone())
    }
}

pub fn parse_nwc_uri(uri: &str) -> Result<NwcConfig, NwcParseError> {
    let rest = uri
        .strip_prefix("nostr+walletconnect://")
        .ok_or(NwcParseError::InvalidScheme)?;

    let (pubkey_part, query) = rest.split_once('?').unwrap_or((rest, ""));

    let wallet_pubkey_hex = pubkey_part.trim();
    if wallet_pubkey_hex.is_empty() {
        return Err(NwcParseError::MissingPubkey);
    }

    let wallet_pubkey = PublicKey::from_hex(wallet_pubkey_hex)
        .map_err(|e| NwcParseError::InvalidPubkey(e.to_string()))?;

    let mut relay: Option<String> = None;
    let mut secret_hex: Option<String> = None;

    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            match key {
                "relay" => relay = Some(value.to_string()),
                "secret" => secret_hex = Some(value.to_string()),
                _ => {}
            }
        }
    }

    let relay = relay.ok_or(NwcParseError::MissingRelay)?;
    let secret_hex = secret_hex.ok_or(NwcParseError::MissingSecret)?;
    let secret = SecretKey::from_hex(&secret_hex)
        .map_err(|e| NwcParseError::InvalidSecret(e.to_string()))?;

    Ok(NwcConfig {
        wallet_pubkey,
        relay,
        secret,
    })
}

pub struct NwcStrategy {
    config: NwcConfig,
}

impl NwcStrategy {
    pub fn new(uri: &str) -> Result<Self, PaymentError> {
        let config = parse_nwc_uri(uri).map_err(PaymentError::NwcParse)?;
        Ok(Self { config })
    }

    pub fn from_config(config: NwcConfig) -> Self {
        Self { config }
    }

    fn encrypt_nip04(&self, _sk: &SecretKey, _peer_pk: &PublicKey, _msg: &str) -> Result<String, PaymentError> {
        Err(PaymentError::NwcEncrypt("NWC encryption not yet migrated to nostr-sdk 0.45-alpha".into()))
    }

    fn decrypt_nip04(&self, _sk: &SecretKey, _peer_pk: &PublicKey, _msg: &str) -> Result<String, PaymentError> {
        Err(PaymentError::NwcEncrypt("NWC decryption not yet migrated to nostr-sdk 0.45-alpha".into()))
    }
}

impl PaymentStrategy for NwcStrategy {
    fn pay(&self, payment_request: &str) -> Result<String, PaymentError> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| PaymentError::NwcRuntime(e.to_string()))?;

        rt.block_on(async {
            self.pay_async(payment_request).await
        })
    }
}

impl NwcStrategy {
    async fn pay_async(&self, payment_request: &str) -> Result<String, PaymentError> {
        let keys = self.config.client_keys();
        let client_pubkey = keys.public_key();

        let request_json = serde_json::json!({
            "method": "pay_cashu_request",
            "params": {
                "payment_request": payment_request
            }
        });
        let request_str = serde_json::to_string(&request_json)
            .map_err(|e| PaymentError::NwcSerialize(e.to_string()))?;

        // TODO: migrate to nostr-sdk 0.45-alpha encryption API
        // The nip04::encrypt/decrypt functions moved in 0.45-alpha
        let encrypted_content = self.encrypt_nip04(
            keys.secret_key(),
            &self.config.wallet_pubkey,
            &request_str,
        )?;

        let event = EventBuilder::new(Kind::Custom(NWC_REQUEST_KIND), encrypted_content)
            .tags(vec![Tag::public_key(self.config.wallet_pubkey)])
            .finalize(&keys)
            .map_err(|e| PaymentError::NwcSign(e.to_string()))?;

        let now = Timestamp::now();

        let client = Client::new();
        client
            .add_relay(&self.config.relay)
            .await
            .map_err(|e| PaymentError::NwcRelay(e.to_string()))?;
        client
            .connect()
            .await;

        let _ = client
            .send_event(&event)
            .await
            .map_err(|e| PaymentError::NwcSend(e.to_string()))?;

        let filter = Filter::new()
            .kind(Kind::Custom(NWC_RESPONSE_KIND))
            .author(self.config.wallet_pubkey)
            .pubkeys([client_pubkey])
            .since(now);

        let events = client
            .fetch_events(filter)
            .timeout(RESPONSE_TIMEOUT)
            .await
            .map_err(|e| PaymentError::NwcTimeout(e.to_string()))?;

        client.disconnect().await;

        let mut found_token: Option<String> = None;

        for event in events.iter() {
            let decrypted = match self.decrypt_nip04(
                keys.secret_key(),
                &self.config.wallet_pubkey,
                &event.content,
            ) {
                Ok(d) => d,
                Err(_) => continue,
            };

            let response: serde_json::Value = match serde_json::from_str(&decrypted) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if let Some(error) = response.get("error") {
                let msg = error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown wallet error");
                return Err(PaymentError::WalletRejected(msg.to_string()));
            }

            if let Some(token) = response
                .get("result")
                .and_then(|r| r.get("preimage"))
                .and_then(|t| t.as_str())
            {
                found_token = Some(token.to_string());
                break;
            }

            if let Some(token) = response
                .get("result")
                .and_then(|r| r.get("token"))
                .and_then(|t| t.as_str())
            {
                found_token = Some(token.to_string());
                break;
            }
        }

        client.disconnect().await;

        found_token.ok_or(PaymentError::NoResponse)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_nwc_uri_valid() {
        let keys = Keys::generate();
        let pk = keys.public_key().to_hex();
        let sk = keys.secret_key().to_secret_hex();
        let uri = format!(
            "nostr+walletconnect://{}?relay=wss://relay.example.com&secret={}",
            pk, sk
        );

        let config = parse_nwc_uri(&uri).expect("should parse valid URI");
        assert_eq!(config.wallet_pubkey.to_hex(), pk);
        assert_eq!(config.relay, "wss://relay.example.com");
    }

    #[test]
    fn test_parse_nwc_uri_invalid_scheme() {
        let result = parse_nwc_uri("https://not-nwc.example.com/path");
        assert!(result.is_err());
        match result.unwrap_err() {
            NwcParseError::InvalidScheme => {}
            other => panic!("expected InvalidScheme, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_nwc_uri_missing_relay() {
        let keys = Keys::generate();
        let pk = keys.public_key().to_hex();
        let sk = keys.secret_key().to_secret_hex();
        let uri = format!("nostr+walletconnect://{}?secret={}", pk, sk);

        let result = parse_nwc_uri(&uri);
        assert!(matches!(result, Err(NwcParseError::MissingRelay)));
    }

    #[test]
    fn test_parse_nwc_uri_missing_secret() {
        let keys = Keys::generate();
        let pk = keys.public_key().to_hex();
        let uri = format!(
            "nostr+walletconnect://{}?relay=wss://relay.example.com",
            pk
        );

        let result = parse_nwc_uri(&uri);
        assert!(matches!(result, Err(NwcParseError::MissingSecret)));
    }

    #[test]
    fn test_parse_nwc_uri_missing_pubkey() {
        let uri = "nostr+walletconnect://?relay=wss://relay.example.com&secret=abc";
        let result = parse_nwc_uri(uri);
        assert!(matches!(result, Err(NwcParseError::MissingPubkey)));
    }

    #[test]
    fn test_parse_nwc_uri_invalid_pubkey() {
        let uri =
            "nostr+walletconnect://invalid_pubkey?relay=wss://r.example&secret=abc";
        let result = parse_nwc_uri(uri);
        assert!(matches!(result, Err(NwcParseError::InvalidPubkey(_))));
    }

    #[test]
    fn test_nwc_config_client_keys() {
        let keys = Keys::generate();
        let config = NwcConfig {
            wallet_pubkey: keys.public_key(),
            relay: "wss://relay.example.com".to_string(),
            secret: keys.secret_key().clone(),
        };

        let client_keys = config.client_keys();
        assert_eq!(client_keys.public_key(), keys.public_key());
    }

    #[test]
    fn test_nwc_strategy_from_config() {
        let keys = Keys::generate();
        let config = NwcConfig {
            wallet_pubkey: keys.public_key(),
            relay: "wss://relay.example.com".to_string(),
            secret: keys.secret_key().clone(),
        };

        let strategy = NwcStrategy::from_config(config);
        assert_eq!(
            strategy.config.wallet_pubkey,
            keys.public_key()
        );
    }

    #[test]
    fn test_nwc_strategy_new_invalid_uri() {
        let result = NwcStrategy::new("not-a-valid-uri");
        assert!(result.is_err());
    }
}
