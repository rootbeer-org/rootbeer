//! The GitHub Actions facts CI planning needs: which verified run a merge promotes, the builds it
//! retained, and the dependency builds of the current run. Nothing here builds or signs.

use std::collections::BTreeMap;
use std::io::Read;
use std::process::Command;
use std::time::Duration;

use rootbeer_packaging::work::Recovery;
use rootbeer_packaging::PackageTask;
use serde_json::Value;

const API: &str = "https://api.github.com";

pub struct GitHub {
    agent: ureq::Agent,
    token: String,
    pub repository: String,
}

impl GitHub {
    pub fn from_env() -> Result<Self, String> {
        let repository = env("GITHUB_REPOSITORY")?;
        let token = std::env::var("GH_TOKEN")
            .or_else(|_| std::env::var("GITHUB_TOKEN"))
            .map_err(|_| "GitHub API calls need GH_TOKEN or GITHUB_TOKEN")?;
        let agent = ureq::Agent::config_builder()
            .https_only(true)
            .timeout_global(Some(Duration::from_secs(120)))
            .redirect_auth_headers(ureq::config::RedirectAuthHeaders::Never)
            .build()
            .into();
        Ok(Self {
            agent,
            token,
            repository,
        })
    }

    fn request(&self, path: &str) -> Result<String, String> {
        let url = format!("{API}/{}", path.trim_start_matches('/'));
        let mut response = self
            .agent
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "rootbeer-forge")
            .call()
            .map_err(|error| format!("GET {url}: {error}"))?;
        let mut text = String::new();
        response
            .body_mut()
            .as_reader()
            .take(64 << 20)
            .read_to_string(&mut text)
            .map_err(|error| format!("GET {url}: {error}"))?;
        Ok(text)
    }

    pub fn get(&self, path: &str) -> Result<Value, String> {
        serde_json::from_str(&self.request(path)?).map_err(|error| format!("{path}: {error}"))
    }

    /// Every item of a paginated collection whose pages list them under `field`.
    pub fn all(&self, path: &str, field: &str) -> Result<Vec<Value>, String> {
        let separator = if path.contains('?') { '&' } else { '?' };
        let mut items = Vec::new();
        for page in 1.. {
            let response = self.get(&format!("{path}{separator}per_page=100&page={page}"))?;
            let batch = response[field]
                .as_array()
                .ok_or_else(|| format!("{path}: no {field} list"))?;
            items.extend(batch.iter().cloned());
            if batch.len() < 100 {
                break;
            }
        }
        Ok(items)
    }

    fn text(&self, path: &str) -> Result<String, String> {
        self.request(path)
    }

    fn merged_pulls(&self, sha: &str) -> Result<Vec<Value>, String> {
        let pulls = self.get(&format!("repos/{}/commits/{sha}/pulls", self.repository))?;
        Ok(pulls
            .as_array()
            .into_iter()
            .flatten()
            .filter(|pull| !pull["merged_at"].is_null() && pull["base"]["ref"] == "main")
            .cloned()
            .collect())
    }
}

pub fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set; run this inside GitHub Actions"))
}

