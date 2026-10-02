use crate::Error;
use data_encoding::HEXLOWER;
use rootbeer_drv::{Fetch, Key, STORE_ROOT, Sha256, fetch_path};
use sha2::Digest;
use std::fs::{File, Permissions};
use std::io::{self, Seek};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

// Covers the entire transfer so it needs to account for the slowest fetch
const TIMEOUT: Duration = Duration::from_secs(30 * 60);
const STALL: Duration = Duration::from_secs(30);

type Failure = Box<dyn std::error::Error>;

/// Downloads a fetch into the store, trying each URL in order until one matches
/// the hash. The file appears at its store path after verification.
pub(crate) fn fetch(key: &Key, fetch: &Fetch) -> Result<(), Error> {
    let path = fetch_path(key);
    let mut failures = Vec::new();
    for url in &fetch.urls {
        match download(url, &fetch.sha256, &path) {
            Ok(()) => return Ok(()),
            Err(failure) => failures.push(format!("{url}: {failure}")),
        }
    }

    Err(Error::Fetch {
        key: key.clone(),
        failures,
    })
}

fn download(url: &str, sha256: &Sha256, path: &Path) -> Result<(), Failure> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(STALL))
        .timeout_recv_body(Some(STALL))
        .timeout_global(Some(TIMEOUT))
        .build()
        .into();

    let response = agent.get(url).call()?;
    let mut staged = tempfile::Builder::new()
        .prefix(".tmp-")
        .tempfile_in(STORE_ROOT)?;

    io::copy(
        &mut response.into_body().into_reader(),
        staged.as_file_mut(),
    )?;

    verify(staged.as_file_mut(), sha256)?;
    staged
        .as_file()
        .set_permissions(Permissions::from_mode(0o444))?;

    staged.as_file().sync_all()?;
    staged.persist(path)?;
    Ok(())
}

fn verify(file: &mut File, sha256: &Sha256) -> Result<(), Failure> {
    file.rewind()?;
    let mut hasher = sha2::Sha256::new();
    io::copy(file, &mut hasher)?;

    let actual = HEXLOWER.encode(&hasher.finalize());
    if actual != sha256.as_str() {
        return Err(format!("expected sha256 {}, got {actual}", sha256.as_str()).into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn verify_rejects_content_that_does_not_match() {
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let mut file = tempfile::tempfile().unwrap();

        file.write_all(b"abc").unwrap();
        verify(&mut file, &abc.to_string().try_into().unwrap()).unwrap();
        file.write_all(b"d").unwrap();

        let error = verify(&mut file, &abc.to_string().try_into().unwrap()).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with(&format!("expected sha256 {abc}, got "))
        );
    }
}
