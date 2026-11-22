use crate::{Error, io_at};
use rustix::fs::XattrFlags;
use rustix::io::Errno;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};
use std::time::{Duration, UNIX_EPOCH};
use tar::{Builder, EntryType, Header};
use walkdir::WalkDir;

// Maximizing compression size since network is the bottleneck
const LEVEL: i32 = 19;

// A signed file with nowhere to hold its signature, like a jar in an app
// bundle, keeps it in these extended attributes. No others are kept, since
// the system adds its own, like quarantine and provenance.
const KEPT: &str = "com.apple.cs.";
const PAX_ATTRIBUTE: &str = "SCHILY.xattr.";

/// Writes a directory into a deterministic zstd archive
pub fn pack(directory: &Path, writer: impl Write) -> Result<(), Error> {
    let encoder = zstd::Encoder::new(writer, LEVEL).map_err(io_at(directory))?;
    let mut builder = Builder::new(encoder);

    for entry in WalkDir::new(directory).min_depth(1).sort_by_file_name() {
        let entry = entry.map_err(|error| io_at(directory)(error.into()))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(directory)
            .map_err(|error| Error::Archive(error.to_string()))?;

        let mut header = Header::new_gnu();
        header.set_mode(0o555);
        header.set_mtime(1);
        header.set_size(0);

        let file_type = entry.file_type();
        if file_type.is_dir() {
            header.set_entry_type(EntryType::Directory);
            builder
                .append_data(&mut header, relative, io::empty())
                .map_err(io_at(path))?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(path).map_err(io_at(path))?;
            header.set_entry_type(EntryType::Symlink);
            builder
                .append_link(&mut header, relative, target)
                .map_err(io_at(path))?;
        } else if file_type.is_file() {
            let file = fs::File::open(path).map_err(io_at(path))?;
            let metadata = file.metadata().map_err(io_at(path))?;
            let attributes = attributes(&file, path)?
                .into_iter()
                .map(|(name, value)| (format!("{PAX_ATTRIBUTE}{name}"), value))
                .collect::<Vec<_>>();
            builder
                .append_pax_extensions(
                    attributes
                        .iter()
                        .map(|(key, value)| (key.as_str(), value.as_slice())),
                )
                .map_err(io_at(path))?;

            header.set_entry_type(EntryType::Regular);
            header.set_mode(metadata.permissions().mode() & 0o555);
            header.set_size(metadata.len());
            builder
                .append_data(&mut header, relative, file)
                .map_err(io_at(path))?;
        } else {
            return Err(Error::Archive(format!(
                "{} is not a file, directory, or symlink",
                path.display()
            )));
        }
    }

    let encoder = builder.into_inner().map_err(io_at(directory))?;
    encoder.finish().map_err(io_at(directory))?;
    Ok(())
}

pub(crate) fn unpack(reader: impl Read, directory: &Path) -> Result<(), Error> {
    let decoder = zstd::Decoder::new(reader).map_err(io_at(directory))?;
    let mut archive = tar::Archive::new(decoder);
    archive.set_overwrite(false);

    let root = fs::canonicalize(directory).map_err(io_at(directory))?;
    let entries = archive.entries().map_err(io_at(directory))?;
    for entry in entries {
        let mut entry = entry.map_err(io_at(directory))?;
        let relative = entry.path().map_err(io_at(directory))?.into_owned();

        let is_plain = relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)));

        if !is_plain {
            return Err(Error::Archive(format!(
                "{} leaves the archive",
                relative.display()
            )));
        }

        // An earlier symlink entry must not redirect a later one outside.
        let path = root.join(&relative);
        let parent = path.parent().unwrap_or(&root);
        if fs::canonicalize(parent).map_err(io_at(parent))? != parent {
            return Err(Error::Archive(format!(
                "{} is inside a symlink",
                relative.display()
            )));
        }

        // A write or setuid bit on a file would outlive the seal.
        let mode = entry.header().mode().map_err(io_at(&path))?;
        if mode & !0o555 != 0 {
            return Err(Error::Archive(format!(
                "{} has mode {mode:o}",
                relative.display()
            )));
        }

        // Packing keeps extended attributes only on files.
        let kind = entry.header().entry_type();
        let attributes = pax_attributes(&mut entry, &relative.display().to_string())?;
        if !attributes.is_empty() && kind != EntryType::Regular {
            return Err(Error::Archive(format!(
                "{} has extended attributes but isn't a file",
                relative.display()
            )));
        }

        match kind {
            // Made writable until the end, so their contents can be unpacked.
            EntryType::Directory => fs::create_dir(&path).map_err(io_at(&path))?,
            EntryType::Symlink => {
                entry.unpack(&path).map_err(io_at(&path))?;
            }
            // Writable until sealed too, since only the owner of a writable
            // file can set its extended attributes.
            EntryType::Regular => {
                let modified = entry.header().mtime().map_err(io_at(&path))?;
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(mode | 0o200)
                    .open(&path)
                    .map_err(io_at(&path))?;
                io::copy(&mut entry, &mut file).map_err(io_at(&path))?;

                for (name, value) in attributes {
                    rustix::fs::fsetxattr(&file, name.as_str(), &value, XattrFlags::empty())
                        .map_err(|error| io_at(&path)(error.into()))?;
                }

                let modified = UNIX_EPOCH
                    .checked_add(Duration::from_secs(modified))
                    .ok_or_else(|| {
                        Error::Archive(format!("{} has mtime {modified}", relative.display()))
                    })?;
                file.set_modified(modified).map_err(io_at(&path))?;
            }
            other => {
                return Err(Error::Archive(format!(
                    "{} is a {other:?} entry",
                    relative.display()
                )));
            }
        }
    }

    Ok(())
}

