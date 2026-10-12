use crate::add::remove;
use crate::{Error, Store, db, io_at};
use rootbeer_drv::Key;
use rustix::fs::FlockOperation;
use rustix::io::Errno;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

const LOCK: &str = "var/gc.lock";

impl Store {
    /// Prevents garbage collection from running while the lock is held so that
    /// installs or builds don't get pruned in the middle of their work.
    pub fn hold(&self) -> Result<fs::File, Error> {
        let lock = self.gc_lock()?;
        rustix::fs::flock(&lock, FlockOperation::LockShared)
            .map_err(|error| io_at(&self.root.join(LOCK))(error.into()))?;

        Ok(lock)
    }

    pub fn collect(
        &mut self,
        roots: impl FnOnce() -> Result<BTreeSet<Key>, String>,
        waiting: impl FnOnce(),
    ) -> Result<Vec<String>, Error> {
        let path = self.root.join(LOCK);
        let gc = self.gc_lock()?;

        match rustix::fs::flock(&gc, FlockOperation::NonBlockingLockExclusive) {
            Err(Errno::WOULDBLOCK) => {
                waiting();
                rustix::fs::flock(&gc, FlockOperation::LockExclusive)
                    .map_err(|error| io_at(&path)(error.into()))?;
            }
            result => result.map_err(|error| io_at(&path)(error.into()))?,
        }

        let _lock = self.exclusive()?;
        let roots = roots().map_err(Error::Refused)?;
        let live = self.live(&roots)?;

        let mut outputs = db::outputs(&self.connection)?;
        outputs.retain(|key, _| !live.contains(key));

        db::unregister(&mut self.connection, outputs.keys())?;
        let registered = db::outputs(&self.connection)?
            .into_values()
            .collect::<BTreeSet<_>>();

        let owner = rustix::process::geteuid().as_raw();
        let store = self.root.join("store");
        let mut removed = Vec::new();

        for item in fs::read_dir(&store).map_err(io_at(&store))? {
            let item = item.map_err(io_at(&store))?;
            let path = item.path();

            let metadata = item.metadata().map_err(io_at(&path))?;
            let Some(entry) = item.file_name().to_str().map(str::to_string) else {
                continue;
            };

            let is_garbage = !item.file_name().as_bytes().starts_with(b".")
                && !registered.contains(&entry)
                && metadata.uid() == owner;

            if !is_garbage {
                continue;
            }

            remove(&path)?;
            removed.push(entry);
        }

        fs::File::open(&store)
            .and_then(|directory| directory.sync_all())
            .map_err(io_at(&store))?;

        Ok(removed)
    }

    fn gc_lock(&self) -> Result<fs::File, Error> {
        let path = self.root.join(LOCK);
        match fs::File::open(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            opened => return opened.map_err(io_at(&path)),
        }

        let created = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o644)
            .open(&path)
            .and_then(|lock| {
                lock.set_permissions(fs::Permissions::from_mode(0o644))?;
                Ok(lock)
            });

        created.map_err(|error| match error.kind() {
            io::ErrorKind::PermissionDenied => Error::Refused(format!(
                "{} is missing, so run `rb-helper setup` again",
                path.display()
            )),
            _ => io_at(&path)(error),
        })
    }

    fn live(&self, roots: &BTreeSet<Key>) -> Result<BTreeSet<Key>, Error> {
        let mut live = BTreeSet::new();
        let mut pending = roots.iter().cloned().collect::<Vec<_>>();

        while let Some(key) = pending.pop() {
            if !live.insert(key.clone()) {
                continue;
            }

            pending.extend(self.references(&key)?);
        }

        Ok(live)
    }
}
