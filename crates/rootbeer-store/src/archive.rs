use crate::{Error, io_at};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};
use tar::{Builder, EntryType, Header};
use walkdir::WalkDir;

// Maximizing compression size since network is the bottleneck
const LEVEL: i32 = 19;

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

        match entry.header().entry_type() {
            // Made writable until the end, so their contents can be unpacked.
            EntryType::Directory => fs::create_dir(&path).map_err(io_at(&path))?,
            EntryType::Regular | EntryType::Symlink => {
                entry.unpack(&path).map_err(io_at(&path))?;
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

pub(crate) fn seal(root: &Path) -> Result<(), Error> {
    for entry in WalkDir::new(root).contents_first(true) {
        let entry = entry.map_err(|error| io_at(root)(error.into()))?;
        let path = entry.path();
        let file_type = entry.file_type();
        if file_type.is_symlink() {
            continue;
        }

        if file_type.is_dir() {
            let permissions = fs::Permissions::from_mode(0o555);
            fs::set_permissions(path, permissions).map_err(io_at(path))?;
        }

        fs::File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(io_at(path))?;
    }

    Ok(())
}
