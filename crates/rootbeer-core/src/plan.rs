use std::path::PathBuf;

use crate::package::PackageRequest;

/// Identity location; key material is resolved only when decrypting.
#[derive(Debug, Clone, PartialEq)]
pub enum AgeIdentity {
    File(PathBuf),
    OnePassword(String),
}

/// File contents, either inline bytes or a provider resolved during apply.
/// Debug output omits inline bytes, which may contain composed secrets.
#[derive(Clone, PartialEq)]
pub enum WriteSource {
    /// Bytes already produced during planning.
    Bytes(Vec<u8>),
    /// Fetched from 1Password via `op document get <reference>` at apply time.
    OpDocument { reference: String },
    /// Age ciphertext decrypted at apply time, written atomically with this mode.
    AgeFile {
        path: PathBuf,
        identity: AgeIdentity,
        mode: u32,
    },
}

impl std::fmt::Debug for WriteSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bytes(bytes) => f.debug_struct("Bytes").field("len", &bytes.len()).finish(),
            Self::OpDocument { reference } => f
                .debug_struct("OpDocument")
                .field("reference", reference)
                .finish(),
            Self::AgeFile {
                path,
                identity,
                mode,
            } => f
                .debug_struct("AgeFile")
                .field("path", path)
                .field("identity", identity)
                .field("mode", mode)
                .finish(),
        }
    }
}

impl WriteSource {
    /// Convenience for callers producing text content (most codecs, scripts,
    /// `rb.file`). Equivalent to `Bytes(s.into().into_bytes())`.
    pub fn text(s: impl Into<String>) -> Self {
        Self::Bytes(s.into().into_bytes())
    }

    /// Returns the inline content as `&str` when the source is `Bytes` and
    /// the bytes are valid UTF-8. Returns `None` for deferred sources.
    /// Primarily used by tests and pretty-printing.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Bytes(b) => std::str::from_utf8(b).ok(),
            _ => None,
        }
    }

    /// Byte length if known at planning time (i.e. `Bytes` sources). Deferred
    /// sources return `None` because the size isn't known until apply.
    pub fn known_size(&self) -> Option<usize> {
        match self {
            Self::Bytes(b) => Some(b.len()),
            _ => None,
        }
    }

    /// Human-readable label for the upcoming fetch when the source is
    /// secret-backed. Returns `None` for inline `Bytes` (no fetch happens).
    /// Used by the CLI to print a `fetch …` preamble so users see why an
    /// apply is pausing on Touch ID / network. New providers add their
    /// branch here; the CLI never needs to know about them directly.
    pub fn fetch_label(&self) -> Option<String> {
        match self {
            Self::Bytes(_) => None,
            Self::OpDocument { reference } => Some(format!("op-document {reference}")),
            Self::AgeFile { path, .. } => Some(format!("age {}", path.display())),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    WriteFile {
        path: PathBuf,
        source: WriteSource,
    },
    Symlink {
        src: PathBuf,
        dst: PathBuf,
    },
    CopyFileIfMissing {
        src: PathBuf,
        dst: PathBuf,
    },
    Exec {
        cmd: String,
        args: Vec<String>,
        cwd: PathBuf,
    },
    Chmod {
        path: PathBuf,
        mode: u32,
    },
    SetRemoteUrl {
        dir: PathBuf,
        url: String,
    },
    Package {
        request: PackageRequest,
    },
}
