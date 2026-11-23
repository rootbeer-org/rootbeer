use crate::tree::Tree;
use std::collections::BTreeMap;
use std::io::{self, Read};

const HEADER: usize = 76;
const MAGIC: &[u8] = b"070707";
const TRAILER: &str = "TRAILER!!!";

const KIND: u32 = 0o170000;
const FILE: u32 = 0o100000;
const DIRECTORY: u32 = 0o040000;
const SYMLINK: u32 = 0o120000;

// Longer than any path a payload holds, so a corrupt size fails fast.
const LONGEST_NAME: u64 = 64 * 1024;

/// Unpacks a POSIX portable (odc) cpio archive, the format of a package's
/// payload, under `prefix` in `tree`.
pub(crate) fn unpack(mut archive: impl Read, tree: &mut Tree, prefix: &str) -> Result<(), String> {
    let mut seen: BTreeMap<(u64, u64), String> = BTreeMap::new();
    loop {
        let mut header = [0u8; HEADER];
        archive
            .read_exact(&mut header)
            .map_err(|error| format!("the payload ends early ({error})"))?;

        let field = |start: usize, length: usize| octal(&header, start, length);
        if header.get(..6) != Some(MAGIC) {
            return Err("the payload isn't a portable cpio archive".into());
        }

        let (dev, ino, mode) = (field(6, 6)?, field(12, 6)?, field(18, 6)?);
        let nlink = field(36, 6)?;
        let (name_size, size) = (field(59, 6)?, field(65, 11)?);
        if name_size > LONGEST_NAME {
            return Err(format!("a payload entry's name is {name_size} bytes"));
        }

        let name = read(&mut archive, name_size)?;
        let name = name.strip_suffix(&[0]).unwrap_or(&name);
        let name = String::from_utf8(name.to_vec())
            .map_err(|_| "a payload entry's name isn't UTF-8".to_string())?;
        if name == TRAILER {
            return Ok(());
        }

        let path = format!("{prefix}/{name}");
        let mode = u32::try_from(mode).map_err(|_| format!("{name} has mode {mode}"))?;
        let mut contents = (&mut archive).take(size);
        match mode & KIND {
            DIRECTORY => tree.directory(&path)?,
            SYMLINK => {
                let target = read(&mut contents, size)?;
                let target = String::from_utf8(target)
                    .map_err(|_| format!("{name} links to a target that isn't UTF-8"))?;
                tree.symlink(&path, &target)?;
            }
            // A link after the first can carry no data, naming the file
            // that holds it instead.
            FILE if nlink > 1 && size == 0 && seen.contains_key(&(dev, ino)) => {
                let original = seen.get(&(dev, ino)).cloned().unwrap_or_default();
                tree.hard_link(&path, &original)?;
            }
            FILE => {
                let written = tree.file(&path, mode, &mut contents)?;
                if written != size {
                    return Err(format!("the payload ends inside {name}"));
                }

                if nlink > 1 {
                    seen.insert((dev, ino), path);
                }
            }
            _ => {
                return Err(format!(
                    "{name} is a device or a pipe, which apps don't hold"
                ));
            }
        }

        io::copy(&mut contents, &mut io::sink()).map_err(|error| error.to_string())?;
    }
}

fn octal(header: &[u8], start: usize, length: usize) -> Result<u64, String> {
    let digits = start
        .checked_add(length)
        .and_then(|end| header.get(start..end))
        .and_then(|digits| std::str::from_utf8(digits).ok())
        .ok_or("a payload header is cut short")?;

    u64::from_str_radix(digits, 8).map_err(|_| format!("a payload header holds {digits:?}"))
}

fn read(reader: &mut impl Read, size: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(size)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;

    if u64::try_from(bytes.len()).ok() != Some(size) {
        return Err("the payload ends early".into());
    }

    Ok(bytes)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tree::tests::entries;
    use rootbeer_tree::Kind;

    pub(crate) fn entry(
        archive: &mut Vec<u8>,
        name: &str,
        mode: u32,
        ino: u32,
        nlink: u32,
        data: &[u8],
    ) {
        let header = format!(
            "070707{:06o}{:06o}{:06o}{:06o}{:06o}{:06o}{:06o}{:011o}{:06o}{:011o}",
            1,
            ino,
            mode,
            0,
            0,
            nlink,
            0,
            0,
            name.len().saturating_add(1),
            data.len()
        );
        archive.extend_from_slice(header.as_bytes());
        archive.extend_from_slice(name.as_bytes());
        archive.push(0);
        archive.extend_from_slice(data);
    }

    pub(crate) fn payload() -> Vec<u8> {
        let mut archive = Vec::new();
        entry(&mut archive, ".", 0o040755, 1, 2, b"");
        entry(&mut archive, "./Tool.app", 0o040755, 2, 2, b"");
        entry(&mut archive, "./Tool.app/tool", 0o100755, 3, 2, b"#!");
        entry(&mut archive, "./Tool.app/again", 0o100755, 3, 2, b"");
        entry(&mut archive, "./Tool.app/Current", 0o120755, 4, 1, b"tool");
        entry(&mut archive, TRAILER, 0, 0, 1, b"");
        archive
    }

    #[test]
    fn a_payload_unpacks_with_its_links() {
        let mut tree = Tree::new().unwrap();
        unpack(payload().as_slice(), &mut tree, "Applications").unwrap();

        let tool = Kind::File {
            is_executable: true,
            attributes: Vec::new(),
            size: 2,
        };
        let current = Kind::Symlink {
            target: b"tool".to_vec(),
        };
        assert_eq!(
            entries(&tree),
            [
                ("Applications".to_string(), Kind::Directory),
                ("Applications/Tool.app".to_string(), Kind::Directory),
                ("Applications/Tool.app/Current".to_string(), current),
                ("Applications/Tool.app/again".to_string(), tool.clone()),
                ("Applications/Tool.app/tool".to_string(), tool),
            ]
        );

        let truncated = payload();
        let truncated = truncated
            .get(..truncated.len().saturating_sub(100))
            .unwrap();
        assert!(unpack(truncated, &mut Tree::new().unwrap(), "").is_err());
    }
}
