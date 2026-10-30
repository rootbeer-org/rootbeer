use super::{git, github, load, Sources};
use globset::{Glob, GlobSetBuilder};
use rootbeer_drv::Platform;
use rootbeer_eval::{Catalog, Rolling, Upstream};
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
        #[serde(skip_serializing_if = "Option::is_none")]
        serial: Option<u64>,
        #[serde(rename = "automerge", skip_serializing_if = "std::ops::Not::not")]
        should_automerge: bool,
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

/// A stable version an upstream tag names, or the build a rolling tag names.
#[derive(Debug)]
struct Candidate {
    numbers: Vec<u64>,
    version: String,
    tag: String,
    commit: String,
    serial: Option<u64>,
    is_rolling: bool,
}

/// A platform's advance to a version.
#[derive(Clone)]
struct Advance {
    platform: Platform,
    version: String,
    commit: String,
    serial: Option<u64>,
}

/// What a run needs of a GitHub release.
#[derive(Clone)]
struct Release {
    is_draft: bool,
    is_prerelease: bool,
    assets: BTreeSet<String>,
}

impl Release {
    /// A rolling tag's release is usually a prerelease, and still its builds.
    fn is_published(&self, is_rolling: bool) -> bool {
        !self.is_draft && (is_rolling || !self.is_prerelease)
    }
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

