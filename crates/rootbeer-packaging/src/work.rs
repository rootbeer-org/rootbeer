//! A runner's work plan: every package it builds, recovers from a verified run, or reuses from
//! GHCR, fixed once so each job executes its part without planning again.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use rootbeer_package::distribution::{read_record, verify_record};
use rootbeer_package::ghcr::{tagged_manifest, Tagged};
use rootbeer_package::repository::{Repository, RepositoryPin, RepositoryResolver};
use rootbeer_package::{BuildArtifact, LockedSource, PackageCatalog, ResolveContext};

use crate::{plan_packages, BuildCache, BuildOptions, PackageTask};

/// Dependency levels a plan may use; CI runs one job per level.
pub const LEVELS: usize = 8;

const SCHEMA: u32 = 1;
const RECORD_MEDIA_TYPE: &str = "application/vnd.rootbeer.package.record.v1+json";

/// Where packages are published: the signed PDR, and the GHCR namespace holding each build.
#[derive(Debug, Clone)]
pub struct Distribution {
    pub pdr: Repository,
    /// Builds of `name` are pushed to `ghcr.io/<registry>/<name>`.
    pub registry: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkPlan {
    pub schema: u32,
    pub system: String,
    /// Identity of the runner image the plan was made on.
    pub context: String,
    pub catalog_sha256: String,
    /// Engine generation, so a plan is executed only by the engine that made it.
    pub engine: String,
    /// The PDR root every task's published dependencies come from.
    pub pdr: RepositoryPin,
    pub registry: String,
    pub tasks: Vec<WorkTask>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkTask {
    pub package: String,
    pub name: String,
    /// Input key the task's result is published under.
    pub key: String,
    pub work: Work,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Work {
    /// Compile, installing each dependency build an earlier level of this run makes.
    Build {
        level: usize,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        dependencies: Vec<Dependency>,
    },
    /// Take the build a verified run already qualified; it needs no job.
    Recover { run: u64, artifact: u64 },
    /// A signed result for these inputs is already published.
    Reuse { key: String, record: String },
    /// The package cannot be planned; its job reports this and fails.
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub package: String,
    pub key: String,
}

/// A build a verified run retained, and the input key it was qualified under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovery {
    pub run: u64,
    pub artifact: u64,
    pub key: String,
}

impl WorkPlan {
    pub fn read(path: &Path) -> Result<Self, String> {
        let bytes = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
        let plan: Self = serde_json::from_slice(&bytes)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if plan.schema != SCHEMA {
            return Err(format!("{}: unsupported work plan schema", path.display()));
        }
        Ok(plan)
    }

    pub fn task(&self, package: &str) -> Result<&WorkTask, String> {
        self.tasks
            .iter()
            .find(|task| task.package == package)
            .ok_or_else(|| format!("{package} is not in this plan"))
    }

    /// Builds at each dependency level, which CI runs one level after another.
    pub fn levels(&self) -> Vec<Vec<&WorkTask>> {
        let mut levels = vec![Vec::new(); LEVELS];
        for task in &self.tasks {
            if let Some(level) = task.work.level() {
                levels[level].push(task);
            }
        }
        levels.truncate(
            levels
                .iter()
                .rposition(|level| !level.is_empty())
                .map_or(0, |last| last + 1),
        );
        levels
    }
}

impl Work {
    /// The level a job runs at; recovered and reused results need no job, and a task that
    /// cannot be planned fails in the first.
    pub fn level(&self) -> Option<usize> {
        match self {
            Self::Build { level, .. } => Some(*level),
            Self::Error(_) => Some(0),
            Self::Recover { .. } | Self::Reuse { .. } => None,
        }
    }
}

/// Plans `requests` and every source dependency with no published build, reusing results GHCR
/// already holds and recovering the builds `recover` admits. Reused records are written to
/// `records` so publication can include them.
pub fn plan_work(
    catalog: &PackageCatalog,
    requests: &[String],
    context: &str,
    distribution: &Distribution,
    recover: &mut dyn FnMut(&PackageTask) -> Result<Option<Recovery>, String>,
    records: Option<&Path>,
) -> Result<WorkPlan, String> {
    catalog.validate()?;
    let system = ResolveContext::current().system;
    let pin = distribution
        .pdr
        .select(&rootbeer_package::state_dir(), true)?
        .pin;
    let pdr = RepositoryResolver::new(&pin);
    let mut planned = BTreeMap::new();
    let mut errors = BTreeMap::new();
    let mut pending: Vec<String> = requests.iter().rev().cloned().collect();
    while let Some(request) = pending.pop() {
        if planned.contains_key(&request) || errors.contains_key(&request) {
            continue;
        }
        match plan_packages(
            catalog,
            std::slice::from_ref(&request),
            &BuildOptions::default(),
            context,
            Some(&pdr),
        ) {
            Ok(tasks) => {
                for task in tasks {
                    pending.extend(task.builds.iter().cloned());
                    planned.insert(task.package.clone(), task);
                }
            }
            Err(error) => {
                errors.insert(request, error);
            }
        }
    }

    let mut tasks = Vec::new();
    let mut in_run = BTreeMap::new();
    for task in planned.values() {
        let work = match signed_result(distribution, &pdr, task, &system)? {
            Some((key, record, bytes)) => {
                if let Some(records) = records {
                    fs::create_dir_all(records).map_err(|error| error.to_string())?;
                    fs::write(records.join(format!("{key}.json")), bytes)
                        .map_err(|error| error.to_string())?;
                }
                Work::Reuse { key, record }
            }
            None => match recover(task)? {
                Some(recovery) => {
                    in_run.insert(task.package.clone(), recovery.key.clone());
                    Work::Recover {
                        run: recovery.run,
                        artifact: recovery.artifact,
                    }
                }
                None => {
                    in_run.insert(task.package.clone(), task.key.clone());
                    Work::Build {
                        level: 0,
                        dependencies: Vec::new(),
                    }
                }
            },
        };
        let key = match &work {
            Work::Recover { .. } => in_run[&task.package].clone(),
            _ => task.key.clone(),
        };
        tasks.push(WorkTask {
            package: task.package.clone(),
            name: task.name.clone(),
            key,
            work,
        });
    }
    for (request, error) in errors {
        tasks.push(WorkTask {
            name: request.split('@').next().unwrap_or(&request).to_string(),
            package: request,
            key: String::new(),
            work: Work::Error(error),
        });
    }
    assign_levels(&mut tasks, &planned, &in_run)?;
    tasks.sort_by(|left, right| {
        (left.work.level(), &left.package).cmp(&(right.work.level(), &right.package))
    });
    Ok(WorkPlan {
        schema: SCHEMA,
        system,
        context: context.to_string(),
        catalog_sha256: catalog.sha256(),
        engine: rootbeer_build::engine_generation().to_string(),
        pdr: pin,
        registry: distribution.registry.clone(),
        tasks,
    })
}

/// Orders the builds of one run so each follows the builds it installs instead of compiling.
///
/// A build installs its dependencies' builds only when this run makes or recovers all of them;
/// otherwise it compiles them inline, as a build published outside the run's root cannot be
/// installed. A recovered build is ready from the start, so only builds add levels.
fn assign_levels(
    tasks: &mut [WorkTask],
    planned: &BTreeMap<String, PackageTask>,
    in_run: &BTreeMap<String, String>,
) -> Result<(), String> {
    fn level(
        package: &str,
        planned: &BTreeMap<String, PackageTask>,
        building: &BTreeSet<String>,
        levels: &mut BTreeMap<String, usize>,
    ) -> usize {
        if let Some(level) = levels.get(package) {
            return *level;
        }
        let deepest = planned[package]
            .builds
            .iter()
            .filter(|build| building.contains(*build))
            .map(|build| level(build, planned, building, levels) + 1)
            .max()
            .unwrap_or(0);
        levels.insert(package.to_string(), deepest);
        deepest
    }
    let building: BTreeSet<String> = tasks
        .iter()
        .filter(|task| matches!(task.work, Work::Build { .. }))
        .map(|task| task.package.clone())
        .collect();
    let mut levels = BTreeMap::new();
    for task in tasks.iter_mut() {
        let package = task.package.clone();
        let Work::Build {
            level: task_level,
            dependencies,
        } = &mut task.work
        else {
            continue;
        };
        *task_level = level(&package, planned, &building, &mut levels);
        let builds = &planned[&package].builds;
        if builds.iter().all(|build| in_run.contains_key(build)) {
            *dependencies = builds
                .iter()
                .map(|build| Dependency {
                    package: build.clone(),
                    key: in_run[build].clone(),
                })
                .collect();
        }
        if *task_level >= LEVELS {
            return Err(format!(
                "{package}: dependency chains deeper than {LEVELS} builds need more CI levels"
            ));
        }
    }
    Ok(())
}

/// The first signed result GHCR holds for the task's inputs, under this engine's key or a
/// reviewed predecessor's.
fn signed_result(
    distribution: &Distribution,
    pdr: &RepositoryResolver,
    task: &PackageTask,
    system: &str,
) -> Result<Option<(String, String, Vec<u8>)>, String> {
    #[derive(Deserialize)]
    struct Manifest {
        layers: Vec<Layer>,
    }
    #[derive(Deserialize)]
    struct Layer {
        #[serde(rename = "mediaType")]
        media_type: String,
        digest: String,
    }
    let repository = format!("{}/{}", distribution.registry, task.name);
    for key in std::iter::once(&task.key).chain(&task.compatible_keys) {
        let tag = format!("inputs-{key}");
        let bytes = match retrying(|| {
            tagged_manifest(&repository, &tag).map_err(|error| error.to_string())
        })? {
            Tagged::Manifest(bytes) => bytes,
            Tagged::Missing => continue,
            Tagged::Denied if publishes_namespace(pdr, distribution, &task.name)? => {
                return Err(format!("GHCR denied {repository}, which the PDR publishes"));
            }
            Tagged::Denied => return Ok(None),
        };
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|error| format!("{repository}:{tag}: {error}"))?;
        let digests: Vec<_> = manifest
            .layers
            .iter()
            .filter(|layer| layer.media_type == RECORD_MEDIA_TYPE)
            .filter_map(|layer| layer.digest.strip_prefix("sha256:"))
            .collect();
        let [digest] = digests[..] else {
            return Err(format!(
                "{repository}:{tag}: expected one signed package record"
            ));
        };
        if !rootbeer_catalog::is_sha256(digest) {
            return Err(format!("{repository}:{tag}: invalid record digest"));
        }
        let reference = format!("ghcr://{repository}@sha256:{digest}");
        let bytes = retrying(|| read_record(&reference))?;
        let record = verify_record(&bytes, &distribution.pdr.public_key, &task.package, system)?;
        if record.input_key() != *key {
            return Err(format!("{reference}: signed inputs differ from {tag}"));
        }
        return Ok(Some((key.clone(), reference, bytes)));
    }
    Ok(None)
}

/// Retries a registry request that failed in transit; a missing tag or a denied repository is an
/// answer, not a failure, so it is returned at once.
fn retrying<T>(mut request: impl FnMut() -> Result<T, String>) -> Result<T, String> {
    let mut delay = std::time::Duration::from_secs(2);
    for _ in 1..4 {
        match request() {
            Ok(value) => return Ok(value),
            Err(error) => eprintln!("retrying after {error}"),
        }
        std::thread::sleep(delay);
        delay *= 2;
    }
    request()
}

/// Whether any published record of `name` is a build in this registry. Catalog entries can
/// precede builds, and upstream-binary packages have none, so only a record establishes one.
fn publishes_namespace(
    pdr: &RepositoryResolver,
    distribution: &Distribution,
    name: &str,
) -> Result<bool, String> {
    let Ok((name, package)) = pdr.package(name) else {
        return Ok(false);
    };
    let prefix = format!("ghcr://{}/{name}@", distribution.registry);
    for (version, published) in pdr.document(name, package)?.versions {
        for system in published.platforms.keys() {
            let (record, _, _) =
                pdr.signed_record_of(name, package, Some(&version), &ResolveContext::new(system))?;
            if matches!(&record.artifact.package.source, LockedSource::Url { url, .. } if url.starts_with(&prefix))
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// What building one planned task produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Built {
        key: String,
    },
    /// A result for these inputs was published after planning; its record is kept instead.
    Reused {
        key: String,
        record: String,
    },
}

fn check_plan(catalog: &PackageCatalog, plan: &WorkPlan) -> Result<String, String> {
    let system = ResolveContext::current().system;
    if plan.system != system {
        return Err(format!(
            "this plan builds for {}, not {system}",
            plan.system
        ));
    }
    if plan.engine != rootbeer_build::engine_generation() {
        return Err("this plan was made by another engine".into());
    }
    if plan.catalog_sha256 != catalog.sha256() {
        return Err("recipes changed since this plan was made".into());
    }
    Ok(system)
}

/// The key each planned package has on a runner with `context`: its planned key on the planner's
/// runner, and otherwise the key a job there builds it under, since runner images can roll out
/// between planning and building.
pub fn runner_keys(
    catalog: &PackageCatalog,
    plan: &WorkPlan,
    packages: &[&str],
    context: &str,
) -> Result<BTreeMap<String, String>, String> {
    check_plan(catalog, plan)?;
    if context == plan.context || packages.is_empty() {
        return packages
            .iter()
            .map(|package| Ok((package.to_string(), plan.task(package)?.key.clone())))
            .collect();
    }
    let requests: Vec<_> = packages.iter().map(|package| package.to_string()).collect();
    let pdr = RepositoryResolver::new(&plan.pdr);
    let planned = plan_packages(
        catalog,
        &requests,
        &BuildOptions::default(),
        context,
        Some(&pdr),
    )?;
    packages
        .iter()
        .map(|package| {
            let task = planned
                .iter()
                .find(|task| task.package == *package)
                .ok_or_else(|| format!("{package} has no recipe for {}", plan.system))?;
            Ok((package.to_string(), task.key.clone()))
        })
        .collect()
}

/// Builds one planned task into `output`, installing the dependency builds found in
/// `dependencies`. A job on another runner image than its planner keeps its own key.
pub fn build_task(
    catalog: &PackageCatalog,
    plan: &WorkPlan,
    package: &str,
    output: &Path,
    dependencies: &Path,
    cache: &Path,
    context: &str,
) -> Result<Outcome, String> {
    let system = check_plan(catalog, plan)?;
    let task = plan.task(package)?;
    let expected = match &task.work {
        Work::Build { dependencies, .. } => dependencies,
        Work::Recover { run, .. } => {
            return Err(format!("{package} is recovered from run {run}, not built"))
        }
        Work::Reuse { record, .. } => {
            return Err(format!("{package} is already published as {record}"))
        }
        Work::Error(error) => return Err(error.clone()),
    };
    let pdr = RepositoryResolver::new(&plan.pdr);
    let [planned] = &plan_packages(
        catalog,
        &[package.to_string()],
        &BuildOptions::default(),
        context,
        Some(&pdr),
    )?[..] else {
        return Err(format!("{package} has no recipe for {system}"));
    };
    if context == plan.context && planned.key != task.key {
        return Err(format!("{package}: inputs changed since planning"));
    }
    let distribution = Distribution {
        pdr: Repository {
            url: plan.pdr.url.clone(),
            public_key: plan.pdr.public_key.clone(),
        },
        registry: plan.registry.clone(),
    };
    if let Some((key, record, bytes)) = signed_result(&distribution, &pdr, planned, &system)? {
        fs::create_dir_all(output).map_err(|error| error.to_string())?;
        fs::write(output.join("record.json"), bytes).map_err(|error| error.to_string())?;
        return Ok(Outcome::Reused { key, record });
    }
    let builds = dependency_builds(dependencies, expected)?;
    crate::prepare_package(
        catalog,
        package,
        output,
        &BuildOptions {
            cache: Some(BuildCache {
                directory: cache.to_path_buf(),
                context: context.to_string(),
                recheck: false,
            }),
            ..Default::default()
        },
        Some(&pdr),
        &builds,
    )?;
    Ok(Outcome::Built {
        key: planned.key.clone(),
    })
}

/// The handed-in build of each expected dependency, found by its receipt so directory names
/// don't matter: builds sit in `directory`, or one level into groups such as this run's and a
/// recovered run's, and a group holding one download has it extracted in place.
fn dependency_builds(directory: &Path, expected: &[Dependency]) -> Result<Vec<PathBuf>, String> {
    let wanted: BTreeSet<_> = expected
        .iter()
        .map(|dependency| dependency.package.as_str())
        .collect();
    let mut candidates = Vec::new();
    let mut pending = vec![(directory.to_path_buf(), 0)];
    while let Some((path, depth)) = pending.pop() {
        if path.join("receipt.json").is_file() {
            candidates.push(path);
            continue;
        }
        if depth == 2 || !path.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&path).map_err(|error| error.to_string())? {
            pending.push((entry.map_err(|error| error.to_string())?.path(), depth + 1));
        }
    }
    candidates.sort();
    let mut found = BTreeMap::new();
    for candidate in candidates {
        let bytes = fs::read(candidate.join("receipt.json")).map_err(|error| error.to_string())?;
        let receipt: BuildArtifact =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        let id = receipt.package.id();
        if !wanted.contains(id.as_str()) {
            return Err(format!(
                "{id} was handed in, but this task does not install it"
            ));
        }
        if found.insert(id.clone(), candidate).is_some() {
            return Err(format!("{id} was handed in twice"));
        }
    }
    if let Some(missing) = wanted.iter().find(|package| !found.contains_key(**package)) {
        return Err(format!("the build of {missing} was not handed in"));
    }
    Ok(found.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(package: &str, builds: &[&str]) -> PackageTask {
        PackageTask {
            package: package.into(),
            name: package.split('@').next().unwrap().into(),
            system: "aarch64-macos".into(),
            key: rootbeer_store::hash_bytes(package.as_bytes()),
            pdr_root: None,
            compatible_keys: Vec::new(),
            builds: builds.iter().map(|build| build.to_string()).collect(),
        }
    }

    fn levels(planned: &[PackageTask], recovered: &[&str]) -> Vec<WorkTask> {
        let planned: BTreeMap<_, _> = planned
            .iter()
            .map(|task| (task.package.clone(), task.clone()))
            .collect();
        let in_run = planned
            .values()
            .map(|task| (task.package.clone(), task.key.clone()))
            .collect();
        let mut tasks: Vec<_> = planned
            .values()
            .map(|task| WorkTask {
                package: task.package.clone(),
                name: task.name.clone(),
                key: task.key.clone(),
                work: if recovered.contains(&task.package.as_str()) {
                    Work::Recover {
                        run: 1,
                        artifact: 2,
                    }
                } else {
                    Work::Build {
                        level: 0,
                        dependencies: Vec::new(),
                    }
                },
            })
            .collect();
        assign_levels(&mut tasks, &planned, &in_run).unwrap();
        tasks
    }

    fn find<'a>(tasks: &'a [WorkTask], package: &str) -> &'a WorkTask {
        tasks.iter().find(|task| task.package == package).unwrap()
    }

