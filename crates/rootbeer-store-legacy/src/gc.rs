//! Garbage collection for the shared store. Each user publishes roots under
//! `var/roots/<uid>/`, one file per owner listing the entry names it keeps
//! alive, and collection deletes every entry no root lists.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime};

use crate::Store;

/// Entries this young are kept even when unrooted, so an install can finish
/// writing its root after committing or reusing an entry.
pub const GRACE: Duration = Duration::from_secs(60 * 60);

const MAX_ROOT_BYTES: u64 = 1 << 20;

#[derive(Debug, Default)]
pub struct Report {
    pub removed: Vec<String>,
    pub kept: usize,
}

impl Store {
    fn roots_dir(&self) -> PathBuf {
        self.root.parent().unwrap_or(&self.root).join("var/roots")
    }

    /// Records the entries `owner` keeps alive, replacing its previous root.
    pub fn write_root(&self, owner: &str, entries: &BTreeSet<PathBuf>) -> io::Result<()> {
        if !is_valid_owner(owner) {
            return Err(io::Error::other(format!("invalid root name '{owner}'")));
        }

        let dir = self.roots_dir().join(unsafe { libc::getuid() }.to_string());
        if let Err(error) = fs::create_dir_all(&dir) {
            if error.kind() != io::ErrorKind::PermissionDenied || !self.helper().is_file() {
                return Err(error);
            }
            self.run_helper("roots")?;
        }

        let mut contents = String::new();
        for entry in entries {
            let Some(name) = entry.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            contents.push_str(name);
            contents.push('\n');
        }

        let tmp = dir.join(format!(".{owner}.{}", std::process::id()));
        fs::write(&tmp, contents)?;
        fs::rename(tmp, dir.join(owner))
    }

    pub fn has_root(&self, owner: &str) -> bool {
        let uid = unsafe { libc::getuid() }.to_string();
        self.roots_dir().join(uid).join(owner).is_file()
    }

    /// Creates the caller's roots directory. Run by `rb-store`, so as root the
    /// directory is handed to the real user rather than left to be squatted.
    pub fn create_user_roots(&self) -> io::Result<PathBuf> {
        let roots = self.roots_dir();
        fs::create_dir_all(&roots)?;

        let uid = unsafe { libc::getuid() };
        let dir = roots.join(uid.to_string());
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        std::os::unix::fs::chown(&dir, Some(uid), Some(unsafe { libc::getgid() }))?;
        Ok(dir)
    }

    /// Deletes every entry no root lists, keeping those younger than `grace`.
    /// Holds the root's `.lock` so it never overlaps a bootstrap or migration.
    pub fn collect_garbage(&self, grace: Duration) -> io::Result<Report> {
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.parent().unwrap_or(&self.root).join(".lock"))?;
        lock.lock()?;

        let live = self.read_roots()?;
        let cutoff = SystemTime::now() - grace;
        let mut report = Report::default();
        let mut children = fs::read_dir(&self.root)?.collect::<Result<Vec<_>, _>>()?;
        children.sort_by_key(|child| child.file_name());

