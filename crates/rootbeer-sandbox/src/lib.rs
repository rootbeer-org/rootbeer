//! rootbeer-sandbox realizes a derivation into the store. It fetches assets,
//! builds the derivation in a sandbox, and scans outputs for references.

mod build;
mod darwin;
mod fetch;

use rootbeer_drv::{Derivation, Key};
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::path::PathBuf;

/// One derivation to realize. Everything it names must already be realized.
pub struct Request<'a> {
    pub key: &'a Key,
    pub graph: &'a BTreeMap<Key, Derivation>,
    pub jobs: NonZeroUsize,
    pub log: &'a mut dyn Write,
}

#[derive(Debug)]
pub enum Error {
    Fetch { key: Key, failures: Vec<String> },
    Build { key: Key, reason: String },
    Io { path: PathBuf, source: io::Error },
}

/// Fetches, builds, or checks a derivation, replacing any partial output an
/// interrupted attempt left at its store path.
pub fn realize(request: &mut Request) -> Result<(), Error> {
    match derivation(request, request.key)? {
        Derivation::Fetch(fetch) => fetch::fetch(request.key, fetch),
        Derivation::Build(build) => build::build(request, build),
        Derivation::Check(check) => build::check(request, check),
    }
}

fn derivation<'a>(request: &Request<'a>, key: &Key) -> Result<&'a Derivation, Error> {
    let graph: &'a BTreeMap<Key, Derivation> = request.graph;
    let failure = |reason: String| Error::Build {
        key: key.clone(),
        reason,
    };

    let derivation = graph
        .get(key)
        .ok_or_else(|| failure("is not in the graph".into()))?;

    let actual = derivation
        .key()
        .map_err(|error| failure(error.to_string()))?;

    if actual != *key {
        return Err(failure(format!("names a derivation whose key is {actual}")));
    }

    Ok(derivation)
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Fetch { key, failures } => {
                write!(f, "fetch {key} failed: {}", failures.join("; "))
            }
            Error::Build { key, reason } => write!(f, "{key} {reason}"),
            Error::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for Error {}
