use crate::{Error, Package, outside_runtime, runtime, tuf};
use jiff::{SignedDuration, Timestamp};
use rootbeer_drv::is_package_name;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tough::editor::RepositoryEditor;
use tough::editor::signed::PathExists;
use tough::key_source::{KeySource, LocalKeySource};
use tough::schema::{Root, Signed, Target};
use tough::{
    ExpirationEnforcement, FilesystemTransport, IntoVec, Repository, RepositoryLoader, TargetName,
    Transport, TransportError, TransportErrorKind, TransportStream, async_trait,
};
use url::Url;

const TIMESTAMP: SignedDuration = SignedDuration::from_hours(336);
const TARGETS: SignedDuration = SignedDuration::from_hours(720);

pub fn sign(repository: &Path, key: &Path, packages: &[Package]) -> Result<(), Error> {
    outside_runtime()?;
    let mut names = BTreeSet::new();
    for package in packages {
        if !is_package_name(&package.name) {
            return Err(Error::Name(format!(
                "{:?} isn't a package name",
                package.name
            )));
        }

        if !names.insert(&package.name) {
            return Err(Error::Invalid(format!("{} is given twice", package.name)));
        }
    }

    runtime()?.block_on(sign_into(repository, key, packages))
}

async fn sign_into(repository: &Path, key: &Path, packages: &[Package]) -> Result<(), Error> {
    let metadata = repository.join("metadata");
    let targets = repository.join("targets");
    fs::create_dir_all(&targets).map_err(|error| at(&targets, error))?;

    let roots = roots(&metadata)?;
    let Some((_, newest)) = roots.first() else {
        return Err(Error::Invalid(format!(
            "{} has no <version>.root.json from the root ceremony",
            metadata.display()
        )));
    };

    let root: Signed<Root> = serde_json::from_slice(&read(newest)?)
        .map_err(|error| Error::Invalid(format!("{}: {error}", newest.display())))?;

    if root.signed.expires < expiring(TIMESTAMP)? {
        return Err(Error::Invalid(format!(
            "the root expires at {}, so renew it with the offline keys first",
            root.signed.expires
        )));
    }

    let keys: Vec<Box<dyn KeySource>> = vec![Box::new(LocalKeySource {
        path: key.to_path_buf(),
    })];

    let mut previous = BTreeMap::new();
    let mut listed = BTreeMap::new();
    let (mut editor, version) = match is_fresh(&metadata)? {
        true => (RepositoryEditor::new(newest).await.map_err(tuf)?, 1),
        false => {
            let current = current(&roots, &metadata, &targets).await?;
            listed = listed_targets(&current);
            for package in packages {
                let name = TargetName::new(format!("{}.json", package.name)).map_err(tuf)?;
                let Some(stream) = current.read_target(&name).await.map_err(tuf)? else {
                    continue;
                };

                let bytes = stream.into_vec().await.map_err(tuf)?;
                let entry: Map<String, Value> = serde_json::from_slice(&bytes)
                    .map_err(|error| Error::Invalid(format!("{}: {error}", name.raw())))?;
                previous.insert(package.name.clone(), entry);
            }

            let version = current
                .targets()
                .signed
                .version
                .max(current.snapshot().signed.version)
                .max(current.timestamp().signed.version)
                .get()
                .checked_add(1)
                .ok_or_else(|| Error::Invalid("metadata versions are exhausted".into()))?;

            let editor = RepositoryEditor::from_repo(newest, current)
                .await
                .map_err(tuf)?;

            (editor, version)
        }
    };

    let staging = tempfile::tempdir().map_err(|error| Error::Invalid(error.to_string()))?;
    let now = Timestamp::now().as_second();
    let mut staged = Vec::new();
    let mut aliases = BTreeMap::new();

    for package in packages {
        let merged = merge(previous.remove(&package.name), package, now)?;
        let name = format!("{}.json", package.name);
        let path = staging.path().join(&name);
        let json =
            serde_json::to_vec(&merged).map_err(|error| Error::Invalid(error.to_string()))?;

        let read = serde_json::from_slice::<Package>(&json)
            .map_err(|error| Error::Invalid(format!("{name} after merging: {error}")))?;

        fs::write(&path, json).map_err(|error| at(&path, error))?;
        let mut target = Target::from_path(&path)
            .await
            .map_err(|error| Error::Invalid(error.to_string()))?;

        // Clients resolve an alias from the signed targets, without reading
        // every entry.
        if !read.aliases.is_empty() {
            target
                .custom
                .insert("aliases".into(), serde_json::json!(read.aliases));
        }

        aliases.insert(package.name.clone(), read.aliases);

        let name = TargetName::new(name).map_err(tuf)?;
        editor.add_target(name.clone(), target).map_err(tuf)?;
        staged.push((path, name));
    }

    check_aliases(&aliases)?;

    // A package missing from this run, such as one pdr dropped, gives up an
    // alias a signed one now claims, so publishing never wedges on it.
    let claimed: BTreeSet<&String> = aliases.keys().chain(aliases.values().flatten()).collect();
    for (name, mut target) in listed {
        let own = aliases_of(&target);
        let kept: BTreeSet<String> = own
            .iter()
            .filter(|alias| !claimed.contains(alias))
            .cloned()
            .collect();
        if aliases.contains_key(&name) || kept == own {
            continue;
        }

        match kept.is_empty() {
            true => target.custom.remove("aliases"),
            false => target
                .custom
                .insert("aliases".into(), serde_json::json!(kept)),
        };

        let name = TargetName::new(format!("{name}.json")).map_err(tuf)?;
        editor.add_target(name, target).map_err(tuf)?;
    }

    let version = version
        .try_into()
        .map_err(|_| Error::Invalid("metadata versions start at 1".into()))?;

    editor
        .snapshot_version(version)
        .snapshot_expires(expiring(TARGETS)?)
        .timestamp_version(version)
        .timestamp_expires(expiring(TIMESTAMP)?);

    editor.targets_version(version).map_err(tuf)?;
    editor.targets_expires(expiring(TARGETS)?).map_err(tuf)?;

    let signed = editor.sign(&keys).await.map_err(tuf)?;
    for (path, name) in &staged {
        signed
            .copy_target(path, &targets, PathExists::Skip, Some(name))
            .await
            .map_err(tuf)?;
    }

    signed.write(&metadata).await.map_err(tuf)
}

