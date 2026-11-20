use crate::{Error, Profile, Schema, compare_versions, io_at, sync, taken};
use rootbeer_drv::{AppSettings, Key, Location, Updates};
use rustix::fs::{CWD, RenameFlags, renameat_with};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

const RECEIPT_SCHEMA: u32 = 1;
const RECEIPT: &str = ".apps.json";

/// The folders apps are installed in, by location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applications {
    /// `~/Applications`
    pub user: PathBuf,
    /// `/Applications`
    pub system: PathBuf,
}

impl Applications {
    pub fn path(&self, location: Location, app: &str) -> PathBuf {
        match location {
            Location::User => self.user.join(app),
            Location::System => self.system.join(app),
        }
    }
}

/// How the user asked for a package's apps to be installed, over what their
/// exports say.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updates: Option<Updates>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
}

impl Overrides {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// What `newer` sets replaces what this sets.
    pub fn merge(self, newer: Overrides) -> Overrides {
        Overrides {
            updates: newer.updates.or(self.updates),
            location: newer.location.or(self.location),
        }
    }

    /// An export that needs `/Applications` keeps it.
    pub fn apply(self, export: AppSettings) -> AppSettings {
        let location = match export.location {
            Location::System => Location::System,
            Location::User => self.location.unwrap_or(Location::User),
        };

        AppSettings {
            updates: self.updates.unwrap_or(export.updates),
            location,
        }
    }

    /// Refuses what can't apply to `package`, given its apps and how their
    /// exports install them.
    pub fn check(
        self,
        package: &str,
        apps: &BTreeMap<String, String>,
        settings: &BTreeMap<String, AppSettings>,
    ) -> Result<(), Error> {
        if self.is_empty() {
            return Ok(());
        }

        if apps.is_empty() {
            return Err(Error::Refused(format!(
                "{package} has no apps, so updates and location don't apply to it"
            )));
        }

        let system = settings
            .iter()
            .find(|(_, settings)| settings.location == Location::System);
        match (self.location, system) {
            (Some(Location::User), Some((app, _))) => Err(Error::Refused(format!(
                "{app} only works from /Applications"
            ))),
            _ => Ok(()),
        }
    }
}

/// What happens to the copy of an app the profile no longer selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    Delete,
    /// Hands the copy to the user, so rb never touches it again.
    Keep,
}

/// What happens to the copy of an app rb updates when it changed since rb
/// installed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Changed {
    /// Reports it and leaves it as it is.
    Leave,
    Replace,
}

#[derive(Debug, Default)]
pub struct Deployed {
    /// Apps that couldn't be installed or removed. The rest still were.
    pub problems: Vec<Error>,
    /// Apps that were links into the profile and are now copies.
    pub migrated: Vec<String>,
    /// Apps that changed since rb installed them, which it left alone.
    pub changed: Vec<String>,
    /// Apps replaced while they ran, which keep the old version until they
    /// restart.
    pub running: Vec<String>,
}

/// A copy the profile installed, as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledApp {
    pub path: PathBuf,
    pub package: String,
    /// The package's key when rb installed the copy.
    pub key: Key,
    /// The version the copy's Info.plist states now.
    pub version: Option<String>,
    /// Whether the copy changed since rb installed it, as an app that
    /// updates itself does.
    pub has_changed: bool,
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
    version: String,
    key: Key,
    /// Absent when the bundle had no Info.plist rb could read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bundle: Option<Bundle>,
}

/// What a bundle's Info.plist says. It's read under Apple's names and kept
/// in the receipt under short ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Bundle {
    #[serde(alias = "CFBundleIdentifier")]
    id: String,
    #[serde(
        alias = "CFBundleShortVersionString",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    version: Option<String>,
    #[serde(
        alias = "CFBundleVersion",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    build: Option<String>,
}

struct Wanted {
    package: String,
    version: String,
    key: Key,
    updates: Updates,
    location: Location,
    bundle: PathBuf,
}

impl Wanted {
    fn installed(&self) -> Installed {
        Installed {
            package: self.package.clone(),
            version: self.version.clone(),
            key: self.key.clone(),
            bundle: read_bundle(&self.bundle),
        }
    }
}

enum Placement {
    New,
    Replace,
    OverLink,
}

enum Action {
    Keep,
    Replace,
    Changed,
    Disown,
}