        for child in children {
            let Some(name) = child.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let is_trash = name.starts_with(".trash-");
            if !is_trash && !name.starts_with("sha256-") && !name.starts_with(".tmp-") {
                continue;
            }
            if live.contains(&name) {
                report.kept += 1;
                continue;
            }

            let metadata = fs::symlink_metadata(child.path())?;
            if !is_trash && metadata.modified()? > cutoff {
                report.kept += 1;
                continue;
            }

            // Renamed first so a half-deleted tree never passes as a sealed entry.
            let trash = if is_trash {
                child.path()
            } else {
                let trash = self
                    .root
                    .join(format!(".trash-{}-{name}", std::process::id()));
                fs::rename(child.path(), &trash)?;
                trash
            };
            if metadata.is_dir() {
                fs::remove_dir_all(&trash)?;
            } else {
                fs::remove_file(&trash)?;
            }
            if !is_trash {
                report.removed.push(name);
            }
        }
        Ok(report)
    }

    /// Runs collection itself when it owns the store, else through `rb-store`.
    pub fn collect_garbage_as_owner(&self) -> io::Result<Vec<String>> {
        let metadata = fs::metadata(&self.root)?;
        let is_owner = metadata.uid() == unsafe { libc::geteuid() };
        if is_owner || !self.helper().is_file() {
            return Ok(self.collect_garbage(GRACE)?.removed);
        }

        let stdout = self.run_helper("gc")?;
        Ok(stdout.lines().map(str::to_owned).collect())
    }

    fn read_roots(&self) -> io::Result<BTreeSet<String>> {
        let mut live = BTreeSet::new();
        let dirs = match fs::read_dir(self.roots_dir()) {
            Ok(dirs) => dirs,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(live),
            Err(error) => return Err(error),
        };

        for dir in dirs {
            let dir = dir?;
            let Some(uid) = dir
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let metadata = fs::symlink_metadata(dir.path())?;
            if !metadata.is_dir() || metadata.uid() != uid {
                continue;
            }

            for file in fs::read_dir(dir.path())? {
                let file = file?;
                let is_owner = file.file_name().to_str().is_some_and(is_valid_owner);
                let metadata = fs::symlink_metadata(file.path())?;
                if !is_owner || !metadata.is_file() || metadata.uid() != uid {
                    continue;
                }

                let mut contents = String::new();
                fs::File::open(file.path())?
                    .take(MAX_ROOT_BYTES)
                    .read_to_string(&mut contents)?;
                live.extend(
                    contents
                        .lines()
                        .filter(|line| line.starts_with("sha256-") && !line.contains('/'))
                        .map(str::to_owned),
                );
            }
        }
        Ok(live)
    }

    fn run_helper(&self, command: &str) -> io::Result<String> {
        let helper = self.helper();
        let output = Command::new(&helper)
            .arg(command)
            .env("ROOTBEER_ROOT", self.root.parent().unwrap_or(&self.root))
            .stderr(std::process::Stdio::inherit())
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "{} {command} failed",
                helper.display()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

fn is_valid_owner(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(store: &Store, name: &str) -> PathBuf {
        let source = tempfile::tempdir().unwrap();
        fs::write(source.path().join("bin"), name).unwrap();
        store.add_tree(name, "1", source.path()).unwrap().path
    }

    #[test]
    fn removes_only_what_no_root_lists() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path().join("store"));
        let kept = entry(&store, "kept");
        let dropped = entry(&store, "dropped");
        fs::create_dir(store.root().join(".tmp-stale")).unwrap();
        store
            .write_root("user", &BTreeSet::from([kept.clone()]))
            .unwrap();

        let report = store.collect_garbage(Duration::ZERO).unwrap();

        let name = dropped.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            report.removed,
            vec![".tmp-stale".to_owned(), name.to_owned()]
        );
        assert_eq!(report.kept, 1);
        assert!(kept.is_dir() && !dropped.exists());
        assert_eq!(fs::read_dir(store.root()).unwrap().count(), 1);
    }

    #[test]
    fn keeps_young_entries_and_ignores_foreign_roots() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path().join("store"));
        let young = entry(&store, "young");
        let unlisted = entry(&store, "unlisted");
        let foreign = store.roots_dir().join("0");
        fs::create_dir_all(&foreign).unwrap();
        let name = unlisted.file_name().unwrap().to_str().unwrap();
        fs::write(foreign.join("user"), format!("{name}\n")).unwrap();

        assert!(store.collect_garbage(GRACE).unwrap().removed.is_empty());
        assert!(young.is_dir());

        let is_root = unsafe { libc::getuid() } == 0;
        let removed = store.collect_garbage(Duration::ZERO).unwrap().removed;
        assert_eq!(removed.contains(&name.to_owned()), !is_root);
    }

    #[test]
    fn rewriting_a_root_replaces_it() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path().join("store"));
        let first = entry(&store, "first");
        store
            .write_root("user", &BTreeSet::from([first.clone()]))
            .unwrap();
        store.write_root("user", &BTreeSet::new()).unwrap();

        store.collect_garbage(Duration::ZERO).unwrap();
        assert!(!first.exists());
        assert!(store.write_root("../escape", &BTreeSet::new()).is_err());
    }
}
