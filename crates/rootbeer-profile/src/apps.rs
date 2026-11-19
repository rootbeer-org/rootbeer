use crate::{Error, Profile, Schema, io_at, sync, taken};
use rootbeer_drv::Key;
use rustix::fs::{CWD, RenameFlags, renameat_with};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

const RECEIPT_SCHEMA: u32 = 1;
const RECEIPT: &str = ".apps.json";

/// What happens to the copy of an app the profile no longer selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    Delete,
    /// Hands the copy to the user, so rb never touches it again.
    Keep,
}

#[derive(Debug, Default)]
pub struct Deployed {
    /// Apps that couldn't be installed or removed. The rest still were.
    pub problems: Vec<Error>,
    /// Apps that were links into the profile and are now copies.
    pub migrated: Vec<String>,
}

/// The copies a profile installed, by path. A copy can't show who put it
/// there, so a profile replaces or removes only what its receipt lists.
#[derive(Serialize, Deserialize)]
struct Receipt {
    schema: u32,
    apps: BTreeMap<PathBuf, Installed>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Installed {
    package: String,
    key: Key,
}

struct Wanted {
    installed: Installed,
    bundle: PathBuf,
}

enum Placement {
    New,
    Replace,
    OverLink,
}

impl Profile {
    /// Installs the current generation's apps in `applications` as copies of
    /// their store bundles, and removes the copies it no longer selects.
    pub fn install_apps(&self, applications: &Path, removal: Removal) -> Result<Deployed, Error> {
        self.writable()?;
        let wanted = self.wanted(applications)?;
        let others = self.others()?;
        let mut receipt = read_receipt(&self.receipt())?;
        let mut deployed = Deployed::default();

        deployed.problems.extend(self.sweep(applications));

        // Before installing, so an app whose name changed case frees it.
        let stale = receipt
            .apps
            .keys()
            .filter(|path| !wanted.contains_key(*path))
            .cloned()
            .collect::<Vec<_>>();

        for path in stale {
            if removal == Removal::Delete
                && let Err(error) = remove_tree(&path)
            {
                deployed.problems.push(error);
                continue;
            }

            receipt.apps.remove(&path);
            self.write_receipt(&receipt)?;
        }

        let links = self.links(applications, &wanted, &mut deployed.problems);
        if wanted.is_empty() {
            return Ok(deployed);
        }

        if let Err(error) = fs::create_dir_all(applications) {
            deployed.problems.push(io_at(applications)(error));
            return Ok(deployed);
        }

        for (path, wanted) in &wanted {
            let recorded = receipt.apps.get(path);
            let placement = match (recorded, fs::symlink_metadata(path)) {
                (Some(recorded), Ok(_)) if *recorded == wanted.installed => continue,
                (Some(_), Ok(_)) => Placement::Replace,
                (None, _) if let Some(owner) = others.get(&fold(path)) => {
                    deployed.problems.push(owned(path, owner));
                    continue;
                }
                (_, Err(error)) if error.kind() == io::ErrorKind::NotFound => Placement::New,
                (_, Err(error)) => {
                    deployed.problems.push(io_at(path)(error));
                    continue;
                }
                (None, Ok(_)) if links.contains(path) => Placement::OverLink,
                (None, Ok(_)) => {
                    deployed.problems.push(taken(path));
                    continue;
                }
            };

            let is_link = matches!(placement, Placement::OverLink);
            if is_link && let Err(error) = fs::remove_file(path) {
                deployed.problems.push(io_at(path)(error));
                continue;
            }

            let placed = match placement {
                // Recorded first, so a crash leaves a copy rb still owns. A
                // copy that failed to land was never rb's.
                Placement::New | Placement::OverLink => {
                    receipt.apps.insert(path.clone(), wanted.installed.clone());
                    self.write_receipt(&receipt)?;
                    let placed = self.place(&wanted.bundle, path, RenameFlags::NOREPLACE);
                    if placed.is_err() {
                        receipt.apps.remove(path);
                        self.write_receipt(&receipt)?;
                    }

                    placed
                }
                Placement::Replace => self
                    .place(&wanted.bundle, path, RenameFlags::EXCHANGE)
                    .and_then(|()| {
                        receipt.apps.insert(path.clone(), wanted.installed.clone());
                        self.write_receipt(&receipt)
                    }),
            };

            match placed {
                Ok(()) if is_link => {
                    let name = path.file_name().unwrap_or_default();
                    deployed.migrated.push(name.to_string_lossy().into_owned());
                }
                Ok(()) => {}
                Err(error) => deployed.problems.push(error),
            }
        }

        Ok(deployed)
    }

