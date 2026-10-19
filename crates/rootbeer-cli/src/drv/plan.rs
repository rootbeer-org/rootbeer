use super::cache::{self, Caches};
use super::{load, target, Sources};
use rootbeer_drv::{DependencyKind, Derivation, Key, Platform};
use rootbeer_eval::{Catalog, Graph, Target};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Serialize)]
struct Plan {
    platform: Platform,
    levels: Vec<Vec<String>>,
}

pub(super) fn plan(
    sources: &Sources,
    packages: &[String],
    platform: Platform,
    caches: &Caches,
) -> Result<(), String> {
    let (catalog, hosts) = load(sources)?;
    let targets = targets(&catalog, packages, platform)?;

    let graph = catalog
        .evaluate(&hosts, &targets)
        .map_err(|error| error.to_string())?;

    let caches = caches.open();
    let levels = levels(&graph, &targets, |name, key| {
        Ok(cache::find(&caches, name, key)?.is_some())
    })?;

    let plan = Plan { platform, levels };
    let json = serde_json::to_string(&plan).map_err(|error| error.to_string())?;
    println!("{json}");
    Ok(())
}

fn targets(
    catalog: &Catalog,
    packages: &[String],
    platform: Platform,
) -> Result<Vec<Target>, String> {
    if packages.is_empty() {
        let mut targets = catalog.targets();
        targets.retain(|target| target.platform == platform);
        return Ok(targets);
    }

    let mut targets = Vec::new();
    for package in packages {
        let (name, version) = match package.split_once('@') {
            Some((name, version)) => (name, Some(version)),
            None => (package.as_str(), None),
        };

        if !catalog.contains(name) {
            return Err(format!("{name} is not in the catalog"));
        }

        if version.is_none() && catalog.default_version(name, platform).is_none() {
            eprintln!("{name} has no default version on {platform}, left out");
            continue;
        }

        targets.push(target(catalog, package, platform)?);
    }

    Ok(targets)
}

fn levels(
    graph: &Graph,
    targets: &[Target],
    is_cached: impl FnMut(&str, &Key) -> Result<bool, String>,
) -> Result<Vec<Vec<String>>, String> {
    let mut planner = Planner {
        graph,
        is_cached,
        builds: graph
            .packages
            .iter()
            .map(|(target, package)| (&package.build, target))
            .collect(),
        levels: BTreeMap::new(),
    };

    for target in targets {
        planner.level(target)?;
    }

    let mut levels = BTreeMap::<usize, Vec<String>>::new();
    for (target, level) in &planner.levels {
        if let Some(level) = level {
            let name = format!("{}@{}", target.name, target.version);
            levels.entry(*level).or_default().push(name);
        }
    }

    Ok(levels.into_values().collect())
}

struct Planner<'a, F> {
    graph: &'a Graph,
    is_cached: F,
    builds: BTreeMap<&'a Key, &'a Target>,
    levels: BTreeMap<&'a Target, Option<usize>>,
}

