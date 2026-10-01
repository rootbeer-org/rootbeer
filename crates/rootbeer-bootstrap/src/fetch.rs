use crate::{Error, Release, location, platform, release};
use data_encoding::HEXLOWER;
use flate2::read::GzDecoder;
use ring::digest::{SHA256, digest};
use std::fs::Permissions;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const DOCUMENT_LIMIT: u64 = 64 * 1024;
const ARCHIVE_LIMIT: u64 = 512 * 1024 * 1024;

const DOCUMENT_TIMEOUT: Duration = Duration::from_secs(60);
const ARCHIVE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

pub fn newer(keys: &[[u8; 32]], serial: u64) -> Result<Option<Release>, Error> {
    let platform = platform();
    let document = get(&location(&platform), DOCUMENT_LIMIT, DOCUMENT_TIMEOUT)?;
    release::newer(&document, keys, &platform, serial)
}

pub fn fetch(release: &Release) -> Result<PathBuf, Error> {
    let archive = get(&release.0.url, ARCHIVE_LIMIT, ARCHIVE_TIMEOUT)?;
    unpack(&archive, release)
}

pub fn exec(rb: &Path) -> io::Error {
    Command::new(rb).args(std::env::args_os().skip(1)).exec()
}

pub(crate) fn unpack(archive: &[u8], release: &Release) -> Result<PathBuf, Error> {
    let actual = HEXLOWER.encode(digest(&SHA256, archive).as_ref());
    if actual != release.0.sha256 {
        return Err(Error::Digest {
            expected: release.0.sha256.clone(),
            actual,
        });
    }

    let directory = tempfile::Builder::new()
        .prefix("rb-bootstrap-")
        .permissions(Permissions::from_mode(0o700))
        .tempdir()?;

    tar::Archive::new(GzDecoder::new(archive)).unpack(directory.path())?;
    if !directory.path().join("rb").is_file() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "archive has no rb").into());
    }

    Ok(directory.keep().join("rb"))
}

fn get(url: &str, limit: u64, timeout: Duration) -> Result<Vec<u8>, Error> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_global(Some(timeout))
        .build()
        .into();

    agent
        .get(url)
        .call()
        .and_then(|response| {
            response
                .into_body()
                .with_config()
                .limit(limit)
                .read_to_vec()
        })
        .map_err(|error| Error::Fetch {
            url: url.to_string(),
            reason: error.to_string(),
        })
}
