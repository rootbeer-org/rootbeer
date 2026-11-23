use rootbeer_tree::{Kind, Reader};
use rootbeer_unpack::Format;
use serde::Deserialize;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;

const STALL: Duration = Duration::from_secs(30);
const DOWNLOAD: Duration = Duration::from_secs(600);

// Far beyond any Info.plist, so a huge one fails instead of filling memory.
const LONGEST_INFO: u64 = 1 << 20;

#[derive(Deserialize)]
struct Info {
    #[serde(rename = "CFBundleShortVersionString")]
    version: String,
}

/// The version of `app` in the download at `url`, a DMG, zip, or package.
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

    let mut file = File::open(&download).map_err(|error| error.to_string())?;
    let format = rootbeer_unpack::detect(&mut file)?
        .ok_or("the download is neither a DMG, a zip, nor a package")?;

    // A DMG can hold an installer package instead of the app.
    match unpacked(&download, format, app) {
        Err(_) if format == Format::Dmg => unpacked(&download, Format::Pkg, app),
        read => read,
    }
    .map_err(|error| format!("{url}: {error}"))
}

fn unpacked(archive: &Path, format: Format, app: &str) -> Result<String, String> {
    rootbeer_sandbox::unpack(archive, format, |stream| read(stream, app))
        .map_err(|error| error.to_string())
}

/// The version in the Info.plist of `app` in a tree, at its top or one
/// directory down, where a DMG's volume puts it.
fn read(stream: impl Read, app: &str) -> Result<String, String> {
    let mut reader = Reader::new(stream).map_err(|error| error.to_string())?;
    let mut found = Vec::new();
    while let Some(entry) = reader.entry().map_err(|error| error.to_string())? {
        let path = String::from_utf8_lossy(&entry.path);
        let bundle = path
            .strip_suffix("/Contents/Info.plist")
            .unwrap_or_default();
        let is_info = bundle == app
            || bundle
                .strip_suffix(app)
                .and_then(|volume| volume.strip_suffix('/'))
                .is_some_and(|volume| !volume.is_empty() && !volume.contains('/'));
        if !is_info || !matches!(entry.kind, Kind::File { .. }) {
            continue;
        }

        let mut info = Vec::new();
        (&mut reader.contents())
            .take(LONGEST_INFO)
            .read_to_end(&mut info)
            .map_err(|error| format!("{path}: {error}"))?;
        found.push(info);
    }

    reader.finish().map_err(|error| error.to_string())?;
    let [info] = found.as_slice() else {
        return Err(format!(
            "expected one {app} in the download, found {}",
            found.len()
        ));
    };

    let info: Info = plist::from_bytes(info).map_err(|error| format!("{app}: {error}"))?;
    Ok(info.version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rootbeer_tree::Writer;
    use std::collections::BTreeMap;

    fn info(version: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleShortVersionString</key><string>{version}</string>
</dict></plist>"#
        )
    }

    enum Item {
        Directory,
        File(String),
        Link(&'static str),
    }

    /// A tree with a bundle at each of `bundles`, by its path and version,
    /// and each of `links`. Paths keyed by their components sort the way the
    /// stream lists them.
    fn tree(bundles: &[(&str, &str)], links: &[(&str, &'static str)]) -> Vec<u8> {
        let mut items = BTreeMap::new();
        let split = |path: &str| path.split('/').map(String::from).collect::<Vec<_>>();
        for (bundle, version) in bundles {
            let contents = split(&format!("{bundle}/Contents"));
            for end in 1..=contents.len() {
                items.insert(contents[..end].to_vec(), Item::Directory);
            }

            let plist = split(&format!("{bundle}/Contents/Info.plist"));
            items.insert(plist, Item::File(info(version)));
        }

        for (link, target) in links {
            items.insert(split(link), Item::Link(target));
        }

        let mut writer = Writer::new(Vec::new()).unwrap();
        for (path, item) in items {
            let path = path.join("/");
            let path = path.as_bytes();
            match item {
                Item::Directory => writer.directory(path).unwrap(),
                Item::Link(target) => writer.symlink(path, target.as_bytes()).unwrap(),
                Item::File(contents) => {
                    let size = u64::try_from(contents.len()).unwrap();
                    writer
                        .file(path, false, &[], size, &mut contents.as_bytes())
                        .unwrap();
                }
            }
        }

        writer.finish().unwrap()
    }

    #[test]
    fn the_version_comes_from_the_one_bundle_at_the_top_or_in_a_volume() {
        let zipped = tree(&[("Tool.app", "1.2.3")], &[]);
        assert_eq!(read(zipped.as_slice(), "Tool.app").unwrap(), "1.2.3");

        let imaged = tree(
            &[("Tool/Tool.app", "4.5.6.7")],
            &[("Tool/Applications", "/Applications")],
        );
        assert_eq!(read(imaged.as_slice(), "Tool.app").unwrap(), "4.5.6.7");

        let twice = tree(
            &[("Tool/Tool.app", "4.5.6.7"), ("Other/Tool.app", "8.0")],
            &[],
        );
        let error = read(twice.as_slice(), "Tool.app").unwrap_err();
        assert!(error.contains("found 2"), "{error}");

        let deep = tree(&[("a/b/Tool.app", "9.0")], &[]);
        assert!(read(deep.as_slice(), "Tool.app")
            .unwrap_err()
            .contains("found 0"));
        assert!(read(imaged.as_slice(), "Missing.app")
            .unwrap_err()
            .contains("found 0"));
    }
}
