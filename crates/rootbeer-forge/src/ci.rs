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
    /// Find the builds of a task's dependencies this run uploaded, for download by ID
    Dependencies { plan: PathBuf, task: String },
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
            let Work::Build { dependencies, .. } = &plan.task(&task)?.work else {
                return output("ids", "");
            };
            if dependencies.is_empty() {
                return output("ids", "");
            }
            let keys: Vec<_> = dependencies
                .iter()
                .map(|dependency| dependency.key.clone())
                .collect();
            let ids = github::run_builds(&GitHub::from_env()?, &keys)?;
            output(
                "ids",
                &ids.iter().map(u64::to_string).collect::<Vec<_>>().join(","),
            )
        }
    }
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
            platforms.push(json!({"runner": runner, "packages": selected.join(" ")}));
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
    let system = rootbeer_packaging::ResolveContext::current().system;
    let runner = config.runner(&system)?.to_string();
    let mut reuse_run = reuse_run.filter(|run| !run.is_empty());
    let attempt: u64 = std::env::var("GITHUB_RUN_ATTEMPT")
        .ok()
        .and_then(|attempt| attempt.parse().ok())
        .unwrap_or(1);
    if reuse_run.is_none() && attempt > 1 {
        reuse_run = Some(github::env("GITHUB_RUN_ID")?);
    }
    let github = reuse_run.as_ref().map(|_| GitHub::from_env()).transpose()?;
    let retained = match (&github, &reuse_run) {
        (Some(github), Some(run)) => Some(github::admit(
            github,
            run,
            config.workflow()?,
            &config.ci.trusted,
        )?),
        _ => None,
    };
    let mut recover = |task: &rootbeer_packaging::PackageTask| match (&github, &retained) {
        (Some(github), Some(retained)) => retained.recovery(github, &runner, task),
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
    let levels = plan.levels();
    for index in 0..rootbeer_packaging::work::LEVELS {
        let entries: Vec<_> = levels
            .get(index)
            .into_iter()
            .flatten()
            .map(|task| {
                let mut entry = json!({
                    "package": task.package, "name": task.name, "system": plan.system, "key": task.key,
                });
                match &task.work {
                    Work::Recover { run, artifact, .. } => {
                        entry["artifact"] = json!(artifact.to_string());
                        entry["reuse_run"] = json!(run.to_string());
                    }
                    Work::Error(error) => entry["error"] = json!(error),
                    Work::Build { .. } | Work::Reuse { .. } => {}
                }
                entry
            })
            .collect();
        output(
            &format!("level_{index}"),
            &json!({"include": entries}).to_string(),
        )?;
    }
    output("depth", &levels.len().to_string())?;
    output("has-work", if levels.is_empty() { "false" } else { "true" })?;
    summary(&describe(&plan))
}

/// A readable account of a plan: what each task does, and why.
pub fn describe(plan: &WorkPlan) -> String {
    let mut text = String::new();
    let jobs = plan
        .tasks
        .iter()
        .filter(|task| task.work.level().is_some())
        .count();
    let reused = plan.tasks.len() - jobs;
    text.push_str(&format!(
        "{jobs} packages to build or recover on {}; {reused} signed results reused.\n\n",
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
            Work::Recover { level, run, .. } => {
                format!(
                    "- Recover `{}` from run {run} (level {level})\n",
                    task.package
                )
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
}
