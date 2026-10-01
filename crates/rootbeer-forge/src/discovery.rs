//! The discovery lanes: advancing the Rootbeer recipe to CI-verified engine commits, and opening
//! reviewed pull requests for the recipe updates discovery found.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::Command;

use rootbeer_packaging::PackageDefinition;
use serde_json::{json, Value};

use crate::ci::{output, summary};
use crate::config::{Config, Engine};
use crate::github::GitHub;

const INTRO: &str = "Update available package recipes. Each affected package/platform is verified \
                     independently; merging promotes those exact artifacts.";

/// Advances the engine's recipe to the newest main commit its CI verified, writing the candidate
/// discovery proposes. `probe` only reports whether there is one.
pub fn advance_engine(config: &Config, catalog: &Path, is_probe: bool) -> Result<(), String> {
    let engine = config.engine()?;
    let github = GitHub::from_env()?;
    let repository = &engine.repository;
    let revision = github.get(&format!("repos/{repository}/commits/main"))?["sha"]
        .as_str()
        .filter(|sha| is_revision(sha))
        .ok_or("invalid upstream revision")?
        .to_string();
    let runs = github.get(&format!(
        "repos/{repository}/actions/workflows/{}/runs?event=push&head_sha={revision}&per_page=100",
        engine.workflow
    ))?;
    let is_verified = runs["workflow_runs"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|run| {
            run["head_sha"] == revision.as_str()
                && run["head_branch"] == "main"
                && run["path"] == format!(".github/workflows/{}", engine.workflow)
                && run["head_repository"]["full_name"] == repository.as_str()
                && run["conclusion"] == "success"
        });
    if !is_verified {
        println!("{repository} main is awaiting successful CI");
        return Ok(());
    }

    let path = catalog.join(format!("{}.lua", engine.package));
    let source =
        fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let definition = PackageDefinition::from_lua(&source)?;
    let current = pinned_revision(&definition)?;
    if revision.starts_with(&current) {
        return Ok(());
    }
    let comparison = github.get(&format!(
        "repos/{repository}/compare/{current}...{revision}"
    ))?;
    if comparison["status"] != "ahead" {
        return Err(format!(
            "{} must advance its pinned source revision",
            engine.package
        ));
    }
    if is_probe {
        return output("changed", "true");
    }

    let archive = download(&format!(
        "https://codeload.github.com/{repository}/tar.gz/{revision}"
    ))?;
    let version = manifest_version(
        &archive,
        &format!(
            "{}-{revision}/{}",
            repository_name(repository),
            engine.manifest
        ),
    )?;
    let digest = rootbeer_packaging::store::hash_bytes(&archive);
    let advanced = advance(definition, &revision, &version, &digest)?;

    let candidates = Path::new("candidates");
    let destination = candidates.join("packages");
    fs::create_dir_all(&destination).map_err(|error| error.to_string())?;
    if !has_recipes(&destination)? {
        for entry in fs::read_dir(catalog).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?.path();
            if entry
                .extension()
                .is_some_and(|extension| extension == "lua")
            {
                fs::copy(&entry, destination.join(entry.file_name().unwrap()))
                    .map_err(|error| error.to_string())?;
            }
        }
    }
    fs::write(
        destination.join(format!("{}.lua", engine.package)),
        advanced.to_lua()?,
    )
    .map_err(|error| error.to_string())?;
    let report_path = candidates.join("report.json");
    let mut report: Value =
        serde_json::from_slice(&fs::read(&report_path).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    let mut updated: Vec<String> =
        serde_json::from_value(report["updated"].clone()).unwrap_or_default();
    if !updated.contains(&engine.package) {
        updated.push(engine.package.clone());
        updated.sort();
    }
    report["updated"] = json!(updated);
    if let Some(untracked) = report["untracked"].as_array_mut() {
        untracked.retain(|name| name != engine.package.as_str());
    }
    fs::write(
        &report_path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
        ),
    )
    .map_err(|error| error.to_string())?;
    let mut text = fs::read_to_string(candidates.join("summary.md")).unwrap_or_default();
    text.push_str(&format!(
        "\n{}: advance to CI-verified `{revision}`.\n",
        engine.package
    ));
    fs::write(candidates.join("summary.md"), text).map_err(|error| error.to_string())
}

