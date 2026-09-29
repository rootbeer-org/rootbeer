use std::collections::BTreeMap;

use rootbeer_package::github::Release;
use rootbeer_package::{PackageDefinition, PackageUpstream};

fn version_key(version: &str) -> Result<Vec<u64>, String> {
    if version.is_empty()
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return Err(format!(
            "unsupported version `{version}`; expected a dotted numeric stable version"
        ));
    }
    let mut parts = version
        .split('.')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| {
            format!("unsupported version `{version}`; expected a dotted numeric stable version")
        })?;
    while parts.len() > 1 && parts.last() == Some(&0) {
        parts.pop();
    }
    Ok(parts)
}

/// Stable versions an upstream tags, ordered oldest to newest, with every tag that
/// normalizes to each. Old repositories often carry both `v3.4` and `v3.4.0`.
fn stable_versions<'a>(
    upstream: &PackageUpstream,
    tags: &'a BTreeMap<String, String>,
) -> BTreeMap<Vec<u64>, Vec<(String, &'a str)>> {
    let mut ordered: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for tag in tags.keys() {
        if upstream.exclude_tags.contains(tag) {
            continue;
        }
        let Some(version) = upstream.version_of(tag) else {
            continue;
        };
        let Ok(key) = version_key(&version) else {
            continue;
        };
        ordered
            .entry(key)
            .or_default()
            .push((version, tag.as_str()));
    }
    ordered
}

/// GitHub releases looked up by tag, each fetched at most once per upstream.
struct Releases<F> {
    fetch: F,
    known: BTreeMap<String, Option<Release>>,
}

impl<F: FnMut(&str) -> Result<Option<Release>, String>> Releases<F> {
    fn get(&mut self, tag: &str) -> Result<Option<&Release>, String> {
        if !self.known.contains_key(tag) {
            let release = (self.fetch)(tag)?;
            self.known.insert(tag.to_string(), release);
        }
        Ok(self.known[tag].as_ref())
    }
}

/// The digest of exactly what `system` downloads for `version`, or None when that release
/// does not publish it.
fn pin(
    definition: &PackageDefinition,
    upstream: &PackageUpstream,
    system: &str,
    version: &str,
    commit: Option<&str>,
    releases: &mut Releases<impl FnMut(&str) -> Result<Option<Release>, String>>,
    hash: &mut impl FnMut(&str) -> Result<String, String>,
) -> Result<Option<String>, String> {
    let candidate = definition.candidate(system, version, commit)?;
    if let Some(build) = &candidate.build {
        return hash(&build.url).map(Some);
    }
    let source = candidate
        .source
        .as_deref()
        .ok_or("a candidate downloads nothing")?;
    let Some(name) = &candidate.asset else {
        return hash(source).map(Some);
    };
    let (repository, tag) = source
        .strip_prefix("github:")
        .and_then(|reference| reference.rsplit_once('@'))
        .ok_or_else(|| format!("unsupported release source `{source}`"))?;
    if !upstream
        .github()
        .is_some_and(|expected| repository.eq_ignore_ascii_case(expected))
    {
        return Err(format!(
            "downloads from {repository}, but discovers from {}",
            upstream.label()
        ));
    }
    let Some(asset) = releases
        .get(tag)?
        .and_then(|release| release.assets.iter().find(|asset| &asset.name == name))
    else {
        return Ok(None);
    };
    // Pinning the digest upstream published closes the window where an asset is replaced
    // between discovery proposing a version and CI qualifying it.
    let digest = asset
        .sha256()
        .ok_or_else(|| format!("release asset `{name}` publishes no sha256 digest"))?;
    Ok(Some(digest.to_string()))
}