    /// Refuses apps whose names something other than this profile holds in
    /// `applications`.
    pub fn check_apps<'a>(
        &self,
        applications: &Path,
        apps: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), Error> {
        let receipt = read_receipt(&self.receipt())?;
        let others = self.others()?;
        let ours = receipt
            .apps
            .keys()
            .map(|path| fold(path))
            .collect::<BTreeSet<_>>();
        for path in apps.into_iter().map(|app| applications.join(app)) {
            if let Some(owner) = others.get(&fold(&path)) {
                return Err(owned(&path, owner));
            }

            let is_ours = ours.contains(&fold(&path)) || self.target(&path).is_some();
            if !is_ours && fs::symlink_metadata(&path).is_ok() {
                return Err(taken(&path));
            }
        }

        Ok(())
    }

    fn wanted(&self, applications: &Path) -> Result<BTreeMap<PathBuf, Wanted>, Error> {
        let Some(generation) = self.current()? else {
            return Ok(BTreeMap::new());
        };

        let farm = self.farm();
        let mut wanted = BTreeMap::new();
        for (package, member) in generation.packages {
            for app in member.apps.keys() {
                let link = farm.join(app);
                let bundle = fs::read_link(&link).map_err(io_at(&link))?;
                let installed = Installed {
                    package: package.clone(),
                    key: member.key.clone(),
                };

                wanted.insert(applications.join(app), Wanted { installed, bundle });
            }
        }

        Ok(wanted)
    }

    /// Links into the profile's farm, which apps were before they were
    /// copies. Removes those for apps it no longer selects, and returns the
    /// rest for their copies to replace.
    fn links(
        &self,
        applications: &Path,
        wanted: &BTreeMap<PathBuf, Wanted>,
        problems: &mut Vec<Error>,
    ) -> BTreeSet<PathBuf> {
        let entries = match fs::read_dir(applications) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return BTreeSet::new(),
            Err(error) => {
                problems.push(io_at(applications)(error));
                return BTreeSet::new();
            }
        };

        let mut links = BTreeSet::new();
        for entry in entries {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(error) => {
                    problems.push(io_at(applications)(error));
                    continue;
                }
            };

            if self.target(&path).is_none() {
                continue;
            }

            match wanted.contains_key(&path) {
                true => {
                    links.insert(path);
                }
                false => problems.extend(fs::remove_file(&path).map_err(io_at(&path)).err()),
            }
        }

        links
    }

    /// Every other profile's receipt, by the folded path of each copy.
    fn others(&self) -> Result<BTreeMap<String, String>, Error> {
        let entries = fs::read_dir(&self.directory).map_err(io_at(&self.directory))?;
        let mut owners = BTreeMap::new();
        for entry in entries {
            let entry = entry.map_err(io_at(&self.directory))?;
            let file_name = entry.file_name();
            let Some(profile) = file_name
                .to_str()
                .and_then(|name| name.strip_suffix(RECEIPT))
            else {
                continue;
            };

            if profile == self.name {
                continue;
            }

            for path in read_receipt(&entry.path())?.apps.into_keys() {
                owners.insert(fold(&path), profile.to_string());
            }
        }

        Ok(owners)
    }

    fn receipt(&self) -> PathBuf {
        self.directory.join(format!("{}{RECEIPT}", self.name))
    }

    fn write_receipt(&self, receipt: &Receipt) -> Result<(), Error> {
        let path = self.receipt();
        let mut json = serde_json::to_vec_pretty(&receipt).map_err(|error| Error::Invalid {
            path: path.clone(),
            reason: error.to_string(),
        })?;

        json.push(b'\n');
        let mut staged = tempfile::Builder::new()
            .prefix(".tmp-")
            .tempfile_in(&self.directory)
            .map_err(io_at(&self.directory))?;

        staged
            .write_all(&json)
            .and_then(|()| staged.as_file().sync_all())
            .map_err(io_at(staged.path()))?;

        staged
            .persist(&path)
            .map_err(|error| io_at(&path)(error.error))?;

        sync(&self.directory)
    }

    /// Copies `bundle` beside `path` under a hidden name, then renames it into
    /// place with `flags`. Swapping leaves the old bundle under the hidden
    /// name to delete.
    fn place(&self, bundle: &Path, path: &Path, flags: RenameFlags) -> Result<(), Error> {
        let staged = self.staged(path);
        remove_tree(&staged)?;

        let renamed = copy_bundle(bundle, &staged)
            .and_then(|()| make_writable(&staged))
            .map_err(io_at(&staged))
            .and_then(|()| {
                renameat_with(CWD, &staged, CWD, path, flags)
                    .map_err(|error| io_at(path)(error.into()))
            });

        // Best effort either way, since the next install sweeps what's left.
        let _ = remove_tree(&staged);
        renamed
    }

    fn staged(&self, path: &Path) -> PathBuf {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        path.with_file_name(format!("{}{name}.tmp", self.staging_prefix()))
    }

    fn staging_prefix(&self) -> String {
        format!(".rb-{}-", self.name)
    }

    /// Removes copies a crash left half made or half replaced.
    fn sweep(&self, applications: &Path) -> Vec<Error> {
        let Ok(entries) = fs::read_dir(applications) else {
            return Vec::new();
        };

        let prefix = self.staging_prefix();
        entries
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".tmp"))
            })
            .filter_map(|entry| remove_tree(&entry.path()).err())
            .collect()
    }
}

