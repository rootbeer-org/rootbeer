//! rootbeer-profile manages the user profiles and generations. Each profile is
//! a symlink to its current generation and each generation is a directory with
//! symlinks into the store and a recorded manifest.

use rootbeer_drv::{Format, Key, Sha256, is_app_name, is_file_name, is_inside, is_package_name};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

mod lock;
mod roots;
#[cfg(test)]
mod tests;

pub use lock::{LOCK_SCHEMA, Lock, Locked};
pub use roots::{hold_while_running, roots};

/// Generations from schema 1 hold only index members, so this rb reads them.
pub const SCHEMA: u32 = 2;
const OLDEST_SCHEMA: u32 = 1;
pub(crate) const RECORD: &str = "generation.json";
const APPLICATIONS: &str = "Applications";

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
    /// The signed index's manifest digest, for a member from the index.
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

/// A GitHub release asset unpacked into the store, with what it takes to
/// fetch and unpack it again. `paths` maps each bin to its file in the output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub repository: String,
    pub asset: String,
    pub url: String,
    pub sha256: Sha256,
    pub format: Format,
    pub paths: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub libc: Option<Libc>,
    /// The request named the asset, so an update asks for it again.
    #[serde(default, rename = "keep_asset", skip_serializing_if = "is_false")]
    pub should_keep_asset: bool,
    /// The request named the bins, so an update asks for them again.
    #[serde(default, rename = "keep_paths", skip_serializing_if = "is_false")]
    pub should_keep_paths: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// The C library a Linux asset says it was built against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Libc {
    Gnu,
    Musl,
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
    Older { path: PathBuf, schema: u32 },
    Refused(String),
}

/// Whether `repository` is a GitHub `owner/repo`.
pub fn is_repository(repository: &str) -> bool {
    let is_part = |part: &str| {
        !matches!(part, "" | "." | "..")
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    };

    repository
        .split_once('/')
        .is_some_and(|(owner, repo)| is_part(owner) && is_part(repo))
}

