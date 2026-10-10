//! rootbeer-profile manages the user profiles and generations. Each profile is
//! a symlink to its current generation and each generation is a directory with
//! symlinks into the store and a recorded manifest.

use rootbeer_drv::{Key, is_file_name, is_package_name};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests;

pub const SCHEMA: u32 = 1;
const RECORD: &str = "generation.json";

pub struct Profile {
    directory: PathBuf,
    name: String,
    is_writable: bool,
    _lock: File,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Generation {
    pub number: u64,
    pub packages: BTreeMap<String, Member>,
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub version: String,
    #[serde(rename = "pinned")]
    pub is_pinned: bool,
    pub key: Key,
    pub manifest: String,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub bins: BTreeSet<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Serialize, Deserialize)]
struct Record {
    schema: u32,
    packages: BTreeMap<String, Member>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct Schema {
    schema: u32,
}

#[derive(Debug)]
pub enum Error {
    Io { path: PathBuf, source: io::Error },
    Invalid { path: PathBuf, reason: String },
    Newer { path: PathBuf, schema: u32 },
    Refused(String),
}

pub fn bin(directory: &Path, name: &str) -> PathBuf {
    directory.join(name).join("bin")
}

impl Profile {
    pub fn open(directory: &Path, name: &str) -> Result<Profile, Error> {
        Profile::locked(directory, name, true)
    }

    pub fn open_shared(directory: &Path, name: &str) -> Result<Profile, Error> {
        Profile::locked(directory, name, false)
    }

    fn locked(directory: &Path, name: &str, is_writable: bool) -> Result<Profile, Error> {
        let is_name = is_file_name(name) && !name.starts_with('.') && !name.contains('-');
        if !is_name {
            return Err(Error::Refused(format!("{name:?} isn't a profile name")));
        }

        let path = directory.join(".lock");
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(io_at(&path))?;

        match is_writable {
            true => lock.lock(),
            false => lock.lock_shared(),
        }
        .map_err(io_at(&path))?;

        Ok(Profile {
            directory: directory.to_path_buf(),
            name: name.to_string(),
            is_writable,
            _lock: lock,
        })
    }

    pub fn current(&self) -> Result<Option<Generation>, Error> {
        match self.current_number()? {
            Some(number) => self.read(number).map(Some),
            None => Ok(None),
        }
    }

    pub fn create(
        &self,
        generation: Generation,
        locate: impl Fn(&Key) -> Result<PathBuf, String>,
    ) -> Result<u64, Error> {
        self.writable()?;
        let Generation {
            packages, extra, ..
        } = generation;

        let mut links = BTreeMap::new();
        for (name, member) in &packages {
            validate(name, member).map_err(Error::Refused)?;
            let output = locate(&member.key).map_err(Error::Refused)?;
            for bin in &member.bins {
                let target = output.join("bin").join(bin);
                if fs::symlink_metadata(&target).is_err() {
                    return Err(Error::Refused(format!(
                        "{name} has no {}",
                        target.display()
                    )));
                }

                if let Some((other, _)) = links.insert(bin, (name, target)) {
                    return Err(Error::Refused(format!(
                        "{other} and {name} both provide {bin}"
                    )));
                }
            }
        }

        self.clean()?;
        let number = match self.generations()?.last() {
            Some(last) => last
                .checked_add(1)
                .ok_or_else(|| Error::Refused("out of generation numbers".into()))?,
            None => 1,
        };

        let staging = tempfile::Builder::new()
            .prefix(".tmp-")
            .tempdir_in(&self.directory)
            .map_err(io_at(&self.directory))?;

        let root = staging.path();
        fs::set_permissions(root, fs::Permissions::from_mode(0o755)).map_err(io_at(root))?;

        let bin = root.join("bin");
        fs::create_dir(&bin).map_err(io_at(&bin))?;
        for (name, (_, target)) in &links {
            let link = bin.join(name);
            symlink(target, &link).map_err(io_at(&link))?;
        }

        let record = Record {
            schema: SCHEMA,
            packages,
            extra,
        };

        let path = root.join(RECORD);
        let mut json = serde_json::to_vec_pretty(&record).map_err(|error| Error::Invalid {
            path: path.clone(),
            reason: error.to_string(),
        })?;

        json.push(b'\n');
        let write = || -> io::Result<()> {
            let mut file = File::create_new(&path)?;
            file.write_all(&json)?;
            file.sync_all()
        };

        write().map_err(io_at(&path))?;
        for directory in [&bin, root] {
            sync(directory)?;
        }

        let staged = staging.keep();
        let generation = self.directory.join(self.generation(number));
        fs::rename(&staged, &generation).map_err(io_at(&generation))?;

        self.switch(number)?;
        Ok(number)
    }