impl Profile {
    /// Installs the current generation's apps in `applications` as copies of
    /// their store bundles, and removes the copies it no longer selects.
    pub fn install_apps(
        &self,
        applications: &Applications,
        removal: Removal,
        changed: Changed,
    ) -> Result<Deployed, Error> {
        self.writable()?;
        let wanted = self.wanted(applications)?;
        let others = self.others()?;
        let mut receipt = read_receipt(&self.receipt())?;
        let mut deployed = Deployed::default();

        for folder in [&applications.user, &applications.system] {
            deployed.problems.extend(self.sweep(folder));
        }

        // The file system ignores case, so an app whose name changed case
        // frees it first. One that moved keeps its copy until the new one
        // lands.
        let wanted_names = wanted
            .keys()
            .map(|path| fold(path))
            .collect::<BTreeSet<_>>();
        let (renamed, moved) = receipt
            .apps
            .keys()
            .filter(|path| !wanted.contains_key(*path))
            .cloned()
            .partition::<Vec<_>, _>(|path| wanted_names.contains(&fold(path)));

        self.remove_stale(&mut receipt, renamed, removal, &mut deployed.problems)?;

        let links = self.links(&applications.user, &wanted, &mut deployed.problems);
        let has_user_apps = wanted.values().any(|app| app.location == Location::User);
        if has_user_apps && let Err(error) = fs::create_dir_all(&applications.user) {
            deployed.problems.push(io_at(&applications.user)(error));
            return Ok(deployed);
        }

        for (path, wanted) in &wanted {
            let recorded = receipt.apps.get(path);
            let placement = match (recorded, fs::symlink_metadata(path)) {
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
                (Some(recorded), Ok(_)) => match reconcile(recorded, path, wanted, changed) {
                    Action::Keep => continue,
                    Action::Replace => Placement::Replace,
                    Action::Changed => {
                        deployed.changed.push(name(path));
                        continue;
                    }
                    Action::Disown => {
                        receipt.apps.remove(path);
                        self.write_receipt(&receipt)?;
                        deployed.problems.push(disowned(path));
                        continue;
                    }
                },
            };

            if wanted.location == Location::System && !can_write(&applications.system) {
                deployed
                    .problems
                    .push(unwritable(path, &applications.system));
                continue;
            }

            let is_link = matches!(placement, Placement::OverLink);
            if is_link && let Err(error) = fs::remove_file(path) {
                deployed.problems.push(io_at(path)(error));
                continue;
            }

            let is_running = matches!(placement, Placement::Replace) && is_running(path);
            let installed = wanted.installed();
            let placed = match placement {
                // Recorded first, so a crash leaves a copy rb still owns. A
                // copy that failed to land was never rb's.
                Placement::New | Placement::OverLink => {
                    receipt.apps.insert(path.clone(), installed);
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
                        receipt.apps.insert(path.clone(), installed);
                        self.write_receipt(&receipt)
                    }),
            };

            match placed {
                Ok(()) if is_link => deployed.migrated.push(name(path)),
                Ok(()) if is_running => deployed.running.push(name(path)),
                Ok(()) => {}
                Err(error) => deployed.problems.push(error),
            }
        }

        // A move whose new copy failed keeps the old one.
        let app_name = |path: &Path| path.file_name().map(|name| fold(Path::new(name)));
        let has_landed = |path: &PathBuf| {
            wanted
                .keys()
                .filter(|wanted| app_name(wanted) == app_name(path))
                .all(|wanted| receipt.apps.contains_key(wanted))
        };