async fn current(
    roots: &[(u64, PathBuf)],
    metadata: &Path,
    targets: &Path,
) -> Result<Repository, Error> {
    let mut first = None;
    for (version, path) in roots {
        let datastore = tempfile::tempdir().map_err(|error| Error::Invalid(error.to_string()))?;
        let loaded = RepositoryLoader::new(&read(path)?, directory(metadata)?, directory(targets)?)
            .transport(Pinned { newest: *version })
            .expiration_enforcement(ExpirationEnforcement::Unsafe)
            .datastore(datastore.path())
            .load()
            .await;

        match loaded {
            Ok(repository) => return Ok(repository),
            Err(error) => {
                first.get_or_insert(error);
            }
        }
    }

    Err(first.map_or_else(|| Error::Invalid("no root to load with".into()), tuf))
}

#[derive(Debug, Clone, Copy)]
struct Pinned {
    newest: u64,
}

#[async_trait]
impl Transport for Pinned {
    async fn fetch(&self, url: Url) -> Result<TransportStream, TransportError> {
        let version = url
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .and_then(|name| name.strip_suffix(".root.json"))
            .and_then(|version| version.parse::<u64>().ok());

        if version.is_some_and(|version| version > self.newest) {
            return Err(TransportError::new(TransportErrorKind::FileNotFound, url));
        }

        FilesystemTransport.fetch(url).await
    }
}

/// Each package the repository lists, by name.
fn listed_targets(repository: &Repository) -> BTreeMap<String, Target> {
    let mut listed = BTreeMap::new();
    for (name, target) in repository.all_targets() {
        let Some(name) = name.raw().strip_suffix(".json") else {
            continue;
        };

        if is_package_name(name) {
            listed.insert(name.to_string(), target.clone());
        }
    }

    listed
}