        let should_automerge = upstreams
            .iter()
            .any(|(_, upstream)| upstream.should_automerge);
        let mut advanced = Vec::new();
        for (platforms, upstream) in upstreams {
            let (repository, github) = locate(upstream).map_err(problem)?;
            if let Some(rolling) = &upstream.rolling {
                let Some(github) = github else {
                    return Err(error(
                        "its rolling upstream needs a github repository".into(),
                    ));
                };

                advanced.extend(
                    self.rolling(name, platforms, upstream, rolling, github)
                        .map_err(problem)?,
                );
                continue;
            }

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
                        advanced.push(Advance::to(platform, candidate));
                        break;
                    }
                }
            }
        }

        Ok(group(
            name,
            advanced,
            catalog.has_commit_placeholder(name),
            should_automerge,
        ))
    }

    /// The platforms that advance to the build a rolling tag names now. The
    /// build is versioned by its commit and ordered by when it was committed.
    fn rolling(
        &self,
        name: &str,
        platforms: Vec<Platform>,
        upstream: &Upstream,
        rolling: &Rolling,
        github: &str,
    ) -> Result<Vec<Advance>, Problem> {
        let error = Problem::Error;
        if upstream.tag.is_some()
            || upstream.separator.is_some()
            || !upstream.exclude_tags.is_empty()
        {
            return Err(error(
                "its rolling upstream follows one tag, so tag, separator, and exclude_tags \
                 don't apply"
                    .into(),
            ));
        }

        let repository = format!("https://github.com/{github}.git");
        let tags = git::tags(&self.agent, &repository)
            .or_else(|_| git::tags(&self.agent, &repository))
            .map_err(error)?;
        let commit = tags
            .into_iter()
            .find(|tag| tag.name == rolling.tag)
            .map(|tag| tag.commit)
            .ok_or_else(|| error(format!("its rolling tag {} doesn't exist", rolling.tag)))?;

        let version = format!("{}+{}", rolling.version, &commit[..12]);
        let behind: Vec<Platform> = platforms
            .into_iter()
            .filter(|&platform| self.catalog.default_version(name, platform) != Some(&version))
            .collect();
        if behind.is_empty() {
            return Ok(Vec::new());
        }

        self.verify(Some(github), upstream.repository_id)
            .map_err(error)?;
        let committed = github::committed(github, &commit).map_err(error)?;
        let serial = serial(&committed).ok_or_else(|| {
            error(format!(
                "commit {commit} has the unexpected date {committed}"
            ))
        })?;

        let candidate = Candidate {
            numbers: Vec::new(),
            version,
            tag: rolling.tag.clone(),
            commit,
            serial: Some(serial),
            is_rolling: true,
        };

        let mut advanced = Vec::new();
        for platform in behind {
            if self.is_available(name, Some(github), &candidate, platform)? {
                advanced.push(Advance::to(platform, &candidate));
            }
        }

        Ok(advanced)
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
            if release.is_some_and(|release| !release.is_published(candidate.is_rolling)) {
                return Ok(false);
            }
        }

        let version = &candidate.version;
        let download = self
            .catalog
            .download(
                name,
                version,
                Some(&candidate.commit),
                candidate.serial,
                platform,
            )
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
        Ok(release.is_some_and(|release| {
            release.is_published(candidate.is_rolling) && release.assets.contains(&asset.asset)
        }))
    }

    fn release(&self, repository: &str, tag: &str) -> Result<Option<Release>, String> {
        let key = (repository.to_string(), tag.to_string());
        if let Some(release) = self.lock().get(&key) {
            return Ok(release.clone());
        }

        let release = github::release(repository, tag)?.map(|release| Release {
            is_draft: release.draft,
            is_prerelease: release.prerelease,
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
                serial: None,
                is_rolling: false,
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

/// A commit date like `2026-10-30T00:57:35Z` as the serial `20261030005735`.
fn serial(date: &str) -> Option<u64> {
    let digits: String = date.chars().filter(char::is_ascii_digit).collect();
    (digits.len() == 14).then(|| digits.parse().ok()).flatten()
}

/// One update line per version, naming every platform that advances to it.
/// Platforms that would build from different commits get their own lines.
fn group(
    name: &str,
    advanced: Vec<Advance>,
    has_commit: bool,
    should_automerge: bool,
) -> Vec<Line> {
    type Key = (String, Option<String>, Option<u64>);
    let mut versions: BTreeMap<Key, Vec<Platform>> = BTreeMap::new();
    for advance in advanced {
        let commit = has_commit.then_some(advance.commit);
        versions
            .entry((advance.version, commit, advance.serial))
            .or_default()
            .push(advance.platform);
    }

    versions
        .into_iter()
        .map(|((version, commit, serial), mut platforms)| {
            platforms.sort_by_key(|platform| platform.to_string());
            Line::Update {
                name: name.to_string(),
                version,
                platforms,
                commit,
                serial,
                should_automerge,
            }
        })
        .collect()
}

impl Advance {
    fn to(platform: Platform, candidate: &Candidate) -> Advance {
        Advance {
            platform,
            version: candidate.version.clone(),
            commit: candidate.commit.clone(),
            serial: candidate.serial,
        }
    }
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
            rolling: None,
            should_automerge: false,
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
        let advance = |platform, commit: &str| Advance {
            platform,
            version: "1.1".into(),
            commit: commit.into(),
            serial: None,
        };
        let advanced = vec![
            advance(Platform::X86_64Linux, "a"),
            advance(Platform::Aarch64Macos, "b"),
            advance(Platform::Aarch64Linux, "a"),
        ];

        let line = |platforms: Vec<Platform>, commit: Option<&str>| Line::Update {
            name: "tool".into(),
            version: "1.1".into(),
            platforms,
            commit: commit.map(String::from),
            serial: None,
            should_automerge: false,
        };

        assert_eq!(
            group("tool", advanced.clone(), false, false),
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
            group("tool", advanced, true, false),
            [
                line(
                    vec![Platform::Aarch64Linux, Platform::X86_64Linux],
                    Some("a")
                ),
                line(vec![Platform::Aarch64Macos], Some("b")),
            ]
        );
    }

    #[test]
    fn a_rolling_build_records_its_serial_and_may_merge_itself() {
        let advanced = vec![Advance {
            platform: Platform::Aarch64Macos,
            version: "0.1.0-main+2838e7cfa851".into(),
            commit: "2838e7cfa8513e37f603bad6f20b34c3e760f983".into(),
            serial: serial("2026-10-30T00:57:35Z"),
        }];

        let lines = group("tool", advanced, true, true);
        assert_eq!(
            serde_json::to_string(&lines[0]).unwrap(),
            r#"{"name":"tool","version":"0.1.0-main+2838e7cfa851","platforms":["aarch64-macos"],"commit":"2838e7cfa8513e37f603bad6f20b34c3e760f983","serial":20261030005735,"automerge":true}"#
        );

        assert_eq!(serial("2026-10-30"), None);
    }

    #[test]
    fn only_a_rolling_tag_takes_builds_from_a_prerelease() {
        let release = |is_draft, is_prerelease| Release {
            is_draft,
            is_prerelease,
            assets: BTreeSet::new(),
        };

        assert!(release(false, false).is_published(false));
        assert!(!release(false, true).is_published(false));
        assert!(release(false, true).is_published(true));
        assert!(!release(true, false).is_published(true));
    }
}
