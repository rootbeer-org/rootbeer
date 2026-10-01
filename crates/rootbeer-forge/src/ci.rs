//! GitHub Actions steps: each reads the run's context, calls the same planning and building code
//! as the local commands, and writes step outputs and a summary.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::Subcommand;
use rootbeer_packaging::selection::{changed_requests, is_supported};
use rootbeer_packaging::work::{plan_work, Work, WorkPlan};
use rootbeer_packaging::{PackageCatalog, PackageDefinition};
use serde_json::json;

use crate::config::{detect_context, Config};
use crate::github::{self, GitHub};

#[derive(Subcommand, Debug)]
pub enum Ci {
    /// Choose the packages each runner builds: explicit ones, or those changed since a base
    Select {
        packages: Vec<String>,
        /// Revision whose approved recipes the checkout is compared against
        #[arg(long)]
        base: Option<String>,
    },
    /// Find the verified run a merge promotes, or report that it is still running
    Producer {
        /// Promote this run instead of looking one up
        #[arg(long)]
        reuse_run: Option<String>,
    },
    /// Plan this runner's work, recovering builds a verified run retained
    Plan {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Verified run whose retained builds are recovered instead of rebuilt
        #[arg(long)]
        reuse_run: Option<String>,
        #[arg(short, long, default_value = "plan.json")]
        output: PathBuf,
        /// Where records of reused results are saved for publication
        #[arg(long, default_value = "discovery-records")]
        records: PathBuf,
    },
    /// Find the builds a task installs, from this run or the verified run it recovers from
    Dependencies { plan: PathBuf, task: String },
    /// Find every build this runner publishes, from this run or the verified run it recovers from
    Builds { plan: PathBuf },
    /// Sign and push every downloaded build of a plan, one failure at a time
    Publish {
        plan: PathBuf,
        /// Directory holding the downloaded builds
        #[arg(default_value = "builds")]
        builds: PathBuf,
        /// Where each published record is saved for discovery
        #[arg(long, default_value = "discovery-records")]
        records: PathBuf,
    },
    /// Advance the engine's recipe to the newest main commit its CI verified
    Engine {
        /// Only report whether there is one, as the `changed` output
        #[arg(long)]
        probe: bool,
    },
    /// Open or update the reviewed pull requests proposing discovered recipe updates
    Propose {
        #[arg(required = true)]
        packages: Vec<String>,
    },
}

pub fn run(command: Ci, config: &Config, catalog: Option<&PackageCatalog>) -> Result<(), String> {
    let catalog = || catalog.ok_or("this command needs a catalog; set `catalog` in forge.toml");
    match command {
        Ci::Select { packages, base } => select(config, catalog()?, packages, base),
        Ci::Producer { reuse_run } => {
            let github = GitHub::from_env()?;
            let producer = github::producer(
                &github,
                config.workflow()?,
                reuse_run.filter(|run| !run.is_empty()),
            )?;
            output("reuse-run", producer.reuse_run.as_deref().unwrap_or(""))?;
            output(
                "waiting",
                if producer.is_waiting { "true" } else { "false" },
            )?;
            if let Some(base) = &producer.base {
                output("base", base)?;
            }
            Ok(())
        }
        Ci::Plan {
            packages,
            reuse_run,
            output: path,
            records,
        } => plan(config, catalog()?, &packages, reuse_run, &path, &records),
        Ci::Engine { probe } => {
            let directory = config.catalog.as_deref().unwrap_or(Path::new("packages"));
            crate::discovery::advance_engine(config, directory, probe)
        }
        Ci::Propose { packages } => crate::discovery::propose(config, &packages),
        Ci::Dependencies { plan, task } => {
            let plan = WorkPlan::read(&plan)?;
            let packages: Vec<_> = match &plan.task(&task)?.work {
                Work::Build { dependencies, .. } => dependencies
                    .iter()
                    .map(|dependency| dependency.package.as_str())
                    .collect(),
                _ => Vec::new(),
            };
            downloads(catalog()?, &plan, &packages, Some(&task))
        }
        Ci::Builds { plan } => {
            let plan = WorkPlan::read(&plan)?;
            let packages: Vec<_> = plan
                .tasks
                .iter()
                .filter(|task| matches!(task.work, Work::Build { .. } | Work::Recover { .. }))
                .map(|task| task.package.as_str())
                .collect();
            downloads(catalog()?, &plan, &packages, None)
        }
        Ci::Publish {
            plan,
            builds,
            records,
        } => crate::publish::publish(
            config,
            catalog()?,
            &WorkPlan::read(&plan)?,
            &builds,
            &records,
        ),
    }
}

