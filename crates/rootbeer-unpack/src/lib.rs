//! Unpacks the archives prebuilt software ships in, including macOS disk
//! images and installer packages, on any platform and without mounting
//! anything.

mod apple_double;
mod archive;
mod cpio;
mod dmg;
mod pkg;
mod tree;
mod xar;

use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::Path;
use std::str::FromStr;
use tree::Tree;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// A UDIF disk image holding an HFS+ or APFS volume.
    Dmg,
    /// A flat installer package, or a disk image holding one.
    Pkg,
    Zip,
    TarBz2,
}

impl FromStr for Format {
    type Err = String;

    fn from_str(format: &str) -> Result<Format, String> {
        match format {
            "dmg" => Ok(Format::Dmg),
            "pkg" => Ok(Format::Pkg),
            "zip" => Ok(Format::Zip),
            "tar.bz2" => Ok(Format::TarBz2),
            _ => Err(format!("{format} isn't dmg, pkg, zip, or tar.bz2")),
        }
    }
}

/// The format of `archive` from its contents, when it's a disk image, a
/// package, or a zip.
pub fn detect(archive: &Path) -> Result<Option<Format>, String> {
    if dmg::is_image(archive)? {
        return Ok(Some(Format::Dmg));
    }

    let mut magic = [0u8; 4];
    let read = File::open(archive).and_then(|mut file| file.read_exact(&mut magic));
    match (read, &magic) {
        (Ok(()), b"xar!") => Ok(Some(Format::Pkg)),
        (Ok(()), b"PK\x03\x04") => Ok(Some(Format::Zip)),
        (Ok(()), _) => Ok(None),
        (Err(error), _) if error.kind() == ErrorKind::UnexpectedEof => Ok(None),
        (Err(error), _) => Err(format!("{}: {error}", archive.display())),
    }
}

/// Unpacks `archive` into `directory`, creating it if needed. Entries never
/// land outside it, and nothing in the archive runs.
pub fn unpack(format: Format, archive: &Path, directory: &Path) -> Result<(), String> {
    let tree = Tree::new(directory)?;
    match format {
        Format::Dmg => dmg::unpack(archive, &tree),
        Format::Pkg if dmg::is_image(archive)? => {
            let image = tempfile::tempdir().map_err(|error| error.to_string())?;
            dmg::unpack(archive, &Tree::new(image.path())?)?;
            pkg::unpack(&pkg::only(image.path())?, &tree)
        }
        Format::Pkg => pkg::unpack(archive, &tree),
        Format::Zip => archive::zip(archive, &tree),
        Format::TarBz2 => archive::tar_bz2(archive, &tree),
    }
}