/// The recipe with a version built from `revision` as every platform's default. Its source and
/// build come from the recipe's `{commit}` templates, so the version records only its commit.
fn advance(
    mut definition: PackageDefinition,
    revision: &str,
    base: &str,
    digest: &str,
) -> Result<PackageDefinition, String> {
    if !is_revision(revision) {
        return Err("invalid source revision".into());
    }
    let parts: Vec<_> = base.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(format!("unsupported base version `{base}`"));
    }
    if !rootbeer_packaging::is_sha256(digest) {
        return Err("invalid source digest".into());
    }
    let version = format!("{base}-main+{}", &revision[..12]);
    let systems = definition.platforms();
    if systems
        .iter()
        .all(|system| definition.package.default_version_for(system) == Some(version.as_str()))
    {
        return Ok(definition);
    }
    if definition.package.versions.contains_key(&version) {
        return Err(format!(
            "refusing to move back to retained version {version}"
        ));
    }
    let digests: BTreeMap<_, _> = systems
        .iter()
        .map(|system| (system.clone(), digest.to_string()))
        .collect();
    definition.add_version(&version, digests, None, Some(revision.to_string()))?;
    for system in &systems {
        definition.set_default_version(system, &version)?;
    }
    Ok(definition)
}

/// The engine commit every platform's default version is built from.
fn pinned_revision(definition: &PackageDefinition) -> Result<String, String> {
    let mut revisions: Vec<_> = definition
        .platforms()
        .iter()
        .filter_map(|system| definition.package.default_version_for(system))
        .filter_map(|version| {
            version
                .rsplit_once('+')
                .map(|(_, revision)| revision.to_string())
        })
        .collect();
    revisions.dedup();
    match &revisions[..] {
        [revision]
            if revision.len() == 12 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
        {
            Ok(revision.clone())
        }
        _ => Err("expected every platform pinned to one main revision".into()),
    }
}

fn is_revision(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn repository_name(repository: &str) -> &str {
    repository.rsplit('/').next().unwrap_or(repository)
}

fn has_recipes(directory: &Path) -> Result<bool, String> {
    Ok(fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .any(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "lua")
        }))
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    let mut response = ureq::get(url)
        .call()
        .map_err(|error| format!("{url}: {error}"))?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(512 << 20)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{url}: {error}"))?;
    Ok(bytes)
}

/// The package version a Cargo manifest inside a source archive declares.
fn manifest_version(archive: &[u8], path: &str) -> Result<String, String> {
    let mut entries = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in entries.entries().map_err(|error| error.to_string())? {
        let mut entry = entry.map_err(|error| error.to_string())?;
        if entry
            .path()
            .map_err(|error| error.to_string())?
            .to_string_lossy()
            != path
        {
            continue;
        }
        let mut text = String::new();
        entry
            .read_to_string(&mut text)
            .map_err(|error| error.to_string())?;
        let manifest: toml::Table =
            toml::from_str(&text).map_err(|error| format!("{path}: {error}"))?;
        return manifest
            .get("package")
            .and_then(|package| package.get("version"))
            .and_then(|version| version.as_str())
            .map(str::to_string)
            .ok_or_else(|| format!("{path} declares no package version"));
    }
    Err(format!("the source archive has no {path}"))
}

