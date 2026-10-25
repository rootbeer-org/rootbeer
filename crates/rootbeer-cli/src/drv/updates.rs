use super::{git, github, load, Sources};
use globset::{Glob, GlobSetBuilder};
use rootbeer_drv::Platform;
use rootbeer_eval::{Catalog, Upstream};
use rootbeer_profile::is_repository;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

const WORKERS: usize = 8;
const STALL: Duration = Duration::from_secs(30);
const TIMEOUT: Duration = Duration::from_secs(120);

#[derive(clap::Args, Debug)]
pub(super) struct Args {
    /// The recipes to check, or every recipe when none are named
    names: Vec<String>,
    #[command(flatten)]
    sources: Sources,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
enum Line {
    Update {
        name: String,
        version: String,
        platforms: Vec<Platform>,
        #[serde(skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
    },
    Error {
        name: String,
        error: String,
    },
    Skipped {
        name: String,
        skipped: String,
    },
}

/// A stable version an upstream tag names.
#[derive(Debug)]
struct Candidate {
    numbers: Vec<u64>,
    version: String,
    tag: String,
    commit: String,
}

/// What a run needs of a GitHub release.
#[derive(Clone)]
struct Release {
    is_stable: bool,
    assets: BTreeSet<String>,
}

struct Checker<'a> {
    catalog: &'a Catalog,
    agent: ureq::Agent,
    releases: Mutex<BTreeMap<(String, String), Option<Release>>>,
}

/// Prints, as JSON lines, the newest stable upstream version each platform of
/// each recipe can advance to, and why a recipe couldn't be checked.
pub(super) fn updates(args: &Args) -> Result<(), String> {
    let (catalog, _) = load(&args.sources)?;
    let names: BTreeSet<&str> = match args.names.is_empty() {
        true => catalog.names().collect(),
        false => args.names.iter().map(String::as_str).collect(),
    };
    let names: Vec<&str> = names.into_iter().collect();

    let checker = Checker {
        catalog: &catalog,
        agent: ureq::Agent::config_builder()
            .https_only(true)
            .timeout_connect(Some(STALL))
            .timeout_recv_body(Some(STALL))
            .timeout_global(Some(TIMEOUT))
            .build()
            .into(),
        releases: Mutex::new(BTreeMap::new()),
    };

    let next = AtomicUsize::new(0);
    let lines = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..WORKERS)
            .map(|_| {
                scope.spawn(|| {
                    let mut lines = Vec::new();
                    while let Some(name) = names.get(next.fetch_add(1, Ordering::Relaxed)) {
                        lines.extend(checker.updates(name).unwrap_or_else(|line| vec![line]));
                    }

                    lines
                })
            })
            .collect();

        workers
            .into_iter()
            .map(|worker| worker.join().map_err(|_| "a worker panicked".to_string()))
            .collect::<Result<Vec<_>, String>>()
    })?;

    // One worker writes all of a recipe's lines, so a stable sort keeps them
    // in order.
    let mut lines: Vec<Line> = lines.into_iter().flatten().collect();
    lines.sort_by(|a, b| a.name().cmp(b.name()));

    let mut out = io::stdout().lock();
    for line in lines {
        let json = serde_json::to_string(&line).map_err(|error| error.to_string())?;
        writeln!(out, "{json}").map_err(|error| error.to_string())?;
    }

    Ok(())
}

