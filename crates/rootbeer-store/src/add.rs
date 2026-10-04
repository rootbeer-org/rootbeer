use crate::{Error, Origin, Store, io_at};
use data_encoding::HEXLOWER;
use rootbeer_drv::Key;
use rustix::fs::{
    AtFlags, Dir, FileType, Gid, Mode, OFlags, Stat, Uid, chownat, fchmod, fchown, fstat, fsync,
    openat, statat,
};
use rustix::io::Errno;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::{CStr, OsStr};
use std::fs;
use std::io::{self, Read};
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

// Nonblocking so that opening a FIFO returns, and it can then be refused.
const OPEN: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

impl Store {
    /// Seals an output that `builder`, a trusted user, built in place. It
    /// becomes root's, read-only, and synced, then is registered as local.
    pub fn seal(
        &mut self,
        key: &Key,
        entry: &str,
        builder: u32,
        references: &BTreeSet<Key>,
    ) -> Result<PathBuf, Error> {
        if let Some(path) = self.path(key)? {
            return Ok(path);
        }

        let path = self.entry(key, entry)?;
        let store_path = self.root.join("store");
        let store = rustix::fs::open(
            &store_path,
            OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(errno_at(&store_path))?;

        let output = openat(&store, entry, OPEN, Mode::empty()).map_err(|error| match error {
            Errno::LOOP => Error::Refused(format!("{entry} is a symlink")),
            other => errno_at(&path)(other),
        })?;

        let sealer = Sealer {
            builder,
            is_root: rustix::process::geteuid().is_root(),
        };

        let sealed = sealer.seal(output, &path)?;
        let current = statat(&store, entry, AtFlags::SYMLINK_NOFOLLOW).map_err(errno_at(&path))?;
        if (current.st_dev, current.st_ino) != (sealed.st_dev, sealed.st_ino) {
            return Err(Error::Refused(format!("{entry} moved while it was sealed")));
        }

        fsync(&store).map_err(errno_at(&store_path))?;
        self.register(key, entry, Origin::Local, None, references)?;
        Ok(path)
    }

    /// Unpacks a pulled archive for `key` and registers it with the sha256 of
    /// the archive as read, so the digest never comes from the caller.
    pub fn pull(
        &mut self,
        key: &Key,
        entry: &str,
        references: &BTreeSet<Key>,
        archive: impl Read,
    ) -> Result<PathBuf, Error> {
        if let Some(path) = self.path(key)? {
            return Ok(path);
        }

        // Anything already there was interrupted, since it isn't registered.
        let path = self.entry(key, entry)?;
        remove(&path)?;

        let mut hashing = Hashing {
            reader: archive,
            hasher: Sha256::new(),
        };

        self.ingest(entry, &mut hashing)?;
        io::copy(&mut hashing, &mut io::sink()).map_err(io_at(&path))?;

        let digest = format!("sha256:{}", HEXLOWER.encode(&hashing.hasher.finalize()));
        self.register(key, entry, Origin::Pulled, Some(&digest), references)?;
        Ok(path)
    }

    /// The path of `entry`, which must be named for `key`.
    fn entry(&self, key: &Key, entry: &str) -> Result<PathBuf, Error> {
        if !entry.starts_with(&format!("{key}-")) {
            return Err(Error::Refused(format!("{entry} isn't named for {key}")));
        }

        crate::validate(entry)?;
        Ok(self.root.join("store").join(entry))
    }
}

struct Sealer {
    builder: u32,
    is_root: bool,
}

impl Sealer {
    /// Seals `fd` and everything under it. A directory is root's and
    /// read-only before it is read, so its entries can no longer change.
    fn seal(&self, fd: OwnedFd, path: &Path) -> Result<Stat, Error> {
        let at = errno_at(path);
        let stat = fstat(&fd).map_err(&at)?;
        self.check(&stat, path)?;

        let kind = FileType::from_raw_mode(stat.st_mode);
        if !matches!(kind, FileType::RegularFile | FileType::Directory) {
            return Err(Error::Refused(format!(
                "{} isn't a file, directory, or symlink",
                path.display()
            )));
        }

        if stat.st_mode & 0o7000 != 0 {
            return Err(Error::Refused(format!(
                "{} is setuid, setgid, or sticky",
                path.display()
            )));
        }

        // Owned by root first, so the builder can't change the mode after.
        if self.is_root {
            fchown(&fd, Some(Uid::ROOT), Some(Gid::ROOT)).map_err(&at)?;
        }

        fchmod(&fd, Mode::from_raw_mode(stat.st_mode & 0o555)).map_err(&at)?;
        #[cfg(target_os = "macos")]
        clear_acl(&fd).map_err(io_at(path))?;
        fsync(&fd).map_err(&at)?;

        if kind == FileType::RegularFile {
            return Ok(stat);
        }

        let mut directory = Dir::new(fd).map_err(&at)?;
        while let Some(item) = directory.read() {
            let item = item.map_err(&at)?;
            let name = item.file_name();
            if name == c"." || name == c".." {
                continue;
            }

            let parent = directory.fd().map_err(&at)?;
            let child = path.join(OsStr::from_bytes(name.to_bytes()));
            self.seal_child(parent, name, &child)?;
        }

        Ok(stat)
    }

