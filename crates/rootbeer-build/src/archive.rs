use flate2::{write::GzEncoder, Compression};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub fn pack(root: &Path, output: &Path) -> io::Result<()> {
    fn paths(root: &Path, directory: &Path, entries: &mut Vec<PathBuf>) -> io::Result<()> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if directory == root && entry.file_name() == ".rootbeer" {
                continue;
            }
            entries.push(entry.path().strip_prefix(root).unwrap().to_path_buf());
            if entry.file_type()?.is_dir() {
                paths(root, &entry.path(), entries)?;
            }
        }
        Ok(())
    }
    let mut entries = Vec::new();
    paths(root, root, &mut entries)?;
    entries.sort();
    let mut archive = tar::Builder::new(GzEncoder::new(
        fs::File::create(output)?,
        Compression::default(),
    ));
    archive.mode(tar::HeaderMode::Deterministic);
    archive.follow_symlinks(false);
    for path in entries {
        let source = root.join(&path);
        let metadata = fs::symlink_metadata(&source)?;
        if !metadata.file_type().is_symlink() {
            archive.append_path_with_name(source, &path)?;
            continue;
        }

        // Symlink spelling is part of the output hash and upstream bundle signatures.
        let target = fs::read_link(source)?;
        let bytes = target.as_os_str().as_encoded_bytes();
        let mut header = tar::Header::new_gnu();
        header.set_metadata_in_mode(&metadata, tar::HeaderMode::Deterministic);
        header.set_size(0);
        if header.set_link_name_literal(bytes).is_err() {
            archive.append_pax_extensions([("linkpath", bytes)])?;
        }
        archive.append_data(&mut header, &path, io::empty())?;
    }
    archive.into_inner()?.finish()?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn archives_preserve_literal_short_and_long_symlink_targets() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("resource"), "preserved").unwrap();
        for (name, target) in [
            ("short", "././resource".to_string()),
            ("long", format!("{}resource", "./".repeat(70))),
        ] {
            symlink(target, source.join(name)).unwrap();
        }
        let archive = root.path().join("package.tar.gz");
        pack(&source, &archive).unwrap();
        let installed = root.path().join("installed");
        fs::create_dir(&installed).unwrap();
        rootbeer_package::realize::extract_archive(
            &archive,
            rootbeer_package::ArchiveFormat::TarGz,
            &installed,
        )
        .unwrap();
        for name in ["short", "long"] {
            assert_eq!(
                fs::read_link(source.join(name))
                    .unwrap()
                    .as_os_str()
                    .as_encoded_bytes(),
                fs::read_link(installed.join(name))
                    .unwrap()
                    .as_os_str()
                    .as_encoded_bytes()
            );
        }
        assert_eq!(
            rootbeer_store_legacy::hash_tree(&source).unwrap(),
            rootbeer_store_legacy::hash_tree(&installed).unwrap()
        );
    }
}
