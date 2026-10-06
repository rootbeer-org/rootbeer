use super::cache::{self, find, helper_pull, references_first, Caches, Registry};
use super::{evaluate, store_error, Sources};
use data_encoding::HEXLOWER;
use rootbeer_cache::Output;
use rootbeer_drv::{output_path, Build, Derivation, Key, STORE_ROOT};
use rootbeer_store::{pack, Store, ROOT};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

const DERIVATION: &str = "derivation.json";
const REFERENCES: &str = "references";
const ARCHIVE: &str = "archive.tar.zst";
const MANIFEST: &str = "manifest.json";

struct Artifact {
    key: Key,
    build: Build,
    references: BTreeMap<Key, String>,
    archive: PathBuf,
    manifest: PathBuf,
}

pub(super) fn export(sources: &Sources, package: &str, directory: &Path) -> Result<(), String> {
    let (graph, target) = evaluate(sources, package, None)?;
    let package = graph
        .packages
        .get(&target)
        .ok_or_else(|| format!("{target} did not evaluate"))?;

    let store = Store::open_read_only(Path::new(ROOT)).map_err(store_error)?;
    let mut order = Vec::new();
    references_first(&store, &package.build, &mut BTreeSet::new(), &mut order)?;

    let build_of = |key: &Key| match graph.derivations.get(key) {
        Some(Derivation::Build(build)) => Ok(build),
        _ => Err(format!(
            "{key} isn't a build in the graph, so it can't be exported"
        )),
    };

    let at = |error: io::Error| format!("{}: {error}", directory.display());
    fs::create_dir_all(directory).map_err(at)?;
    for (key, references) in &order {
        let out = directory.join(key.as_str());
        if out.exists() {
            continue;
        }

        let build = build_of(key)?;
        let path = store
            .path(key)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("{key} isn't built, so run `rb drv build` first"))?;

        let references = references
            .iter()
            .map(|reference| Ok((reference.clone(), build_of(reference)?.name.clone())))
            .collect::<Result<BTreeMap<_, _>, String>>()?;

        let lines = references
            .iter()
            .map(|(reference, name)| format!("{name}:{reference}\n"))
            .collect::<String>();

        // Renamed into place AFTER it's completed
        let staging = tempfile::Builder::new()
            .prefix(".tmp-")
            .tempdir_in(directory)
            .map_err(at)?;

        let staged = |name: &str| staging.path().join(name);
        let at = |error: io::Error| format!("{}: {error}", staging.path().display());
        let derivation = Derivation::Build(build.clone());
        let json = serde_json::to_vec(&derivation).map_err(|error| error.to_string())?;

        fs::write(staged(DERIVATION), json).map_err(at)?;
        fs::write(staged(REFERENCES), lines).map_err(at)?;
        let archive = File::create(staged(ARCHIVE)).map_err(at)?;
        pack(&path, archive).map_err(|error| error.to_string())?;

        // What CI attests, and so the exact bytes publish must push
        let manifest = rootbeer_cache::manifest(&Output {
            key,
            derivation: &derivation,
            references: &references,
            archive: &staged(ARCHIVE),
        })
        .map_err(|error| error.to_string())?;

        fs::write(staged(MANIFEST), manifest).map_err(at)?;

        fs::rename(staging.path(), &out).map_err(|error| format!("{}: {error}", out.display()))?;
        eprintln!("exported {key} {}", build.name);
    }

    Ok(())
}

/// Each derivation must hash to its directory's key, which reading checks, and
/// each manifest must be exactly the one publish would push.
pub(super) fn validate(directory: &Path) -> Result<(), String> {
    for Artifact {
        key,
        build,
        references,
        archive,
        manifest,
    } in read(directory)?
    {
        let expected = rootbeer_cache::manifest(&Output {
            key: &key,
            derivation: &Derivation::Build(build),
            references: &references,
            archive: &archive,
        })
        .map_err(|error| error.to_string())?;

        let actual =
            fs::read(&manifest).map_err(|error| format!("{}: {error}", manifest.display()))?;
        if actual != expected {
            return Err(format!("{} doesn't describe {key}", manifest.display()));
        }
    }

    Ok(())
}