fn git(arguments: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(arguments)
        .output()
        .map_err(|error| format!("git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {}: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The verified run a push or completed PR run promotes, if its results can be recovered now.
pub struct Producer {
    pub reuse_run: Option<String>,
    /// The producing PR run has not finished; its completion promotes it instead.
    pub is_waiting: bool,
    /// Base revision whose recipes the promotion is compared against.
    pub base: Option<String>,
}

pub fn producer(
    github: &GitHub,
    workflow: &str,
    reuse_run: Option<String>,
) -> Result<Producer, String> {
    let event: Value = serde_json::from_str(
        &std::fs::read_to_string(env("GITHUB_EVENT_PATH")?).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let source = event.get("workflow_run").filter(|run| !run.is_null());
    let pulls = match source {
        Some(run) => github.merged_pulls(run["head_sha"].as_str().unwrap_or_default())?,
        None if env("GITHUB_EVENT_NAME")? == "push" => github.merged_pulls(&env("GITHUB_SHA")?)?,
        None => Vec::new(),
    };
    promotion(source, &pulls, reuse_run, |head| {
        let runs = github.get(&format!(
            "repos/{}/actions/workflows/{workflow}/runs?head_sha={head}&per_page=100",
            github.repository
        ))?;
        Ok(runs["workflow_runs"]
            .as_array()
            .cloned()
            .unwrap_or_default())
    })
}

/// What a merge promotes, given the PRs it merged and a lookup of each PR head's runs.
fn promotion(
    source: Option<&Value>,
    pulls: &[Value],
    reuse_run: Option<String>,
    mut runs_for: impl FnMut(&str) -> Result<Vec<Value>, String>,
) -> Result<Producer, String> {
    let mut producer = Producer {
        reuse_run,
        is_waiting: source.is_some() && pulls.is_empty(),
        base: None,
    };
    for pull in pulls {
        let head = pull["head"]["sha"].as_str().unwrap_or_default();
        if source.is_some_and(|run| run["head_sha"] != head) {
            continue;
        }
        let runs = runs_for(head)?;
        let Some(run) = verification_run(&runs) else {
            continue;
        };
        producer.is_waiting = run["status"] != "completed";
        if !producer.is_waiting {
            producer.reuse_run = run["id"].as_u64().map(|id| id.to_string());
        }
        break;
    }
    if let Some(run) = source {
        producer.base = pulls
            .iter()
            .find(|pull| pull["head"]["sha"] == run["head_sha"])
            .and_then(|pull| pull["merge_commit_sha"].as_str())
            .map(|sha| format!("{sha}^"));
    }
    Ok(producer)
}

/// A PR head's successful run, else its newest; an unstarted rerun never displaces a result.
fn verification_run(runs: &[Value]) -> Option<&Value> {
    runs.iter()
        .max_by_key(|run| (run["conclusion"] == "success", run["id"].as_u64()))
}

/// A verified run whose retained builds a promotion may recover without rebuilding.
pub struct Retained {
    run: Value,
    artifacts: Vec<Value>,
    jobs: Vec<Value>,
}

/// Admits `run_id`'s retained builds only when it ran the package workflow on approved main, or
/// on the exact PR revision that merged, with the trusted tooling this checkout approves.
pub fn admit(
    github: &GitHub,
    run_id: &str,
    workflow: &str,
    trusted: &[String],
) -> Result<Retained, String> {
    if run_id.is_empty()
        || !run_id.bytes().all(|byte| byte.is_ascii_digit())
        || run_id.starts_with('0')
    {
        return Err("reuse requires a workflow run ID".into());
    }
    let base = format!("repos/{}/actions/runs/{run_id}", github.repository);
    let run = github.get(&base)?;
    let is_retry = run_id == env("GITHUB_RUN_ID")?;
    if !is_retry {
        if run["status"] != "completed" {
            return Err("producer verification is still running".into());
        }
        let head = run["head_sha"]
            .as_str()
            .ok_or("producer run has no head revision")?;
        let pulls = if run["event"] == "pull_request" || run["head_branch"] != "main" {
            github.merged_pulls(head)?
        } else {
            Vec::new()
        };
        let revision = approved_revision(&run, &pulls, &github.repository)?;
        git(&["fetch", "--no-tags", "origin", &revision])?;
        git(&["merge-base", "--is-ancestor", &revision, "HEAD"])?;
        git(&["fetch", "--no-tags", "origin", head])?;
        let mut arguments = vec!["diff", "--name-only", head, &revision, "--"];
        arguments.extend(trusted.iter().map(String::as_str));
        if !git(&arguments)?.is_empty() {
            return Err("producer verification tooling differs from approved tooling".into());
        }
    }
    if run["path"] != format!(".github/workflows/{workflow}") {
        return Err("unrecognized package producer".into());
    }
    Ok(Retained {
        artifacts: github.all(&format!("{base}/artifacts"), "artifacts")?,
        jobs: github.all(&format!("{base}/jobs?filter=all"), "jobs")?,
        run,
    })
}

/// The approved revision a finished run's tooling must match: main's own commit, or the merge of
/// the exact PR revision it verified.
fn approved_revision(run: &Value, pulls: &[Value], repository: &str) -> Result<String, String> {
    let head = run["head_sha"]
        .as_str()
        .ok_or("producer run has no head revision")?;
    if run["event"] == "pull_request" || run["head_branch"] != "main" {
        let approved = pulls
            .iter()
            .find(|pull| pull["head"]["sha"] == head)
            .ok_or("PR results require the exact contributor revision to be merged")?;
        return approved["merge_commit_sha"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "merged PR has no merge commit".into());
    }
    if run["head_repository"]["full_name"] != repository {
        return Err("producer must run on approved main or an exactly merged PR".into());
    }
    Ok(head.to_string())
}

impl Retained {
    /// The build a successful job of the admitted run retained for `task`, with the key it was
    /// built under; a builder on another runner image qualified other inputs than planned.
    pub fn recovery(
        &self,
        github: &GitHub,
        runner: &str,
        task: &PackageTask,
    ) -> Result<Option<Recovery>, String> {
        let expected = format!(
            "{runner} / {} ({}) / Build {}",
            task.package, task.system, task.package
        );
        let Some(job) = build_job(&self.jobs, &expected) else {
            return Ok(None);
        };
        let log = github.text(&format!(
            "repos/{}/actions/jobs/{}/logs",
            github.repository,
            job["id"].as_u64().ok_or("job has no ID")?
        ))?;
        let attempts = self.run["run_attempt"].as_u64().unwrap_or(1);
        let (artifact, key) =
            retained_upload(&log, &self.artifacts, attempts).ok_or_else(|| {
                format!(
                    "{} {}: no intact retained artifact; refusing to rebuild",
                    task.package, task.system
                )
            })?;
        Ok(Some(Recovery {
            run: self.run["id"].as_u64().ok_or("run has no ID")?,
            artifact,
            key,
        }))
    }
}

/// The newest successful job that built or recovered the task; none means it never finished,
/// so it may run again.
fn build_job<'a>(jobs: &'a [Value], expected: &str) -> Option<&'a Value> {
    jobs.iter()
        .filter(|job| {
            job["name"]
                .as_str()
                .is_some_and(|name| name.ends_with(expected))
                && job["conclusion"] == "success"
                && job["steps"].as_array().into_iter().flatten().any(|step| {
                    matches!(
                        step["name"].as_str(),
                        Some(
                            "Build and check this package" | "Recover the admitted verified build"
                        )
                    ) && step["conclusion"] == "success"
                })
        })
        .max_by_key(|job| job["run_attempt"].as_u64())
}

/// The one intact, unexpired artifact a job log reports uploading, and the key it was built under.
fn retained_upload(log: &str, artifacts: &[Value], attempts: u64) -> Option<(u64, String)> {
    let uploads = uploaded_builds(log);
    let [(name, key, attempt, id)] = &uploads[..] else {
        return None;
    };
    let matches: Vec<_> = artifacts
        .iter()
        .filter(|artifact| {
            artifact["id"].as_u64() == Some(*id)
                && artifact["name"] == name.as_str()
                && artifact["expired"] == false
                && (1..=attempts).contains(attempt)
        })
        .collect();
    let [artifact] = &matches[..] else {
        return None;
    };
    let is_intact = artifact["digest"]
        .as_str()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .is_some_and(rootbeer_packaging::is_sha256);
    is_intact.then(|| (*id, key.clone()))
}

/// The package builds a job log reports uploading, as (name, input key, attempt, artifact ID).
fn uploaded_builds(log: &str) -> Vec<(String, String, u64, u64)> {
    const MARKER: &str = " successfully finalized. Artifact ID ";
    log.lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(MARKER)?;
            let name = before.rsplit("Artifact ").next()?.trim();
            let rest = name.strip_prefix("package-")?;
            let (key, attempt) = rest.rsplit_once('-')?;
            if !rootbeer_packaging::is_sha256(key) {
                return None;
            }
            let id = after
                .split(|character: char| !character.is_ascii_digit())
                .next()?;
            Some((
                name.to_string(),
                key.to_string(),
                attempt.parse().ok()?,
                id.parse().ok()?,
            ))
        })
        .collect()
}

