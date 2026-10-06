//! rootbeer-store manages `/opt/rb`, the canonical location for outputs. It
//! tracks which outputs are present through a database along with their
//! references.

mod add;
mod archive;
mod db;

pub use archive::pack;
use archive::{seal, unpack};
use rootbeer_drv::Key;
use rusqlite::Connection;
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests;

pub const ROOT: &str = "/opt/rb";

/// How an output reached the store
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Built on this machine by a trusted user
    Local,
    /// Downloaded from a cache
    Pulled,
}

pub struct Store {
    root: PathBuf,
    connection: Connection,
}

#[derive(Debug)]
pub enum Error {
    Io { path: PathBuf, source: io::Error },
    Database(String),
    Archive(String),
    Refused(String),
}

impl Store {
    pub fn open_read_only(root: &Path) -> Result<Store, Error> {
        Ok(Store {
            root: root.to_path_buf(),
            connection: db::open_read_only(&root.join("var/db.sqlite"))?,
        })
    }

    pub fn open(root: &Path) -> Result<Store, Error> {
        let var = root.join("var");
        fs::create_dir_all(&var).map_err(io_at(&var))?;

        Ok(Store {
            root: root.to_path_buf(),
            connection: db::open(&var.join("db.sqlite"))?,
        })
    }

    pub fn path(&self, key: &Key) -> Result<Option<PathBuf>, Error> {
        let entry = db::entry(&self.connection, key)?;
        Ok(entry.map(|entry| self.root.join("store").join(entry)))
    }

    pub fn references(&self, key: &Key) -> Result<BTreeSet<Key>, Error> {
        db::references(&self.connection, key)
    }

    pub(crate) fn register(
        &mut self,
        key: &Key,
        entry: &str,
        origin: Origin,
        digest: Option<&str>,
        references: &BTreeSet<Key>,
    ) -> Result<(), Error> {
        db::register(&mut self.connection, key, entry, origin, digest, references)
    }

    /// Unpacks `archive` into a staging directory, then renames it into place
    /// and seals it only once `verify` accepts what was read.
    pub(crate) fn ingest<R: Read>(
        &self,
        entry: &str,
        archive: &mut R,
        verify: impl FnOnce(&mut R) -> Result<(), Error>,
    ) -> Result<PathBuf, Error> {
        validate(entry)?;

        let store = self.root.join("store");
        let path = store.join(entry);
        if fs::exists(&path).map_err(io_at(&path))? {
            return Err(io_at(&path)(io::ErrorKind::AlreadyExists.into()));
        }

        let staging = tempfile::Builder::new()
            .prefix(".tmp-")
            .tempdir_in(&store)
            .map_err(io_at(&store))?;

        // Sealed only after verifying, so a refused staging directory is still
        // writable and its drop can remove it without root.
        unpack(&mut *archive, staging.path())?;
        verify(archive)?;

        // Sealed in place, since macOS refuses a non-root rename of a directory
        // that isn't writable. The entry is private and unregistered until then.
        fs::rename(staging.path(), &path).map_err(io_at(&path))?;
        seal(&path)?;
        fs::File::open(&store)
            .and_then(|directory| directory.sync_all())
            .map_err(io_at(&store))?;

        Ok(path)
    }
}

impl Origin {
    fn as_str(self) -> &'static str {
        match self {
            Origin::Local => "local",
            Origin::Pulled => "pulled",
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Error::Database(error.to_string())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { path, source } => {
                write!(f, "{}: {source}", path.display())?;
                match std::error::Error::source(source) {
                    Some(cause) => write!(f, ": {cause}"),
                    None => Ok(()),
                }
            }
            Error::Database(reason) => write!(f, "store database: {reason}"),
            Error::Archive(reason) => write!(f, "archive: {reason}"),
            Error::Refused(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for Error {}

fn validate(entry: &str) -> Result<(), Error> {
    let is_name = !entry.is_empty() && !entry.starts_with('.') && !entry.contains('/');
    if !is_name {
        return Err(Error::Refused(format!("{entry:?} is not a store entry")));
    }

    Ok(())
}

fn io_at(path: &Path) -> impl Fn(io::Error) -> Error {
    let path = path.to_path_buf();
    move |source| Error::Io {
        path: path.clone(),
        source,
    }
}