pub(super) fn publish(
    directory: &Path,
    registry: &Registry,
    skip: &[String],
) -> Result<(), String> {
    let cache = registry.cache();
    let skipped = skip
        .iter()
        .map(|namespace| registry.cache_at(namespace))
        .collect::<Vec<_>>();

    let digest = |name: &str, key: &Key| cache.digest(name, key).map_err(|error| error.to_string());

    for Artifact {
        key,
        build,
        references,
        archive,
        manifest,
    } in read(directory)?
    {
        if find(&skipped, &build.name, &key)?.is_some() {
            eprintln!("skipped {key} {}", build.name);
            continue;
        }

        let manifest =
            fs::read(&manifest).map_err(|error| format!("{}: {error}", manifest.display()))?;

        // A different manifest under the key was built elsewhere, perhaps by
        // workflows that no longer verify, so this one replaces it
        let ours = format!("sha256:{}", HEXLOWER.encode(&Sha256::digest(&manifest)));
        match digest(&build.name, &key)? {
            Some(theirs) if theirs == ours => {
                eprintln!("cached {key} {}", build.name);
                continue;
            }
            Some(_) => eprintln!("replacing {key} {}", build.name),
            None => eprintln!("publishing {key} {}", build.name),
        }

        for (reference, name) in &references {
            if digest(name, reference)?.is_none() && find(&skipped, name, reference)?.is_none() {
                return Err(format!(
                    "{key} references {name} {reference}, which isn't published"
                ));
            }
        }

        let output = Output {
            key: &key,
            derivation: &Derivation::Build(build),
            references: &references,
            archive: &archive,
        };

        cache
            .push(&output, &manifest)
            .map_err(|error| error.to_string())?;
    }

    Ok(())
}

pub(super) fn import(directory: &Path, caches: &Caches) -> Result<(), String> {
    let root = Path::new(ROOT);
    let is_root = rustix::process::geteuid().is_root();
    if is_root {
        fs::create_dir_all(STORE_ROOT).map_err(|error| format!("{STORE_ROOT}: {error}"))?;
    }

    let mut store = match is_root {
        true => Store::open(root),
        false => Store::open_read_only(root),
    }
    .map_err(store_error)?;

    let sources = caches.open();
    let is_present = |store: &Store, key: &Key| {
        store
            .path(key)
            .map(|path| path.is_some())
            .map_err(|error| error.to_string())
    };

    for Artifact {
        key,
        build,
        references,
        archive,
        ..
    } in read(directory)?
    {
        if is_present(&store, &key)? {
            continue;
        }

        for (reference, name) in &references {
            if is_present(&store, reference)?
                || cache::substitute(&sources, &mut store, is_root, name, reference)?
            {
                continue;
            }

            return Err(format!(
                "{key} references {reference}, which is neither exported, present, nor cached"
            ));
        }

        let path = output_path(&key, &build.name, &build.version, "out");
        let entry = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("{} has no entry name", path.display()))?;

        let mut hasher = Sha256::new();
        let at = |error: io::Error| format!("{}: {error}", archive.display());
        io::copy(&mut File::open(&archive).map_err(at)?, &mut hasher).map_err(at)?;
        let digest = format!("sha256:{}", HEXLOWER.encode(&hasher.finalize()));

        eprintln!("importing {key} {}@{}", build.name, build.version);
        let references = references.into_keys().collect::<BTreeSet<_>>();
        let archive = File::open(&archive).map_err(at)?;
        if is_root {
            store
                .pull(&key, entry, &digest, &references, archive)
                .map_err(|error| error.to_string())?;
            continue;
        }

        helper_pull(&key, entry, &digest, &references, Box::new(archive))?;
    }

    Ok(())
}

fn read(directory: &Path) -> Result<Vec<Artifact>, String> {
    let at = |error: io::Error| format!("{}: {error}", directory.display());
    let mut artifacts = BTreeMap::new();
    for item in fs::read_dir(directory).map_err(at)? {
        let item = item.map_err(at)?;
        if item.file_name().as_encoded_bytes().starts_with(b".") {
            continue;
        }

        let path = item.path();
        let at = |error: io::Error| format!("{}: {error}", path.display());
        let json = fs::read(path.join(DERIVATION)).map_err(at)?;
        let derivation: Derivation = serde_json::from_slice(&json)
            .map_err(|error| format!("{}: {error}", path.display()))?;

        let key = derivation.key().map_err(|error| error.to_string())?;
        if path.file_name() != Some(key.as_str().as_ref()) {
            return Err(format!("{} holds the derivation of {key}", path.display()));
        }

        let Derivation::Build(build) = derivation else {
            return Err(format!("{} isn't a build output", path.display()));
        };

        let references = fs::read_to_string(path.join(REFERENCES))
            .map_err(at)?
            .lines()
            .map(|line| {
                line.split_once(':')
                    .and_then(|(name, key)| Some((key.parse().ok()?, name.to_string())))
                    .ok_or_else(|| format!("{}: {line:?} isn't name:key", path.display()))
            })
            .collect::<Result<BTreeMap<Key, String>, String>>()?;

        artifacts.insert(
            key.clone(),
            Artifact {
                key,
                build,
                references,
                archive: path.join(ARCHIVE),
                manifest: path.join(MANIFEST),
            },
        );
    }

    let mut ordered = Vec::new();
    let keys = artifacts.keys().cloned().collect::<Vec<_>>();
    for key in keys {
        take(&key, &mut artifacts, &mut ordered);
    }

    Ok(ordered)
}