impl<'a, F: FnMut(&str, &Key) -> Result<bool, String>> Planner<'a, F> {
    fn level(&mut self, target: &'a Target) -> Result<Option<usize>, String> {
        if let Some(level) = self.levels.get(target) {
            return Ok(*level);
        }

        let package = self
            .graph
            .packages
            .get(target)
            .ok_or_else(|| format!("{target} did not evaluate"))?;

        let Some(Derivation::Build(build)) = self.graph.derivations.get(&package.build) else {
            return Err(format!("{target} has no build derivation"));
        };

        if (self.is_cached)(&build.name, &package.build)? {
            self.levels.insert(target, None);
            return Ok(None);
        }

        let mut level = 0;
        for dependency in &build.dependencies {
            if dependency.kind == DependencyKind::Runtime {
                continue;
            }

            let below = self
                .builds
                .get(&dependency.key)
                .copied()
                .ok_or_else(|| format!("{target}'s {} did not evaluate", dependency.name))?;

            if let Some(below) = self.level(below)? {
                level = level.max(below.saturating_add(1));
            }
        }

        self.levels.insert(target, Some(level));
        Ok(Some(level))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rootbeer_drv::{Build, Dependency};
    use rootbeer_eval::{Metadata, Package};

    fn add(
        graph: &mut Graph,
        name: &str,
        version: &str,
        dependencies: &[(&Target, DependencyKind)],
    ) -> Target {
        let platform = Platform::try_from("x86_64-linux".to_string()).unwrap();
        let dependencies = dependencies
            .iter()
            .map(|(target, kind)| Dependency {
                key: graph.packages[*target].build.clone(),
                name: target.name.clone(),
                kind: *kind,
            })
            .collect();

        let build = Derivation::Build(Build {
            name: name.into(),
            version: version.into(),
            platform,
            sandbox: "linux-v1".into(),
            allow: Default::default(),
            host: Default::default(),
            inputs: Default::default(),
            dependencies,
            env: Default::default(),
            script: "make".into(),
            outputs: ["out".to_string()].into(),
        });

        let key = build.key().unwrap();
        graph.derivations.insert(key.clone(), build);

        let target = Target {
            name: name.into(),
            version: version.into(),
            platform,
        };

        let metadata = Metadata {
            description: String::new(),
            homepage: String::new(),
            license: String::new(),
            aliases: Vec::new(),
            maintainers: Vec::new(),
            bins: Default::default(),
            apps: Default::default(),
        };

        graph.packages.insert(
            target.clone(),
            Package {
                build: key,
                check: None,
                metadata,
            },
        );

        target
    }

    #[test]
    fn levels_stop_at_cached_packages_and_ignore_runtime_dependencies() {
        use DependencyKind::{Build, Linked, Runtime};

        let mut graph = Graph::default();
        let cmake3 = add(&mut graph, "cmake", "3", &[]);
        let cmake4 = add(&mut graph, "cmake", "4", &[]);
        let xz = add(&mut graph, "xz", "5", &[]);
        let docs = add(&mut graph, "docs", "1", &[]);
        let zlib = add(&mut graph, "zlib", "1", &[(&cmake3, Build)]);
        let zstd = add(
            &mut graph,
            "zstd",
            "1",
            &[
                (&cmake4, Build),
                (&docs, Runtime),
                (&xz, Build),
                (&xz, Linked),
                (&zlib, Linked),
            ],
        );

        let targets = [zstd.clone(), zstd];
        let uncached = levels(&graph, &targets, |_, _| Ok(false)).unwrap();
        assert_eq!(
            uncached,
            [
                vec!["cmake@3", "cmake@4", "xz@5"],
                vec!["zlib@1"],
                vec!["zstd@1"]
            ]
        );

        let zlib = graph.packages[&zlib].build.clone();
        let cached = levels(&graph, &targets, |_, key| Ok(*key == zlib)).unwrap();
        assert_eq!(cached, [vec!["cmake@4", "xz@5"], vec!["zstd@1"]]);
    }

    #[test]
    fn targets_leave_out_other_platforms_but_not_unknown_names() {
        let recipe = format!(
            r#"return {{
                name = "mac", description = "", homepage = "", default_license = "MIT",
                source = {{ url = "https://example.com/mac.tar.gz" }},
                build = {{ backend = "custom", steps = {{}} }},
                platforms = {{ ["aarch64-macos"] = {{ default_version = "1" }} }},
                versions = {{ ["1"] = {{ digests = {{ ["aarch64-macos"] = "{}" }} }} }},
            }}"#,
            "0".repeat(64)
        );
        let catalog = Catalog::parse([("mac.lua", recipe.as_str())]).unwrap();
        let linux = Platform::try_from("x86_64-linux".to_string()).unwrap();
        let names = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };

        assert!(targets(&catalog, &names(&["mac"]), linux)
            .unwrap()
            .is_empty());
        assert_eq!(
            targets(&catalog, &names(&["typo"]), linux).unwrap_err(),
            "typo is not in the catalog"
        );
        let pinned = targets(&catalog, &names(&["mac@1"]), linux).unwrap();
        assert_eq!(pinned.len(), 1);
    }

    #[test]
    fn naming_nothing_plans_every_declared_version_on_the_platform() {
        let digest = "0".repeat(64);
        let recipe = format!(
            r#"return {{
                name = "mac", description = "", homepage = "", default_license = "MIT",
                source = {{ url = "https://example.com/mac.tar.gz" }},
                build = {{ backend = "custom", steps = {{}} }},
                platforms = {{ ["aarch64-macos"] = {{ default_version = "2" }} }},
                versions = {{
                    ["1"] = {{ digests = {{ ["aarch64-macos"] = "{digest}" }} }},
                    ["2"] = {{ digests = {{ ["aarch64-macos"] = "{digest}" }} }},
                }},
            }}"#
        );
        let catalog = Catalog::parse([("mac.lua", recipe.as_str())]).unwrap();
        let platform = |name: &str| Platform::try_from(name.to_string()).unwrap();

        let macos = targets(&catalog, &[], platform("aarch64-macos")).unwrap();
        let versions: Vec<_> = macos.iter().map(|target| target.version.as_str()).collect();
        assert_eq!(versions, ["1", "2"]);
        assert!(targets(&catalog, &[], platform("x86_64-linux"))
            .unwrap()
            .is_empty());
    }
}
