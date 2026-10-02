//! rootbeer-sandbox realizes a derivation into the store. It fetches assets,
//! builds the derivation in a sandbox, and scans outputs for references.

mod build;
mod darwin;
mod fetch;
mod linux;
mod scan;

use rootbeer_drv::{Derivation, Key};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

/// One derivation to realize. Everything it names must already be realized.
pub struct Request<'a> {
    pub key: &'a Key,
    pub graph: &'a BTreeMap<Key, Derivation>,
    /// Runtime references of realized builds, as `realize` returned them.
    pub references: &'a BTreeMap<Key, BTreeSet<Key>>,
    pub jobs: NonZeroUsize,
    pub log: &'a mut dyn Write,
}

#[derive(Debug)]
pub enum Error {
    Fetch {
        key: Key,
        failures: Vec<String>,
    },
    Build {
        key: Key,
        reason: String,
    },
    Scan {
        key: Key,
        out: PathBuf,
        problems: Vec<String>,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
}

/// Fetches, builds, or checks a derivation, replacing any partial output an
/// interrupted attempt left at its store path. Returns the output's runtime
/// references when successful.
pub fn realize(request: &mut Request) -> Result<BTreeSet<Key>, Error> {
    match derivation(request, request.key)? {
        Derivation::Fetch(fetch) => fetch::fetch(request.key, fetch).map(|()| BTreeSet::new()),
        Derivation::Build(build) => build::build(request, build),
        Derivation::Check(check) => build::check(request, check).map(|()| BTreeSet::new()),
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
            Error::Scan { key, out, problems } => write!(
                f,
                "{key} failed its scan, output kept at {}:\n  {}",
                out.display(),
                problems.join("\n  ")
            ),
            Error::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for Error {}

fn io_at(path: &Path) -> impl Fn(io::Error) -> Error {
    let path = path.to_path_buf();
    move |source| Error::Io {
        path: path.clone(),
        source,
    }
}
