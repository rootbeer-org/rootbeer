use crate::apple_double;
use crate::tree::Tree;
use bzip2::read::BzDecoder;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};
use tar::EntryType;

// Attributes hold a signature or two, so anything near this is corrupt.
const LONGEST_APPLE_DOUBLE: u64 = 16 * 1024 * 1024;

pub(crate) fn zip(archive: &Path, tree: &Tree) -> Result<(), String> {
    let file = File::open(archive).map_err(|error| format!("{}: {error}", archive.display()))?;
    let mut archive =
        zip::ZipArchive::new(BufReader::new(file)).map_err(|error| error.to_string())?;

    let mut described = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
        let name = entry.name().to_string();
        if is_apple_double(&name) {
            // Only macOS can hold the attributes, and elsewhere the file is
            // dropped as before.
            if let (true, Some(file)) = (cfg!(target_os = "macos"), apple_double::described(&name))
            {
                let mut bytes = Vec::new();
                (&mut entry)
                    .take(LONGEST_APPLE_DOUBLE)
                    .read_to_end(&mut bytes)
                    .map_err(|error| format!("{name}: {error}"))?;
                let attributes =
                    apple_double::attributes(&bytes).map_err(|error| format!("{name}: {error}"))?;
                described.push((file, attributes));
            }

            continue;
        }

        let mode = entry
            .unix_mode()
            .unwrap_or(if entry.is_dir() { 0o755 } else { 0o644 });
        if entry.is_dir() {
            tree.directory(&name, mode)?;
        } else if entry.is_symlink() {
            let mut target = String::new();
            entry
                .read_to_string(&mut target)
                .map_err(|_| format!("{name} links to a target that isn't UTF-8"))?;
            tree.symlink(&name, &target)?;
        } else {
            tree.file(&name, mode, &mut entry)?;
        }
    }

    // Set once every file is written, since a zip can list a file's
    // attributes before the file.
    for (file, attributes) in described {
        tree.set_attributes(&file, &attributes)?;
    }

    Ok(())
}

/// A zip made on macOS keeps each file's extended attributes in an
/// AppleDouble file beside it or under `__MACOSX`. Inside an app bundle the
/// files break its seal, so they're read for their attributes and dropped.
fn is_apple_double(name: &str) -> bool {
    name.split('/').next() == Some("__MACOSX")
        || name
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .is_some_and(|file| file.starts_with("._"))
}

pub(crate) fn tar_bz2(archive: &Path, tree: &Tree) -> Result<(), String> {
    let file = File::open(archive).map_err(|error| format!("{}: {error}", archive.display()))?;
    let mut archive = tar::Archive::new(BzDecoder::new(BufReader::new(file)));

    for entry in archive.entries().map_err(|error| error.to_string())? {
        let mut entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path().map_err(|error| error.to_string())?;
        let path = path
            .to_str()
            .ok_or("an entry's path isn't UTF-8")?
            .to_string();
        let mode = entry.header().mode().map_err(|error| error.to_string())?;
        let link = || -> Result<String, String> {
            let link = entry.link_name().map_err(|error| error.to_string())?;
            let link = link.ok_or_else(|| format!("{path} links nowhere"))?;
            link.to_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{path} links to a target that isn't UTF-8"))
        };

        match entry.header().entry_type() {
            EntryType::Directory => tree.directory(&path, mode)?,
            EntryType::Symlink => tree.symlink(&path, &link()?)?,
            EntryType::Link => tree.hard_link(&path, &link()?)?,
            // Source builds compare mtimes, so make would try to rebuild
            // generated files that look older than their sources.
            EntryType::Regular | EntryType::Continuous => {
                let modified = entry.header().mtime().map_err(|error| error.to_string())?;
                let mut file = tree.create(&path, mode)?;
                io::copy(&mut entry, &mut file).map_err(|error| format!("{path}: {error}"))?;
                let modified = UNIX_EPOCH
                    .checked_add(Duration::from_secs(modified))
                    .ok_or_else(|| format!("{path} has mtime {modified}"))?;
                file.set_modified(modified)
                    .map_err(|error| format!("{path}: {error}"))?;
            }
            // Extended headers that carry metadata for the next entry.
            EntryType::XGlobalHeader | EntryType::XHeader => {}
            _ => {
                return Err(format!(
                    "{path} is a device or a pipe, which apps don't hold"
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_double_files_are_recognized_anywhere() {
        assert!(is_apple_double("__MACOSX/Tool.app/._Info.plist"));
        assert!(is_apple_double("Tool.app/Contents/._Info.plist"));
        assert!(is_apple_double("._Tool.app/"));
        assert!(!is_apple_double("Tool.app/Contents/Info.plist"));
        assert!(!is_apple_double("Tool.app/Contents/a._b"));
    }

    #[test]
    fn tar_entries_keep_their_mtimes() {
        let mut builder = tar::Builder::new(bzip2::write::BzEncoder::new(
            Vec::new(),
            bzip2::Compression::fast(),
        ));
        for (name, modified) in [("src/doc.texi", 1_000), ("src/doc.info", 2_000)] {
            let mut header = tar::Header::new_ustar();
            header.set_size(2);
            header.set_mode(0o644);
            header.set_mtime(modified);
            builder
                .append_data(&mut header, name, "hi".as_bytes())
                .unwrap();
        }

        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("source.tar.bz2");
        std::fs::write(&archive, builder.into_inner().unwrap().finish().unwrap()).unwrap();
        let tree = Tree::new(&directory.path().join("out")).unwrap();
        tar_bz2(&archive, &tree).unwrap();

        let modified = |name: &str| {
            std::fs::metadata(directory.path().join("out/src").join(name))
                .unwrap()
                .modified()
                .unwrap()
        };
        assert_eq!(
            modified("doc.texi"),
            UNIX_EPOCH + Duration::from_secs(1_000)
        );
        assert_eq!(
            modified("doc.info"),
            UNIX_EPOCH + Duration::from_secs(2_000)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_zips_code_signature_attributes_land_on_their_files() {
        use crate::apple_double::tests::apple_double;
        use zip::write::SimpleFileOptions;

        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("app.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        let signed = apple_double(&[
            ("com.apple.cs.CodeSignature", b"signed"),
            ("com.apple.quarantine", b"0081;"),
        ]);
        for (name, contents) in [
            ("__MACOSX/Tool.app/._tool.jar", signed.as_slice()),
            ("Tool.app/tool.jar", b"PK".as_slice()),
            ("__MACOSX/Tool.app/._missing.jar", signed.as_slice()),
        ] {
            writer.start_file(name, options).unwrap();
            std::io::Write::write_all(&mut writer, contents).unwrap();
        }
        writer.finish().unwrap();

        let out = directory.path().join("out");
        zip(&archive, &Tree::new(&out).unwrap()).unwrap();

        let jar = out.join("Tool.app/tool.jar");
        let mut value = [0u8; 16];
        let size =
            rustix::fs::getxattr(&jar, "com.apple.cs.CodeSignature", &mut value[..]).unwrap();
        assert_eq!(value.get(..size), Some(&b"signed"[..]));
        assert!(rustix::fs::getxattr(&jar, "com.apple.quarantine", &mut value[..]).is_err());
        assert!(!out.join("__MACOSX").exists());
    }
}
