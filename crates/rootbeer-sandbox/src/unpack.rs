use crate::build::{failure, read_only, remove};
use crate::{Error, Request, derivation, scan};
use flate2::read::GzDecoder;
use lzma_rust2::XzReader;
use rootbeer_drv::{Derivation, Format, Key, Unpack, fetch_path, output_path};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::path::{Component, Path};

/// Lays a fetched source out as the output and links each bin into `bin/`.
/// Nothing from the source runs, so there's no sandbox, and the output may
/// reference nothing in the store.
pub(crate) fn unpack(request: &mut Request, unpack: &Unpack) -> Result<BTreeSet<Key>, Error> {
    let key = request.key;
    if !matches!(derivation(request, &unpack.source)?, Derivation::Fetch(_)) {
        return Err(failure(
            key,
            &format!("unpacks {}, which isn't a fetch", unpack.source),
        ));
    }

    let source = fetch_path(&unpack.source);
    if !source.exists() {
        return Err(failure(
            key,
            &format!("needs {}, which is not realized", unpack.source),
        ));
    }

    let out = output_path(key, &unpack.name, &unpack.version, "out");
    remove(&out)?;

    let unpacked = lay_out(&source, &out, unpack)
        .map_err(|error| failure(key, &format!("can't unpack {}: {error}", source.display())))
        .and_then(|()| link(&out, &unpack.bins).map_err(|reason| failure(key, &reason)))
        .and_then(|()| {
            let log = &mut *request.log;
            scan::scan(key, &out, &BTreeSet::new(), &BTreeSet::new(), false, log)
        });

    // The failure is what matters, and a leftover is replaced on the next try.
    let references = unpacked.inspect_err(|_| {
        let _ = remove(&out);
    })?;
    read_only(&out)?;
    Ok(references)
}

// Private until sealed, since the archive's modes can let others write.
fn lay_out(source: &Path, out: &Path, unpack: &Unpack) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(out)?;

    let file = File::open(source)?;
    match (unpack.format, unpack.bins.values().next()) {
        (Format::TarGz | Format::TarXz, _) => tar(reader(file, unpack.format), out),
        (Format::Zip, _) => zip(file, out),
        (Format::Binary, Some(path)) => binary(file, &out.join(path)),
        (Format::Binary, None) => Ok(()),
    }
}

/// The executable files a fetched archive holds, as paths inside it, so a
/// caller can choose which to link before unpacking it.
pub fn executables(source: &Path, format: Format) -> io::Result<Vec<String>> {
    let file = File::open(source)?;
    let mut paths = Vec::new();
    match format {
        Format::Binary => {}
        Format::Zip => {
            let mut archive = zip::ZipArchive::new(file)?;
            for index in 0..archive.len() {
                let entry = archive.by_index(index)?;
                let mode = entry.unix_mode().unwrap_or(0);
                let is_executable = entry.is_file() && !entry.is_symlink() && mode & 0o111 != 0;
                if let (true, Some(path)) = (is_executable, entry.enclosed_name()) {
                    paths.extend(normal(&path));
                }
            }
        }
        Format::TarGz | Format::TarXz => {
            let mut archive = tar::Archive::new(reader(file, format));
            for entry in archive.entries()? {
                let entry = entry?;
                let header = entry.header();
                if header.entry_type().is_file() && header.mode()? & 0o111 != 0 {
                    paths.extend(normal(&entry.path()?));
                }
            }
        }
    }

    Ok(paths)
}

fn reader(file: File, format: Format) -> Box<dyn Read> {
    match format {
        Format::TarXz => Box::new(XzReader::new(file, true)),
        _ => Box::new(GzDecoder::new(file)),
    }
}

// Archives often prefix entries with `./`, which an unpack's bins never have.
fn normal(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::CurDir => {}
            _ => return None,
        }
    }

    (!parts.is_empty()).then(|| parts.join("/"))
}

fn tar(reader: impl Read, out: &Path) -> io::Result<()> {
    let mut archive = tar::Archive::new(reader);
    archive.set_mask(0o022);
    for entry in archive.entries()? {
        if !entry?.unpack_in(out)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "an entry leaves the output",
            ));
        }
    }

    Ok(())
}

