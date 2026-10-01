//! Derivations: complete descriptions of builds, and the keys that identify them.
//!
//! A key is the hash of a derivation's canonical encoding. Derivations with the
//! same key produce the same output, so a key already in a trusted cache never
//! needs building again.
//!
//! The encoding is permanent. Every optional field is omitted at its default, so
//! a missing field and an empty one hash the same, and a new optional field
//! leaves existing keys unchanged.

mod error;
mod key;
mod validate;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use error::Error;
pub use key::{Key, Sha256};

/// Root of the store. Fixed by this encoding version; outputs embed it.
pub const STORE_ROOT: &str = "/opt/rb/store";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Derivation {
    Build(Build),
    Fetch(Fetch),
    Check(Check),
}

/// Produces store outputs by running a script in a sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    pub name: String,
    pub version: String,
    pub platform: Platform,
    pub sandbox: String,
    /// Fetch derivations, by the variable name the script sees them under.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, Key>,
    /// Ordered: earlier dependencies take precedence on search paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deps: Vec<Dep>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    pub script: String,
    pub outputs: BTreeSet<String>,
}

/// Content known in advance by hash. Only the hash is keyed, so mirrors can
/// change without rebuilding anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fetch {
    pub sha256: Sha256,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
}

/// Verifies a build's output without changing it, so editing a check never
/// rebuilds its target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    pub target: Key,
    pub platform: Platform,
    pub sandbox: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deps: Vec<Dep>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    pub script: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dep {
    pub key: Key,
    pub name: String,
    pub kind: DepKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepKind {
    /// Runs on the builder: compilers, cmake, pkgconf.
    Build,
    /// Linked into the output: headers and libraries.
    Host,
    /// Needed at runtime without being referenced by path.
    Run,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Platform {
    #[serde(rename = "aarch64-macos")]
    Aarch64Macos,
    #[serde(rename = "aarch64-linux")]
    Aarch64Linux,
    #[serde(rename = "x86_64-linux")]
    X86_64Linux,
}

impl Derivation {
    pub fn key(&self) -> Result<Key, Error> {
        self.validate()?;

        Ok(Key::digest(&self.canonical_bytes()))
    }

    // RFC 8785 JSON of the keyed fields: everything except fetch URLs.
    fn canonical_bytes(&self) -> Vec<u8> {
        let keyed = match self {
            Derivation::Fetch(fetch) => &Derivation::Fetch(Fetch {
                urls: Vec::new(),
                ..fetch.clone()
            }),
            _ => self,
        };

        serde_json_canonicalizer::to_vec(keyed).expect("derivations have string map keys")
    }
}

/// Store path of one output of a build.
pub fn output_path(key: &Key, name: &str, version: &str, output: &str) -> PathBuf {
    let path = format!("{STORE_ROOT}/{key}-{name}-{version}");

    match output {
        "out" => path.into(),
        _ => format!("{path}-{output}").into(),
    }
}

#[cfg(test)]
mod tests;
