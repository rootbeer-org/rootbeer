//! Archives become trees only through rb's own sandboxed unpacker. An
//! extract's tree is pinned by its digest, and a `github:` unpack lays out
//! the same tree unpinned.

use crate::build::{failure, remove};
use crate::tool::{self, Tool};
use crate::{Error, Request, derivation, io_at};
use rootbeer_drv::{Derivation, Extract, Format, Key, Sha256, extract_path, fetch_path};
use std::fmt::Display;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Hands the rootbeer-tree stream of what `archive` holds to `read`.
pub fn unpack<T, E: Display>(
    archive: &Path,
    format: Format,
    read: impl FnOnce(&mut dyn Read) -> Result<T, E>,
) -> Result<T, Error> {
    let input = File::open(archive).map_err(io_at(archive))?;
    tool::run(Tool::Unpack(format), input, |stream| read(stream))
}

/// The digest of the tree `archive` holds, which an extract of it checks.
pub fn tree(archive: &Path, format: Format) -> Result<Sha256, Error> {
    unpack(archive, format, |stream| rootbeer_tree::digest(stream))
}

/// Writes the tree `archive` holds to `out` and returns its digest.
pub(crate) fn lay_out(archive: &Path, format: Format, out: &Path) -> Result<Sha256, Error> {
    unpack(archive, format, |stream| {
        rootbeer_tree::materialize(stream, out)
    })
}

/// Lays out an extract's tree, which must match the digest it records, since
/// the unpacker is in no key and only the digest vouches for the output.
pub(crate) fn extract(request: &Request, extract: &Extract) -> Result<(), Error> {
    let key = request.key;
    let source = fetched(request, &extract.source)?;
    let out = extract_path(key);
    remove(&out)?;

    let laid = lay_out(&source, extract.format, &out)
        .map_err(|error| failure(key, &error.to_string()))
        .and_then(|actual| match actual == extract.tree {
            true => Ok(()),
            false => Err(failure(
                key,
                &format!(
                    "unpacked {} to the tree {}, not {}, so the tree digest its recipe \
                     records needs updating",
                    extract.source,
                    actual.as_str(),
                    extract.tree.as_str()
                ),
            )),
        });

    // The failure is what matters, and a leftover is replaced on the next try.
    laid.inspect_err(|_| {
        let _ = remove(&out);
    })
}

/// The realized fetch at `source`, which a derivation unpacks.
pub(crate) fn fetched(request: &Request, source: &Key) -> Result<PathBuf, Error> {
    if !matches!(derivation(request, source)?, Derivation::Fetch(_)) {
        return Err(failure(
            request.key,
            &format!("unpacks {source}, which isn't a fetch"),
        ));
    }

    let path = fetch_path(source);
    if !path.exists() {
        return Err(failure(
            request.key,
            &format!("needs {source}, which is not realized"),
        ));
    }

    Ok(path)
}