/// The extended attributes an archive keeps for `file`, sorted so packing
/// stays deterministic.
fn attributes(file: &fs::File, path: &Path) -> Result<Vec<(String, Vec<u8>)>, Error> {
    let at = |error: Errno| io_at(path)(error.into());
    let names = match sized(|buffer| rustix::fs::flistxattr(file, buffer)) {
        // Linux gives the two the same number, so they can't both be patterns.
        Err(error) if [Errno::NOTSUP, Errno::OPNOTSUPP].contains(&error) => {
            return Ok(Vec::new());
        }
        names => names.map_err(at)?,
    };

    let mut kept = Vec::new();
    for name in names.split(|byte| *byte == 0) {
        let Some(name) = std::str::from_utf8(name)
            .ok()
            .filter(|name| name.starts_with(KEPT))
        else {
            continue;
        };

        let value = sized(|buffer| rustix::fs::fgetxattr(file, name, buffer)).map_err(at)?;
        kept.push((name.to_string(), value));
    }

    kept.sort();
    Ok(kept)
}

/// Reads a value whose size is asked for first, as the xattr calls report.
fn sized(mut call: impl FnMut(&mut [u8]) -> Result<usize, Errno>) -> Result<Vec<u8>, Errno> {
    let size = call(&mut [])?;
    let mut buffer = vec![0; size];
    let size = call(&mut buffer)?;
    buffer.truncate(size);
    Ok(buffer)
}

/// The extended attributes an entry's PAX records carry. Any the archive
/// wouldn't have kept refuses it.
fn pax_attributes<R: Read>(
    entry: &mut tar::Entry<R>,
    relative: &str,
) -> Result<Vec<(String, Vec<u8>)>, Error> {
    let Some(records) = entry
        .pax_extensions()
        .map_err(|error| Error::Archive(error.to_string()))?
    else {
        return Ok(Vec::new());
    };

    let mut attributes = Vec::new();
    for record in records {
        let record = record.map_err(|error| Error::Archive(error.to_string()))?;
        let Some(name) = record
            .key()
            .ok()
            .and_then(|key| key.strip_prefix(PAX_ATTRIBUTE))
        else {
            continue;
        };

        if !name.starts_with(KEPT) {
            return Err(Error::Archive(format!(
                "{relative} has the extended attribute {name}"
            )));
        }

        attributes.push((name.to_string(), record.value_bytes().to_vec()));
    }

    Ok(attributes)
}

pub(crate) fn seal(root: &Path) -> Result<(), Error> {
    for entry in WalkDir::new(root).contents_first(true) {
        let entry = entry.map_err(|error| io_at(root)(error.into()))?;
        let path = entry.path();
        let file_type = entry.file_type();
        if file_type.is_symlink() {
            continue;
        }

        // The same modes a sealed build gets, whatever the archive recorded.
        let metadata = entry
            .metadata()
            .map_err(|error| io_at(path)(error.into()))?;
        let is_executable = metadata.permissions().mode() & 0o111 != 0;
        let mode = match file_type.is_dir() || is_executable {
            true => 0o555,
            false => 0o444,
        };

        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(io_at(path))?;
        fs::File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(io_at(path))?;
    }

    Ok(())
}
