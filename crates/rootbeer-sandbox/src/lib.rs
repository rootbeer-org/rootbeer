//! rootbeer-sandbox realizes a derivation into the store. It fetches assets,
//! builds the derivation in a sandbox, and scans outputs for references.

mod fetch;

pub use fetch::fetch;
use rootbeer_drv::Key;
use std::fmt;

#[derive(Debug)]
pub enum Error {
    Fetch { key: Key, failures: Vec<String> },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Fetch { key, failures } => {
                write!(f, "fetch {key} failed: {}", failures.join("; "))
            }
        }
    }
}

impl std::error::Error for Error {}
