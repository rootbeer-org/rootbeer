//! rootbeer-bootstrap is a minimal crate that implements a frozen protocol for
//! getting an rb version new enough to handle a user command. It's used for a
//! scenario when breaking changes are made or we need to move to the store.
//!
//! It is still validated with an ED25519 signature and SHA256 digest, so the
//! side-channel is not inherently less secure than the regular catalog.

mod error;
mod fetch;
mod release;

pub use error::Error;
pub use fetch::{exec, fetch, newer};
pub use release::Release;

pub fn platform() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

fn location(platform: &str) -> String {
    format!("https://rbpkg.com/bootstrap/v1/{platform}.json")
}

#[cfg(test)]
mod tests;