/// Advances each platform to the newest tag that publishes what it downloads.
///
/// A platform never moves below its current version, and one whose asset is missing from
/// newer releases stays where it is. A tag whose GitHub release is a draft or prerelease
/// is skipped. Errors are returned per platform rather than raised, so one platform's
/// failure cannot hold the others back.
pub(super) fn discover(
    upstream: &PackageUpstream,
    systems: &[String],
    tags: &BTreeMap<String, String>,
    definition: &mut PackageDefinition,
    mut hash: impl FnMut(&str) -> Result<String, String>,
    release_of: impl FnMut(&str) -> Result<Option<Release>, String>,
) -> Result<Vec<String>, String> {
    let versions = stable_versions(upstream, tags);
    if versions.is_empty() {
        return Err("no matching stable tags".into());
    }
    let mut releases = Releases {
        fetch: release_of,
        known: BTreeMap::new(),
    };

    let mut hashed: BTreeMap<String, String> = BTreeMap::new();
    let mut hash_once = |url: &str| {
        if let Some(digest) = hashed.get(url) {
            return Ok(digest.clone());
        }
        let digest = hash(url)?;
        hashed.insert(url.to_string(), digest.clone());
        Ok(digest)
    };
    let is_commit_needed = definition.uses_commit();
    let mut selected: BTreeMap<(&str, &str), BTreeMap<String, String>> = BTreeMap::new();
    let mut errors = Vec::new();
    for system in systems {
        let current = definition
            .package
            .default_version_for(system)
            .map(version_key)
            .transpose();
        let current = match current {
            Ok(current) => current,
            Err(error) => {
                errors.push(format!("{system}: {error}"));
                continue;
            }
        };
        for (key, candidates) in versions.iter().rev() {
            if current.as_ref().is_some_and(|current| key <= current) {
                break;
            }
            let [(version, tag)] = candidates.as_slice() else {
                let tags: Vec<&str> = candidates.iter().map(|(_, tag)| *tag).collect();
                errors.push(format!(
                    "{system}: tags {} publish one version; narrow the upstream tag",
                    tags.join(", ")
                ));
                break;
            };
            match releases.get(tag) {
                Ok(Some(release)) if release.draft || release.prerelease => continue,
                Ok(_) => {}
                Err(error) => {
                    errors.push(format!("{system}: {version}: {error}"));
                    break;
                }
            }
            let commit = is_commit_needed.then(|| tags[*tag].as_str());
            match pin(
                definition,
                upstream,
                system,
                version,
                commit,
                &mut releases,
                &mut hash_once,
            ) {
                Ok(Some(digest)) => {
                    selected
                        .entry((version.as_str(), *tag))
                        .or_default()
                        .insert(system.clone(), digest);
                    break;
                }
                Ok(None) => continue,
                Err(error) => {
                    errors.push(format!("{system}: {error}"));
                    break;
                }
            }
        }
    }

    for ((version, tag), digests) in selected {
        let systems: Vec<String> = digests.keys().cloned().collect();
        let commit = is_commit_needed.then(|| tags[tag].clone());
        definition.add_version(version, digests, None, commit)?;
        for system in &systems {
            definition.set_default_version(system, version)?;
        }
    }
    Ok(errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A distinct digest per asset name, so a test can tell which asset was pinned.
    fn digest_of(name: &str) -> String {
        let hex: String = name.bytes().map(|byte| format!("{byte:02x}")).collect();
        format!("{hex:0<64}")[..64].to_string()
    }

    fn release(tag: &str, assets: &[&str]) -> Release {
        serde_json::from_value(serde_json::json!({
            "id": 1, "tag_name": tag,
            "assets": assets.iter().map(|name| serde_json::json!({
                "name": name,
                "browser_download_url": "https://example.com/archive",
                "digest": format!("sha256:{}", digest_of(name)),
            })).collect::<Vec<_>>()
        }))
        .unwrap()
    }

    fn definition(upstream: &str, version: &str, systems: &[&str]) -> PackageDefinition {
        let platforms = systems
            .iter()
            .map(|system| {
                format!(
                    r#"["{system}"] = {{ target = "{system}", default_version = "{version}" }},"#
                )
            })
            .collect::<String>();
        let digests = systems
            .iter()
            .map(|system| format!(r#"["{system}"] = "{}","#, "a".repeat(64)))
            .collect::<String>();
        PackageDefinition::from_lua(&format!(
            r#"return {{
                name = "tool", description = "A tool", homepage = "https://example.com",
                default_license = "MIT",
                upstream = {upstream},
                prebuilt = {{ github = "owner/tool", asset = "tool-{{version}}-{{target}}.tar.gz" }},
                outputs = {{ bins = {{ "tool" }}, checks = {{ {{ "tool", "--version" }} }} }},
                platforms = {{ {platforms} }},
                versions = {{ ["{version}"] = {{ digests = {{ {digests} }} }} }},
            }}"#
        ))
        .unwrap()
    }

    const UPSTREAM: &str = r#"{ github = "owner/tool" }"#;

    /// Tags every release, as a repository with only released tags would.
    fn tags(releases: &[Release]) -> BTreeMap<String, String> {
        releases
            .iter()
            .map(|release| (release.tag_name.clone(), "e".repeat(40)))
            .collect()
    }

    fn lookup(releases: &[Release]) -> impl FnMut(&str) -> Result<Option<Release>, String> + '_ {
        |tag| {
            Ok(releases
                .iter()
                .find(|release| release.tag_name == tag)
                .cloned())
        }
    }

    fn run(recipe: &mut PackageDefinition, releases: &[Release]) -> Vec<String> {
        let upstream = recipe.upstreams().remove(0);
        discover(
            &upstream.0,
            &upstream.1,
            &tags(releases),
            recipe,
            |url| panic!("a prebuilt must not download {url}"),
            lookup(releases),
        )
        .unwrap()
    }

    fn pinned(recipe: &PackageDefinition, version: &str, system: &str) -> Option<String> {
        recipe.package.versions[version].platforms[system]
            .sha256
            .clone()
    }

    #[test]
    fn selects_the_highest_version_each_platform_can_install() {
        let mut recipe = definition(UPSTREAM, "1", &["aarch64-macos", "x86_64-linux"]);
        let releases = [
            release(
                "2",
                &["tool-2-aarch64-macos.tar.gz", "tool-2-x86_64-linux.tar.gz"],
            ),
            release(
                "1",
                &["tool-1-aarch64-macos.tar.gz", "tool-1-x86_64-linux.tar.gz"],
            ),
        ];
        assert!(run(&mut recipe, &releases).is_empty());

        for system in ["aarch64-macos", "x86_64-linux"] {
            assert_eq!(recipe.package.default_version_for(system), Some("2"));
            assert_eq!(
                pinned(&recipe, "2", system),
                Some(digest_of(&format!("tool-2-{system}.tar.gz")))
            );
        }
        assert!(
            recipe.package.versions.contains_key("1"),
            "retains the old version"
        );
    }

    #[test]
    fn pins_the_asset_its_template_names_rather_than_a_lookalike() {
        let mut recipe = definition(UPSTREAM, "1", &["aarch64-macos"]);
        let releases = [release(
            "2",
            &[
                "tool-2-aarch64-macos-debug.tar.gz",
                "tool-2-aarch64-macos.tar.gz",
                "tool-2-aarch64-macos.tar.gz.sha256",
            ],
        )];
        run(&mut recipe, &releases);
        assert_eq!(
            pinned(&recipe, "2", "aarch64-macos"),
            Some(digest_of("tool-2-aarch64-macos.tar.gz"))
        );
    }

    #[test]
    fn a_platform_whose_asset_disappeared_keeps_the_version_it_had() {
        let mut recipe = definition(UPSTREAM, "1", &["aarch64-macos", "x86_64-linux"]);
        let releases = [
            release("2", &["tool-2-aarch64-macos.tar.gz"]),
            release(
                "1",
                &["tool-1-aarch64-macos.tar.gz", "tool-1-x86_64-linux.tar.gz"],
            ),
        ];
        assert!(run(&mut recipe, &releases).is_empty());
        assert_eq!(
            recipe.package.default_version_for("aarch64-macos"),
            Some("2")
        );
        assert_eq!(
            recipe.package.default_version_for("x86_64-linux"),
            Some("1"),
            "a platform without an asset must not be dragged forward"
        );
        assert!(!recipe.package.versions["2"]
            .platforms
            .contains_key("x86_64-linux"));
    }

    #[test]
    fn a_failing_platform_does_not_hold_back_the_others() {
        let mut recipe = definition(UPSTREAM, "1", &["aarch64-macos", "x86_64-linux"]);
        let mut releases = [release(
            "2",
            &["tool-2-aarch64-macos.tar.gz", "tool-2-x86_64-linux.tar.gz"],
        )];
        releases[0].assets[1] = serde_json::from_value(serde_json::json!({
            "name": "tool-2-x86_64-linux.tar.gz",
            "browser_download_url": "https://example.com/archive"
        }))
        .unwrap();
        let errors = run(&mut recipe, &releases);

        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("x86_64-linux: "), "{errors:?}");
        assert!(errors[0].contains("no sha256 digest"), "{errors:?}");
        assert_eq!(
            recipe.package.default_version_for("aarch64-macos"),
            Some("2")
        );
        assert_eq!(
            recipe.package.default_version_for("x86_64-linux"),
            Some("1")
        );
    }

    #[test]
    fn a_tag_template_maps_versions_and_their_separators() {
        let mut recipe = definition(
            r#"{ github = "owner/tool", tag = "tool-{version}", separator = "_" }"#,
            "8.21.0",
            &["aarch64-macos"],
        );
        let releases = [
            release("tool-8_22_0", &["tool-8.22.0-aarch64-macos.tar.gz"]),
            release("other-9_0_0", &["tool-9.0.0-aarch64-macos.tar.gz"]),
            release("tool-8_21_0", &["tool-8.21.0-aarch64-macos.tar.gz"]),
        ];
        run(&mut recipe, &releases);
        assert_eq!(
            recipe.package.default_version_for("aarch64-macos"),
            Some("8.22.0")
        );
        assert_eq!(
            recipe.package.versions["8.22.0"].platforms["aarch64-macos"]
                .source
                .as_deref(),
            Some("github:owner/tool@tool-8_22_0")
        );
    }

    #[test]
    fn tags_that_normalize_to_one_version_are_rejected_rather_than_guessed() {
        let mut recipe = definition(UPSTREAM, "1", &["aarch64-macos"]);
        let releases = [
            release("2", &["tool-2-aarch64-macos.tar.gz"]),
            release("2.0", &["tool-2.0-aarch64-macos.tar.gz"]),
        ];
        let errors = run(&mut recipe, &releases);
        assert!(
            errors[0].contains("tags 2, 2.0 publish one version"),
            "{errors:?}"
        );
        assert_eq!(
            recipe.package.default_version_for("aarch64-macos"),
            Some("1")
        );

        let mut current = definition(UPSTREAM, "3", &["aarch64-macos"]);
        let releases = [
            release("2", &["tool-2-aarch64-macos.tar.gz"]),
            release("2.0", &["tool-2.0-aarch64-macos.tar.gz"]),
            release("4", &["tool-4-aarch64-macos.tar.gz"]),
        ];
        assert!(
            run(&mut current, &releases).is_empty(),
            "older ties are irrelevant"
        );
        assert_eq!(
            current.package.default_version_for("aarch64-macos"),
            Some("4")
        );
    }

    #[test]
    fn drafts_prereleases_and_foreign_tags_are_skipped() {
        let mut recipe = definition(
            r#"{ github = "owner/tool", tag = "v{version}" }"#,
            "1",
            &["aarch64-macos"],
        );
        let mut draft = release("v3", &["tool-3-aarch64-macos.tar.gz"]);
        draft.draft = true;
        let mut prerelease = release("v4", &["tool-4-aarch64-macos.tar.gz"]);
        prerelease.prerelease = true;
        let releases = [
            draft,
            prerelease,
            release("nightly-9", &["tool-9-aarch64-macos.tar.gz"]),
            release("v2", &["tool-2-aarch64-macos.tar.gz"]),
        ];
        run(&mut recipe, &releases);
        assert_eq!(
            recipe.package.default_version_for("aarch64-macos"),
            Some("2")
        );
    }

    #[test]
    fn a_version_records_the_commit_its_templates_embed() {
        let mut recipe = PackageDefinition::from_lua(&format!(
            r#"return {{
                name = "tool", description = "A tool", homepage = "https://example.com",
                default_license = "MIT",
                upstream = {{ github = "owner/tool", tag = "v{{version}}" }},
                source = {{ url = "https://example.com/tool-{{tag}}.tar.gz", archive = "tar.gz",
                            strip_prefix = "tool-{{version}}" }},
                build = {{ backend = "go", go = {{
                    binaries = {{ tool = "./cmd" }},
                    variables = {{ commit = "{{commit}}" }},
                }} }},
                outputs = {{ bins = {{ "tool" }}, checks = {{ {{ "tool", "--version" }} }} }},
                platforms = {{
                    ["aarch64-macos"] = {{ default_version = "98" }},
                    ["x86_64-linux"] = {{ default_version = "98" }},
                }},
                versions = {{ ["98"] = {{ commit = "{old}", digests = {{
                    ["aarch64-macos"] = "{digest}", ["x86_64-linux"] = "{digest}",
                }} }} }},
            }}"#,
            old = "a".repeat(40),
            digest = "b".repeat(64)
        ))
        .unwrap();
        let tags = BTreeMap::from([
            ("v99".to_string(), "d".repeat(40)),
            ("v98".to_string(), "a".repeat(40)),
        ]);
        let (upstream, systems) = recipe.upstreams().remove(0);

        discover(
            &upstream,
            &systems,
            &tags,
            &mut recipe,
            |_| Ok("c".repeat(64)),
            |_| Ok(None),
        )
        .unwrap();
        for system in &systems {
            let build = recipe.package.versions["99"].platforms[system]
                .build
                .as_ref()
                .unwrap();
            assert_eq!(
                build.go.as_ref().unwrap().variables["commit"],
                "d".repeat(40)
            );
        }
        let rendered = recipe.to_lua().unwrap();
        assert!(
            rendered.contains(&format!("commit = \"{}\"", "d".repeat(40))),
            "{rendered}"
        );
    }

    /// Tag-only upstreams such as krb5 publish no GitHub releases at all.
    #[test]
    fn source_discovery_hashes_one_archive_for_every_platform() {
        let mut recipe = PackageDefinition::from_lua(&format!(
            r#"return {{
                name = "tool", description = "A tool", homepage = "https://example.com",
                default_license = "MIT",
                upstream = {{ github = "owner/tool", tag = "v{{version}}" }},
                source = {{ url = "https://example.com/tool-{{tag}}.tar.gz", archive = "tar.gz",
                            strip_prefix = "tool-{{version}}" }},
                build = {{ backend = "autotools" }},
                outputs = {{ bins = {{ "tool" }}, checks = {{ {{ "tool", "--version" }} }} }},
                platforms = {{
                    ["aarch64-macos"] = {{ default_version = "98" }},
                    ["x86_64-linux"] = {{ default_version = "98" }},
                }},
                versions = {{ ["98"] = {{ digests = {{
                    ["aarch64-macos"] = "{digest}", ["x86_64-linux"] = "{digest}",
                }} }} }},
            }}"#,
            digest = "b".repeat(64)
        ))
        .unwrap();
        let tags = tags(&[release("v99", &[]), release("v98", &[])]);
        let (upstream, systems) = recipe.upstreams().remove(0);

        let mut fetched = Vec::new();
        discover(
            &upstream,
            &systems,
            &tags,
            &mut recipe,
            |url| {
                fetched.push(url.to_string());
                Ok("c".repeat(64))
            },
            |_| Ok(None),
        )
        .unwrap();
        assert_eq!(fetched, ["https://example.com/tool-v99.tar.gz"]);
        for system in &systems {
            assert_eq!(recipe.package.default_version_for(system), Some("99"));
            let build = recipe.package.versions["99"].platforms[system]
                .build
                .as_ref()
                .unwrap();
            assert_eq!(build.sha256, "c".repeat(64));
        }

        discover(
            &upstream,
            &systems,
            &tags,
            &mut recipe,
            |_| panic!("an unchanged source must not be downloaded again"),
            |_| Ok(None),
        )
        .unwrap();
    }

    #[test]
    fn a_release_asset_waits_for_its_release_to_be_published() {
        let mut recipe = definition(UPSTREAM, "1", &["aarch64-macos"]);
        let released = [release("1", &["tool-1-aarch64-macos.tar.gz"])];
        let mut tags = tags(&released);
        tags.insert("2".into(), "f".repeat(40));
        let (upstream, systems) = recipe.upstreams().remove(0);
        let errors = discover(
            &upstream,
            &systems,
            &tags,
            &mut recipe,
            |url| panic!("a prebuilt must not download {url}"),
            lookup(&released),
        )
        .unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            recipe.package.default_version_for("aarch64-macos"),
            Some("1")
        );
    }

    #[test]
    fn a_git_upstream_cannot_pin_github_release_assets() {
        let mut recipe = definition(
            r#"{ git = "https://codeberg.org/owner/tool.git" }"#,
            "1",
            &["aarch64-macos"],
        );
        let (upstream, systems) = recipe.upstreams().remove(0);
        let errors = discover(
            &upstream,
            &systems,
            &tags(&[release("2", &[])]),
            &mut recipe,
            |url| panic!("a prebuilt must not download {url}"),
            |_| Ok(None),
        )
        .unwrap();
        assert!(
            errors[0].contains("discovers from https://codeberg.org"),
            "{errors:?}"
        );
    }
}
