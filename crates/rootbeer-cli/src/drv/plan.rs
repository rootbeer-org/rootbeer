use super::cache::{self, Caches};
use super::{load, target, Sources};
use rootbeer_drv::{DependencyKind, Derivation, Key, Platform};
use rootbeer_eval::{Graph, Target};
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
    let targets = packages
        .iter()
        .map(|package| target(&catalog, package, platform))
        .collect::<Result<Vec<_>, _>>()?;

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
}