fn zip(file: File, out: &Path) -> io::Result<()> {
    let mut archive = zip::ZipArchive::new(file)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let Some(relative) = entry.enclosed_name() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} leaves the output", entry.name()),
            ));
        };

        if entry.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is a symlink, which zips can't hold here", entry.name()),
            ));
        }

        let path = out.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&path)?;
            continue;
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mode = entry.unix_mode().unwrap_or(0o644);
        let mut file = File::create_new(&path)?;
        io::copy(&mut entry, &mut file)?;
        file.set_permissions(fs::Permissions::from_mode(mode & 0o755))?;
    }

    Ok(())
}

fn binary(mut file: File, path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut copy = File::create_new(path)?;
    io::copy(&mut file, &mut copy)?;
    copy.set_permissions(fs::Permissions::from_mode(0o755))
}

/// Each bin is a file inside the output, made executable, with a relative
/// link in `bin/` unless it's already there.
fn link(out: &Path, bins: &BTreeMap<String, String>) -> Result<(), String> {
    let root = fs::canonicalize(out).map_err(|error| format!("{}: {error}", out.display()))?;
    let directory = out.join("bin");
    for (bin, path) in bins {
        let target = fs::canonicalize(out.join(path))
            .ok()
            .filter(|target| target.starts_with(&root) && target.is_file())
            .ok_or_else(|| format!("has no file at {path} for {bin}"))?;

        let at = |error: io::Error| format!("{}: {error}", target.display());
        let mode = fs::metadata(&target).map_err(at)?.permissions().mode();
        fs::set_permissions(&target, fs::Permissions::from_mode(mode | 0o111)).map_err(at)?;

        if *path == format!("bin/{bin}") {
            continue;
        }

        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err("has a bin that isn't a directory".into()),
            Err(_) => fs::DirBuilder::new()
                .mode(0o755)
                .create(&directory)
                .map_err(|error| format!("{}: {error}", directory.display()))?,
        }

        let link = directory.join(bin);
        if fs::symlink_metadata(&link).is_ok() {
            return Err(format!("already has bin/{bin}, so it can't link {path}"));
        }

        symlink(format!("../{path}"), &link)
            .map_err(|error| format!("{}: {error}", link.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn executable(path: &Path) -> bool {
        fs::metadata(path).unwrap().permissions().mode() & 0o111 != 0
    }

    #[test]
    fn a_tar_unpacks_and_bins_link_into_bin() {
        let out = tempfile::tempdir().unwrap();
        let out = out.path().join("out");
        fs::create_dir(&out).unwrap();

        let mut builder = tar::Builder::new(Vec::new());
        for (path, mode) in [("rg-1/rg", 0o755), ("rg-1/doc/rg.1", 0o644)] {
            let mut header = tar::Header::new_gnu();
            header.set_size(3);
            header.set_mode(mode);
            header.set_cksum();
            builder.append_data(&mut header, path, &b"abc"[..]).unwrap();
        }
        tar(&builder.into_inner().unwrap()[..], &out).unwrap();

        let bins = BTreeMap::from([("rg".to_string(), "rg-1/rg".to_string())]);
        link(&out, &bins).unwrap();
        assert_eq!(
            fs::read_link(out.join("bin/rg")).unwrap(),
            Path::new("../rg-1/rg")
        );
        assert_eq!(fs::read(out.join("bin/rg")).unwrap(), b"abc");

        let docs = BTreeMap::from([("man".to_string(), "rg-1/doc/rg.1".to_string())]);
        link(&out, &docs).unwrap();
        assert!(executable(&out.join("rg-1/doc/rg.1")));

        let error = link(&out, &bins).unwrap_err();
        assert!(error.contains("already has bin/rg"), "{error}");

        let missing = BTreeMap::from([("fd".to_string(), "fd".to_string())]);
        assert_eq!(
            link(&out, &missing).unwrap_err(),
            "has no file at fd for fd"
        );
    }

    #[test]
    fn executables_are_listed_without_unpacking() {
        let directory = tempfile::tempdir().unwrap();
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for (path, mode) in [("./rg-1/rg", 0o755), ("./rg-1/README", 0o644)] {
            let mut header = tar::Header::new_gnu();
            header.set_size(1);
            header.set_mode(mode);
            header.set_cksum();
            builder.append_data(&mut header, path, &b"x"[..]).unwrap();
        }

        let source = directory.path().join("rg.tar.gz");
        fs::write(&source, builder.into_inner().unwrap().finish().unwrap()).unwrap();
        assert_eq!(executables(&source, Format::TarGz).unwrap(), ["rg-1/rg"]);
        assert!(executables(&source, Format::Binary).unwrap().is_empty());
    }

    #[test]
    fn a_bin_never_links_outside_the_output() {
        let out = tempfile::tempdir().unwrap();
        symlink("/bin/sh", out.path().join("sh")).unwrap();
        let bins = BTreeMap::from([("sh".to_string(), "sh".to_string())]);
        assert!(
            link(out.path(), &bins)
                .unwrap_err()
                .contains("no file at sh")
        );

        let out = tempfile::tempdir().unwrap();
        symlink("/usr/bin", out.path().join("bin")).unwrap();
        fs::write(out.path().join("tool"), b"x").unwrap();
        let bins = BTreeMap::from([("tool".to_string(), "tool".to_string())]);
        assert!(
            link(out.path(), &bins)
                .unwrap_err()
                .contains("isn't a directory")
        );
    }

    fn append(builder: &mut tar::Builder<Vec<u8>>, kind: tar::EntryType, path: &str, link: &str) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_size(0);
        header.set_mode(0o777);
        builder.append_link(&mut header, path, link).unwrap();
    }

    #[test]
    fn a_tar_never_writes_through_a_link_it_holds() {
        let outside = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        append(
            &mut builder,
            tar::EntryType::Symlink,
            "d",
            outside.path().to_str().unwrap(),
        );
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_mode(0o644);
        builder.append_data(&mut header, "d/x", &b"x"[..]).unwrap();

        assert!(tar(&builder.into_inner().unwrap()[..], out.path()).is_err());
        assert!(!outside.path().join("x").exists());

        let secret = outside.path().join("secret");
        fs::write(&secret, b"s").unwrap();
        let out = tempfile::tempdir().unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        append(
            &mut builder,
            tar::EntryType::Link,
            "copy",
            secret.to_str().unwrap(),
        );

        assert!(tar(&builder.into_inner().unwrap()[..], out.path()).is_err());
        assert!(!out.path().join("copy").exists());
    }

    #[test]
    fn nothing_unpacked_is_writable_by_others() {
        let out = tempfile::tempdir().unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_mode(0o777);
        builder
            .append_data(&mut header, "open/run", &b"x"[..])
            .unwrap();
        tar(&builder.into_inner().unwrap()[..], out.path()).unwrap();

        let mode = fs::metadata(out.path().join("open/run"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn a_tar_entry_that_leaves_the_output_fails() {
        let out = tempfile::tempdir().unwrap();
        let mut archive = Vec::new();
        let mut header = tar::Header::new_gnu();
        header.as_gnu_mut().unwrap().name[..9].copy_from_slice(b"../escape");
        header.set_size(1);
        header.set_mode(0o644);
        header.set_cksum();
        archive.extend_from_slice(header.as_bytes());
        archive.extend_from_slice(&[b'x'; 512]);
        archive.extend_from_slice(&[0; 1024]);

        assert!(tar(&archive[..], out.path()).is_err());
        assert!(!out.path().parent().unwrap().join("escape").exists());
    }

    #[test]
    fn a_zip_keeps_its_modes_and_refuses_symlinks() {
        let out = tempfile::tempdir().unwrap();
        let mut bytes = io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(&mut bytes);
        let options = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
        writer.start_file("tool/run", options).unwrap();
        writer.write_all(b"#!/bin/sh\n").unwrap();
        writer.finish().unwrap();

        let file = tempfile::tempfile().unwrap();
        (&file).write_all(bytes.get_ref()).unwrap();
        zip(file.try_clone().unwrap(), out.path()).unwrap();
        assert!(executable(&out.path().join("tool/run")));

        let mut bytes = io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(&mut bytes);
        writer
            .add_symlink("link", "/etc", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.finish().unwrap();

        let file = tempfile::tempfile().unwrap();
        (&file).write_all(bytes.get_ref()).unwrap();
        let out = tempfile::tempdir().unwrap();
        let error = zip(file, out.path()).unwrap_err();
        assert!(error.to_string().contains("is a symlink"), "{error}");
    }
}