/// The newest unexpired build of each input key this run uploaded; a missing one means its job
/// failed, or found a result published after planning.
pub fn run_builds(github: &GitHub, keys: &[String]) -> Result<Vec<u64>, String> {
    let artifacts = github.all(
        &format!(
            "repos/{}/actions/runs/{}/artifacts",
            github.repository,
            env("GITHUB_RUN_ID")?
        ),
        "artifacts",
    )?;
    newest_builds(&artifacts, keys)
}

fn newest_builds(artifacts: &[Value], keys: &[String]) -> Result<Vec<u64>, String> {
    let mut builds: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    for artifact in artifacts {
        let Some(rest) = artifact["name"]
            .as_str()
            .and_then(|name| name.strip_prefix("package-"))
        else {
            continue;
        };
        let Some((key, attempt)) = rest.rsplit_once('-') else {
            continue;
        };
        let (Ok(attempt), Some(id)) = (attempt.parse::<u64>(), artifact["id"].as_u64()) else {
            continue;
        };
        let Some(key) = keys.iter().find(|wanted| wanted.as_str() == key) else {
            continue;
        };
        if artifact["expired"] == false
            && builds
                .get(key.as_str())
                .is_none_or(|(newest, _)| attempt > *newest)
        {
            builds.insert(key, (attempt, id));
        }
    }
    let missing: Vec<_> = keys
        .iter()
        .filter(|key| !builds.contains_key(key.as_str()))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "no build in this run for dependency inputs {}: their job failed, or found a result \
             published after planning; re-run all jobs to plan against it",
            missing.join(", ")
        ));
    }
    Ok(keys.iter().map(|key| builds[key.as_str()].1).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_each_uploaded_build_from_a_job_log() {
        let key = "a".repeat(64);
        let log = format!(
            "2026-09-27T12:00:00.0Z Artifact package-{key}-2 successfully finalized. Artifact ID 4417\n\
             2026-09-27T12:00:01.0Z Artifact published-records-x-1 successfully finalized. Artifact ID 9\n"
        );
        assert_eq!(
            uploaded_builds(&log),
            [(format!("package-{key}-2"), key, 2, 4417)]
        );
    }

    #[test]
    fn the_newest_attempt_of_each_dependency_build_is_used() {
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        let mut artifacts = vec![
            json!({"id": 1, "name": format!("package-{a}-1"), "expired": false}),
            json!({"id": 2, "name": format!("package-{a}-2"), "expired": false}),
            json!({"id": 3, "name": format!("package-{b}-1"), "expired": false}),
            json!({"id": 4, "name": format!("published-records-{b}"), "expired": false}),
        ];
        assert_eq!(
            newest_builds(&artifacts, &[a.clone(), b.clone()]).unwrap(),
            [2, 3]
        );
        artifacts[2]["expired"] = json!(true);
        assert!(newest_builds(&artifacts, &[a, b])
            .unwrap_err()
            .contains("re-run"));
    }

    #[test]
    fn a_completed_verification_wins_over_an_unstarted_rerun() {
        let verified = json!({"id": 10, "status": "completed", "conclusion": "success"});
        let unstarted = json!({"id": 11, "status": "completed", "conclusion": "failure"});
        assert_eq!(
            verification_run(&[verified.clone(), unstarted.clone()]),
            Some(&verified)
        );
        assert_eq!(
            verification_run(std::slice::from_ref(&unstarted)),
            Some(&unstarted)
        );
    }

    #[test]
    fn a_merge_waits_for_its_running_pr_then_recovers_it() {
        let pulls = [json!({"merged_at": "now", "base": {"ref": "main"}, "head": {"sha": "head"}})];
        for (status, is_waiting, reuse) in [
            ("in_progress", true, None),
            ("completed", false, Some("123")),
        ] {
            let producer = promotion(None, &pulls, None, |head| {
                assert_eq!(head, "head");
                Ok(vec![json!({"id": 123, "status": status})])
            })
            .unwrap();
            assert_eq!(producer.is_waiting, is_waiting);
            assert_eq!(producer.reuse_run.as_deref(), reuse);
        }
        let source = json!({"head_sha": "head"});
        let merged = [
            json!({"merged_at": "now", "base": {"ref": "main"}, "head": {"sha": "head"}, "merge_commit_sha": "merge"}),
        ];
        let producer = promotion(Some(&source), &merged, None, |_| Ok(Vec::new())).unwrap();
        assert_eq!(producer.base.as_deref(), Some("merge^"));
        assert!(
            promotion(Some(&source), &[], None, |_| Ok(Vec::new()))
                .unwrap()
                .is_waiting
        );
    }

    #[test]
    fn only_merged_contributor_revisions_and_main_are_admitted() {
        let pr = json!({"head_sha": "abc", "event": "pull_request", "head_branch": "feature"});
        assert!(approved_revision(&pr, &[], "rootbeer-org/pdr")
            .unwrap_err()
            .contains("exact contributor revision"));
        let merged = [json!({"head": {"sha": "abc"}, "merge_commit_sha": "def"})];
        assert_eq!(
            approved_revision(&pr, &merged, "rootbeer-org/pdr").unwrap(),
            "def"
        );
        let other = [json!({"head": {"sha": "other"}, "merge_commit_sha": "def"})];
        assert!(approved_revision(&pr, &other, "rootbeer-org/pdr").is_err());
        let main = json!({"head_sha": "abc", "event": "push", "head_branch": "main", "head_repository": {"full_name": "rootbeer-org/pdr"}});
        assert_eq!(
            approved_revision(&main, &[], "rootbeer-org/pdr").unwrap(),
            "abc"
        );
        let fork = json!({"head_sha": "abc", "event": "push", "head_branch": "main", "head_repository": {"full_name": "fork/pdr"}});
        assert!(approved_revision(&fork, &[], "rootbeer-org/pdr").is_err());
    }

    fn job(conclusion: &str) -> Value {
        json!({"id": 7, "run_attempt": 1, "name": "macos-15 / tool@1 (aarch64-macos) / Build tool@1",
               "conclusion": conclusion, "steps": [{"name": "Build and check this package", "conclusion": "success"}]})
    }

    #[test]
    fn jobs_that_never_finished_may_run_again() {
        let expected = "macos-15 / tool@1 (aarch64-macos) / Build tool@1";
        assert!(build_job(&[], expected).is_none());
        assert!(build_job(&[job("failure")], expected).is_none());
        assert_eq!(
            build_job(&[job("success")], expected),
            Some(&job("success"))
        );
    }

    #[test]
    fn a_retained_build_keeps_its_key_and_is_never_rebuilt_when_damaged() {
        let key = "b".repeat(64);
        let artifact = json!({"name": format!("package-{key}-1"), "id": 123, "expired": false, "digest": format!("sha256:{}", "a".repeat(64))});
        let log = format!("Artifact package-{key}-1 successfully finalized. Artifact ID 123");
        assert_eq!(
            retained_upload(&log, std::slice::from_ref(&artifact), 2),
            Some((123, key.clone()))
        );
        let mut expired = artifact.clone();
        expired["expired"] = json!(true);
        let mut undigested = artifact.clone();
        undigested["digest"] = json!("");
        let mut other = artifact.clone();
        other["id"] = json!(124);
        for artifacts in [vec![], vec![expired], vec![undigested], vec![other]] {
            assert_eq!(retained_upload(&log, &artifacts, 2), None, "{artifacts:?}");
        }
        assert_eq!(
            retained_upload("", std::slice::from_ref(&artifact), 2),
            None
        );
        assert_eq!(retained_upload(&log, &[artifact], 0), None);
    }
}