/// Opens or updates one reviewed pull request per lane for the candidate recipes of `requests`.
pub fn propose(config: &Config, requests: &[String]) -> Result<(), String> {
    let engine = config.engine()?;
    if requests.is_empty() || requests.iter().any(|request| !is_request(request)) {
        return Err("expected exact package versions".into());
    }
    let github = GitHub::from_env()?;
    let repository = github.repository.clone();
    let mut pulls = Vec::new();
    for pull in github.all(&format!("repos/{repository}/pulls?state=open"), "")? {
        let number = pull["number"]
            .as_u64()
            .ok_or("pull request has no number")?;
        let files = github.all(&format!("repos/{repository}/pulls/{number}/files"), "")?;
        pulls.push(Proposal {
            number,
            branch: pull["head"]["ref"].as_str().unwrap_or_default().to_string(),
            url: pull["html_url"].as_str().unwrap_or_default().to_string(),
            files: files
                .iter()
                .filter_map(|file| file["filename"].as_str().map(str::to_string))
                .collect(),
        });
    }

    git(&["fetch", "origin", "main"])?;
    if git(&["rev-parse", "origin/main"])? != crate::github::env("GITHUB_SHA")? {
        return Err(
            "main advanced during discovery; rerun discovery against current recipes".into(),
        );
    }
    let bot = format!("{}[bot]", crate::github::env("APP_SLUG")?);
    let bot_id = github.get(&format!("users/{bot}"))?["id"]
        .as_u64()
        .ok_or("bot has no user ID")?;
    git(&["config", "user.name", &bot])?;
    git(&[
        "config",
        "user.email",
        &format!("{bot_id}+{bot}@users.noreply.github.com"),
    ])?;
    let token = std::env::var("GH_TOKEN").map_err(|_| "proposals need GH_TOKEN")?;
    let credentials = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        format!("x-access-token:{token}"),
    );
    git(&[
        "config",
        "http.https://github.com/.extraheader",
        &format!("AUTHORIZATION: basic {credentials}"),
    ])?;

    for (lane, lane_requests) in split_lanes(requests, &engine.package) {
        propose_lane(&github, &pulls, lane, &lane_requests, engine)?;
    }
    Ok(())
}

struct Proposal {
    number: u64,
    branch: String,
    url: String,
    files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Lane {
    /// The engine's recipe alone, auto-merged once its checks pass.
    Engine,
    Packages,
}

impl Lane {
    fn name(self) -> &'static str {
        match self {
            Self::Engine => "engine",
            Self::Packages => "packages",
        }
    }
}

/// The engine always proposes on its own, so its auto-merged lane never carries other packages.
fn split_lanes(requests: &[String], engine: &str) -> BTreeMap<Lane, Vec<String>> {
    let mut lanes: BTreeMap<Lane, Vec<String>> = BTreeMap::new();
    for request in requests {
        let lane = if package_name(request) == engine {
            Lane::Engine
        } else {
            Lane::Packages
        };
        lanes.entry(lane).or_default().push(request.clone());
    }
    lanes
}

fn lane_proposal(pulls: &[Proposal], lane: Lane) -> Option<&Proposal> {
    pulls.iter().find(|pull| {
        pull.branch
            .starts_with(&format!("updates/{}-", lane.name()))
    })
}

/// Proposals replace whole recipe files, so two touching one recipe would clobber each other.
fn conflicting_proposal<'a>(
    pulls: &'a [Proposal],
    names: &[&str],
    current: Option<u64>,
) -> Option<&'a Proposal> {
    pulls.iter().find(|pull| {
        Some(pull.number) != current
            && pull.branch.starts_with("updates/")
            && names
                .iter()
                .any(|name| pull.files.contains(&format!("packages/{name}.lua")))
    })
}

/// A lane keeps one open proposal, so a package's new versions replace its pending ones.
fn body_text(requests: &[String]) -> String {
    let list: Vec<_> = requests
        .iter()
        .map(|request| format!("- `{request}`"))
        .collect();
    format!("{INTRO}\n\n{}\n", list.join("\n"))
}