fn read_receipt(path: &Path) -> Result<Receipt, Error> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Receipt {
                schema: RECEIPT_SCHEMA,
                apps: BTreeMap::new(),
            });
        }
        Err(error) => return Err(io_at(path)(error)),
    };

    let invalid = |error: serde_json::Error| Error::Invalid {
        path: path.to_path_buf(),
        reason: error.to_string(),
    };

    let Schema { schema } = serde_json::from_slice(&bytes).map_err(invalid)?;
    if schema > RECEIPT_SCHEMA {
        return Err(Error::Newer {
            path: path.to_path_buf(),
            schema,
        });
    }

    serde_json::from_slice(&bytes).map_err(invalid)
}

/// Clones on APFS, where the copy shares the store's blocks. Across volumes
/// it copies instead.
fn copy_bundle(bundle: &Path, target: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    match clone(bundle, target) {
        Err(error) if is_unclonable(&error) => {}
        cloned => return cloned,
    }

    copy_tree(bundle, target)
}

#[cfg(target_os = "macos")]
fn is_unclonable(error: &io::Error) -> bool {
    use rustix::io::Errno;

    let errno = Errno::from_io_error(error);
    [Errno::XDEV, Errno::NOTSUP, Errno::OPNOTSUPP]
        .iter()
        .any(|unclonable| errno == Some(*unclonable))
}

#[cfg(target_os = "macos")]
fn clone(bundle: &Path, target: &Path) -> io::Result<()> {
    use std::ffi::{CString, c_char, c_int};
    use std::os::unix::ffi::OsStrExt;

    // From sys/clonefile.h. The user owns the clone, rather than root.
    const CLONE_NOFOLLOW: u32 = 0x0001;
    const CLONE_NOOWNERCOPY: u32 = 0x0002;

    unsafe extern "C" {
        fn clonefile(source: *const c_char, target: *const c_char, flags: u32) -> c_int;
    }

    let bundle = CString::new(bundle.as_os_str().as_bytes())?;
    let target = CString::new(target.as_os_str().as_bytes())?;
    let flags = CLONE_NOFOLLOW | CLONE_NOOWNERCOPY;
    match unsafe { clonefile(bundle.as_ptr(), target.as_ptr(), flags) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// `fs::copy` clones each file where it can, and on macOS keeps its
/// extended attributes.
fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    let kind = metadata.file_type();
    if kind.is_symlink() {
        return symlink(fs::read_link(source)?, target);
    }

    if !kind.is_dir() {
        return fs::copy(source, target).map(drop);
    }

    fs::create_dir(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        copy_tree(&entry.path(), &target.join(entry.file_name()))?;
    }

    fs::set_permissions(target, metadata.permissions())
}

/// The store's modes are read-only, and an updater moving the bundle needs
/// write access to every directory in it.
fn make_writable(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }

    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o200);
    fs::set_permissions(path, permissions)?;

    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            make_writable(&entry?.path())?;
        }
    }

    Ok(())
}

/// A copy a crash interrupted can keep the store's read-only directories,
/// which nothing can be deleted from until they're writable again.
fn remove_tree(path: &Path) -> Result<(), Error> {
    let removed = fs::remove_dir_all(path).or_else(|error| match error.kind() {
        io::ErrorKind::PermissionDenied => {
            make_writable(path).and_then(|()| fs::remove_dir_all(path))
        }
        _ => Err(error),
    });

    match removed {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(io_at(path)(error)),
        _ => Ok(()),
    }
}

/// macOS file systems ignore case by default.
fn fold(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

fn owned(path: &Path, owner: &str) -> Error {
    Error::Refused(format!(
        "{} belongs to your {owner} profile",
        path.display()
    ))
}
