//! Cashu payment handling for BlossomFS (BUD-07 payment flow).
//!
//! When a Blossom server returns HTTP 402, it includes an `X-Cashu` header
//! containing a NUT-18 payment request (`creqA...`). This module provides
//! strategies for producing a payment proof (`cashuB...` token) to retry
//! the upload.
//!
//! Two strategies are supported:
//! - `TokenStrategy`: pre-funded Cashu token from a file (CI/automated use)
//! - `NwcStrategy`: NWC/CWC wallet connection via Nostr relays (interactive use)

#![allow(dead_code)]

pub mod cbor;
pub mod nwc;
pub mod token;

use thiserror::Error;

#[allow(unused_imports)]
pub use cbor::{CashuToken, Nut18Request, Proof};
pub use nwc::NwcStrategy;
pub use token::TokenStrategy;

#[derive(Error, Debug)]
pub enum PaymentError {
    #[error("failed to read token file: {0}")]
    TokenFileRead(String),
    #[error("invalid token: {0}")]
    InvalidToken(String),
    #[error("NWC URI parse error: {0}")]
    NwcParse(#[from] nwc::NwcParseError),
    #[error("NWC serialization error: {0}")]
    NwcSerialize(String),
    #[error("NWC encryption error: {0}")]
    NwcEncrypt(String),
    #[error("NWC event signing error: {0}")]
    NwcSign(String),
    #[error("NWC relay error: {0}")]
    NwcRelay(String),
    #[error("NWC send error: {0}")]
    NwcSend(String),
    #[error("NWC timeout or fetch error: {0}")]
    NwcTimeout(String),
    #[error("NWC runtime error: {0}")]
    NwcRuntime(String),
    #[error("wallet rejected payment: {0}")]
    WalletRejected(String),
    #[error("no response from wallet within timeout")]
    NoResponse,
    #[error("payment not configured")]
    NotConfigured,
}

pub trait PaymentStrategy: Send + Sync {
    fn pay(&self, payment_request: &str) -> Result<String, PaymentError>;
}

pub enum PaymentHandler {
    Token {
        token: String,
    },
    Nwc {
        uri: String,
        relay: String,
        secret: String,
        wallet_pubkey: String,
    },
    None,
}

impl PaymentHandler {
    pub fn build_strategy(&self) -> Option<Box<dyn PaymentStrategy>> {
        match self {
            PaymentHandler::Token { token } => Some(Box::new(TokenStrategy::new(token.clone()))),
            PaymentHandler::Nwc { uri, .. } => match NwcStrategy::new(uri) {
                Ok(s) => Some(Box::new(s)),
                Err(e) => {
                    tracing::error!("failed to create NWC strategy: {}", e);
                    None
                }
            },
            PaymentHandler::None => None,
        }
    }
}

pub struct NoPayment;

impl PaymentStrategy for NoPayment {
    fn pay(&self, _payment_request: &str) -> Result<String, PaymentError> {
        Err(PaymentError::NotConfigured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_payment_returns_not_configured() {
        let np = NoPayment;
        let result = np.pay("creqAtest");
        assert!(matches!(result, Err(PaymentError::NotConfigured)));
    }

    #[test]
    fn test_payment_handler_none_builds_none() {
        let handler = PaymentHandler::None;
        let strategy = handler.build_strategy();
        assert!(strategy.is_none());
    }

    #[test]
    fn test_payment_handler_token_builds_strategy() {
        let handler = PaymentHandler::Token {
            token: "cashuBdGVzdA".to_string(),
        };
        let strategy = handler.build_strategy();
        assert!(strategy.is_some());
    }

    #[test]
    fn test_payment_handler_nwc_invalid_returns_none() {
        let handler = PaymentHandler::Nwc {
            uri: "invalid-uri".to_string(),
            relay: String::new(),
            secret: String::new(),
            wallet_pubkey: String::new(),
        };
        let strategy = handler.build_strategy();
        assert!(strategy.is_none());
    }
}