impl Checker<'_> {
    fn updates(&self, name: &str) -> Result<Vec<Line>, Line> {
        let error = |error: String| Line::Error {
            name: name.to_string(),
            error,
        };
        let skipped = |skipped: String| Line::Skipped {
            name: name.to_string(),
            skipped,
        };
        let problem = |problem: Problem| match problem {
            Problem::Error(message) => error(message),
            Problem::Skipped(message) => skipped(message),
        };

        let catalog = self.catalog;
        if !catalog.contains(name) {
            return Err(error("it isn't in the catalog".into()));
        }

        let upstreams = catalog.upstreams(name);
        if upstreams.is_empty() {
            return Err(skipped("it declares no upstream".into()));
        }

        let mut advanced = Vec::new();
        for (platforms, upstream) in upstreams {
            let (repository, github) = locate(upstream).map_err(problem)?;

            let currents = platforms
                .into_iter()
                .map(|platform| {
                    let current = catalog.default_version(name, platform).unwrap_or_default();
                    let numbers = numbers(current).ok_or_else(|| {
                        skipped(format!(
                            "its {platform} default {current} isn't a plain dotted version"
                        ))
                    })?;
                    Ok((platform, numbers))
                })
                .collect::<Result<Vec<_>, Line>>()?;

            // A pooled connection the server already closed fails once, so
            // a second try gets a fresh one.
            let tags = git::tags(&self.agent, &repository)
                .or_else(|_| git::tags(&self.agent, &repository))
                .map_err(error)?;
            let candidates = candidates(upstream, tags).map_err(error)?;
            let mut is_verified = false;
            for (platform, current) in currents {
                let newer = newer(&candidates, &current).map_err(error)?;
                if newer.is_empty() {
                    continue;
                }

                if !is_verified {
                    self.verify(github, upstream.repository_id).map_err(error)?;
                    is_verified = true;
                }

                for candidate in newer {
                    let is_available = self
                        .is_available(name, github, candidate, platform)
                        .map_err(problem)?;
                    if is_available {
                        advanced.push((
                            platform,
                            candidate.version.clone(),
                            candidate.commit.clone(),
                        ));
                        break;
                    }
                }
            }
        }

        Ok(group(name, advanced, catalog.has_commit_placeholder(name)))
    }

    /// Fails when the GitHub repository at the upstream's name isn't the one
    /// the recipe pinned, as after a rename lets someone else take the name.
    fn verify(&self, github: Option<&str>, expected: Option<u64>) -> Result<(), String> {
        let (Some(repository), Some(expected)) = (github, expected) else {
            return Ok(());
        };

        let id = github::repository_id(repository)?;
        if id != expected {
            return Err(format!(
                "github:{repository} is repository {id}, not the pinned {expected}, so it \
                 isn't followed"
            ));
        }

        Ok(())
    }

    /// Whether `candidate` is released and its download for `platform` exists.
    fn is_available(
        &self,
        name: &str,
        github: Option<&str>,
        candidate: &Candidate,
        platform: Platform,
    ) -> Result<bool, Problem> {
        if let Some(repository) = github {
            let release = self
                .release(repository, &candidate.tag)
                .map_err(Problem::Error)?;
            if release.is_some_and(|release| !release.is_stable) {
                return Ok(false);
            }
        }

        let version = &candidate.version;
        let download = self
            .catalog
            .download(name, version, Some(&candidate.commit), None, platform)
            .map_err(|error| {
                Problem::Skipped(format!(
                    "{version} on {platform} has no download rb can find: {error}"
                ))
            })?;

        let Some(asset) = download.release else {
            return self.exists(&download.url).map_err(Problem::Error);
        };

        let release = self
            .release(&asset.repository, &asset.tag)
            .map_err(Problem::Error)?;
        Ok(release
            .is_some_and(|release| release.is_stable && release.assets.contains(&asset.asset)))
    }

    fn release(&self, repository: &str, tag: &str) -> Result<Option<Release>, String> {
        let key = (repository.to_string(), tag.to_string());
        if let Some(release) = self.lock().get(&key) {
            return Ok(release.clone());
        }

        let release = github::release(repository, tag)?.map(|release| Release {
            is_stable: !release.draft && !release.prerelease,
            assets: release.assets.into_iter().map(|asset| asset.name).collect(),
        });
        self.lock().insert(key, release.clone());
        Ok(release)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<(String, String), Option<Release>>> {
        self.releases
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether a URL answers with success after redirects. Some servers refuse
    /// HEAD, or answer it with a 400 from a cold CDN cache, so a HEAD that
    /// fails short of not found asks for the first byte instead.
    fn exists(&self, url: &str) -> Result<bool, String> {
        let response = match self.agent.head(url).call() {
            Err(ureq::Error::StatusCode(status))
                if (400..500).contains(&status) && !matches!(status, 404 | 410) =>
            {
                self.agent.get(url).header("Range", "bytes=0-0").call()
            }
            response => response,
        };

        match response {
            Ok(response) => Ok(response.status().is_success()),
            Err(ureq::Error::StatusCode(403 | 404 | 410)) => Ok(false),
            Err(error) => Err(format!("{url}: {error}")),
        }
    }
}

enum Problem {
    Error(String),
    Skipped(String),
}

/// The URL an upstream's tags are fetched from, and its GitHub repository.
fn locate(upstream: &Upstream) -> Result<(String, Option<&str>), Problem> {
    match (&upstream.github, &upstream.git) {
        (Some(repository), None) if is_repository(repository) => Ok((
            format!("https://github.com/{repository}.git"),
            Some(repository),
        )),
        (Some(repository), None) => Err(Problem::Error(format!(
            "its upstream github:{repository} isn't in the form owner/repo"
        ))),
        (None, Some(url)) if url.starts_with("https://") => Ok((url.clone(), None)),
        (None, Some(url)) => Err(Problem::Error(format!(
            "its upstream git repository {url} isn't an https URL"
        ))),
        (Some(_), Some(_)) => Err(Problem::Error(
            "its upstream names both a github and a git repository".into(),
        )),
        (None, None) => Err(Problem::Skipped(
            "its upstream names neither a github nor a git repository".into(),
        )),
    }
}

/// The stable versions tags name, newest first.
fn candidates(upstream: &Upstream, tags: Vec<git::Tag>) -> Result<Vec<Candidate>, String> {
    let mut excluded = GlobSetBuilder::new();
    for pattern in &upstream.exclude_tags {
        let glob = Glob::new(pattern)
            .map_err(|err| format!("its excluded tag {pattern} isn't a valid glob ({err})"))?;
        excluded.add(glob);
    }
    let excluded = excluded
        .build()
        .map_err(|err| format!("its excluded tags don't form a glob set ({err})"))?;

    let mut candidates: Vec<Candidate> = tags
        .into_iter()
        .filter(|tag| !excluded.is_match(&tag.name))
        .filter_map(|tag| {
            let version = upstream.version_of(&tag.name)?;
            Some(Candidate {
                numbers: numbers(&version)?,
                version,
                tag: tag.name,
                commit: tag.commit,
            })
        })
        .collect();

    candidates.sort_by(|a, b| (&b.numbers, &b.version).cmp(&(&a.numbers, &a.version)));
    Ok(candidates)
}

/// The candidates newer than `current`, newest first. Two tags naming one
/// version, like `1.2` and `1.2.0`, leave which to take unclear.
fn newer<'a>(candidates: &'a [Candidate], current: &[u64]) -> Result<Vec<&'a Candidate>, String> {
    let newer: Vec<&Candidate> = candidates
        .iter()
        .take_while(|candidate| candidate.numbers.as_slice() > current)
        .collect();

    let same = newer.windows(2).find_map(|pair| match pair {
        [a, b] if a.numbers == b.numbers => Some((a, b)),
        _ => None,
    });
    if let Some((a, b)) = same {
        return Err(format!(
            "tags {} and {} name the same version, so which to take is unclear",
            b.tag, a.tag
        ));
    }

    Ok(newer)
}