    #[test]
    fn builds_follow_the_builds_they_install() {
        let tasks = levels(
            &[
                task("libiconv@1", &[]),
                task("libunistring@1", &["libiconv@1"]),
                task("libidn2@1", &["libiconv@1", "libunistring@1"]),
            ],
            &[],
        );
        assert_eq!(find(&tasks, "libiconv@1").work.level(), Some(0));
        assert_eq!(find(&tasks, "libunistring@1").work.level(), Some(1));
        let Work::Build {
            level,
            dependencies,
        } = &find(&tasks, "libidn2@1").work
        else {
            panic!()
        };
        assert_eq!(*level, 2);
        assert_eq!(
            dependencies
                .iter()
                .map(|dependency| dependency.package.as_str())
                .collect::<Vec<_>>(),
            ["libiconv@1", "libunistring@1"]
        );
        assert_eq!(dependencies[0].key, find(&tasks, "libiconv@1").key);
    }

    #[test]
    fn recovered_builds_are_handed_in_like_new_ones() {
        let tasks = levels(
            &[task("ncurses@1", &[]), task("telnet@1", &["ncurses@1"])],
            &["ncurses@1"],
        );
        assert_eq!(find(&tasks, "ncurses@1").work.level(), None);
        let Work::Build {
            level: 0,
            dependencies,
        } = &find(&tasks, "telnet@1").work
        else {
            panic!()
        };
        assert_eq!(dependencies.len(), 1);
    }

