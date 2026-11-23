use crate::tree::Tree;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};

// Bookkeeping the filesystem keeps at the root of a volume, which no app
// ships.
const METADATA: [&str; 5] = [
    ".fseventsd",
    ".Trashes",
    ".journal",
    ".journal_info_block",
    ".HFS+ Private Directory Data\r",
];

// HFS+ keeps the one copy of a hard-linked file here, under a name with four
// leading NULs, and leaves a pointer where each link was.
const HARD_LINKS: &str = "\0\0\0\0HFS+ Private Data";

#[derive(PartialEq)]
enum Kind {
    File,
    Directory,
    Symlink,
}

trait Volume {
    fn entries(&mut self) -> Result<Vec<(String, Kind)>, String>;
    fn mode_and_size(&mut self, path: &str) -> Result<(u32, u64), String>;
    fn write(&mut self, path: &str, to: &mut dyn Write) -> Result<u64, String>;
}

/// Whether `archive` is a disk image, which ends in a koly trailer. It's left
/// rewound.
pub(crate) fn is_image(mut archive: &File) -> Result<bool, String> {
    let at = |error: std::io::Error| format!("the archive: {error}");
    let is_long_enough = archive.seek(SeekFrom::End(0)).map_err(at)? >= 512;
    let mut magic = [0u8; 4];
    if is_long_enough {
        archive
            .seek(SeekFrom::End(-512))
            .and_then(|_| archive.read_exact(&mut magic))
            .map_err(at)?;
    }

    archive.rewind().map_err(at)?;
    Ok(&magic == b"koly")
}

#[derive(Clone, Copy)]
enum Filesystem {
    Apfs,
    Hfs,
}

/// Unpacks the filesystem in a disk image, HFS+ or APFS, with the volume's
/// contents at the top of `tree`.
pub(crate) fn unpack(archive: File, tree: &mut Tree) -> Result<(), String> {
    let mut image =
        udif::DmgReader::new(BufReader::new(archive)).map_err(|error| error.to_string())?;

    // The tool that made the image names its partitions, and some name the
    // filesystem only "disk image", so each one is checked for a volume.
    let partitions = image
        .partitions()
        .iter()
        .map(|partition| partition.id)
        .collect::<Vec<_>>();
    let mut found = Vec::new();
    for partition in partitions {
        let mut raw = tempfile::tempfile().map_err(|error| error.to_string())?;
        image
            .decompress_partition_to(partition, &mut raw)
            .map_err(|error| error.to_string())?;

        if let Some(filesystem) = filesystem(&mut raw)? {
            found.push((filesystem, raw));
        }
    }

    let count = found.len();
    let Ok([(filesystem, raw)]) = <[_; 1]>::try_from(found) else {
        return Err(format!(
            "expected one HFS+ or APFS partition in the image, found {count}"
        ));
    };

    let raw = BufReader::new(raw);
    match filesystem {
        Filesystem::Apfs => {
            let volume = apfs::ApfsVolume::open(raw).map_err(|error| error.to_string())?;
            copy(volume, tree)
        }
        Filesystem::Hfs => {
            let volume = hfsplus::HfsVolume::open(raw).map_err(|error| error.to_string())?;
            copy(volume, tree)
        }
    }
}

/// The filesystem a partition holds, read from the signature in its first
/// blocks, with the partition rewound to its start.
fn filesystem(raw: &mut std::fs::File) -> Result<Option<Filesystem>, String> {
    let mut magic = [0u8; 1028];
    raw.seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let is_long_enough = raw.read_exact(&mut magic).is_ok();
    raw.seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    if !is_long_enough {
        return Ok(None);
    }

    Ok(match (magic.get(32..36), magic.get(1024..1026)) {
        (Some(b"NXSB"), _) => Some(Filesystem::Apfs),
        (_, Some(b"H+" | b"HX")) => Some(Filesystem::Hfs),
        _ => None,
    })
}

fn copy(mut volume: impl Volume, tree: &mut Tree) -> Result<(), String> {
    for (path, kind) in volume.entries()? {
        let top = path
            .trim_start_matches('/')
            .split('/')
            .next()
            .unwrap_or_default();
        if top == HARD_LINKS {
            if kind == Kind::File {
                return Err("the image holds hard links, which unpacking can't follow".into());
            }
            continue;
        }

        if METADATA.contains(&top) {
            continue;
        }

        // HFS+ leaves the mode zero on a file with no BSD permissions, which
        // the system shows with the defaults.
        let (mode, size) = volume.mode_and_size(&path)?;
        let mode = match (mode & 0o777, &kind) {
            (0, Kind::File) => 0o644,
            (0, _) => 0o755,
            _ => mode,
        };

        match kind {
            Kind::Directory => tree.directory(&path)?,
            Kind::Symlink => {
                let mut target = Vec::new();
                volume.write(&path, &mut target)?;
                let target = String::from_utf8(target)
                    .map_err(|_| format!("{path} links to a target that isn't UTF-8"))?;
                tree.symlink(&path, &target)?;
            }
            Kind::File => {
                let written =
                    tree.file_with(&path, mode, |file| volume.write(&path, file).map(drop))?;
                if written != size {
                    return Err(format!(
                        "{path} holds {size} bytes but {written} could be read"
                    ));
                }
            }
        }
    }

    Ok(())
}

impl<R: Read + Seek> Volume for hfsplus::HfsVolume<R> {
    fn entries(&mut self) -> Result<Vec<(String, Kind)>, String> {
        let entries = self.walk().map_err(|error| error.to_string())?;
        Ok(entries
            .into_iter()
            .map(|walked| {
                let kind = match walked.entry.kind {
                    hfsplus::EntryKind::File => Kind::File,
                    hfsplus::EntryKind::Directory => Kind::Directory,
                    hfsplus::EntryKind::Symlink => Kind::Symlink,
                };
                (walked.path, kind)
            })
            .collect())
    }

    fn mode_and_size(&mut self, path: &str) -> Result<(u32, u64), String> {
        let stat = self
            .stat(path)
            .map_err(|error| format!("{path}: {error}"))?;
        Ok((u32::from(stat.permissions.mode), stat.size))
    }

    fn write(&mut self, path: &str, to: &mut dyn Write) -> Result<u64, String> {
        self.read_file_to(path, to)
            .map_err(|error| format!("{path}: {error}"))
    }
}

impl<R: Read + Seek> Volume for apfs::ApfsVolume<R> {
    fn entries(&mut self) -> Result<Vec<(String, Kind)>, String> {
        let entries = self.walk().map_err(|error| error.to_string())?;
        Ok(entries
            .into_iter()
            .map(|walked| {
                let kind = match walked.entry.kind {
                    apfs::EntryKind::File => Kind::File,
                    apfs::EntryKind::Directory => Kind::Directory,
                    apfs::EntryKind::Symlink => Kind::Symlink,
                };
                (walked.path, kind)
            })
            .collect())
    }

    fn mode_and_size(&mut self, path: &str) -> Result<(u32, u64), String> {
        let stat = self
            .stat(path)
            .map_err(|error| format!("{path}: {error}"))?;
        Ok((u32::from(stat.mode), stat.size))
    }

    fn write(&mut self, path: &str, mut to: &mut dyn Write) -> Result<u64, String> {
        self.read_file_to(path, &mut to)
            .map_err(|error| format!("{path}: {error}"))
    }
}
