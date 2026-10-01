//! rootbeer-drv is a pure functional library that describes builds with keys.
//!
//! A key is a hash of a derivation's encoding. Identical keys will always
//! produce the same output so a key in a cache can avoid rebuilding. Encoding
//! is critical: it MUST be stable and deterministic because it underpins the
//! entire system that we use to build packages. New fields MUST be optional and
//! omitted at their default, or every existing key changes.

mod error;
mod key;
mod validate;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

pub use error::Error;
pub use key::{Key, Sha256};

/// The store root (fixed by the encoding version and embedded in every output)
pub const STORE_ROOT: &str = "/opt/rb/store";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Derivation {
    Build(Build),
    Fetch(Fetch),
    Check(Check),
}

/// A build that produces outputs from a set of inputs with a script and an
/// environment. Inputs are keyed derivations, so the build is reproducible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    pub name: String,
    pub version: String,
    pub platform: Platform,
    pub sandbox: String,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, Key>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<Dependency>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    pub script: String,
    pub outputs: BTreeSet<String>,
}

/// Remote content known in advance with a hash. The URLs can change as long
/// as the hashes continue to match, allowing mirrors to be added/removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fetch {
    pub sha256: Sha256,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
}

/// Validates a build's output without modifying it, allowing checks to be
/// edited without triggering cache invalidations and rebuilds of their targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    pub target: Key,
    pub platform: Platform,
    pub sandbox: String,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<Dependency>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    pub script: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub key: Key,
    pub name: String,
    pub kind: DependencyKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyKind {
    /// Required to build the derivation but not a part of its output
    Build,
    /// Linked into the output (e.g. headers, libraries, etc.)
    Linked,
    /// Required at runtime without being referenced by path (e.g. plugins/tools
    /// found on PATH). Linked libraries are discovered by scanning the output
    Runtime,
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

    // RFC 8785 JSON of the keyed fields (minus the fetched URLs)
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

/// Store path of one output of a build
pub fn output_path(key: &Key, name: &str, version: &str, output: &str) -> PathBuf {
    let path = format!("{STORE_ROOT}/{key}-{name}-{version}");

    match output {
        "out" => path.into(),
        _ => format!("{path}-{output}").into(),
    }
}

#[cfg(test)]
mod tests;