/// Step outputs naming the artifacts of `packages`: `ids` from this run, and `recovered-ids`
/// from the verified run `recovered-run`. A build is found under the key this runner builds it
/// with, else its planned key. A build this run failed to make is skipped, unless `dependent`
/// installs it and cannot build without it.
fn downloads(
    catalog: &PackageCatalog,
    plan: &WorkPlan,
    packages: &[&str],
    dependent: Option<&str>,
) -> Result<(), String> {
    let mut built = Vec::new();
    let mut recovered = Vec::new();
    let mut runs = std::collections::BTreeSet::new();
    for package in packages {
        let task = plan.task(package)?;
        match &task.work {
            Work::Recover { run, artifact } => {
                recovered.push(artifact.to_string());
                runs.insert(*run);
            }
            _ => built.push(*package),
        }
    }
    if runs.len() > 1 {
        return Err("a plan recovers builds from one verified run".into());
    }
    let context = detect_context();
    let local = rootbeer_packaging::work::runner_keys(catalog, plan, &built, &context)?;
    let mut keys = Vec::new();
    for package in &built {
        keys.push(local[*package].clone());
        keys.push(plan.task(package)?.key.clone());
    }
    let found = match keys.is_empty() {
        true => Vec::new(),
        false => github::run_builds(&GitHub::from_env()?, &keys)?,
    };
    let ids: Vec<_> = found
        .chunks(2)
        .map(|candidates| candidates[0].or(candidates[1]))
        .collect();
    let failed: Vec<_> = built
        .iter()
        .zip(&ids)
        .filter(|(_, id)| id.is_none())
        .map(|(package, _)| *package)
        .collect();
    if let (Some(dependent), false) = (dependent, failed.is_empty()) {
        let runners = match context == plan.context {
            true => String::new(),
            false => format!(
                "; this runner ({context}) differs from the planner's ({}), so a build from a \
                 third runner image cannot be installed here",
                plan.context
            ),
        };
        return Err(format!(
            "{} has no build in this run for {dependent} to install{runners}. If it failed, fix it \
             first; if it was published after planning, re-run all jobs to plan against it",
            failed.join(", ")
        ));
    }
    let ids: Vec<_> = ids.iter().flatten().map(u64::to_string).collect();
    output("ids", &ids.join(","))?;
    output("recovered-ids", &recovered.join(","))?;
    output(
        "recovered-run",
        &runs.first().map(u64::to_string).unwrap_or_default(),
    )
}

fn select(
    config: &Config,
    catalog: &PackageCatalog,
    packages: Vec<String>,
    base: Option<String>,
) -> Result<(), String> {
    let base = base.filter(|base| !base.is_empty() && !base.bytes().all(|byte| byte == b'0'));
    let before = match (&base, packages.is_empty()) {
        (Some(base), true) => Some(base_catalog(config, base)?),
        _ => None,
    };
    let requests = match &before {
        Some(before) => changed_requests(before, catalog, None),
        None => packages.clone(),
    };
    let mut platforms = Vec::new();
    for (system, runner) in &config.ci.runners {
        let selected: Vec<_> = match &before {
            Some(before) => changed_requests(before, catalog, Some(system)),
            None => requests.clone(),
        }
        .into_iter()
        .filter(|request| is_supported(catalog, request, system))
        .collect();
        if !selected.is_empty() {
            platforms
                .push(json!({"runner": runner, "system": system, "packages": selected.join(" ")}));
        }
    }
    output("platforms", &json!({"include": platforms}).to_string())?;
    output("packages", &requests.join(" "))?;
    let mut text = format!("{} changed package versions.\n", requests.len());
    for request in &requests {
        text.push_str(&format!("- `{request}`\n"));
    }
    summary(&text)
}