fn propose_lane(
    github: &GitHub,
    pulls: &[Proposal],
    lane: Lane,
    requests: &[String],
    engine: &Engine,
) -> Result<(), String> {
    let repository = &github.repository;
    let mut names: Vec<&str> = requests
        .iter()
        .map(|request| package_name(request))
        .collect();
    names.sort();
    names.dedup();
    let current = lane_proposal(pulls, lane);
    if let Some(blocking) = conflicting_proposal(pulls, &names, current.map(|pull| pull.number)) {
        return summary(&format!(
            "Another proposal already updates these recipes: {}\n",
            blocking.url
        ));
    }
    let is_engine = lane == Lane::Engine;
    // Each run proposes every available update, so a lane restarts from main and never
    // conflicts with recipe changes merged since its last run.
    let branch = match current {
        Some(pull) => pull.branch.clone(),
        None => format!(
            "updates/{}-{}",
            lane.name(),
            crate::github::env("GITHUB_RUN_ID")?
        ),
    };
    git(&["switch", "--force-create", &branch, "origin/main"])?;
    let paths: Vec<String> = names
        .iter()
        .map(|name| format!("packages/{name}.lua"))
        .collect();
    for path in &paths {
        fs::copy(Path::new("candidates").join(path), path)
            .map_err(|error| format!("{path}: {error}"))?;
    }
    let mut add = vec!["add", "--"];
    add.extend(paths.iter().map(String::as_str));
    git(&add)?;
    if git(&["status", "--porcelain"])?.is_empty() {
        let url = current.map_or("", |pull| pull.url.as_str());
        return summary(&format!("{url} already proposes these recipes\n"));
    }
    git(&["commit", "-m", "chore(packages): propose upstream updates"])?;
    let target = format!("HEAD:refs/heads/{branch}");
    git(&["push", "--force", "origin", target.as_str()])?;

    let title = if is_engine {
        format!("chore(packages): update {}", engine.package)
    } else {
        "chore(packages): update available packages".to_string()
    };
    let body = body_text(requests);
    let (number, url) = match current {
        Some(pull) => {
            github.patch(
                &format!("repos/{repository}/pulls/{}", pull.number),
                &json!({"title": title, "body": body}),
            )?;
            (pull.number, pull.url.clone())
        }
        None => {
            let created = github.post(
                &format!("repos/{repository}/pulls"),
                &json!({"title": title, "body": body, "head": branch, "base": "main"}),
            )?;
            (
                created["number"]
                    .as_u64()
                    .ok_or("created pull request has no number")?,
                created["html_url"].as_str().unwrap_or_default().to_string(),
            )
        }
    };
    if is_engine {
        // Required package checks still gate the merge, and merging publishes their verified builds.
        let node = github.get(&format!("repos/{repository}/pulls/{number}"))?["node_id"]
            .as_str()
            .ok_or("pull request has no node ID")?
            .to_string();
        github.graphql(
            "mutation($id: ID!, $headline: String!) { enablePullRequestAutoMerge(input: {pullRequestId: $id, \
             mergeMethod: SQUASH, commitHeadline: $headline, commitBody: \"\"}) { clientMutationId } }",
            json!({"id": node, "headline": format!("{title} (#{number})")}),
        )?;
    }
    summary(&format!("Recipe changes and per-package checks: {url}\n"))
}

fn package_name(request: &str) -> &str {
    request.split('@').next().unwrap_or(request)
}

