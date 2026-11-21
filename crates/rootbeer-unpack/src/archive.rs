use crate::tree::Tree;
use bzip2::read::BzDecoder;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use tar::EntryType;

pub(crate) fn zip(archive: &Path, tree: &Tree) -> Result<(), String> {
    let file = File::open(archive).map_err(|error| format!("{}: {error}", archive.display()))?;
    let mut archive =
        zip::ZipArchive::new(BufReader::new(file)).map_err(|error| error.to_string())?;

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
        let name = entry.name().to_string();
        if is_apple_double(&name) {
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

    Ok(())
}

/// A zip made on macOS keeps each file's extended attributes in an
/// AppleDouble file beside it or under `__MACOSX`. The store can't hold
/// extended attributes, and inside an app bundle the files break its seal.
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

    #[test]
    fn apple_double_files_are_recognized_anywhere() {
        assert!(is_apple_double("__MACOSX/Tool.app/._Info.plist"));
        assert!(is_apple_double("Tool.app/Contents/._Info.plist"));
        assert!(is_apple_double("._Tool.app/"));
        assert!(!is_apple_double("Tool.app/Contents/Info.plist"));
        assert!(!is_apple_double("Tool.app/Contents/a._b"));
    }
}