        let moved = moved.into_iter().filter(has_landed).collect();
        self.remove_stale(&mut receipt, moved, removal, &mut deployed.problems)?;
        Ok(deployed)
    }

    /// Removes the copies at `stale`, which the profile no longer selects.
    fn remove_stale(
        &self,
        receipt: &mut Receipt,
        stale: Vec<PathBuf>,
        removal: Removal,
        problems: &mut Vec<Error>,
    ) -> Result<(), Error> {
        for path in stale {
            let Some(installed) = receipt.apps.get(&path) else {
                continue;
            };

            let is_present = fs::symlink_metadata(&path).is_ok();
            if is_present && !is_same_app(installed, read_bundle(&path).as_ref()) {
                problems.push(disowned(&path));
            } else if removal == Removal::Delete
                && let Err(error) = remove_tree(&path)
            {
                problems.push(error);
                continue;
            }

            receipt.apps.remove(&path);
            self.write_receipt(receipt)?;
        }

        Ok(())
    }

    /// The copies this profile installed, as they are now.
    pub fn installed_apps(&self) -> Result<Vec<InstalledApp>, Error> {
        let receipt = read_receipt(&self.receipt())?;
        let apps = receipt.apps.into_iter().map(|(path, installed)| {
            let current = read_bundle(&path);
            let has_changed = has_changed(&installed, current.as_ref());
            InstalledApp {
                path,
                package: installed.package,
                key: installed.key,
                version: current.and_then(|bundle| bundle.version.or(bundle.build)),
                has_changed,
            }
        });

        Ok(apps.collect())
    }

    /// Refuses apps whose paths something other than this profile holds, or
    /// that go where the user can't write.
    pub fn check_apps<'a>(
        &self,
        applications: &Applications,
        apps: impl IntoIterator<Item = (&'a str, Location)>,
    ) -> Result<(), Error> {
        let receipt = read_receipt(&self.receipt())?;
        let others = self.others()?;
        let ours = receipt
            .apps
            .keys()
            .map(|path| fold(path))
            .collect::<BTreeSet<_>>();
        for (app, location) in apps {
            let path = applications.path(location, app);
            if location == Location::System && !can_write(&applications.system) {
                return Err(unwritable(&path, &applications.system));
            }

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

    fn wanted(&self, applications: &Applications) -> Result<BTreeMap<PathBuf, Wanted>, Error> {
        let Some(generation) = self.current()? else {
            return Ok(BTreeMap::new());
        };

        let farm = self.farm();
        let mut wanted = BTreeMap::new();
        for (package, member) in generation.packages {
            for app in member.apps.keys() {
                let link = farm.join(app);
                let bundle = fs::read_link(&link).map_err(io_at(&link))?;
                let settings = member.app(app);
                let entry = Wanted {
                    package: package.clone(),
                    version: member.version.clone(),
                    key: member.key.clone(),
                    updates: settings.updates,
                    location: settings.location,
                    bundle,
                };

                wanted.insert(applications.path(settings.location, app), entry);
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

    /// Every user's profiles stage in `/Applications`, and each user has a
    /// profile with each name, so the uid keeps them apart.
    fn staging_prefix(&self) -> String {
        let uid = rustix::process::getuid().as_raw();
        format!(".rb-{uid}-{}-", self.name)
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

/// `/Applications` is `root:admin` with group write, so an admin writes
/// there without sudo, as dragging an app in does.
fn can_write(folder: &Path) -> bool {
    rustix::fs::access(folder, rustix::fs::Access::WRITE_OK).is_ok()
}

fn unwritable(path: &Path, folder: &Path) -> Error {
    Error::Refused(format!(
        "installing {} needs an admin user, who can write to {}",
        path.display(),
        folder.display()
    ))
}

fn disowned(path: &Path) -> Error {
    Error::Refused(format!(
        "{} is no longer the app rb installed, so rb stopped managing it",
        path.display()
    ))
}

fn name(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default();
    name.to_string_lossy().into_owned()
}

/// What to do with a copy rb installed, now that the profile selects
/// `wanted` there.
fn reconcile(recorded: &Installed, path: &Path, wanted: &Wanted, changed: Changed) -> Action {
    let current = read_bundle(path);
    if !is_same_app(recorded, current.as_ref()) {
        return Action::Disown;
    }

    let has_changed = has_changed(recorded, current.as_ref());
    let is_forced = changed == Changed::Replace;
    let is_selected = recorded.key == wanted.key;
    match wanted.updates {
        Updates::Rootbeer if is_selected && !has_changed => Action::Keep,
        Updates::Rootbeer if has_changed && !is_forced => Action::Changed,
        Updates::Rootbeer => Action::Replace,
        // Bundle versions don't order, so rb moves an app forward only by
        // its own versions, and only while the app's updater hasn't.
        Updates::Itself if is_selected => Action::Keep,
        Updates::Itself
            if compare_versions(&wanted.version, &recorded.version) != Ordering::Greater =>
        {
            Action::Keep
        }
        Updates::Itself if has_changed && !is_forced => Action::Keep,
        Updates::Itself => Action::Replace,
    }
}

/// An app that updates itself keeps its identifier, so a different one
/// means someone put another app there.
fn is_same_app(installed: &Installed, current: Option<&Bundle>) -> bool {
    let Some(recorded) = &installed.bundle else {
        return true;
    };

    current.is_some_and(|current| current.id == recorded.id)
}

/// Without an Info.plist at install there's nothing to compare against.
fn has_changed(installed: &Installed, current: Option<&Bundle>) -> bool {
    installed.bundle.is_some() && installed.bundle.as_ref() != current
}

fn read_bundle(app: &Path) -> Option<Bundle> {
    plist::from_file(app.join("Contents/Info.plist")).ok()
}

/// Whether a process runs from inside `app`.
#[cfg(target_os = "macos")]
pub(crate) fn is_running(app: &Path) -> bool {
    use std::ffi::{OsStr, c_int};
    use std::os::unix::ffi::OsStrExt;

    let Ok(count) = usize::try_from(unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) })
    else {
        return false;
    };

    // Room for processes that start between the two calls.
    let mut pids: Vec<c_int> = vec![0; count.saturating_add(64)];
    let Some(size) = pids
        .len()
        .checked_mul(size_of::<c_int>())
        .and_then(|size| c_int::try_from(size).ok())
    else {
        return false;
    };

    let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) };
    let Ok(count) = usize::try_from(count) else {
        return false;
    };

    let (Ok(size), Ok(capacity)) = (
        usize::try_from(libc::PROC_PIDPATHINFO_MAXSIZE),
        u32::try_from(libc::PROC_PIDPATHINFO_MAXSIZE),
    ) else {
        return false;
    };

    let mut buffer = vec![0_u8; size];

    pids.iter().take(count).any(|pid| {
        let length = unsafe { libc::proc_pidpath(*pid, buffer.as_mut_ptr().cast(), capacity) };
        let length = usize::try_from(length).unwrap_or_default();
        buffer
            .get(..length)
            .is_some_and(|path| Path::new(OsStr::from_bytes(path)).starts_with(app))
    })
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn is_running(_: &Path) -> bool {
    false
}
