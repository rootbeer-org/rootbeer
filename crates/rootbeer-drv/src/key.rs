use crate::Error;
use data_encoding::BASE32_NOPAD;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::fmt;
use std::str::FromStr;

const DOMAIN: &[u8] = b"rootbeer-drv-v1\0";

/// Represents a 32 character derivation key
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct Key(String);

/// Represents a SHA-256 digest of some content
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct Sha256(String);

impl Key {
    pub(crate) fn digest(canonical: &[u8]) -> Self {
        let hash = sha2::Sha256::new()
            .chain_update(DOMAIN)
            .chain_update(canonical)
            .finalize();

        // 160 bits is exactly 32 characters, so there is never padding.
        Key(BASE32_NOPAD.encode(&hash[..20]).to_ascii_lowercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Sha256 {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Key {
    type Error = Error;

    fn try_from(value: String) -> Result<Self, Error> {
        let is_valid = value.len() == 32
            && value
                .bytes()
                .all(|c| matches!(c, b'a'..=b'z' | b'2'..=b'7'));

        if !is_valid {
            return Err(Error::invalid(
                "key",
                &value,
                "must be 32 lowercase base32 characters",
            ));
        }

        Ok(Key(value))
    }
}

impl TryFrom<String> for Sha256 {
    type Error = Error;

    fn try_from(value: String) -> Result<Self, Error> {
        let is_valid = value.len() == 64
            && value
                .bytes()
                .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'));

        if !is_valid {
            return Err(Error::invalid(
                "sha256",
                &value,
                "must be 64 lowercase hex characters",
            ));
        }

        Ok(Sha256(value))
    }
}

impl FromStr for Key {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Error> {
        value.to_string().try_into()
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