    fn seal_child(&self, parent: BorrowedFd, name: &CStr, path: &Path) -> Result<(), Error> {
        let at = errno_at(path);
        let stat = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(&at)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::Symlink {
            let fd = openat(parent, name, OPEN, Mode::empty()).map_err(&at)?;
            self.seal(fd, path)?;
            return Ok(());
        }

        // The parent is already sealed, so the name still holds this symlink.
        self.check(&stat, path)?;
        if self.is_root {
            let flags = AtFlags::SYMLINK_NOFOLLOW;
            chownat(parent, name, Some(Uid::ROOT), Some(Gid::ROOT), flags).map_err(&at)?;
        }

        Ok(())
    }

    fn check(&self, stat: &Stat, path: &Path) -> Result<(), Error> {
        if stat.st_uid != self.builder {
            return Err(Error::Refused(format!(
                "{} wasn't built by its caller",
                path.display()
            )));
        }

        Ok(())
    }
}

/// Drops the ACL, which macOS checks before the mode, so an entry the
/// builder added can't keep a sealed file writable.
#[cfg(target_os = "macos")]
fn clear_acl(fd: &OwnedFd) -> io::Result<()> {
    use std::ffi::{c_int, c_void};
    use std::os::fd::AsRawFd;

    const ACL_TYPE_EXTENDED: c_int = 0x100;
    unsafe extern "C" {
        fn acl_init(count: c_int) -> *mut c_void;
        fn acl_set_fd_np(fd: c_int, acl: *mut c_void, kind: c_int) -> c_int;
        fn acl_free(object: *mut c_void) -> c_int;
    }

    let acl = unsafe { acl_init(0) };
    if acl.is_null() {
        return Err(io::Error::last_os_error());
    }

    let result = unsafe { acl_set_fd_np(fd.as_raw_fd(), acl, ACL_TYPE_EXTENDED) };
    let error = io::Error::last_os_error();
    unsafe { acl_free(acl) };
    match result {
        0 => Ok(()),
        _ => Err(error),
    }
}

struct Hashing<R> {
    reader: R,
    hasher: Sha256,
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.reader.read(buffer)?;
        self.hasher.update(buffer.get(..count).unwrap_or_default());
        Ok(count)
    }
}

fn errno_at(path: &Path) -> impl Fn(Errno) -> Error {
    let at = io_at(path);
    move |error| at(error.into())
}

fn remove(path: &Path) -> Result<(), Error> {
    let result = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    };

    result.map_err(io_at(path))
}
