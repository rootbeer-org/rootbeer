//! Serializes a tree for `rb-store`, the privileged helper that inserts into a
//! store the user cannot write. The helper never opens a path the user names;
//! it only unpacks this stream into a directory it created itself.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, OpenOptionsExt};
use std::path::Path;

use crate::{collect_entries, MANIFEST_DIR};

/// The stream format `rb` sends; see [`crate::helper::PROTOCOLS`] for what a helper reads.
pub const PROTOCOL: u32 = 1;
const MAGIC: &[u8] = b"rootbeer-tree-1\n";
const MAX_PATH: usize = 4096;

const DIR: u8 = b'd';
const FILE: u8 = b'f';
const EXECUTABLE: u8 = b'x';
const SYMLINK: u8 = b'l';
const END: u8 = b'.';

/// Writes `root` in the order and with the normalization [`crate::hash_tree`] uses.
pub fn write_tree(root: &Path, output: &mut impl Write) -> io::Result<()> {
    let mut entries = Vec::new();
    collect_entries(root, root, &mut entries)?;
    entries.sort_by(|a, b| a.relative.cmp(&b.relative));

    output.write_all(MAGIC)?;
    for entry in entries {
        let path = root.join(&entry.relative);
        match entry.kind.as_str() {
            "dir" => write_header(output, DIR, &entry.relative)?,
            "file" => {
                let tag = if entry.executable { EXECUTABLE } else { FILE };
                write_header(output, tag, &entry.relative)?;
                let mut file = fs::File::open(&path)?;
                output.write_all(&file.metadata()?.len().to_be_bytes())?;
                io::copy(&mut file, output)?;
            }
            "symlink" => {
                write_header(output, SYMLINK, &entry.relative)?;
                write_bytes(output, fs::read_link(&path)?.as_os_str().as_bytes())?;
            }
            _ => unreachable!(),
        }
    }
    output.write_all(&[END])
}

/// Unpacks a stream into `destination`, which must not exist yet.
pub fn read_tree(input: &mut impl Read, destination: &Path) -> io::Result<()> {
    let mut magic = [0; MAGIC.len()];
    input.read_exact(&mut magic)?;
    if magic != MAGIC {
        return Err(invalid("not a rootbeer tree stream"));
    }

    fs::create_dir(destination)?;
    set_mode(destination, 0o755)?;

    // Parents must be directories this stream created, so no entry can be
    // written through a symlink or outside the destination.
    let mut dirs = BTreeSet::from([Vec::new()]);
    loop {
        let tag = read_array::<1>(input)?[0];
        if tag == END {
            return Ok(());
        }

        let relative = read_bytes(input)?;
        validate(&relative, &dirs)?;
        let path = destination.join(OsStr::from_bytes(&relative));
        match tag {
            DIR => {
                fs::create_dir(&path)?;
                set_mode(&path, 0o755)?;
                dirs.insert(relative);
            }
            FILE | EXECUTABLE => {
                let length = u64::from_be_bytes(read_array(input)?);
                let mode = if tag == EXECUTABLE { 0o755 } else { 0o644 };
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(mode)
                    .open(&path)?;
                if io::copy(&mut input.take(length), &mut file)? != length {
                    return Err(invalid("truncated file contents"));
                }
                set_mode(&path, mode)?;
            }
            SYMLINK => symlink(OsStr::from_bytes(&read_bytes(input)?), &path)?,
            _ => return Err(invalid("unknown entry kind")),
        }
    }
}

fn validate(relative: &[u8], dirs: &BTreeSet<Vec<u8>>) -> io::Result<()> {
    let components: Vec<&[u8]> = relative.split(|byte| *byte == b'/').collect();
    let is_invalid = components
        .iter()
        .any(|component| matches!(*component, b"" | b"." | b"..") || component.contains(&0));
    if is_invalid || components[0] == MANIFEST_DIR.as_bytes() {
        return Err(invalid(
            "entry path must be relative and stay inside the tree",
        ));
    }

    let parent = &relative[..relative.len() - components[components.len() - 1].len()];
    let parent = parent.strip_suffix(b"/").unwrap_or(parent);
    if !dirs.contains(parent) {
        return Err(invalid("entry parent is not a directory in the tree"));
    }
    Ok(())
}

fn write_header(output: &mut impl Write, tag: u8, relative: &str) -> io::Result<()> {
    output.write_all(&[tag])?;
    write_bytes(output, relative.as_bytes())
}

fn write_bytes(output: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    output.write_all(&(bytes.len() as u32).to_be_bytes())?;
    output.write_all(bytes)
}

fn read_bytes(input: &mut impl Read) -> io::Result<Vec<u8>> {
    let length = u32::from_be_bytes(read_array(input)?) as usize;
    if length > MAX_PATH {
        return Err(invalid("path exceeds 4096 bytes"));
    }
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn read_array<const N: usize>(input: &mut impl Read) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash_tree;

    fn sample(root: &Path) {
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("share/empty")).unwrap();
        fs::write(root.join("bin/tool"), "#!/bin/sh\n").unwrap();
        set_mode(&root.join("bin/tool"), 0o755).unwrap();
        fs::write(root.join("share/data"), "data").unwrap();
        symlink("../bin/tool", root.join("share/link")).unwrap();
    }

    fn stream(records: &[(u8, &[u8])]) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        for (tag, path) in records {
            bytes.push(*tag);
            write_bytes(&mut bytes, path).unwrap();
            match *tag {
                FILE => bytes.extend(0u64.to_be_bytes()),
                SYMLINK => write_bytes(&mut bytes, b"/etc").unwrap(),
                _ => {}
            }
        }
        bytes.push(END);
        bytes
    }

    #[test]
    fn magic_names_the_protocol() {
        assert_eq!(MAGIC, format!("rootbeer-tree-{PROTOCOL}\n").as_bytes());
    }

    #[test]
    fn round_trips_with_the_same_hash() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        sample(&source);
        let mut bytes = Vec::new();
        write_tree(&source, &mut bytes).unwrap();

        let copy = root.path().join("copy");
        read_tree(&mut bytes.as_slice(), &copy).unwrap();

        assert_eq!(hash_tree(&copy).unwrap(), hash_tree(&source).unwrap());
        assert!(copy.join("share/empty").is_dir());
    }

    #[test]
    fn rejects_entries_that_escape_the_tree() {
        let cases: &[&[(u8, &[u8])]] = &[
            &[(FILE, b"../escape")],
            &[(FILE, b"/etc/escape")],
            &[(DIR, b"a"), (FILE, b"a/../../escape")],
            &[(SYMLINK, b"a"), (FILE, b"a/escape")],
            &[(FILE, b"missing/escape")],
            &[(DIR, b".rootbeer"), (FILE, b".rootbeer/manifest.json")],
            &[(FILE, b"a"), (FILE, b"a")],
        ];
        for records in cases {
            let root = tempfile::tempdir().unwrap();
            let result = read_tree(&mut stream(records).as_slice(), &root.path().join("tree"));
            assert!(result.is_err(), "{records:?}");
        }
    }
}
