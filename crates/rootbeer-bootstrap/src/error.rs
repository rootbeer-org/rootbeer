use std::{fmt, io};

#[derive(Debug)]
pub enum Error {
    Fetch { url: String, reason: String },
    Invalid(String),
    Untrusted,
    Io(io::Error),
    Digest { expected: String, actual: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Fetch { url, reason } => write!(f, "fetching {url}: {reason}"),
            Error::Invalid(reason) => write!(f, "invalid release document: {reason}"),
            Error::Untrusted => f.write_str("release document is not signed by a trusted key"),
            Error::Io(error) => write!(f, "unpacking release: {error}"),
            Error::Digest { expected, actual } => {
                write!(f, "archive sha256 is {actual}, expected {expected}")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Error::Io(error)
    }
}