fn take(key: &Key, artifacts: &mut BTreeMap<Key, Artifact>, ordered: &mut Vec<Artifact>) {
    let Some(artifact) = artifacts.remove(key) else {
        return;
    };

    for reference in artifact.references.keys() {
        take(reference, artifacts, ordered);
    }

    ordered.push(artifact);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn derivation(name: &str) -> Derivation {
        serde_json::from_value(json!({
            "kind": "build", "name": name, "version": "1", "platform": "aarch64-linux",
            "sandbox": "linux-v1", "script": "true", "outputs": ["out"],
        }))
        .unwrap()
    }

    fn write(directory: &Path, entry: &str, derivation: &Derivation, references: &str) {
        let path = directory.join(entry);
        fs::create_dir_all(&path).unwrap();
        fs::write(
            path.join(DERIVATION),
            serde_json::to_vec(derivation).unwrap(),
        )
        .unwrap();
        fs::write(path.join(REFERENCES), references).unwrap();
    }

    #[test]
    fn references_are_read_before_what_uses_them() {
        let directory = tempfile::tempdir().unwrap();
        let mut builds = ["zlib", "zstd"].map(|name| (derivation(name).key().unwrap(), name));
        builds.sort();

        let [(user, user_name), (used, used_name)] = builds;
        let line = format!("{used_name}:{used}\n");
        write(
            directory.path(),
            user.as_str(),
            &derivation(user_name),
            &line,
        );

        write(directory.path(), used.as_str(), &derivation(used_name), "");
        write(directory.path(), ".tmp-1", &derivation("lz4"), "");
        let keys = read(directory.path())
            .unwrap()
            .into_iter()
            .map(|artifact| artifact.key)
            .collect::<Vec<_>>();

        assert_eq!(keys, [used, user]);
    }

    #[test]
    fn validating_refuses_a_manifest_of_another_key() {
        let directory = tempfile::tempdir().unwrap();
        let [ours, theirs] = ["zlib", "zstd"].map(derivation);
        let key = ours.key().unwrap();
        write(directory.path(), key.as_str(), &ours, "");

        let path = directory.path().join(key.as_str());
        fs::write(path.join(ARCHIVE), b"output").unwrap();
        let manifest = |derivation: &Derivation| {
            rootbeer_cache::manifest(&Output {
                key: &derivation.key().unwrap(),
                derivation,
                references: &BTreeMap::new(),
                archive: &path.join(ARCHIVE),
            })
            .unwrap()
        };

        fs::write(path.join(MANIFEST), manifest(&ours)).unwrap();
        validate(directory.path()).unwrap();

        fs::write(path.join(MANIFEST), manifest(&theirs)).unwrap();
        let error = validate(directory.path()).unwrap_err();
        assert!(
            error.ends_with(&format!("doesn't describe {key}")),
            "{error}"
        );
    }

    #[test]
    fn reading_refuses_what_an_export_never_writes() {
        let zlib = derivation("zlib");
        let key = zlib.key().unwrap();
        let fetch: Derivation = serde_json::from_value(
            json!({ "kind": "fetch", "sha256": "0".repeat(64), "urls": ["https://x"] }),
        )
        .unwrap();

        let cases = [
            (
                "a".repeat(32),
                zlib.clone(),
                String::new(),
                "holds the derivation of",
            ),
            (
                fetch.key().unwrap().to_string(),
                fetch,
                String::new(),
                "isn't a build output",
            ),
            (
                key.to_string(),
                zlib.clone(),
                "zlib\n".into(),
                "isn't name:key",
            ),
            (
                key.to_string(),
                zlib,
                "zlib:nope\n".into(),
                "isn't name:key",
            ),
        ];

        for (entry, derivation, references, reason) in cases {
            let directory = tempfile::tempdir().unwrap();
            write(directory.path(), &entry, &derivation, &references);
            let error = read(directory.path()).err().unwrap();
            assert!(error.contains(reason), "{error}");
        }
    }
}
