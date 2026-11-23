use crate::build::{failure, read_only, remove};
use crate::extract::{self, fetched};
use crate::{Error, Request, scan};
use rootbeer_drv::{Format, Key, Unpack, output_path};
use rootbeer_tree::{Kind, Reader};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::path::Path;

/// Lays a fetched archive or binary out as the output and links each bin
/// into `bin/`. Nothing from the source runs, and the output may reference
/// nothing in the store.
pub(crate) fn unpack(request: &mut Request, unpack: &Unpack) -> Result<BTreeSet<Key>, Error> {
    let key = request.key;
    let source = fetched(request, &unpack.source)?;
    let out = output_path(key, &unpack.name, &unpack.version, "out");
    remove(&out)?;

    let laid = match (unpack.format, unpack.bins.values().next()) {
        (Format::Binary, Some(path)) => binary(&source, &out, path)
            .map_err(|error| failure(key, &format!("can't copy {}: {error}", source.display()))),
        (Format::Binary, None) => Err(failure(key, "names no bin to copy")),
        (format, _) => extract::lay_out(&source, format, &out)
            .map(drop)
            .map_err(|error| failure(key, &error.to_string())),
    };

    let unpacked = laid
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

/// The executable files a fetched archive holds, as paths inside it, so a
/// caller can choose which to link before unpacking it.
pub fn executables(source: &Path, format: Format) -> Result<Vec<String>, Error> {
    if format == Format::Binary {
        return Ok(Vec::new());
    }

    extract::unpack(source, format, |stream| {
        let mut reader = Reader::new(stream)?;
        let mut paths = Vec::new();
        while let Some(entry) = reader.entry()? {
            let is_executable = matches!(
                entry.kind,
                Kind::File {
                    is_executable: true,
                    ..
                }
            );
            if let (true, Ok(path)) = (is_executable, String::from_utf8(entry.path)) {
                paths.push(path);
            }
        }

        reader.finish().map(|_| paths)
    })
}

fn binary(source: &Path, out: &Path, path: &str) -> io::Result<()> {
    let path = out.join(path);
    fs::DirBuilder::new().mode(0o755).create(out)?;
    if let Some(parent) = path.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(parent)?;
    }

    fs::copy(source, &path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
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

    #[test]
    fn bins_link_into_bin_unless_already_there() {
        let out = tempfile::tempdir().unwrap();
        let out = out.path();
        fs::create_dir_all(out.join("rg-1/doc")).unwrap();
        fs::write(out.join("rg-1/rg"), "abc").unwrap();
        fs::write(out.join("rg-1/doc/rg.1"), "abc").unwrap();

        let bins = BTreeMap::from([("rg".to_string(), "rg-1/rg".to_string())]);
        link(out, &bins).unwrap();
        assert_eq!(
            fs::read_link(out.join("bin/rg")).unwrap(),
            Path::new("../rg-1/rg")
        );
        assert_eq!(fs::read(out.join("bin/rg")).unwrap(), b"abc");

        let docs = BTreeMap::from([("man".to_string(), "rg-1/doc/rg.1".to_string())]);
        link(out, &docs).unwrap();
        let mode = fs::metadata(out.join("rg-1/doc/rg.1"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0);

        let error = link(out, &bins).unwrap_err();
        assert!(error.contains("already has bin/rg"), "{error}");

        let missing = BTreeMap::from([("fd".to_string(), "fd".to_string())]);
        assert_eq!(link(out, &missing).unwrap_err(), "has no file at fd for fd");
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
}