/// A plain dotted version's numbers without trailing zeros, so `1.2` and
/// `1.2.0` are equal. Anything else, like `1.2rc1`, is none.
fn numbers(version: &str) -> Option<Vec<u64>> {
    let mut numbers = version
        .split('.')
        .map(|part| {
            let is_number = !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
            is_number.then(|| part.parse().ok()).flatten()
        })
        .collect::<Option<Vec<u64>>>()?;

    while numbers.last() == Some(&0) {
        numbers.pop();
    }

    Some(numbers)
}

/// One update line per version, naming every platform that advances to it.
/// Platforms that would build from different commits get their own lines.
fn group(name: &str, advanced: Vec<(Platform, String, String)>, has_commit: bool) -> Vec<Line> {
    let mut versions: BTreeMap<(String, Option<String>), Vec<Platform>> = BTreeMap::new();
    for (platform, version, commit) in advanced {
        let commit = has_commit.then_some(commit);
        versions
            .entry((version, commit))
            .or_default()
            .push(platform);
    }

    versions
        .into_iter()
        .map(|((version, commit), mut platforms)| {
            platforms.sort_by_key(|platform| platform.to_string());
            Line::Update {
                name: name.to_string(),
                version,
                platforms,
                commit,
            }
        })
        .collect()
}

