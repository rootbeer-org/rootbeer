use crate::{Error, RECORD, io_at, parse};
use rootbeer_drv::Key;
use rustix::fs::{Dir, FileType, Mode, OFlags, fstat, openat};
use rustix::io::Errno;
use rustix::process::{Pid, test_kill_process};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

const FILE: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

const LIMIT: u64 = 16 << 20;

/// A file per running process, named by its id, holding the keys it runs.
const RUNNING: &str = ".running";

/// Traverses the profiles, reading all the manifest files of each generation
/// for each user to determine the set of all keys that were installed, and
/// adds the keys of commands still running without a profile.
pub fn roots(profiles: &Path) -> Result<BTreeSet<Key>, Error> {
    let directory = match rustix::fs::open(profiles, DIRECTORY, Mode::empty()) {
        Ok(directory) => directory,
        Err(Errno::NOENT) => {
            return Err(Error::Refused(format!(
                "{} is missing, so run `rb-helper setup` again",
                profiles.display()
            )));
        }
        Err(error) => return Err(io_at(profiles)(error.into())),
    };

    let mut keys = BTreeSet::new();
    for user in children(&directory, profiles)? {
        let path = profiles.join(&user);
        let Some(user) = subdirectory(&directory, &user, &path)? else {
            continue;
        };

        let owner = fstat(&user)
            .map_err(|error| io_at(&path)(error.into()))?
            .st_uid;

        for name in children(&user, &path)? {
            let number = name
                .rsplit_once('-')
                .filter(|(profile, _)| !profile.is_empty())
                .and_then(|(_, number)| number.parse::<u64>().ok());

            let Some(number) = number else {
                continue;
            };

            let Some(generation) = subdirectory(&user, &name, &path.join(&name))? else {
                continue;
            };

            let record = path.join(&name).join(RECORD);
            let bytes = read(&generation, RECORD, &record, owner)?;

            // This can error if the generation does not belong to the user
            let generation = parse(&record, &bytes, number).map_err(|error| match error {
                Error::Invalid { path, .. } => Error::Invalid {
                    path,
                    reason: "isn't a generation rb can read".into(),
                },
                error => error,
            })?;

            keys.extend(generation.packages.into_values().map(|member| member.key));
        }

        keys.extend(running(&user, &path, owner)?);
    }

    Ok(keys)
}

/// Keeps `keys` from GC while this process runs, for a command run without a
/// profile. The id survives exec, so it covers what the process becomes, and
/// files of processes that are gone are removed first.
pub fn hold_while_running(user: &Path, keys: &BTreeSet<Key>) -> Result<(), Error> {
    let directory = user.join(RUNNING);
    let at = io_at(&directory);
    match fs::DirBuilder::new().mode(0o700).create(&directory) {
        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => return Err(at(error)),
        _ => {}
    }

    for entry in fs::read_dir(&directory).map_err(&at)? {
        let path = entry.map_err(&at)?.path();
        let pid = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(pid);
        if pid.is_some_and(|pid| !is_alive(pid)) {
            fs::remove_file(&path).map_err(io_at(&path))?;
        }
    }

    let path = directory.join(std::process::id().to_string());
    let mut file = tempfile::Builder::new()
        .prefix(".")
        .tempfile_in(&directory)
        .map_err(&at)?;
    let contents: String = keys.iter().map(|key| format!("{key}\n")).collect();
    file.write_all(contents.as_bytes()).map_err(io_at(&path))?;
    file.persist(&path)
        .map_err(|error| io_at(&path)(error.error))?;
    Ok(())
}

fn running(user: &OwnedFd, path: &Path, owner: u32) -> Result<BTreeSet<Key>, Error> {
    let path = path.join(RUNNING);
    let Some(directory) = subdirectory(user, RUNNING, &path)? else {
        return Ok(BTreeSet::new());
    };

    let mut keys = BTreeSet::new();
    for name in children(&directory, &path)? {
        if !pid(&name).is_some_and(is_alive) {
            continue;
        }

        let file = path.join(&name);
        let bytes = read(&directory, &name, &file, owner)?;
        let invalid = || Error::Invalid {
            path: file.clone(),
            reason: "isn't a list of store keys".into(),
        };
        let text = std::str::from_utf8(&bytes).map_err(|_| invalid())?;
        for line in text.lines() {
            keys.insert(line.parse().map_err(|_| invalid())?);
        }
    }

    Ok(keys)
}

fn pid(name: &str) -> Option<Pid> {
    let raw = name.parse::<i32>().ok().filter(|raw| *raw > 0)?;
    Pid::from_raw(raw)
}

/// Another user's process can't be signalled, which still says it's running.
fn is_alive(pid: Pid) -> bool {
    !matches!(test_kill_process(pid), Err(Errno::SRCH))
}

fn children(directory: &OwnedFd, path: &Path) -> Result<Vec<String>, Error> {
    let at = |error: Errno| io_at(path)(error.into());
    let mut names = Vec::new();
    for entry in Dir::read_from(directory).map_err(at)? {
        let entry = entry.map_err(at)?;
        let Ok(name) = entry.file_name().to_str() else {
            continue;
        };

        if !name.starts_with('.') {
            names.push(name.to_string());
        }
    }

    Ok(names)
}

fn subdirectory(parent: &OwnedFd, name: &str, path: &Path) -> Result<Option<OwnedFd>, Error> {
    match openat(parent, name, DIRECTORY, Mode::empty()) {
        Ok(directory) => Ok(Some(directory)),
        Err(Errno::NOTDIR | Errno::LOOP | Errno::NOENT) => Ok(None),
        Err(error) => Err(io_at(path)(error.into())),
    }
}

fn read(directory: &OwnedFd, name: &str, path: &Path, owner: u32) -> Result<Vec<u8>, Error> {
    let at = |error: Errno| io_at(path)(error.into());
    let file = openat(directory, name, FILE, Mode::empty()).map_err(at)?;

    let stat = fstat(&file).map_err(at)?;
    let is_small = u64::try_from(stat.st_size).is_ok_and(|size| size <= LIMIT);
    let is_regular = FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile;

    if !is_regular || !is_small || stat.st_uid != owner {
        return Err(Error::Invalid {
            path: path.to_path_buf(),
            reason: "isn't a small regular file of the profile's owner".into(),
        });
    }

    let mut bytes = Vec::new();
    File::from(file)
        .take(LIMIT)
        .read_to_end(&mut bytes)
        .map_err(io_at(path))?;

    Ok(bytes)
}
