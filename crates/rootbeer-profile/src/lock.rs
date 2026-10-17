use crate::{Error, Release, io_at, validate_exports, validate_source};
use rootbeer_drv::{Key, is_package_name};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

/// Locks from schema 4 hold only index entries, so this rb reads them.
pub const LOCK_SCHEMA: u32 = 5;
const OLDEST_LOCK_SCHEMA: u32 = 4;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lock {
    pub packages: BTreeMap<String, BTreeMap<String, Locked>>,
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Locked {
    pub version: String,
    pub key: Key,
    /// The signed index's manifest digest, for an entry from the index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<Release>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub bins: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub apps: BTreeMap<String, String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Serialize, Deserialize)]
struct Record {
    schema: u32,
    packages: BTreeMap<String, BTreeMap<String, Locked>>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct Schema {
    schema: u32,
}

impl Lock {
    pub fn read(path: &Path) -> Result<Option<Lock>, Error> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_at(path)(error)),
        };

        let invalid = |error: serde_json::Error| Error::Invalid {
            path: path.to_path_buf(),
            reason: error.to_string(),
        };

        let Schema { schema } = serde_json::from_slice(&bytes).map_err(invalid)?;
        if schema > LOCK_SCHEMA {
            return Err(Error::Newer {
                path: path.to_path_buf(),
                schema,
            });
        }

        if schema < OLDEST_LOCK_SCHEMA {
            return Err(Error::Older {
                path: path.to_path_buf(),
                schema,
            });
        }

        let record: Record = serde_json::from_slice(&bytes).map_err(invalid)?;
        for (name, platforms) in &record.packages {
            if !is_package_name(name) {
                return Err(Error::Invalid {
                    path: path.to_path_buf(),
                    reason: format!("{name:?} isn't a package name"),
                });
            }

            for locked in platforms.values() {
                validate_source(
                    name,
                    locked.manifest.as_ref(),
                    locked.release.as_ref(),
                    &locked.bins,
                )
                .and_then(|()| validate_exports(name, &locked.bins, &locked.apps))
                .map_err(|reason| Error::Invalid {
                    path: path.to_path_buf(),
                    reason,
                })?;
            }
        }

        Ok(Some(Lock {
            packages: record.packages,
            extra: record.extra,
        }))
    }

    /// Leaves the file alone when it already says the same thing.
    pub fn write(&self, path: &Path) -> Result<(), Error> {
        let record = Record {
            schema: LOCK_SCHEMA,
            packages: self.packages.clone(),
            extra: self.extra.clone(),
        };

        let mut json = serde_json::to_vec_pretty(&record).map_err(|error| Error::Invalid {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
        json.push(b'\n');

        if fs::read(path).is_ok_and(|existing| existing == json) {
            return Ok(());
        }

        let directory = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));

        let mut staged = tempfile::Builder::new()
            .prefix(".rootbeer.lock.")
            .tempfile_in(directory)
            .map_err(io_at(directory))?;

        let write = |file: &mut File| -> io::Result<()> {
            file.write_all(&json)?;
            file.sync_all()
        };

        write(staged.as_file_mut()).map_err(io_at(staged.path()))?;
        staged
            .persist(path)
            .map_err(|error| io_at(path)(error.error))?;

        Ok(())
    }

    pub fn get(&self, name: &str, platform: &str) -> Option<&Locked> {
        self.packages.get(name)?.get(platform)
    }

    pub fn insert(&mut self, name: &str, platform: &str, locked: Locked) {
        let platforms = self.packages.entry(name.to_string()).or_default();
        platforms.insert(platform.to_string(), locked);
    }

    pub fn retain(&mut self, platform: &str, keep: impl Fn(&str) -> bool) {
        for (name, platforms) in &mut self.packages {
            if !keep(name) {
                platforms.remove(platform);
            }
        }

        self.packages.retain(|_, platforms| !platforms.is_empty());
    }
}
