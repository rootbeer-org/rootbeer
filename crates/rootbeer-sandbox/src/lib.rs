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
    match request.graph.get(request.key) {
        Some(Derivation::Fetch(fetch)) => fetch::fetch(request.key, fetch).map(drop),
        Some(Derivation::Build(build)) => build::build(request, build),
        Some(Derivation::Check(check)) => build::check(request, check),
        None => Err(Error::Build {
            key: request.key.clone(),
            reason: "is not in the graph".into(),
        }),
    }
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
