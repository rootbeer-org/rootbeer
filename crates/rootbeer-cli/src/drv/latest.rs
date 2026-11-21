use serde::Deserialize;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const STALL: Duration = Duration::from_secs(30);
const DOWNLOAD: Duration = Duration::from_secs(600);

#[derive(Deserialize)]
struct Info {
    #[serde(rename = "CFBundleShortVersionString")]
    version: String,
}

/// The version of `app` in the download at `url`. The download unpacks with
/// `7zz` from `PATH`, which reads DMGs and zips on every platform.
pub(super) fn version(url: &str, app: &str) -> Result<String, String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let download = directory.path().join("download");
    let response = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_connect(Some(STALL))
        .timeout_recv_body(Some(STALL))
        .timeout_global(Some(DOWNLOAD))
        .build()
        .new_agent()
        .get(url)
        .call()
        .map_err(|error| format!("{url}: {error}"))?;

    let mut file = File::create(&download).map_err(|error| error.to_string())?;
    io::copy(&mut response.into_body().into_reader(), &mut file)
        .map_err(|error| format!("{url}: {error}"))?;

    let unpacked = directory.path().join("unpacked");
    let status = Command::new("7zz")
        .args(["x", "-y", "-bso0", "-bsp0", "-snld20"])
        .arg(format!("-o{}", unpacked.display()))
        .arg(&download)
        .status()
        .map_err(|error| format!("couldn't run 7zz, which reads the download ({error})"))?;
    if !status.success() {
        return Err(format!("7zz couldn't unpack {url} ({status})"));
    }

    read(&unpacked, app)
}

/// The version in the Info.plist of `app`, at the top of `unpacked` or one
/// directory down, where a DMG's volume puts it.
fn read(unpacked: &Path, app: &str) -> Result<String, String> {
    let mut found: Vec<PathBuf> = fs::read_dir(unpacked)
        .map_err(|error| format!("{}: {error}", unpacked.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(app))
        .chain([unpacked.join(app)])
        .filter(|path| path.is_dir() && !path.is_symlink())
        .collect();
    found.dedup();

    let [bundle] = found.as_slice() else {
        return Err(format!(
            "expected one {app} in the download, found {}",
            found.len()
        ));
    };

    let plist = bundle.join("Contents/Info.plist");
    let info: Info =
        plist::from_file(&plist).map_err(|error| format!("{}: {error}", plist.display()))?;
    Ok(info.version)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(root: &Path, version: &str) {
        let contents = root.join("Contents");
        fs::create_dir_all(&contents).unwrap();
        let info = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleShortVersionString</key><string>{version}</string>
</dict></plist>"#
        );
        fs::write(contents.join("Info.plist"), info).unwrap();
    }

    #[test]
    fn the_version_comes_from_the_one_bundle_at_the_top_or_in_a_volume() {
        let zipped = tempfile::tempdir().unwrap();
        bundle(&zipped.path().join("Tool.app"), "1.2.3");
        assert_eq!(read(zipped.path(), "Tool.app").unwrap(), "1.2.3");

        let imaged = tempfile::tempdir().unwrap();
        bundle(&imaged.path().join("Tool/Tool.app"), "4.5.6.7");
        std::os::unix::fs::symlink("/Applications", imaged.path().join("Tool/Applications"))
            .unwrap();
        assert_eq!(read(imaged.path(), "Tool.app").unwrap(), "4.5.6.7");

        bundle(&imaged.path().join("Other/Tool.app"), "8.0");
        let error = read(imaged.path(), "Tool.app").unwrap_err();
        assert!(error.contains("found 2"), "{error}");

        assert!(read(imaged.path(), "Missing.app")
            .unwrap_err()
            .contains("found 0"));
    }
}
