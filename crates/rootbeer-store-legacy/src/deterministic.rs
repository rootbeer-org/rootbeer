//! Small traits for values that participate in Rootbeer's deterministic model.
//!
//! An input "fingerprint" identifies the facts that feed a deterministic phase,
//! such as package resolution or package realization. Outputs are modeled
//! separately because their hash is normally computed from produced content
//! rather than from a serializable input description, meaning we can derive
//! them instead of computing them from the input.

use std::fmt;
use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const INPUT_DOMAIN: &[u8] = b"rootbeer deterministic input v1";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DeterministicFingerprint(String);

impl DeterministicFingerprint {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for DeterministicFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A serializable input to a deterministic phase.
pub trait DeterministicInput: Serialize {
    const KIND: &'static str;

    fn fingerprint(&self) -> Result<DeterministicFingerprint, serde_json::Error> {
        fingerprint(Self::KIND, self)
    }
}

/// A deterministic output whose content hash is known after realization.
pub trait DeterministicOutput {
    const KIND: &'static str;

    fn output_sha256(&self) -> &str;
}

pub fn fingerprint<T>(kind: &str, value: &T) -> Result<DeterministicFingerprint, serde_json::Error>
where
    T: Serialize + ?Sized,
{
    let mut hasher = Sha256::new();
    hasher.update(INPUT_DOMAIN);
    hasher.update([0]);
    hasher.update(kind.as_bytes());
    hasher.update([0]);

    {
        let mut writer = HashWriter(&mut hasher);
        serde_json::to_writer(&mut writer, value)?;
    }

    Ok(DeterministicFingerprint(hex(hasher.finalize().as_slice())))
}

struct HashWriter<'a>(&'a mut Sha256);

impl Write for HashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    const CHARS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);

    for byte in bytes {
        out.push(CHARS[(byte >> 4) as usize] as char);
        out.push(CHARS[(byte & 0x0f) as usize] as char);
    }

    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[derive(Serialize)]
    struct Input {
        values: BTreeMap<String, String>,
    }

    impl DeterministicInput for Input {
        const KIND: &'static str = "test.input";
    }

    #[test]
    fn fingerprints_are_stable_for_same_structural_input() {
        let left = Input {
            values: BTreeMap::from([
                ("a".to_string(), "one".to_string()),
                ("b".to_string(), "two".to_string()),
            ]),
        };
        let right = Input {
            values: BTreeMap::from([
                ("b".to_string(), "two".to_string()),
                ("a".to_string(), "one".to_string()),
            ]),
        };

        assert_eq!(left.fingerprint().unwrap(), right.fingerprint().unwrap());
    }

    #[test]
    fn fingerprints_are_domain_separated_by_kind() {
        let input = Input {
            values: BTreeMap::new(),
        };

        assert_ne!(
            input.fingerprint().unwrap(),
            fingerprint("other.input", &input).unwrap()
        );
    }
}