/// The approved recipes at `base`, or none when this engine cannot read what an older one could,
/// which makes every recipe new.
fn base_catalog(config: &Config, base: &str) -> Result<PackageCatalog, String> {
    let directory = config.catalog.as_deref().unwrap_or(Path::new("packages"));
    let extracted = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut archive = Command::new("git")
        .args(["archive", base, "--"])
        .arg(directory)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("git archive: {error}"))?;
    let status = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(extracted.path())
        .stdin(archive.stdout.take().ok_or("git archive has no output")?)
        .status()
        .map_err(|error| format!("tar: {error}"))?;
    if !archive.wait().map_err(|error| error.to_string())?.success() || !status.success() {
        return Err(format!("cannot read the recipes approved at {base}"));
    }
    let loaded = PackageDefinition::from_directory(&extracted.path().join(directory))
        .and_then(|definitions| PackageCatalog::from_definitions(&definitions));
    match loaded {
        Ok(catalog) => Ok(catalog),
        Err(error) => {
            let pin = config.ci.engine_pin.as_deref().ok_or(error.clone())?;
            let current = std::fs::read_to_string(pin).map_err(|error| error.to_string())?;
            let previous = Command::new("git")
                .arg("show")
                .arg(format!("{base}:{}", pin.display()))
                .output()
                .map_err(|error| error.to_string())?;
            unreadable_base(error, &current, &String::from_utf8_lossy(&previous.stdout))
        }
    }
}

/// Recipes an older engine wrote count as new under a new one; under the same engine an
/// unreadable base is a real failure.
fn unreadable_base(
    error: String,
    current_pin: &str,
    base_pin: &str,
) -> Result<PackageCatalog, String> {
    if current_pin.trim() == base_pin.trim() {
        return Err(error);
    }
    Ok(PackageCatalog {
        packages: Default::default(),
        extra: Default::default(),
    })
}

fn plan(
    config: &Config,
    catalog: &PackageCatalog,
    packages: &[String],
    reuse_run: Option<String>,
    path: &Path,
    records: &Path,
) -> Result<(), String> {
    let mut reuse_run = reuse_run.filter(|run| !run.is_empty());
    let attempt: u64 = std::env::var("GITHUB_RUN_ATTEMPT")
        .ok()
        .and_then(|attempt| attempt.parse().ok())
        .unwrap_or(1);
    if reuse_run.is_none() && attempt > 1 {
        reuse_run = Some(github::env("GITHUB_RUN_ID")?);
    }
    let github = reuse_run.as_ref().map(|_| GitHub::from_env()).transpose()?;
    // A refused producer only costs a rebuild: nothing it retained is trusted, so this run
    // builds and verifies the selected packages itself.
    let retained = match (&github, &reuse_run) {
        (Some(github), Some(run)) => {
            match github::admit(github, run, config.workflow()?, &config.ci.trusted) {
                Ok(retained) => Some(retained),
                Err(error) => {
                    eprintln!("::warning::Not reusing run {run}: {error}; building instead");
                    summary(&format!(
                        "Run {run} was not reused ({error}), so this run builds its packages.\n\n"
                    ))?;
                    None
                }
            }
        }
        _ => None,
    };
    let mut recover = |task: &rootbeer_packaging::PackageTask| match (&github, &retained) {
        (Some(github), Some(retained)) => retained.recovery(github, task),
        _ => Ok(None),
    };
    let plan = plan_work(
        catalog,
        packages,
        &detect_context(),
        &config.distribution()?,
        &mut recover,
        Some(records),
    )?;
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&plan).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let levels = build_matrix(&plan);
    let publishes = plan
        .tasks
        .iter()
        .any(|task| matches!(task.work, Work::Build { .. } | Work::Recover { .. }));
    output("builds", &json!({"include": levels}).to_string())?;
    output(
        "has-builds",
        if levels.is_empty() { "false" } else { "true" },
    )?;
    output("has-publish", if publishes { "true" } else { "false" })?;
    summary(&describe(&plan))
}

/// GitHub rejects a matrix of more than 256 jobs and then creates none of them.
const MATRIX_JOBS: usize = 256;