    #[test]
    fn a_dependency_built_outside_the_run_is_compiled_inline() {
        let planned: BTreeMap<_, _> = [
            task("openssl@1", &[]),
            task("curl@1", &["openssl@1", "zlib@1"]),
        ]
        .into_iter()
        .map(|task| (task.package.clone(), task))
        .collect();
        let in_run: BTreeMap<_, _> = planned
            .values()
            .map(|task| (task.package.clone(), task.key.clone()))
            .collect();
        let mut tasks: Vec<_> = planned
            .values()
            .map(|task| WorkTask {
                package: task.package.clone(),
                name: task.name.clone(),
                key: task.key.clone(),
                work: Work::Build {
                    level: 0,
                    dependencies: Vec::new(),
                },
            })
            .collect();
        assign_levels(&mut tasks, &planned, &in_run).unwrap();
        assert_eq!(
            find(&tasks, "curl@1").work,
            Work::Build {
                level: 1,
                dependencies: Vec::new()
            }
        );
    }

    #[test]
    fn chains_deeper_than_ci_levels_fail_planning() {
        let names: Vec<String> = (0..=LEVELS).map(|index| format!("p{index}@1")).collect();
        let chain: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let previous: Vec<&str> = names[..index]
                    .last()
                    .map(String::as_str)
                    .into_iter()
                    .collect();
                task(name, &previous)
            })
            .collect();
        let planned: BTreeMap<_, _> = chain
            .into_iter()
            .map(|task| (task.package.clone(), task))
            .collect();
        let in_run: BTreeMap<_, _> = planned
            .values()
            .map(|task| (task.package.clone(), task.key.clone()))
            .collect();
        let mut tasks: Vec<_> = planned
            .values()
            .map(|task| WorkTask {
                package: task.package.clone(),
                name: task.name.clone(),
                key: task.key.clone(),
                work: Work::Build {
                    level: 0,
                    dependencies: Vec::new(),
                },
            })
            .collect();
        assert!(assign_levels(&mut tasks, &planned, &in_run).is_err());
    }

    #[test]
    fn handed_in_builds_are_found_by_receipt_and_must_match_the_plan() {
        let root = tempfile::tempdir().unwrap();
        let (_, receipt) = crate::receipt::tests::fixture(&root.path().join("a"));
        let receipt: BuildArtifact = serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
        let id = receipt.package.id();
        let builds = root.path().join("a");
        let expected = [Dependency {
            package: id.clone(),
            key: "k".into(),
        }];
        assert_eq!(
            dependency_builds(&builds, &expected).unwrap(),
            [builds.join("build")]
        );
        let other = [Dependency {
            package: "other@1".into(),
            key: "k".into(),
        }];
        assert!(dependency_builds(&builds, &other)
            .unwrap_err()
            .contains("does not install"));
        let grouped = root.path().join("grouped/recovered");
        fs::create_dir_all(grouped.parent().unwrap()).unwrap();
        fs::rename(builds.join("build"), &grouped).unwrap();
        assert_eq!(
            dependency_builds(&root.path().join("grouped"), &expected).unwrap(),
            [grouped]
        );
        assert!(dependency_builds(&root.path().join("none"), &expected)
            .unwrap_err()
            .contains("not handed in"));
    }

    #[test]
    fn plans_round_trip_and_list_each_level() {
        let plan = WorkPlan {
            schema: SCHEMA,
            system: "aarch64-macos".into(),
            context: "local".into(),
            catalog_sha256: "a".repeat(64),
            engine: "b".repeat(64),
            pdr: RepositoryPin {
                url: "https://example.org/current.json".into(),
                public_key: "c".repeat(66),
                root: "d".repeat(64),
            },
            registry: "owner/pdr".into(),
            tasks: vec![
                WorkTask {
                    package: "app@1".into(),
                    name: "app".into(),
                    key: "e".repeat(64),
                    work: Work::Build {
                        level: 1,
                        dependencies: vec![Dependency {
                            package: "lib@1".into(),
                            key: "f".repeat(64),
                        }],
                    },
                },
                WorkTask {
                    package: "lib@1".into(),
                    name: "lib".into(),
                    key: "f".repeat(64),
                    work: Work::Build {
                        level: 0,
                        dependencies: Vec::new(),
                    },
                },
                WorkTask {
                    package: "tool@1".into(),
                    name: "tool".into(),
                    key: "0".repeat(64),
                    work: Work::Reuse {
                        key: "0".repeat(64),
                        record: "ghcr://owner/pdr/tool@sha256:x".into(),
                    },
                },
            ],
        };
        let json = serde_json::to_string(&plan).unwrap();
        assert_eq!(serde_json::from_str::<WorkPlan>(&json).unwrap(), plan);
        let levels = plan.levels();
        assert_eq!(levels.len(), 2);
        assert_eq!(levels[0][0].package, "lib@1");
        assert_eq!(levels[1][0].package, "app@1");
        assert!(plan.task("missing@1").is_err());
    }

    #[test]
    fn runner_keys_are_planned_keys_only_on_the_planner_runner() {
        let catalog = crate::test_catalog::catalog();
        let mut plan = WorkPlan {
            schema: SCHEMA,
            system: ResolveContext::current().system,
            context: "planner".into(),
            catalog_sha256: catalog.sha256(),
            engine: rootbeer_build::engine_generation().into(),
            pdr: RepositoryPin {
                url: "https://example.invalid/current.json".into(),
                public_key: "c".repeat(66),
                root: "d".repeat(64),
            },
            registry: "owner/pdr".into(),
            tasks: vec![WorkTask {
                package: "missing@1".into(),
                name: "missing".into(),
                key: "f".repeat(64),
                work: Work::Build {
                    level: 0,
                    dependencies: Vec::new(),
                },
            }],
        };

        let keys = runner_keys(catalog, &plan, &["missing@1"], "planner").unwrap();
        assert_eq!(keys["missing@1"], "f".repeat(64));
        // Another runner plans the package itself rather than trusting the planner's key.
        assert!(runner_keys(catalog, &plan, &["missing@1"], "rolled-out").is_err());

        plan.engine = "0".repeat(64);
        let error = runner_keys(catalog, &plan, &["missing@1"], "planner").unwrap_err();
        assert!(error.contains("another engine"), "{error}");
    }
}
