#![allow(dead_code)]

use super::PaymentError;
use super::PaymentStrategy;
use super::cbor::decode_creq;

pub struct TokenStrategy {
    token: String,
}

impl TokenStrategy {
    pub fn new(token: String) -> Self {
        Self { token }
    }

    pub fn from_file(path: &std::path::Path) -> Result<Self, PaymentError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| PaymentError::TokenFileRead(e.to_string()))?;
        let token = content.trim().to_string();
        if !token.starts_with(super::cbor::CASHUB_PREFIX) {
            return Err(PaymentError::InvalidToken(
                "file does not contain a valid cashuB token".to_string(),
            ));
        }
        Ok(Self { token })
    }
}

impl PaymentStrategy for TokenStrategy {
    fn pay(&self, payment_request: &str) -> Result<String, PaymentError> {
        match decode_creq(payment_request) {
            Ok(req) => {
                tracing::info!(
                    "paying {} {} from pre-funded token (server requested {} mint(s))",
                    req.amount,
                    req.unit,
                    req.mints.len()
                );
                Ok(self.token.clone())
            }
            Err(e) => {
                tracing::warn!("failed to decode payment request: {}", e);
                Ok(self.token.clone())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_token_strategy_returns_token() {
        let token = "cashuBdGVzdA".to_string();
        let strategy = TokenStrategy::new(token.clone());
        let creq = "creqAdGVzdA";
        let result = strategy.pay(creq).expect("should return token");
        assert_eq!(result, token);
    }

    #[test]
    fn test_token_strategy_from_file_valid() {
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "cashuBdGVzdA").unwrap();

        let strategy = TokenStrategy::from_file(tmp.path()).expect("should load token");
        assert_eq!(strategy.token, "cashuBdGVzdA");
    }

    #[test]
    fn test_token_strategy_from_file_invalid_prefix() {
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "not-a-cashu-token").unwrap();

        let result = TokenStrategy::from_file(tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_token_strategy_from_file_nonexistent() {
        let result = TokenStrategy::from_file(std::path::Path::new("/nonexistent/token.txt"));
        assert!(result.is_err());
    }

    #[test]
    fn test_token_strategy_from_file_with_whitespace() {
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "\n  cashuBdGVzdA  \n").unwrap();

        let strategy = TokenStrategy::from_file(tmp.path()).expect("should trim and load");
        assert_eq!(strategy.token, "cashuBdGVzdA");
    }
}