/// The plan's levels in order, each split so its matrix fits GitHub's job limit. Entries run
/// one at a time, and an entry's `level` is its own index.
fn build_matrix(plan: &WorkPlan) -> Vec<serde_json::Value> {
    plan.levels()
        .into_iter()
        .flat_map(|tasks| {
            tasks
                .chunks(MATRIX_JOBS)
                .map(|chunk| {
                    chunk
                        .iter()
                        .map(|task| {
                            let mut entry = json!({"package": task.package, "name": task.name, "key": task.key});
                            if let Work::Error(error) = &task.work {
                                entry["error"] = json!(error);
                            }
                            entry
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        })
        .enumerate()
        .map(|(index, tasks)| json!({"level": index, "tasks": {"include": tasks}}))
        .collect()
}

/// A readable account of a plan: what each task does, and why.
pub fn describe(plan: &WorkPlan) -> String {
    let mut text = String::new();
    let reused = plan
        .tasks
        .iter()
        .filter(|task| matches!(task.work, Work::Reuse { .. }))
        .count();
    text.push_str(&format!(
        "{} packages to build or recover on {}; {reused} signed results reused.\n\n",
        plan.tasks.len() - reused,
        plan.system
    ));
    for task in &plan.tasks {
        let line = match &task.work {
            Work::Build {
                level,
                dependencies,
            } if dependencies.is_empty() => {
                format!("- Build `{}` (level {level})\n", task.package)
            }
            Work::Build {
                level,
                dependencies,
            } => format!(
                "- Build `{}` (level {level}) with this run's {}\n",
                task.package,
                dependencies
                    .iter()
                    .map(|dependency| format!("`{}`", dependency.package))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Work::Recover { run, .. } => {
                format!("- Recover `{}` from run {run}\n", task.package)
            }
            Work::Reuse { record, .. } => format!("- Reuse `{}`: `{record}`\n", task.package),
            Work::Error(error) => format!("- Cannot plan `{}`: {error}\n", task.package),
        };
        text.push_str(&line);
    }
    text
}

/// Sets a step output, or prints it outside GitHub Actions.
pub fn output(name: &str, value: &str) -> Result<(), String> {
    append("GITHUB_OUTPUT", &format!("{name}={value}\n"))
}

pub fn summary(text: &str) -> Result<(), String> {
    append("GITHUB_STEP_SUMMARY", text)
}

fn append(variable: &str, text: &str) -> Result<(), String> {
    match std::env::var_os(variable) {
        Some(path) => OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .and_then(|mut file| file.write_all(text.as_bytes()))
            .map_err(|error| format!("{variable}: {error}")),
        None => {
            print!("{text}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipes_an_older_engine_wrote_count_as_new() {
        let catalog = unreadable_base("unknown field".into(), "b\n", "a").unwrap();
        assert!(catalog.packages.is_empty());
        assert_eq!(
            unreadable_base("unknown field".into(), "a\n", "a").unwrap_err(),
            "unknown field"
        );
    }

    #[test]
    fn build_matrix_splits_levels_past_the_job_limit_in_order() {
        let task = |index: usize, level: usize| rootbeer_packaging::work::WorkTask {
            package: format!("p{index}@1"),
            name: format!("p{index}"),
            key: index.to_string(),
            work: Work::Build {
                level,
                dependencies: Vec::new(),
            },
        };
        let mut tasks: Vec<_> = (0..MATRIX_JOBS + 1).map(|index| task(index, 0)).collect();
        tasks.push(task(MATRIX_JOBS + 1, 1));
        let plan = WorkPlan {
            schema: 1,
            system: "x86_64-linux".into(),
            context: String::new(),
            catalog_sha256: String::new(),
            engine: String::new(),
            pdr: rootbeer_packaging::repository::RepositoryPin {
                url: String::new(),
                public_key: String::new(),
                root: String::new(),
            },
            registry: String::new(),
            tasks,
        };

        let matrix = build_matrix(&plan);
        let sizes: Vec<_> = matrix
            .iter()
            .map(|entry| entry["tasks"]["include"].as_array().unwrap().len())
            .collect();
        assert_eq!(sizes, [MATRIX_JOBS, 1, 1]);
        for (index, entry) in matrix.iter().enumerate() {
            assert_eq!(entry["level"], index);
        }
        assert_eq!(
            matrix[2]["tasks"]["include"][0]["name"],
            format!("p{}", MATRIX_JOBS + 1)
        );
    }
}