    pub fn rollback(&self) -> Result<u64, Error> {
        self.writable()?;

        let current = self
            .current_number()?
            .ok_or_else(|| Error::Refused(format!("{} has no generations", self.name)))?;

        let previous = self
            .generations()?
            .into_iter()
            .rev()
            .find(|number| *number < current)
            .ok_or_else(|| {
                Error::Refused(format!(
                    "generation {current} is the oldest of {}",
                    self.name
                ))
            })?;

        self.switch(previous)?;
        Ok(previous)
    }

    fn switch(&self, number: u64) -> Result<(), Error> {
        let link = self.directory.join(&self.name);
        let staged = self.directory.join(format!(".{}.link", self.name));
        remove(&staged)?;

        symlink(self.generation(number), &staged).map_err(io_at(&staged))?;
        fs::rename(&staged, &link).map_err(io_at(&link))?;
        sync(&self.directory)
    }

    pub fn current_number(&self) -> Result<Option<u64>, Error> {
        let link = self.directory.join(&self.name);
        let target = match fs::read_link(&link) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_at(&link)(error)),
        };

        let number = target
            .to_str()
            .and_then(|target| self.number(target))
            .ok_or_else(|| Error::Invalid {
                path: link.clone(),
                reason: format!("points at {}", target.display()),
            })?;

        Ok(Some(number))
    }

    fn writable(&self) -> Result<(), Error> {
        match self.is_writable {
            true => Ok(()),
            false => Err(Error::Refused(format!(
                "{} is open only to read",
                self.name
            ))),
        }
    }

    pub fn read(&self, number: u64) -> Result<Generation, Error> {
        let path = self.directory.join(self.generation(number)).join(RECORD);
        let bytes = fs::read(&path).map_err(io_at(&path))?;
        let invalid = |error: serde_json::Error| Error::Invalid {
            path: path.clone(),
            reason: error.to_string(),
        };

        let Schema { schema } = serde_json::from_slice(&bytes).map_err(invalid)?;
        if schema > SCHEMA {
            return Err(Error::Newer { path, schema });
        }

        if schema != SCHEMA {
            return Err(Error::Invalid {
                path,
                reason: format!("no rb writes schema {schema}"),
            });
        }

        let record: Record = serde_json::from_slice(&bytes).map_err(invalid)?;
        for (name, member) in &record.packages {
            validate(name, member).map_err(|reason| Error::Invalid {
                path: path.clone(),
                reason,
            })?;
        }

        Ok(Generation {
            number,
            packages: record.packages,
            extra: record.extra,
        })
    }

    pub fn generations(&self) -> Result<Vec<u64>, Error> {
        let entries = fs::read_dir(&self.directory).map_err(io_at(&self.directory))?;
        let mut numbers = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io_at(&self.directory))?;
            let number = entry
                .file_name()
                .to_str()
                .and_then(|name| self.number(name));
            numbers.extend(number);
        }

        numbers.sort_unstable();
        Ok(numbers)
    }

    fn clean(&self) -> Result<(), Error> {
        let entries = fs::read_dir(&self.directory).map_err(io_at(&self.directory))?;
        for entry in entries {
            let entry = entry.map_err(io_at(&self.directory))?;
            let is_staging = entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".tmp-"));

            if !is_staging {
                continue;
            }

            let path = entry.path();
            let is_directory = entry.file_type().map_err(io_at(&path))?.is_dir();
            match is_directory {
                true => fs::remove_dir_all(&path),
                false => fs::remove_file(&path),
            }
            .map_err(io_at(&path))?;
        }

        Ok(())
    }

    fn generation(&self, number: u64) -> String {
        format!("{}-{number}", self.name)
    }

    fn number(&self, entry: &str) -> Option<u64> {
        let (name, number) = entry.rsplit_once('-')?;
        let number = number.parse().ok()?;
        (name == self.name && self.generation(number) == entry).then_some(number)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Error::Invalid { path, reason } => write!(f, "{}: {reason}", path.display()),
            Error::Newer { path, schema } => write!(
                f,
                "{} was written by a newer rb (schema {schema})",
                path.display()
            ),
            Error::Refused(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for Error {}

fn validate(name: &str, member: &Member) -> Result<(), String> {
    if !is_package_name(name) {
        return Err(format!("{name:?} isn't a package name"));
    }

    match member.bins.iter().find(|bin| !is_file_name(bin)) {
        Some(bin) => Err(format!("{name} has a bin {bin:?} that isn't a plain name")),
        None => Ok(()),
    }
}

fn sync(directory: &Path) -> Result<(), Error> {
    File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(io_at(directory))
}

fn remove(path: &Path) -> Result<(), Error> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(io_at(path)(error)),
        _ => Ok(()),
    }
}

fn io_at(path: &Path) -> impl Fn(io::Error) -> Error {
    let path = path.to_path_buf();
    move |source| Error::Io {
        path: path.clone(),
        source,
    }
}
