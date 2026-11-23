use crate::Error;
use crate::read::{Kind, Reader};
use rootbeer_drv::Sha256;
use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, XattrFlags};
use std::ffi::OsStr;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};

/// Writes the tree `stream` encodes to `directory`, which must be missing or
/// empty, and returns its digest. Nothing lands outside `directory` or through a
/// symlink, since an entry's parent is always a directory written before it.
/// Files stay writable by their owner so a store can seal them, and every
/// time is 1, since the digest records none.
///
/// A stream that fails partway leaves what it wrote for the caller to remove.
pub fn materialize(stream: impl Read, directory: &Path) -> Result<Sha256, Error> {
    let mut reader = Reader::new(stream)?;
    match DirBuilder::new().mode(0o755).create(directory) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let mut entries = fs::read_dir(directory).map_err(at(directory))?;
            if entries.next().is_some() {
                return Err(at(directory)(error));
            }
        }
        created => created.map_err(at(directory))?,
    }

    let mut dated = vec![directory.to_path_buf()];
    while let Some(entry) = reader.entry()? {
        let path = directory.join(OsStr::from_bytes(&entry.path));
        match entry.kind {
            Kind::Directory => {
                DirBuilder::new()
                    .mode(0o755)
                    .create(&path)
                    .map_err(at(&path))?;
            }
            Kind::Symlink { target } => {
                symlink(OsStr::from_bytes(&target), &path).map_err(at(&path))?;
            }
            Kind::File {
                is_executable,
                attributes,
                ..
            } => {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(if is_executable { 0o755 } else { 0o644 })
                    .open(&path)
                    .map_err(at(&path))?;
                io::copy(&mut reader.contents(), &mut file).map_err(at(&path))?;

                for (name, value) in &attributes {
                    rustix::fs::fsetxattr(&file, name.as_str(), value, XattrFlags::empty())
                        .map_err(|error| at(&path)(error.into()))?;
                }
            }
        }

        dated.push(path);
    }

    let digest = reader.finish()?;

    // Last first, so a directory's time is set after everything inside it.
    for path in dated.iter().rev() {
        date(path)?;
    }

    Ok(digest)
}

fn date(path: &Path) -> Result<(), Error> {
    let time = Timespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    let times = Timestamps {
        last_access: time,
        last_modification: time,
    };

    rustix::fs::utimensat(CWD, path, &times, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| at(path)(error.into()))
}

fn at(path: &Path) -> impl Fn(io::Error) -> Error {
    let path: PathBuf = path.to_path_buf();
    move |source| Error::Io {
        path: path.clone(),
        source,
    }
}
