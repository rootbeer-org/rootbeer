use crate::apple_double;
use crate::tree::Tree;
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use lzma_rust2::XzReader;
use std::fs::File;
use std::io::{BufReader, Read};
use tar::EntryType;

// Attributes hold a signature or two, so anything near this is corrupt.
const LONGEST_APPLE_DOUBLE: u64 = 16 * 1024 * 1024;

pub(crate) fn zip(archive: File, tree: &mut Tree) -> Result<(), String> {
    let mut archive =
        zip::ZipArchive::new(BufReader::new(archive)).map_err(|error| error.to_string())?;

    let mut described = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
        let name = entry
            .name()
            .map_err(|error| error.to_string())?
            .into_owned();
        if is_apple_double(&name) {
            if let Some(file) = apple_double::described(&name) {
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
            tree.directory(&name)?;
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

/// How a tar archive is compressed.
#[derive(Clone, Copy)]
pub(crate) enum Compression {
    Gzip,
    Xz,
    Bzip2,
}

pub(crate) fn tar(archive: File, compression: Compression, tree: &mut Tree) -> Result<(), String> {
    let file = BufReader::new(archive);
    let reader: Box<dyn Read> = match compression {
        Compression::Gzip => Box::new(GzDecoder::new(file)),
        Compression::Xz => Box::new(XzReader::new(file, true)),
        Compression::Bzip2 => Box::new(BzDecoder::new(file)),
    };
    let mut archive = tar::Archive::new(reader);

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
            EntryType::Directory => tree.directory(&path)?,
            EntryType::Symlink => tree.symlink(&path, &link()?)?,
            EntryType::Link => tree.hard_link(&path, &link()?)?,
            EntryType::Regular | EntryType::Continuous => {
                tree.file(&path, mode, &mut entry)?;
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
    use crate::tree::tests::entries;
    use rootbeer_tree::Kind;

    #[test]
    fn apple_double_files_are_recognized_anywhere() {
        assert!(is_apple_double("__MACOSX/Tool.app/._Info.plist"));
        assert!(is_apple_double("Tool.app/Contents/._Info.plist"));
        assert!(is_apple_double("._Tool.app/"));
        assert!(!is_apple_double("Tool.app/Contents/Info.plist"));
        assert!(!is_apple_double("Tool.app/Contents/a._b"));
    }

    #[test]
    fn a_tar_unpacks_however_it_is_compressed() {
        let mut builder = tar::Builder::new(Vec::new());
        for name in ["src/doc.texi", "src/doc.info"] {
            let mut header = tar::Header::new_ustar();
            header.set_size(2);
            header.set_mode(0o644);
            builder
                .append_data(&mut header, name, "hi".as_bytes())
                .unwrap();
        }

        let plain = builder.into_inner().unwrap();
        let compress = |compression| -> Vec<u8> {
            use std::io::Write;
            match compression {
                Compression::Bzip2 => {
                    let mut encoder =
                        bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
                    encoder.write_all(&plain).unwrap();
                    encoder.finish().unwrap()
                }
                Compression::Gzip => {
                    let mut encoder =
                        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                    encoder.write_all(&plain).unwrap();
                    encoder.finish().unwrap()
                }
                Compression::Xz => {
                    let options = lzma_rust2::XzOptions::with_preset(0);
                    let mut encoder = lzma_rust2::XzWriter::new(Vec::new(), options).unwrap();
                    encoder.write_all(&plain).unwrap();
                    encoder.finish().unwrap()
                }
            }
        };

        let doc = Kind::File {
            is_executable: false,
            attributes: Vec::new(),
            size: 2,
        };
        for compression in [Compression::Bzip2, Compression::Gzip, Compression::Xz] {
            let mut archive = tempfile::tempfile().unwrap();
            std::io::Write::write_all(&mut archive, &compress(compression)).unwrap();
            std::io::Seek::rewind(&mut archive).unwrap();
            let mut tree = Tree::new().unwrap();
            tar(archive, compression, &mut tree).unwrap();
            assert_eq!(
                entries(&tree),
                [
                    ("src".to_string(), Kind::Directory),
                    ("src/doc.info".to_string(), doc.clone()),
                    ("src/doc.texi".to_string(), doc.clone()),
                ]
            );
        }
    }

    #[test]
    fn a_zips_code_signature_attributes_land_on_their_files() {
        use crate::apple_double::tests::apple_double;
        use zip::write::SimpleFileOptions;

        let archive = tempfile::tempfile().unwrap();
        let mut writer = zip::ZipWriter::new(archive.try_clone().unwrap());
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

        let mut tree = Tree::new().unwrap();
        zip(archive, &mut tree).unwrap();
        let jar = Kind::File {
            is_executable: false,
            attributes: vec![("com.apple.cs.CodeSignature".to_string(), b"signed".to_vec())],
            size: 2,
        };
        assert_eq!(
            entries(&tree),
            [
                ("Tool.app".to_string(), Kind::Directory),
                ("Tool.app/tool.jar".to_string(), jar),
            ]
        );
    }
}
