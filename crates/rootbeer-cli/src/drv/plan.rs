use super::cache::{self, Caches};
use super::{load, target, Sources};
use rootbeer_drv::{Dependency, DependencyKind, Derivation, Key, Platform};
use rootbeer_eval::{Graph, Target};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Serialize)]
struct Plan<'a> {
    platform: Platform,
    /// What to build, with the key each build must produce, so CI can attest
    /// a job's own output and nothing else
    levels: Vec<Vec<Step<'a>>>,
    /// Every requested package and its build key, cached or not, so CI can
    /// promote them without evaluating again
    targets: Vec<Planned<'a>>,
}

#[derive(Serialize)]
struct Planned<'a> {
    name: &'a str,
    version: &'a str,
    key: &'a Key,
}

#[derive(Serialize)]
struct Step<'a> {
    #[serde(flatten)]
    planned: Planned<'a>,
    /// Planned keys this build imports from earlier levels, which are its
    /// dependencies and the references of those, and nothing else
    needs: BTreeSet<&'a Key>,
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

    let sources = caches.open();
    let levels = levels(&graph, &targets, |name, key| {
        cache::is_available(&sources, name, key)
    })?;

    let targets = targets
        .iter()
        .map(|target| planned(&graph, target))
        .collect::<Result<Vec<_>, String>>()?;

    let plan = Plan {
        platform,
        levels,
        targets,
    };
    let json = serde_json::to_string(&plan).map_err(|error| error.to_string())?;
    println!("{json}");
    Ok(())
}

fn planned<'a>(graph: &'a Graph, target: &'a Target) -> Result<Planned<'a>, String> {
    let package = graph
        .packages
        .get(target)
        .ok_or_else(|| format!("{target} did not evaluate"))?;

    Ok(Planned {
        name: &target.name,
        version: &target.version,
        key: &package.build,
    })
}

fn levels<'a>(
    graph: &'a Graph,
    targets: &'a [Target],
    is_cached: impl FnMut(&str, &Key) -> Result<bool, String>,
) -> Result<Vec<Vec<Step<'a>>>, String> {
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

    let mut levels = BTreeMap::<usize, Vec<Step>>::new();
    for (target, level) in &planner.levels {
        if let Some(level) = level {
            let step = Step {
                planned: planned(graph, target)?,
                needs: planner.needs(target)?,
            };

            levels.entry(*level).or_default().push(step);
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

    /// Building needs the build's own dependencies other than runtime ones,
    /// and importing each of those needs its references. Cached ones are
    /// substituted instead, along with what they reference.
    fn needs(&self, target: &'a Target) -> Result<BTreeSet<&'a Key>, String> {
        let mut needs = BTreeSet::new();
        let mut pending = self
            .dependencies(target)?
            .filter(|dependency| dependency.kind != DependencyKind::Runtime)
            .collect::<Vec<_>>();

        while let Some(dependency) = pending.pop() {
            let below = self
                .builds
                .get(&dependency.key)
                .copied()
                .ok_or_else(|| format!("{target}'s {} did not evaluate", dependency.name))?;

            let is_planned = matches!(self.levels.get(below), Some(Some(_)));
            if !is_planned || !needs.insert(&dependency.key) {
                continue;
            }

            pending.extend(
                self.dependencies(below)?
                    .filter(|dependency| dependency.kind != DependencyKind::Build),
            );
        }

        Ok(needs)
    }

    fn dependencies(
        &self,
        target: &'a Target,
    ) -> Result<impl Iterator<Item = &'a Dependency>, String> {
        let package = self
            .graph
            .packages
            .get(target)
            .ok_or_else(|| format!("{target} did not evaluate"))?;

        let Some(Derivation::Build(build)) = self.graph.derivations.get(&package.build) else {
            return Err(format!("{target} has no build derivation"));
        };

        Ok(build.dependencies.iter())
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
        let names = |levels: &[Vec<Step>]| {
            levels
                .iter()
                .map(|level| {
                    level
                        .iter()
                        .map(|step| format!("{}@{}", step.planned.name, step.planned.version))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };

        let key = |target: &Target| graph.packages[target].build.clone();
        let needs = |levels: &[Vec<Step>], name: &str| {
            levels
                .iter()
                .flatten()
                .find(|step| step.planned.name == name)
                .map(|step| {
                    step.needs
                        .iter()
                        .map(|key| (*key).clone())
                        .collect::<BTreeSet<_>>()
                })
                .unwrap()
        };

        let uncached = levels(&graph, &targets, |_, _| Ok(false)).unwrap();
        // zlib's build tool isn't a reference, and docs is only needed at runtime
        assert_eq!(
            needs(&uncached, "zstd"),
            BTreeSet::from([key(&cmake4), key(&xz), key(&zlib)])
        );
        assert_eq!(
            names(&uncached),
            [
                vec!["cmake@3", "cmake@4", "xz@5"],
                vec!["zlib@1"],
                vec!["zstd@1"]
            ]
        );

        let zlib = key(&zlib);
        let cached = levels(&graph, &targets, |_, key| Ok(*key == zlib)).unwrap();
        assert_eq!(names(&cached), [vec!["cmake@4", "xz@5"], vec!["zstd@1"]]);
        assert_eq!(
            needs(&cached, "zstd"),
            BTreeSet::from([key(&cmake4), key(&xz)])
        );
    }
}
