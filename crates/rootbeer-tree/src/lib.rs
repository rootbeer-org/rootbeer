//! rootbeer-tree is the canonical encoding of a directory tree. A tree has
//! exactly one encoding and the reader accepts nothing else, so the sha256 of
//! a stream names the tree it holds. An extract pins an unpacked archive by
//! that digest, the way a fetch pins a file by its own.
//!
//! The format is frozen as `rootbeer-tree-v1`. A stream is the magic line,
//! a record per entry, then an end record. Integers are little endian.
//!
//! ```text
//! stream     = "rootbeer-tree-v1\n" record* "e"
//! record     = "d" path                    a directory
//!            | "l" path bytes              a symlink and its target
//!            | ("f" | "x") path attributes contents
//!                                          a file, "x" when it's executable
//! path       = bytes                       components joined by "/"
//! attributes = u32 count, then count pairs of bytes, a name and a value
//! contents   = u64 length, then that many bytes
//! bytes      = u32 length, then that many bytes
//! ```
//!
//! Each directory comes before what it holds, and entries are sorted by
//! their components as bytes, so a tree is written depth first. Components
//! are never empty, `.`, or `..`, and hold no NUL. A file records only code
//! signature attributes (`com.apple.cs.*`), sorted by name, since those are
//! the only ones the store keeps. Nothing else is recorded, so owners, times,
//! and every mode bit beyond a file being executable are left out.

mod materialize;
mod order;
mod read;
mod write;

pub use materialize::materialize;
pub use read::{Contents, Entry, Kind, Reader, digest};
pub use write::Writer;

use std::fmt;
use std::io;
use std::path::PathBuf;

const MAGIC: &[u8] = b"rootbeer-tree-v1\n";

/// The only attributes a file records.
pub const KEPT: &str = "com.apple.cs.";

// Bounds on what a record holds before its contents, so a corrupt length
// fails instead of allocating. Signatures are the largest attribute values.
const LONGEST_NAME: u32 = 64 << 10;
const LONGEST_VALUE: u32 = 16 << 20;
const MOST_ATTRIBUTES: u32 = 64;

#[derive(Debug)]
pub enum Error {
    /// The stream breaks the format.
    Malformed(String),
    /// Reading or writing the stream failed.
    Stream(io::Error),
    /// Writing a tree to disk failed.
    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Malformed(reason) => write!(f, "malformed tree: {reason}"),
            Error::Stream(source) => write!(f, "tree stream: {source}"),
            Error::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
