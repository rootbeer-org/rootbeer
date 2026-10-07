//! rootbeer-trust reads the package index from a TUF repository and uses it to
//! verify signed manifests to validate the source of an output.

mod transport;

#[cfg(test)]
mod tests;

use rootbeer_drv::{Key, Platform, Sha256, is_package_name};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::Path;
use tough::schema::{Root, Signed};
use tough::{IntoVec, Repository, RepositoryLoader, TargetName};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Entry")]
pub struct Package {
    pub name: String,
    pub default: BTreeMap<Platform, String>,
    pub versions: BTreeMap<String, BTreeMap<Platform, Output>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    pub key: Key,
    pub manifest: String,
}

pub struct Index {
    runtime: tokio::runtime::Runtime,
    repository: Repository,
}

#[derive(Debug)]
pub enum Error {
    Url(String),
    Name(String),
    Datastore(String),
    Runtime(String),
    Tuf(Box<tough::error::Error>),
    Invalid(String),
}

#[derive(Deserialize)]
struct Entry {
    name: String,
    default: BTreeMap<String, String>,
    versions: BTreeMap<String, BTreeMap<String, Output>>,
}

impl Index {
    pub fn refresh(
        root: &[u8],
        url: &str,
        datastore: &Path,
        is_http_allowed: bool,
    ) -> Result<Index, Error> {
        outside_runtime()?;
        let mut base = Url::parse(url).map_err(|error| Error::Url(format!("{url}: {error}")))?;
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }

        let join = |path: &str| {
            base.join(path)
                .map_err(|error| Error::Url(format!("{base}{path}: {error}")))
        };

        let metadata = join("metadata/")?;
        let targets = join("targets/")?;
        open_datastore(datastore)?;
        let root = anchor(root, datastore)?;

        let clock = datastore.join("latest_known_time.json");
        match fs::remove_file(&clock) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return Err(datastore_error(&clock, error));
            }
            _ => {}
        }

        let loader = RepositoryLoader::new(&root, metadata, targets)
            .transport(transport::Ureq::new(is_http_allowed))
            .datastore(datastore);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| Error::Runtime(error.to_string()))?;

        let repository = runtime.block_on(loader.load()).map_err(tuf)?;
        Ok(Index {
            runtime,
            repository,
        })
    }

    pub fn package(&self, name: &str) -> Result<Option<Package>, Error> {
        outside_runtime()?;
        if !is_package_name(name) {
            return Err(Error::Name(format!("{name:?} isn't a package name")));
        }

        let path = format!("{name}.json");
        let target = TargetName::new(&path).map_err(tuf)?;
        let read = async {
            let Some(stream) = self.repository.read_target(&target).await? else {
                return Ok(None);
            };

            stream.into_vec().await.map(Some)
        };

        let Some(bytes) = self.runtime.block_on(read).map_err(tuf)? else {
            return Ok(None);
        };

        let package: Package = serde_json::from_slice(&bytes)
            .map_err(|error| Error::Invalid(format!("{path}: {error}")))?;

        if package.name != name {
            return Err(Error::Invalid(format!("{path} describes {}", package.name)));
        }

        Ok(Some(package))
    }
}

impl Package {
    pub fn output(&self, platform: Platform, version: Option<&str>) -> Option<&Output> {
        let version = match version {
            Some(version) => version,
            None => self.default.get(&platform)?,
        };

        self.versions.get(version)?.get(&platform)
    }
}

impl TryFrom<Entry> for Package {
    type Error = String;

    fn try_from(entry: Entry) -> Result<Package, String> {
        let known = |platform: String| Platform::try_from(platform).ok();
        let default = entry
            .default
            .into_iter()
            .filter_map(|(platform, version)| Some((known(platform)?, version)))
            .collect();

        let mut versions = BTreeMap::new();
        for (version, outputs) in entry.versions {
            let mut kept = BTreeMap::new();
            for (platform, output) in outputs {
                let Some(platform) = known(platform) else {
                    continue;
                };

                let digest = output.manifest.strip_prefix("sha256:").map(str::to_string);
                if digest
                    .and_then(|digest| Sha256::try_from(digest).ok())
                    .is_none()
                {
                    return Err(format!("{:?} isn't a sha256 digest", output.manifest));
                }

                kept.insert(platform, output);
            }

            versions.insert(version, kept);
        }

        Ok(Package {
            name: entry.name,
            default,
            versions,
        })
    }
}

fn open_datastore(path: &Path) -> Result<(), Error> {
    let at = |error| datastore_error(path, error);
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(at)?;

    let metadata = fs::metadata(path).map_err(at)?;
    let is_private = metadata.mode() & 0o022 == 0;
    if metadata.uid() != rustix::process::geteuid().as_raw() || !is_private {
        return Err(Error::Datastore(format!(
            "{} must be owned and writable only by this user",
            path.display()
        )));
    }

    Ok(())
}

fn anchor(root: &[u8], datastore: &Path) -> Result<Vec<u8>, Error> {
    let path = datastore.join("root.json");
    let stored = match fs::read(&path) {
        Ok(stored) => stored,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(root.to_vec()),
        Err(error) => return Err(datastore_error(&path, error)),
    };

    let built_in =
        version(root).ok_or_else(|| Error::Invalid("the trusted root doesn't verify".into()))?;
    let Some(kept) = version(&stored) else {
        return Err(Error::Datastore(format!(
            "{} doesn't verify, so remove it to start over from rb's own root",
            path.display()
        )));
    };

    match kept > built_in {
        true => Ok(stored),
        false => Ok(root.to_vec()),
    }
}

fn version(root: &[u8]) -> Option<u64> {
    let root: Signed<Root> = serde_json::from_slice(root).ok()?;
    root.signed.verify_role(&root).ok()?;
    Some(root.signed.version.get())
}

fn outside_runtime() -> Result<(), Error> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(Error::Runtime(
            "the index blocks, so it can't run inside an async runtime".into(),
        ));
    }

    Ok(())
}

fn tuf(error: tough::error::Error) -> Error {
    Error::Tuf(Box::new(error))
}

fn datastore_error(path: &Path, error: io::Error) -> Error {
    Error::Datastore(format!("{}: {error}", path.display()))
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Url(reason)
            | Error::Name(reason)
            | Error::Datastore(reason)
            | Error::Runtime(reason)
            | Error::Invalid(reason) => f.write_str(reason),
            Error::Tuf(error) => write!(f, "index: {error}"),
        }
    }
}

impl std::error::Error for Error {}