impl Line {
    fn name(&self) -> &str {
        match self {
            Line::Update { name, .. } | Line::Error { name, .. } | Line::Skipped { name, .. } => {
                name
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(tag: &str, separator: Option<&str>, exclude: &[&str]) -> Upstream {
        Upstream {
            github: None,
            git: None,
            repository_id: None,
            tag: Some(tag.into()),
            separator: separator.map(String::from),
            exclude_tags: exclude.iter().map(|tag| tag.to_string()).collect(),
        }
    }

    fn tags(names: &[&str]) -> Vec<git::Tag> {
        names
            .iter()
            .map(|name| git::Tag {
                name: name.to_string(),
                commit: format!("{name}-commit"),
            })
            .collect()
    }

    fn versions(candidates: &[&Candidate]) -> Vec<String> {
        candidates
            .iter()
            .map(|candidate| candidate.version.clone())
            .collect()
    }

    #[test]
    fn only_stable_versions_that_fit_the_template_are_candidates() {
        let curl = upstream("curl-{version}", Some("_"), &["curl-9_0_0"]);
        let found = candidates(
            &curl,
            tags(&[
                "curl-8_9_1",
                "curl-8_10_0",
                "curl-8.11.0",
                "curl-8_11_0-rc1",
                "curl-9_0_0",
                "v8_12_0",
                "curl-8_2",
            ]),
        )
        .unwrap();

        let found: Vec<&Candidate> = found.iter().collect();
        assert_eq!(versions(&found), ["8.10.0", "8.9.1", "8.2"]);
        assert_eq!(found[0].tag, "curl-8_10_0");
        assert_eq!(found[0].commit, "curl-8_10_0-commit");
    }

    #[test]
    fn excluded_tags_are_globs() {
        let perl = upstream("v{version}", None, &["v5.?[13579].*"]);
        let found = candidates(&perl, tags(&["v5.44.1", "v5.45.4", "v5.46.0", "v5.47.0"])).unwrap();
        let found: Vec<&Candidate> = found.iter().collect();
        assert_eq!(versions(&found), ["5.46.0", "5.44.1"]);

        let broken = upstream("v{version}", None, &["v5.[13"]);
        assert!(candidates(&broken, tags(&["v5.44.1"])).is_err());
    }

    #[test]
    fn newer_versions_compare_by_number_and_equal_ones_are_ambiguous() {
        let plain = upstream("v{version}", None, &[]);
        let found =
            candidates(&plain, tags(&["v1.10", "v1.9.0", "v1.2.0", "v2026.01.05"])).unwrap();

        let newer_than =
            |current: &str| newer(&found, &numbers(current).unwrap()).map(|newer| versions(&newer));
        assert_eq!(newer_than("1.9").unwrap(), ["2026.01.05", "1.10"]);
        assert_eq!(newer_than("2026.1.5.0").unwrap(), Vec::<String>::new());

        let both = candidates(&plain, tags(&["v1.3", "v1.3.0"])).unwrap();
        assert_eq!(
            newer(&both, &numbers("1.2").unwrap()).unwrap_err(),
            "tags v1.3 and v1.3.0 name the same version, so which to take is unclear"
        );

        assert_eq!(numbers("1.2.0"), numbers("1.2"));
        assert_eq!(numbers("+1.2"), None);
        assert_eq!(numbers("1..2"), None);
    }

    #[test]
    fn platforms_that_advance_together_share_a_line() {
        let advanced = vec![
            (Platform::X86_64Linux, "1.1".to_string(), "a".to_string()),
            (Platform::Aarch64Macos, "1.1".to_string(), "b".to_string()),
            (Platform::Aarch64Linux, "1.1".to_string(), "a".to_string()),
        ];

        let line = |platforms: Vec<Platform>, commit: Option<&str>| Line::Update {
            name: "tool".into(),
            version: "1.1".into(),
            platforms,
            commit: commit.map(String::from),
        };

        assert_eq!(
            group("tool", advanced.clone(), false),
            [line(
                vec![
                    Platform::Aarch64Linux,
                    Platform::Aarch64Macos,
                    Platform::X86_64Linux
                ],
                None
            )]
        );
        assert_eq!(
            group("tool", advanced, true),
            [
                line(
                    vec![Platform::Aarch64Linux, Platform::X86_64Linux],
                    Some("a")
                ),
                line(vec![Platform::Aarch64Macos], Some("b")),
            ]
        );
    }
}
