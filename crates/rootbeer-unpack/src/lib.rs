//! Unpacks the archives prebuilt software ships in, including macOS disk
//! images and installer packages, on any platform and without mounting
//! anything. An archive becomes a rootbeer-tree stream, which names the tree
//! it holds the same way on every machine, so nothing an archive names is
//! ever written to disk as a path.

mod apple_double;
mod archive;
mod cpio;
mod dmg;
mod pkg;
mod tree;
mod xar;

use archive::Compression;
pub use rootbeer_drv::Format;
use std::fs::File;
use std::io::{ErrorKind, Read, Seek, Write};
use tree::Tree;

/// The format of `archive` from its contents, when it's a disk image, a
/// package, or a zip. It's left rewound.
pub fn detect(archive: &mut File) -> Result<Option<Format>, String> {
    if dmg::is_image(archive)? {
        return Ok(Some(Format::Dmg));
    }

    let mut magic = [0u8; 4];
    let read = archive.read_exact(&mut magic);
    archive.rewind().map_err(|error| error.to_string())?;
    match (read, &magic) {
        (Ok(()), b"xar!") => Ok(Some(Format::Pkg)),
        (Ok(()), b"PK\x03\x04") => Ok(Some(Format::Zip)),
        (Ok(()), _) => Ok(None),
        (Err(error), _) if error.kind() == ErrorKind::UnexpectedEof => Ok(None),
        (Err(error), _) => Err(error.to_string()),
    }
}

/// Writes the tree `archive` holds to `out` as a rootbeer-tree stream.
/// Nothing in the archive runs. Contents wait in a scratch directory under
/// `TMPDIR` until the stream is written.
pub fn unpack(format: Format, archive: File, out: impl Write) -> Result<(), String> {
    let mut tree = Tree::new()?;
    match format {
        Format::Dmg => dmg::unpack(archive, &mut tree)?,
        Format::Pkg if dmg::is_image(&archive)? => {
            let mut image = Tree::new()?;
            dmg::unpack(archive, &mut image)?;
            pkg::unpack(image.only_package()?, &mut tree)?;
        }
        Format::Pkg => pkg::unpack(archive, &mut tree)?,
        Format::Zip => archive::zip(archive, &mut tree)?,
        Format::TarGz => archive::tar(archive, Compression::Gzip, &mut tree)?,
        Format::TarXz => archive::tar(archive, Compression::Xz, &mut tree)?,
        Format::TarBz2 => archive::tar(archive, Compression::Bzip2, &mut tree)?,
        Format::Binary => return Err("a binary is one file, not an archive".into()),
    }

    tree.write(out)
}