fn is_request(request: &str) -> bool {
    let Some((name, version)) = request.split_once('@') else {
        return false;
    };
    name.bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"+._-".contains(&byte)
        })
        && !version.is_empty()
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+-".contains(&byte))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pull(number: u64, branch: &str, files: &[&str]) -> Proposal {
        Proposal {
            number,
            branch: branch.into(),
            url: format!("https://example.com/{branch}"),
            files: files.iter().map(|file| file.to_string()).collect(),
        }
    }

    fn requests(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn proposals_touching_the_same_recipe_wait_for_review() {
        let open = [pull(1, "updates/packages-1", &["packages/kitty.lua"])];
        assert!(conflicting_proposal(&open, &["rootbeer"], None).is_none());
        let open = [pull(
            1,
            "updates/packages-1",
            &["packages/kitty.lua", "packages/rootbeer.lua"],
        )];
        assert_eq!(
            conflicting_proposal(&open, &["rootbeer"], None)
                .unwrap()
                .number,
            1
        );
        assert!(conflicting_proposal(&open, &["rootbeer"], Some(1)).is_none());
        let unrelated = [pull(1, "fix/something", &["packages/rootbeer.lua"])];
        assert!(conflicting_proposal(&unrelated, &["rootbeer"], None).is_none());
    }

    #[test]
    fn the_engine_always_proposes_on_its_own_lane() {
        let lanes = split_lanes(
            &requests(&[
                "kitty@0.49.0",
                "rootbeer@0.1.0-main+a248a77d983a",
                "zoxide@1.0",
            ]),
            "rootbeer",
        );
        assert_eq!(lanes[&Lane::Packages], ["kitty@0.49.0", "zoxide@1.0"]);
        assert_eq!(lanes[&Lane::Engine], ["rootbeer@0.1.0-main+a248a77d983a"]);
        assert_eq!(
            split_lanes(&requests(&["kitty@0.49.0"]), "rootbeer").len(),
            1
        );
        let open = [
            pull(1, "updates/packages-1", &[]),
            pull(2, "updates/engine-2", &[]),
        ];
        assert_eq!(lane_proposal(&open, Lane::Engine).unwrap().number, 2);
        assert!(lane_proposal(&[], Lane::Engine).is_none());
    }

    #[test]
    fn requests_name_exact_lowercase_versions() {
        assert!(
            is_request("rootbeer@0.1.0-main+a248a77d983a")
                && !is_request("Kitty@1")
                && !is_request("kitty")
        );
    }

    fn recipe() -> PackageDefinition {
        let digest = "a".repeat(64);
        let old = "1".repeat(40);
        PackageDefinition::from_lua(&format!(
            r#"return {{
                name = "rootbeer", description = "Rootbeer", homepage = "https://rbpkg.com",
                default_license = "MIT",
                source = {{ url = "https://codeload.github.com/rootbeer-org/rootbeer/tar.gz/{{commit}}",
                           archive = "tar.gz", strip_prefix = "rootbeer-{{commit}}" }},
                build = {{ backend = "rust", rust = {{ packages = {{ "rootbeer-cli" }},
                          environment = {{ RB_SOURCE_REVISION = "{{commit}}" }} }} }},
                outputs = {{ bins = {{ "rb" }}, checks = {{ {{ "rb", "--version" }} }} }},
                platforms = {{ ["aarch64-macos"] = {{ default_version = "0.1.0-main+{short}" }},
                              ["x86_64-linux"] = {{ default_version = "0.1.0-main+{short}" }} }},
                versions = {{ ["0.1.0-main+{short}"] = {{ commit = "{old}",
                    digests = {{ ["aarch64-macos"] = "{digest}", ["x86_64-linux"] = "{digest}" }} }} }},
            }}"#,
            short = &old[..12],
        ))
        .unwrap()
    }

    #[test]
    fn advancing_pins_every_platform_to_the_new_commit_and_is_idempotent() {
        let revision = "2".repeat(40);
        let digest = "b".repeat(64);
        let advanced = advance(recipe(), &revision, "0.2.0", &digest).unwrap();
        let version = format!("0.2.0-main+{}", &revision[..12]);
        for system in ["aarch64-macos", "x86_64-linux"] {
            assert_eq!(
                advanced.package.default_version_for(system),
                Some(version.as_str())
            );
            let build = advanced.package.versions[&version].platforms[system]
                .build
                .as_ref()
                .unwrap();
            assert_eq!(build.sha256, digest);
            assert!(build.url.ends_with(&revision));
            assert_eq!(
                build.rust.as_ref().unwrap().environment["RB_SOURCE_REVISION"],
                revision
            );
        }
        assert!(advanced
            .package
            .versions
            .contains_key(&format!("0.1.0-main+{}", "1".repeat(12))));
        assert_eq!(pinned_revision(&advanced).unwrap(), &revision[..12]);
        let again = advance(advanced.clone(), &revision, "0.2.0", &digest).unwrap();
        assert_eq!(again.to_lua().unwrap(), advanced.to_lua().unwrap());
    }

    #[test]
    fn advancing_rejects_bad_identities_and_rollbacks() {
        let digest = "b".repeat(64);
        assert!(advance(recipe(), "abc", "0.2.0", &digest).is_err());
        assert!(advance(recipe(), &"2".repeat(40), "0.2", &digest).is_err());
        assert!(advance(recipe(), &"2".repeat(40), "0.2.0", "short").is_err());
        let advanced = advance(recipe(), &"2".repeat(40), "0.2.0", &digest).unwrap();
        let error = advance(advanced, &"1".repeat(40), "0.1.0", &digest).unwrap_err();
        assert!(error.contains("retained"), "{error}");
    }
}