fn aliases_of(target: &Target) -> BTreeSet<String> {
    target
        .custom
        .get("aliases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// Among the packages signed together, an alias must name one package, and
/// never shadow a package's own name.
fn check_aliases(aliases: &BTreeMap<String, BTreeSet<String>>) -> Result<(), Error> {
    let mut owners = BTreeMap::new();
    for (name, listed) in aliases {
        for alias in listed {
            if aliases.contains_key(alias) {
                return Err(Error::Invalid(format!(
                    "{name} has the alias {alias}, which is also a package"
                )));
            }

            if let Some(other) = owners.insert(alias, name) {
                return Err(Error::Invalid(format!(
                    "{alias} is an alias of both {other} and {name}"
                )));
            }
        }
    }

    Ok(())
}

fn merge(
    previous: Option<Map<String, Value>>,
    package: &Package,
    now: i64,
) -> Result<Map<String, Value>, Error> {
    let invalid = |reason: &str| Error::Invalid(format!("{}: {reason}", package.name));
    let Value::Object(mut merged) =
        serde_json::to_value(package).map_err(|error| invalid(&error.to_string()))?
    else {
        return Err(invalid("an entry isn't an object"));
    };

    // Only the signer dates an entry.
    merged.remove("added");
    merged.remove("updated");
    let Some(previous) = previous else {
        stamp(&mut merged, true, now);
        return Ok(merged);
    };

    let listed =
        |map: &Map<String, Value>| (map.get("versions").cloned(), map.get("default").cloned());
    let was = listed(&previous);

    let object = |map: &Map<String, Value>, field: &str| {
        map.get(field)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    };

    let mut retired = object(&previous, "retired");
    retired.extend(object(&merged, "retired"));

    let mut versions = object(&merged, "versions");
    for (version, outputs) in object(&previous, "versions") {
        let Some(outputs) = outputs.as_object() else {
            continue;
        };

        let Some(kept) = versions
            .entry(version)
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
        else {
            return Err(invalid("a version isn't an object"));
        };

        for (platform, output) in outputs {
            let Some(current) = kept.get(platform) else {
                kept.insert(platform.clone(), output.clone());
                continue;
            };

            if current.get("key") != output.get("key")
                && let (Some(key), Some(manifest)) = (
                    output.get("key").and_then(Value::as_str),
                    output.get("manifest"),
                )
            {
                retired.insert(key.to_string(), manifest.clone());
            }
        }
    }

    let mut default = object(&merged, "default");
    for (platform, version) in object(&previous, "default") {
        let is_listed = version
            .as_str()
            .and_then(|version| versions.get(version))
            .and_then(Value::as_object)
            .is_some_and(|outputs| outputs.contains_key(&platform));

        if is_listed {
            default.entry(platform).or_insert(version);
        }
    }

    for (field, value) in previous {
        merged.entry(field).or_insert(value);
    }

    merged.insert("versions".into(), Value::Object(versions));
    merged.insert("default".into(), Value::Object(default));
    if !retired.is_empty() {
        merged.insert("retired".into(), Value::Object(retired));
    }

    let is_changed = listed(&merged) != was;
    stamp(&mut merged, is_changed, now);
    Ok(merged)
}

fn stamp(entry: &mut Map<String, Value>, is_changed: bool, now: i64) {
    entry.entry("added").or_insert(now.into());
    if is_changed {
        entry.insert("updated".into(), now.into());
    }

    entry.entry("updated").or_insert(now.into());
}

fn is_fresh(metadata: &Path) -> Result<bool, Error> {
    let entries = fs::read_dir(metadata).map_err(|error| at(metadata, error))?;
    for entry in entries {
        let name = entry.map_err(|error| at(metadata, error))?.file_name();
        if !name.to_string_lossy().ends_with(".root.json") {
            return Ok(false);
        }
    }

    Ok(true)
}

fn roots(metadata: &Path) -> Result<Vec<(u64, PathBuf)>, Error> {
    let entries = fs::read_dir(metadata).map_err(|error| at(metadata, error))?;
    let mut roots = Vec::new();
    for entry in entries {
        let path = entry.map_err(|error| at(metadata, error))?.path();
        let version = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".root.json"))
            .and_then(|version| version.parse::<u64>().ok());

        if let Some(version) = version {
            roots.push((version, path));
        }
    }

    roots.sort_by_key(|(version, _)| std::cmp::Reverse(*version));
    Ok(roots)
}

