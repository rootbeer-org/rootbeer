//! Records the digest of each prebuilt archive's unpacked tree, which pins
//! the tree so rb extracts it and no unpacker reaches a key.

use super::{bump, github, load, Sources};
use rootbeer_drv::{fetch_path, Derivation, Fetch, Format, Platform, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[derive(clap::Args, Debug)]
pub(super) struct Args {
    /// Recipes to fill, or every recipe when none are named
    names: Vec<String>,
    #[command(flatten)]
    sources: Sources,
}

/// Downloads `url` and returns its hash and its tree. When `expected` is
/// given, the download must hash to it before anything parses it.
pub(super) fn download(
    url: &str,
    expected: Option<&Sha256>,
    format: Format,
) -> Result<(Sha256, Sha256), String> {
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
    let archive = directory.path().join("archive");
    let mut file = fs::File::create_new(&archive).map_err(|error| error.to_string())?;
    let digest = github::download_to(url, &mut file)?;
    if let Some(expected) = expected.filter(|expected| **expected != digest) {
        return Err(format!(
            "{url} hashes to {}, not {}",
            digest.as_str(),
            expected.as_str()
        ));
    }

    let tree = rootbeer_sandbox::tree(&archive, format).map_err(|error| error.to_string())?;
    Ok((digest, tree))
}

/// Fills in every declared version's missing trees, from the store's fetches
/// where it has them.
pub(super) fn trees(args: &Args) -> Result<(), String> {
    let (catalog, _) = load(&args.sources)?;
    let names: Vec<String> = match args.names.is_empty() {
        true => catalog.names().map(str::to_string).collect(),
        false => args.names.clone(),
    };

    for name in &names {
        if !catalog.contains(name) {
            return Err(format!("{name} isn't in the catalog"));
        }

        let mut recorded: BTreeMap<String, BTreeMap<Platform, Sha256>> = BTreeMap::new();
        for platform in catalog.platforms(name) {
            for version in catalog.versions(name, platform) {
                let at = |error: String| format!("{name}@{version} on {platform}: {error}");
                let pinned = catalog.pinned(name, version, platform).map_err(at)?;
                let Some(format) = pinned.download.extract.filter(|_| pinned.tree.is_none()) else {
                    continue;
                };

                let tree = match fetched(&pinned.download.url, &pinned.sha256)? {
                    Some(archive) => {
                        rootbeer_sandbox::tree(&archive, format).map_err(|error| error.to_string())
                    }
                    None => download(&pinned.download.url, Some(&pinned.sha256), format)
                        .map(|(_, tree)| tree),
                }
                .map_err(at)?;

                recorded
                    .entry(version.to_string())
                    .or_default()
                    .insert(platform, tree);
            }
        }

        if recorded.is_empty() {
            continue;
        }

        let path = args.sources.catalog.join(format!("{name}.lua"));
        let original =
            fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        let at = |error: String| format!("{}: {error}", path.display());
        let text = bump::Recipe::read(&original)
            .and_then(|recipe| recipe.with_trees(&recorded))
            .map_err(at)?;
        fs::write(&path, text).map_err(|error| at(error.to_string()))?;

        let line = serde_json::json!({ "name": name, "trees": recorded });
        println!("{line}");
    }

    Ok(())
}

/// The store's copy of a fetch, verified when it was fetched.
fn fetched(url: &str, sha256: &Sha256) -> Result<Option<PathBuf>, String> {
    let fetch = Derivation::Fetch(Fetch {
        sha256: sha256.clone(),
        urls: vec![url.to_string()],
    });

    let path = fetch_path(&fetch.key().map_err(|error| error.to_string())?);
    Ok(path.is_file().then_some(path))
}
