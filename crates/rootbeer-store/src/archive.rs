use crate::{Error, io_at};
use rustix::fs::XattrFlags;
use rustix::io::Errno;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
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

// Packing never writes a PAX header or long name anywhere near this long.
const LONGEST_HEADER: u64 = 16 << 20;

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

    // Raw entries hand over PAX headers and long names unread, since the tar
    // crate splits PAX records at newlines and a signature can hold one.
    let root = fs::canonicalize(directory).map_err(io_at(directory))?;
    let entries = archive.entries().map_err(io_at(directory))?.raw(true);
    let mut held = Held::default();
    for entry in entries {
        let mut entry = entry.map_err(io_at(directory))?;
        let kind = entry.header().entry_type();
        let slot = match kind {
            EntryType::XHeader => Some(&mut held.records),
            EntryType::GNULongName => Some(&mut held.name),
            EntryType::GNULongLink => Some(&mut held.target),
            _ => None,
        };

        if let Some(slot) = slot {
            hold(&mut entry, slot)?;
            continue;
        }

        let name = match held.name.take() {
            Some(name) => name.strip_suffix(&[0]).unwrap_or(&name).to_vec(),
            None => entry.header().path_bytes().into_owned(),
        };
        let relative = PathBuf::from(OsString::from_vec(name));

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
        let records = held.records.take().unwrap_or_default();
        let attributes = pax_attributes(&records, &relative)?;
        if !attributes.is_empty() && kind != EntryType::Regular {
            return Err(Error::Archive(format!(
                "{} has extended attributes but isn't a file",
                relative.display()
            )));
        }

        let target = held.target.take();
        if target.is_some() && kind != EntryType::Symlink {
            return Err(Error::Archive(format!(
                "{} has a link target but isn't a symlink",
                relative.display()
            )));
        }

        match kind {
            // Made writable until the end, so their contents can be unpacked.
            EntryType::Directory => fs::create_dir(&path).map_err(io_at(&path))?,
            EntryType::Symlink => {
                let target = match target {
                    Some(target) => target.strip_suffix(&[0]).unwrap_or(&target).to_vec(),
                    None => entry
                        .header()
                        .link_name_bytes()
                        .ok_or_else(|| {
                            Error::Archive(format!("{} links nowhere", relative.display()))
                        })?
                        .into_owned(),
                };
                symlink(OsString::from_vec(target), &path).map_err(io_at(&path))?;
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

    if held.records.is_some() || held.name.is_some() || held.target.is_some() {
        return Err(Error::Archive(
            "the archive ends with a header for no entry".into(),
        ));
    }

    Ok(())
}

/// What the header entries before an entry say about it.
#[derive(Default)]
struct Held {
    records: Option<Vec<u8>>,
    name: Option<Vec<u8>>,
    target: Option<Vec<u8>>,
}

/// Reads a header entry's data for the entry after it.
fn hold<R: Read>(entry: &mut tar::Entry<R>, slot: &mut Option<Vec<u8>>) -> Result<(), Error> {
    let kind = entry.header().entry_type();
    if slot.is_some() {
        return Err(Error::Archive(format!(
            "an archive has two {kind:?} entries for one entry"
        )));
    }

    if entry.size() > LONGEST_HEADER {
        return Err(Error::Archive(format!(
            "an archive has a {kind:?} entry of {} bytes",
            entry.size()
        )));
    }

    let mut data = Vec::new();
    entry
        .read_to_end(&mut data)
        .map_err(|error| Error::Archive(error.to_string()))?;
    *slot = Some(data);
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

/// The extended attributes a PAX header carries. Records are read by their
/// length, since a value can hold a newline. Any other record refuses the
/// archive, so no reader finds a size or path in it that this one skips.
pub(crate) fn pax_attributes(
    mut records: &[u8],
    relative: &Path,
) -> Result<Vec<(String, Vec<u8>)>, Error> {
    let malformed = || Error::Archive(format!("{} has a malformed PAX record", relative.display()));

    let mut attributes = Vec::new();
    while !records.is_empty() {
        let space = records
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or_else(malformed)?;
        let length = records
            .get(..space)
            .and_then(|digits| std::str::from_utf8(digits).ok())
            .filter(|digits| digits.bytes().all(|digit| digit.is_ascii_digit()))
            .and_then(|digits| digits.parse::<usize>().ok())
            .ok_or_else(malformed)?;
        let (record, rest) = records.split_at_checked(length).ok_or_else(malformed)?;
        records = rest;

        let body = record
            .get(space.saturating_add(1)..)
            .and_then(|body| body.strip_suffix(b"\n"))
            .ok_or_else(malformed)?;
        let (key, value) = body
            .iter()
            .position(|byte| *byte == b'=')
            .and_then(|equals| body.split_at_checked(equals))
            .ok_or_else(malformed)?;
        let value = value.get(1..).ok_or_else(malformed)?;
        let key = std::str::from_utf8(key).map_err(|_| malformed())?;

        let Some(name) = key
            .strip_prefix(PAX_ATTRIBUTE)
            .filter(|name| name.starts_with(KEPT))
        else {
            return Err(Error::Archive(format!(
                "{} has the PAX record {key}",
                relative.display()
            )));
        };

        attributes.push((name.to_string(), value.to_vec()));
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