fn directory(path: &Path) -> Result<Url, Error> {
    let absolute = fs::canonicalize(path).map_err(|error| at(path, error))?;
    Url::from_directory_path(&absolute)
        .map_err(|()| Error::Url(format!("{} isn't a directory path", absolute.display())))
}

fn expiring(after: SignedDuration) -> Result<Timestamp, Error> {
    Timestamp::now()
        .checked_add(after)
        .map_err(|error| Error::Invalid(error.to_string()))
}

fn read(path: &Path) -> Result<Vec<u8>, Error> {
    fs::read(path).map_err(|error| at(path, error))
}

fn at(path: &Path, error: io::Error) -> Error {
    Error::Invalid(format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rootbeer_drv::Platform;

    fn zstd(versions: &[&str]) -> Package {
        let output: crate::Output = serde_json::from_value(serde_json::json!({
            "key": "a".repeat(32),
            "manifest": format!("sha256:{}", "0".repeat(64)),
        }))
        .unwrap();

        let platform = Platform::Aarch64Linux;
        Package {
            name: "zstd".into(),
            default: BTreeMap::from([(platform, versions[0].into())]),
            versions: versions
                .iter()
                .map(|version| {
                    (
                        version.to_string(),
                        BTreeMap::from([(platform, output.clone())]),
                    )
                })
                .collect(),
            ..Package::default()
        }
    }

    fn dates(entry: &Map<String, Value>) -> (Option<i64>, Option<i64>) {
        let date = |field| entry.get(field).and_then(Value::as_i64);
        (date("added"), date("updated"))
    }

    #[test]
    fn added_is_kept_and_updated_moves_only_with_versions_or_defaults() {
        let first = merge(None, &zstd(&["1.0"]), 100).unwrap();
        assert_eq!(dates(&first), (Some(100), Some(100)));

        let mut described = zstd(&["1.0"]);
        described.description = "zstd compresses better".into();
        let same = merge(Some(first.clone()), &described, 200).unwrap();
        assert_eq!(dates(&same), (Some(100), Some(100)));

        let newer = merge(Some(same), &zstd(&["2.0", "1.0"]), 300).unwrap();
        assert_eq!(dates(&newer), (Some(100), Some(300)));

        let dropped = merge(Some(newer.clone()), &zstd(&["2.0"]), 400).unwrap();
        assert_eq!(dates(&dropped), (Some(100), Some(300)));

        let rolled_back = merge(Some(newer), &zstd(&["1.0", "2.0"]), 500).unwrap();
        assert_eq!(dates(&rolled_back), (Some(100), Some(500)));

        let mut undated = dropped;
        undated.retain(|field, _| field != "added" && field != "updated");
        let stamped = merge(Some(undated), &zstd(&["2.0"]), 600).unwrap();
        assert_eq!(dates(&stamped), (Some(600), Some(600)));
    }

    #[test]
    fn dates_an_entry_brings_are_ignored() {
        let previous = merge(None, &zstd(&["1.0"]), 100).unwrap();
        let mut dated = zstd(&["1.0"]);
        dated.added = Some(5);
        dated.updated = Some(6);

        assert_eq!(
            dates(&merge(Some(previous), &dated, 300).unwrap()),
            (Some(100), Some(100))
        );
        assert_eq!(
            dates(&merge(None, &dated, 300).unwrap()),
            (Some(300), Some(300))
        );
    }
}
