#![allow(dead_code)]

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum CborError {
    #[error("invalid prefix: expected {expected}, got {got}")]
    InvalidPrefix { expected: String, got: String },
    #[error("base64 decode error: {0}")]
    Base64Decode(String),
    #[error("CBOR decode error: {0}")]
    CborDecode(String),
    #[error("missing field: {0}")]
    MissingField(&'static str),
    #[error("type mismatch for field {field}: expected {expected}")]
    TypeMismatch {
        field: &'static str,
        expected: &'static str,
    },
}

pub const CREQ_PREFIX: &str = "creqA";
pub const CASHUB_PREFIX: &str = "cashuB";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nut18Request {
    pub amount: u64,
    pub unit: String,
    pub mints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CashuToken {
    pub mint: String,
    pub unit: String,
    pub proofs: Vec<Proof>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proof {
    pub id: String,
    pub amount: u64,
    pub secret: String,
    pub c: String,
}

fn strip_prefix<'a>(token: &'a str, prefix: &str) -> Result<&'a str, CborError> {
    token
        .strip_prefix(prefix)
        .ok_or_else(|| CborError::InvalidPrefix {
            expected: prefix.to_string(),
            got: token.chars().take(prefix.len() + 3).collect(),
        })
}

fn decode_b64url(data: &str) -> Result<Vec<u8>, CborError> {
    URL_SAFE_NO_PAD
        .decode(data)
        .map_err(|e| CborError::Base64Decode(e.to_string()))
}

fn value_as_u64(v: &ciborium::Value, field: &'static str) -> Result<u64, CborError> {
    match v {
        ciborium::Value::Integer(i) => u64::try_from(*i).map_err(|_| CborError::TypeMismatch {
            field,
            expected: "non-negative integer",
        }),
        _ => Err(CborError::TypeMismatch {
            field,
            expected: "integer",
        }),
    }
}

fn value_as_string(v: &ciborium::Value, field: &'static str) -> Result<String, CborError> {
    match v {
        ciborium::Value::Text(s) => Ok(s.clone()),
        ciborium::Value::Bytes(b) => {
            String::from_utf8(b.clone()).map_err(|_| CborError::TypeMismatch {
                field,
                expected: "text or utf8 bytes",
            })
        }
        _ => Err(CborError::TypeMismatch {
            field,
            expected: "text",
        }),
    }
}

fn value_as_string_array(
    v: &ciborium::Value,
    field: &'static str,
) -> Result<Vec<String>, CborError> {
    match v {
        ciborium::Value::Array(items) => {
            let mut result = Vec::with_capacity(items.len());
            for item in items {
                result.push(value_as_string(item, field)?);
            }
            Ok(result)
        }
        _ => Err(CborError::TypeMismatch {
            field,
            expected: "array",
        }),
    }
}

pub fn decode_creq(token: &str) -> Result<Nut18Request, CborError> {
    let body = strip_prefix(token, CREQ_PREFIX)?;
    let bytes = decode_b64url(body)?;

    let value: ciborium::Value =
        ciborium::de::from_reader(&bytes[..]).map_err(|e| CborError::CborDecode(e.to_string()))?;

    let map = match &value {
        ciborium::Value::Map(m) => m,
        _ => {
            return Err(CborError::TypeMismatch {
                field: "root",
                expected: "map",
            });
        }
    };

    let mut amount: Option<u64> = None;
    let mut unit: Option<String> = None;
    let mut mints: Option<Vec<String>> = None;

    for (key, val) in map.iter() {
        match key.as_text() {
            Some("a") => amount = Some(value_as_u64(val, "a")?),
            Some("u") => unit = Some(value_as_string(val, "u")?),
            Some("m") => mints = Some(value_as_string_array(val, "m")?),
            _ => {}
        }
    }

    Ok(Nut18Request {
        amount: amount.ok_or(CborError::MissingField("a"))?,
        unit: unit.ok_or(CborError::MissingField("u"))?,
        mints: mints.ok_or(CborError::MissingField("m"))?,
    })
}

pub fn decode_cashub_token(token: &str) -> Result<CashuToken, CborError> {
    let body = strip_prefix(token, CASHUB_PREFIX)?;
    let bytes = decode_b64url(body)?;

    let value: ciborium::Value =
        ciborium::de::from_reader(&bytes[..]).map_err(|e| CborError::CborDecode(e.to_string()))?;

    let map = match &value {
        ciborium::Value::Map(m) => m,
        _ => {
            return Err(CborError::TypeMismatch {
                field: "root",
                expected: "map",
            });
        }
    };

    let mut mint: Option<String> = None;
    let mut unit: Option<String> = None;
    let mut proofs: Vec<Proof> = Vec::new();

    for (key, val) in map.iter() {
        match key.as_text() {
            Some("m") => mint = Some(value_as_string(val, "m")?),
            Some("u") => unit = Some(value_as_string(val, "u")?),
            Some("t") => match val {
                ciborium::Value::Array(token_entries) => {
                    for entry in token_entries {
                        let entry_map = match entry {
                            ciborium::Value::Map(m) => m,
                            _ => {
                                return Err(CborError::TypeMismatch {
                                    field: "t entry",
                                    expected: "map",
                                });
                            }
                        };

                        let mut keyset_id: Option<String> = None;
                        let mut entry_proofs: Vec<Proof> = Vec::new();

                        for (ek, ev) in entry_map.iter() {
                            match ek.as_text() {
                                Some("i") => {
                                    keyset_id = Some(value_as_string(ev, "i")?);
                                }
                                Some("p") => match ev {
                                    ciborium::Value::Array(proof_items) => {
                                        for proof_val in proof_items {
                                            let proof_map = match proof_val {
                                                ciborium::Value::Map(m) => m,
                                                _ => {
                                                    return Err(CborError::TypeMismatch {
                                                        field: "p entry",
                                                        expected: "map",
                                                    });
                                                }
                                            };

                                            let mut p_amount: Option<u64> = None;
                                            let mut p_secret: Option<String> = None;
                                            let mut p_c: Option<String> = None;

                                            for (pk, pv) in proof_map.iter() {
                                                match pk.as_text() {
                                                    Some("a") => {
                                                        p_amount = Some(value_as_u64(pv, "a")?)
                                                    }
                                                    Some("s") => {
                                                        p_secret = Some(value_as_string(pv, "s")?)
                                                    }
                                                    Some("c") => {
                                                        p_c = Some(value_as_string(pv, "c")?)
                                                    }
                                                    _ => {}
                                                }
                                            }

                                            let id = keyset_id.clone().unwrap_or_default();
                                            entry_proofs.push(Proof {
                                                id,
                                                amount: p_amount
                                                    .ok_or(CborError::MissingField("a"))?,
                                                secret: p_secret
                                                    .ok_or(CborError::MissingField("s"))?,
                                                c: p_c.ok_or(CborError::MissingField("c"))?,
                                            });
                                        }
                                    }
                                    _ => {
                                        return Err(CborError::TypeMismatch {
                                            field: "p",
                                            expected: "array",
                                        });
                                    }
                                },
                                _ => {}
                            }
                        }

                        proofs.extend(entry_proofs);
                    }
                }
                _ => {
                    return Err(CborError::TypeMismatch {
                        field: "t",
                        expected: "array",
                    });
                }
            },
            _ => {}
        }
    }

    Ok(CashuToken {
        mint: mint.ok_or(CborError::MissingField("m"))?,
        unit: unit.ok_or(CborError::MissingField("u"))?,
        proofs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_creq_cbor(amount: u64, unit: &str, mints: &[&str]) -> Vec<u8> {
        let mut buf = Vec::new();
        let map: Vec<(ciborium::Value, ciborium::Value)> = vec![
            (
                ciborium::Value::Text("a".into()),
                ciborium::Value::Integer(amount.into()),
            ),
            (
                ciborium::Value::Text("u".into()),
                ciborium::Value::Text(unit.into()),
            ),
            (
                ciborium::Value::Text("m".into()),
                ciborium::Value::Array(
                    mints
                        .iter()
                        .map(|m| ciborium::Value::Text((*m).into()))
                        .collect(),
                ),
            ),
        ];
        let value = ciborium::Value::Map(map);
        ciborium::ser::into_writer(&value, &mut buf).unwrap();
        buf
    }

    fn make_creq(amount: u64, unit: &str, mints: &[&str]) -> String {
        let cbor = encode_creq_cbor(amount, unit, mints);
        format!("{}{}", CREQ_PREFIX, URL_SAFE_NO_PAD.encode(&cbor))
    }

    fn encode_cashub_cbor(
        mint: &str,
        unit: &str,
        proofs: &[(String, u64, String, String)],
    ) -> Vec<u8> {
        let proof_arr: Vec<ciborium::Value> = proofs
            .iter()
            .map(|(_id, amt, secret, c)| {
                let pmap: Vec<(ciborium::Value, ciborium::Value)> = vec![
                    (
                        ciborium::Value::Text("a".into()),
                        ciborium::Value::Integer((*amt).into()),
                    ),
                    (
                        ciborium::Value::Text("s".into()),
                        ciborium::Value::Text(secret.as_str().into()),
                    ),
                    (
                        ciborium::Value::Text("c".into()),
                        ciborium::Value::Text(c.as_str().into()),
                    ),
                ];
                ciborium::Value::Map(pmap)
            })
            .collect();

        let tmap: Vec<(ciborium::Value, ciborium::Value)> = vec![
            (
                ciborium::Value::Text("i".into()),
                ciborium::Value::Text("keyset1".into()),
            ),
            (
                ciborium::Value::Text("p".into()),
                ciborium::Value::Array(proof_arr),
            ),
        ];

        let map: Vec<(ciborium::Value, ciborium::Value)> = vec![
            (
                ciborium::Value::Text("m".into()),
                ciborium::Value::Text(mint.into()),
            ),
            (
                ciborium::Value::Text("t".into()),
                ciborium::Value::Array(vec![ciborium::Value::Map(tmap)]),
            ),
            (
                ciborium::Value::Text("u".into()),
                ciborium::Value::Text(unit.into()),
            ),
        ];

        let value = ciborium::Value::Map(map);
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&value, &mut buf).unwrap();
        buf
    }

    fn make_cashub(mint: &str, unit: &str, proofs: &[(String, u64, String, String)]) -> String {
        let cbor = encode_cashub_cbor(mint, unit, proofs);
        format!("{}{}", CASHUB_PREFIX, URL_SAFE_NO_PAD.encode(&cbor))
    }

    #[test]
    fn test_decode_creq_basic() {
        let token = make_creq(5, "sat", &["https://testnut.cashu.exchange"]);
        let result = decode_creq(&token).expect("should decode creq");

        assert_eq!(result.amount, 5);
        assert_eq!(result.unit, "sat");
        assert_eq!(result.mints, vec!["https://testnut.cashu.exchange"]);
    }

    #[test]
    fn test_decode_creq_multiple_mints() {
        let token = make_creq(
            100,
            "sat",
            &["https://mint1.example.com", "https://mint2.example.com"],
        );
        let result = decode_creq(&token).expect("should decode creq with multiple mints");

        assert_eq!(result.amount, 100);
        assert_eq!(result.mints.len(), 2);
    }

    #[test]
    fn test_decode_creq_wrong_prefix() {
        let result = decode_creq("cashuBsomething");
        assert!(result.is_err());
        match result.unwrap_err() {
            CborError::InvalidPrefix { .. } => {}
            other => panic!("expected InvalidPrefix, got {other:?}"),
        }
    }

    #[test]
    fn test_decode_creq_invalid_base64() {
        let result = decode_creq("creqA!!!notbase64!!!");
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_cashub_basic() {
        let token = make_cashub(
            "https://testnut.cashu.exchange",
            "sat",
            &[(
                "keyset1".to_string(),
                1,
                "test_secret".to_string(),
                "00112233".to_string(),
            )],
        );
        let result = decode_cashub_token(&token).expect("should decode cashuB");

        assert_eq!(result.mint, "https://testnut.cashu.exchange");
        assert_eq!(result.unit, "sat");
        assert_eq!(result.proofs.len(), 1);
        assert_eq!(result.proofs[0].amount, 1);
        assert_eq!(result.proofs[0].secret, "test_secret");
        assert_eq!(result.proofs[0].c, "00112233");
        assert_eq!(result.proofs[0].id, "keyset1");
    }

    #[test]
    fn test_decode_cashub_multiple_proofs() {
        let token = make_cashub(
            "https://mint.example.com",
            "sat",
            &[
                (
                    "ks".to_string(),
                    2,
                    "secret_a".to_string(),
                    "cafe".to_string(),
                ),
                (
                    "ks".to_string(),
                    4,
                    "secret_b".to_string(),
                    "beef".to_string(),
                ),
                (
                    "ks".to_string(),
                    8,
                    "secret_c".to_string(),
                    "face".to_string(),
                ),
            ],
        );
        let result = decode_cashub_token(&token).expect("should decode cashuB");

        assert_eq!(result.proofs.len(), 3);
        assert_eq!(result.proofs[0].amount, 2);
        assert_eq!(result.proofs[1].amount, 4);
        assert_eq!(result.proofs[2].amount, 8);
    }

    #[test]
    fn test_decode_cashub_wrong_prefix() {
        let result = decode_cashub_token("creqAsomething");
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_cashub_empty_proofs() {
        let token = make_cashub("https://mint.example.com", "sat", &[]);
        let result = decode_cashub_token(&token).expect("should decode cashuB");

        assert_eq!(result.mint, "https://mint.example.com");
        assert_eq!(result.proofs.len(), 0);
    }

    #[test]
    fn test_strip_prefix_valid() {
        let result = strip_prefix("creqAhello", CREQ_PREFIX);
        assert_eq!(result, Ok("hello"));
    }

    #[test]
    fn test_strip_prefix_invalid() {
        let result = strip_prefix("wrongAhello", CREQ_PREFIX);
        assert!(result.is_err());
    }
}