/// The package name a GitHub repository installs under. It's the repo name in
/// lowercase, with `.` and `_` as `-`.
pub fn release_name(repository: &str) -> Option<String> {
    let (_, repo) = repository.split_once('/')?;
    let name = repo
        .chars()
        .map(|c| match c {
            '.' | '_' => '-',
            c => c.to_ascii_lowercase(),
        })
        .collect::<String>();

    is_package_name(&name).then_some(name)
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
            let bins = member
                .bins
                .iter()
                .map(|bin| ("bin", bin, output.join("bin").join(bin)));
            let apps = member
                .apps
                .iter()
                .map(|(app, path)| (APPLICATIONS, app, output.join(path)));

            for (directory, export, target) in bins.chain(apps) {
                let Ok(metadata) = fs::symlink_metadata(&target) else {
                    return Err(Error::Refused(format!(
                        "{name} has no {}",
                        target.display()
                    )));
                };

                if directory == APPLICATIONS && !metadata.is_dir() {
                    return Err(Error::Refused(format!(
                        "{name}'s {} isn't a directory",
                        target.display()
                    )));
                }

                // Folded, since macOS file systems ignore case by default.
                let folded = (directory, export.to_lowercase());
                if let Some((other, ..)) = links.insert(folded, (name, export, target)) {
                    return Err(Error::Refused(format!(
                        "{other} and {name} both provide {export}"
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

        let farm = [root.join("bin"), root.join(APPLICATIONS)];
        for directory in &farm {
            fs::create_dir(directory).map_err(io_at(directory))?;
        }

        for ((directory, _), (_, export, target)) in &links {
            let link = root.join(directory).join(export);
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
        for directory in farm.iter().map(PathBuf::as_path).chain([root]) {
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

    pub fn link_apps(&self, applications: &Path) -> Result<Vec<Error>, Error> {
        self.writable()?;
        let farm = self.farm();
        let apps = match fs::read_dir(&farm) {
            Ok(entries) => entries
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<Result<BTreeSet<_>, _>>()
                .map_err(io_at(&farm))?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeSet::new(),
            Err(error) => return Err(io_at(&farm)(error)),
        };

        // We can't remove first because it frees up a name where the casing
        // might've changed rather than a real name that was removed.
        let mut problems = Vec::new();
        match fs::read_dir(applications) {
            Ok(entries) => {
                for entry in entries {
                    let path = match entry {
                        Ok(entry) => entry.path(),
                        Err(error) => {
                            problems.push(io_at(applications)(error));
                            continue;
                        }
                    };

                    let Some(target) = self.target(&path) else {
                        continue;
                    };

                    let is_current = target.file_name() == path.file_name()
                        && target.file_name().is_some_and(|app| apps.contains(app));
                    if !is_current {
                        problems.extend(remove(&path).err());
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => problems.push(io_at(applications)(error)),
        }

        if apps.is_empty() {
            return Ok(problems);
        }

        if let Err(error) = fs::create_dir_all(applications) {
            problems.push(io_at(applications)(error));
            return Ok(problems);
        }

        for app in &apps {
            let path = applications.join(app);
            let linked = match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    symlink(farm.join(app), &path).map_err(io_at(&path))
                }
                Err(error) => Err(io_at(&path)(error)),
                Ok(_) if self.target(&path).is_some() => Ok(()),
                Ok(_) => Err(taken(&path)),
            };

            problems.extend(linked.err());
        }

        Ok(problems)
    }

    pub fn check_apps<'a>(
        &self,
        applications: &Path,
        apps: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), Error> {
        let mut paths = apps.into_iter().map(|app| applications.join(app));
        match paths.find(|path| fs::symlink_metadata(path).is_ok() && self.target(path).is_none()) {
            Some(path) => Err(taken(&path)),
            None => Ok(()),
        }
    }

    fn target(&self, path: &Path) -> Option<PathBuf> {
        let target = fs::read_link(path).ok()?;
        (target.parent() == Some(self.farm().as_path())).then_some(target)
    }

    fn farm(&self) -> PathBuf {
        self.directory.join(&self.name).join(APPLICATIONS)
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
        parse(&path, &bytes, number)
    }

    // Removes anything older than age AND not the current generation.
    pub fn prune(&self, age: Duration) -> Result<Vec<u64>, Error> {
        self.writable()?;
        let Some(current) = self.current_number()? else {
            return Ok(Vec::new());
        };

        let cutoff = SystemTime::now()
            .checked_sub(age)
            .unwrap_or(SystemTime::UNIX_EPOCH);

        let mut removed = Vec::new();
        for number in self.generations()? {
            let path = self.directory.join(self.generation(number));
            let metadata = fs::symlink_metadata(&path).map_err(io_at(&path))?;
            let modified = metadata.modified().map_err(io_at(&path))?;
            if number == current || modified > cutoff {
                continue;
            }

            let staged = self
                .directory
                .join(format!(".tmp-{}", self.generation(number)));

            fs::rename(&path, &staged).map_err(io_at(&path))?;
            removed.push(number);
        }

        sync(&self.directory)?;
        self.clean()?;
        Ok(removed)
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
            Error::Older { path, schema } => write!(
                f,
                "{} was written by an older rb (schema {schema})",
                path.display()
            ),
            Error::Refused(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for Error {}

fn parse(path: &Path, bytes: &[u8], number: u64) -> Result<Generation, Error> {
    let invalid = |error: serde_json::Error| Error::Invalid {
        path: path.to_path_buf(),
        reason: error.to_string(),
    };

    let Schema { schema } = serde_json::from_slice(bytes).map_err(invalid)?;
    if schema > SCHEMA {
        return Err(Error::Newer {
            path: path.to_path_buf(),
            schema,
        });
    }

    if schema < OLDEST_SCHEMA {
        return Err(Error::Invalid {
            path: path.to_path_buf(),
            reason: format!("no rb writes schema {schema}"),
        });
    }

    let record: Record = serde_json::from_slice(bytes).map_err(invalid)?;
    for (name, member) in &record.packages {
        validate(name, member).map_err(|reason| Error::Invalid {
            path: path.to_path_buf(),
            reason,
        })?;
    }

    Ok(Generation {
        number,
        packages: record.packages,
        extra: record.extra,
    })
}

fn validate(name: &str, member: &Member) -> Result<(), String> {
    if !is_package_name(name) {
        return Err(format!("{name:?} isn't a package name"));
    }

    validate_source(
        name,
        member.manifest.as_ref(),
        member.release.as_ref(),
        &member.bins,
    )?;
    validate_exports(name, &member.bins, &member.apps)
}

/// Every package comes from exactly one place. A release links what its
/// paths name and nothing else.
pub(crate) fn validate_source(
    name: &str,
    manifest: Option<&String>,
    release: Option<&Release>,
    bins: &BTreeSet<String>,
) -> Result<(), String> {
    let release = match (manifest, release) {
        (Some(_), None) => return Ok(()),
        (None, Some(release)) => release,
        (Some(_), Some(_)) => return Err(format!("{name} has both a manifest and a release")),
        (None, None) => return Err(format!("{name} has neither a manifest nor a release")),
    };

    if !is_repository(&release.repository) {
        return Err(format!(
            "{name} comes from {:?}, which isn't owner/repo",
            release.repository
        ));
    }

    if !release.paths.keys().eq(bins.iter()) {
        return Err(format!("{name} links bins its release doesn't name"));
    }

    match release.paths.values().find(|path| !is_inside(path)) {
        Some(path) => Err(format!("{name} has a bin at {path:?} outside its output")),
        None => Ok(()),
    }
}

pub(crate) fn validate_exports(
    name: &str,
    bins: &BTreeSet<String>,
    apps: &BTreeMap<String, String>,
) -> Result<(), String> {
    if let Some(bin) = bins.iter().find(|bin| !is_file_name(bin)) {
        return Err(format!("{name} has a bin {bin:?} that isn't a plain name"));
    }

    if let Some(app) = apps.keys().find(|app| !is_app_name(app)) {
        return Err(format!(
            "{name} has an app {app:?} that isn't a bundle name"
        ));
    }

    match apps.values().find(|path| !is_inside(path)) {
        Some(path) => Err(format!("{name} has an app at {path:?} outside its output")),
        None => Ok(()),
    }
}

fn taken(path: &Path) -> Error {
    Error::Refused(format!("{} already exists", path.display()))
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

pub(crate) fn io_at(path: &Path) -> impl Fn(io::Error) -> Error {
    let path = path.to_path_buf();
    move |source| Error::Io {
        path: path.clone(),
        source,
    }
}
