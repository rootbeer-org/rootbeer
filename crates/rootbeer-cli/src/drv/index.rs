use super::cache::Registry;
use super::{load, Sources};
use rootbeer_drv::{Key, Platform};
use rootbeer_eval::{Graph, Target};
use rootbeer_trust::{Output, Package};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

pub(super) fn index(
    sources: &Sources,
    directory: &Path,
    registry: &Registry,
) -> Result<(), String> {
    let (catalog, hosts) = load(sources)?;
    let targets = catalog.targets();
    let graph = catalog
        .evaluate(&hosts, &targets)
        .map_err(|error| error.to_string())?;

    let cache = registry.cache();
    let packages = packages(
        &graph,
        &targets,
        |name, platform| catalog.default_version(name, platform).map(str::to_string),
        |name, keys| cache.digests(name, keys).map_err(|error| error.to_string()),
    )?;

    fs::create_dir_all(directory).map_err(|error| format!("{}: {error}", directory.display()))?;
    for package in packages {
        let path = directory.join(format!("{}.json", package.name));
        let json = serde_json::to_vec(&package).map_err(|error| error.to_string())?;

        serde_json::from_slice::<Package>(&json)
            .map_err(|error| format!("{}: {error}", package.name))?;

        fs::write(&path, json).map_err(|error| format!("{}: {error}", path.display()))?;
        eprintln!("indexed {}", package.name);
    }

    Ok(())
}

pub(super) fn sign(repository: &Path, key: &Path, entries: Option<&Path>) -> Result<(), String> {
    let mut packages = Vec::new();
    if let Some(entries) = entries {
        let at = |error: std::io::Error| format!("{}: {error}", entries.display());
        for item in fs::read_dir(entries).map_err(at)? {
            let path = item.map_err(at)?.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }

            let bytes = fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
            let package: Package = serde_json::from_slice(&bytes)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            packages.push(package);
        }

        if packages.is_empty() {
            return Err(format!("{} holds no entries", entries.display()));
        }
    }

    rootbeer_trust::sign(repository, key, &packages).map_err(|error| error.to_string())
}

fn packages(
    graph: &Graph,
    targets: &[Target],
    default: impl Fn(&str, Platform) -> Option<String>,
    mut digests: impl FnMut(&str, &[&Key]) -> Result<Vec<Option<String>>, String>,
) -> Result<Vec<Package>, String> {
    let mut by_name = BTreeMap::<&str, Vec<(&Target, &rootbeer_eval::Package)>>::new();
    for target in targets {
        let evaluated = graph
            .packages
            .get(target)
            .ok_or_else(|| format!("{target} did not evaluate"))?;

        by_name
            .entry(&target.name)
            .or_default()
            .push((target, evaluated));
    }

    let mut packages = Vec::new();
    for (name, evaluated) in by_name {
        let keys = evaluated
            .iter()
            .map(|(_, package)| &package.build)
            .collect::<Vec<_>>();

        let digests = digests(name, &keys)?;
        let mut package = Package {
            name: name.to_string(),
            description: String::new(),
            license: String::new(),
            default: BTreeMap::new(),
            versions: BTreeMap::new(),
            retired: BTreeMap::new(),
        };

        for ((target, evaluated), digest) in evaluated.into_iter().zip(digests) {
            let Some(manifest) = digest else {
                continue;
            };

            let metadata = &evaluated.metadata;
            let is_default =
                default(name, target.platform).as_deref() == Some(target.version.as_str());
            if is_default || package.description.is_empty() {
                package.description.clone_from(&metadata.description);
                package.license.clone_from(&metadata.license);
            }

            if is_default {
                package
                    .default
                    .insert(target.platform, target.version.clone());
            }

            let output = Output {
                key: evaluated.build.clone(),
                manifest,
                bins: metadata.bins.clone(),
                apps: metadata.apps.clone(),
            };

            package
                .versions
                .entry(target.version.clone())
                .or_default()
                .insert(target.platform, output);
        }

        if !package.versions.is_empty() {
            packages.push(package);
        }
    }

    Ok(packages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rootbeer_eval::Metadata;

    fn target(name: &str, version: &str, platform: Platform) -> Target {
        Target {
            name: name.into(),
            version: version.into(),
            platform,
        }
    }

    #[test]
    fn only_published_outputs_are_indexed_and_defaults_describe_the_package() {
        let [linux, macos] = [Platform::Aarch64Linux, Platform::Aarch64Macos];
        let targets = [
            target("zstd", "1.5.6", linux),
            target("zstd", "1.5.7", linux),
            target("zstd", "1.5.7", macos),
            target("lz4", "1.10.0", linux),
        ];

        let mut graph = Graph::default();
        for (index, target) in targets.iter().enumerate() {
            let package = rootbeer_eval::Package {
                build: char::from(b'a' + u8::try_from(index).unwrap())
                    .to_string()
                    .repeat(32)
                    .parse()
                    .unwrap(),
                check: None,
                metadata: Metadata {
                    description: format!("{} {}", target.name, target.version),
                    homepage: String::new(),
                    license: "BSD".into(),
                    aliases: Vec::new(),
                    maintainers: Vec::new(),
                    bins: [target.name.clone()].into(),
                    apps: BTreeMap::new(),
                },
            };
            graph.packages.insert(target.clone(), package);
        }

        let published = |name: &str, keys: &[&Key]| {
            let digest = |key: &&Key| {
                let is_published = name == "zstd" && !key.as_str().starts_with('c');
                is_published.then(|| format!("sha256:{}", "0".repeat(64)))
            };
            Ok(keys.iter().map(digest).collect())
        };

        let default = |_: &str, _: Platform| Some("1.5.7".to_string());
        let packages = packages(&graph, &targets, default, published).unwrap();
        let [zstd] = packages.as_slice() else {
            panic!("{packages:?}");
        };

        assert_eq!(zstd.description, "zstd 1.5.7");
        assert_eq!(zstd.default, BTreeMap::from([(linux, "1.5.7".into())]));
        assert_eq!(zstd.versions.keys().collect::<Vec<_>>(), ["1.5.6", "1.5.7"]);
        assert!(zstd
            .versions
            .values()
            .all(|outputs| outputs.keys().eq([&linux])));
        assert!(zstd.output(linux, None).unwrap().bins.contains("zstd"));
    }
}
