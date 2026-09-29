use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use super::discover_upstream;
use super::metadata::{MetadataCache, Statistics};
use crate::{CatalogPackage, PackageCatalog, PackageDefinition};
use rootbeer_package::upstream::validate_upstreams;

/// A discovery report; candidate recipes are unqualified until package export succeeds.
#[derive(Serialize)]
pub struct UpdateReport {
    pub schema: u32,
    pub catalog_sha256: String,
    pub updated: Vec<String>,
    pub unchanged: Vec<String>,
    pub rules_changed: Vec<String>,
    pub errors: BTreeMap<String, String>,
    pub untracked: Vec<String>,
    pub defaults: BTreeMap<String, BTreeMap<String, String>>,
    metadata: Statistics,
}

/// Discovers new versions for every package with an upstream, writing candidate recipes.
pub fn discover_updates(
    definitions: &BTreeMap<String, PackageDefinition>,
    cache: &Path,
    output: &Path,
) -> Result<UpdateReport, String> {
    let cache = std::cell::RefCell::new(MetadataCache::new(cache)?);
    let downloads = rootbeer_package::download::DownloadCache::default();
    let mut report = discover_with_fetch(
        definitions,
        output,
        |url| cache.borrow_mut().fetch(url),
        |url| cache.borrow_mut().ls_refs(url),
        |url| {
            downloads
                .materialize(url, None)
                .map(|file| file.sha256)
                .map_err(|error| error.to_string())
        },
    )?;
    report.metadata = cache.into_inner().statistics;
    write_report(output, &report)?;
    Ok(report)
}

fn discover_with_fetch(
    definitions: &BTreeMap<String, PackageDefinition>,
    output: &Path,
    mut fetch: impl FnMut(&str) -> Result<Option<Value>, String>,
    mut ls_refs: impl FnMut(&str) -> Result<Vec<u8>, String>,
    mut hash: impl FnMut(&str) -> Result<String, String>,
) -> Result<UpdateReport, String> {
    let catalog = PackageCatalog::from_definitions(definitions)?;
    validate_upstreams(definitions)?;
    let staging = crate::staging::staging(output)?;
    let destination = staging.path().join("updates");
    fs::create_dir(&destination).map_err(|e| e.to_string())?;
    fs::create_dir(destination.join("packages")).map_err(|e| e.to_string())?;
    let mut report = UpdateReport {
        schema: 1,
        catalog_sha256: catalog.sha256(),
        updated: Vec::new(),
        unchanged: Vec::new(),
        rules_changed: Vec::new(),
        errors: BTreeMap::new(),
        defaults: BTreeMap::new(),
        metadata: Statistics::default(),
        untracked: definitions
            .iter()
            .filter(|(_, definition)| definition.upstream.is_empty())
            .map(|(name, _)| name.clone())
            .collect(),
    };
    let mut combined = catalog.clone();
    let mut identities: BTreeMap<u64, &str> = BTreeMap::new();
    for (name, definition) in definitions {
        let mut recipe = definition.clone();
        let mut errors = Vec::new();
        let mut has_rule_changes = false;
        for (upstream, systems) in definition.upstreams() {
            eprintln!("Discover {name} from {}", upstream.label());
            let discovery = discover_upstream(
                &upstream,
                &systems,
                &mut recipe,
                &mut fetch,
                &mut ls_refs,
                &mut hash,
            );
            match discovery {
                Ok(discovery) => {
                    let owner = discovery
                        .repository_id
                        .and_then(|id| identities.insert(id, name));
                    if let Some(owner) = owner {
                        if owner != name {
                            errors.push(format!("repository is already tracked as `{owner}`"));
                            recipe = definition.clone();
                            break;
                        }
                    }
                    has_rule_changes |= discovery.is_newly_pinned;
                    errors.extend(
                        discovery
                            .errors
                            .into_iter()
                            .map(|error| format!("{}: {error}", upstream.label())),
                    );
                }
                Err(error) => errors.push(format!("{}: {error}", upstream.label())),
            }
        }

        let mut candidate = combined.clone();
        candidate
            .packages
            .insert(name.clone(), recipe.package.clone());
        if let Err(error) = candidate.validate() {
            errors.push(error);
            report.errors.insert(name.clone(), errors.join("; "));
            continue;
        }
        combined = candidate;
        if !errors.is_empty() {
            report.errors.insert(name.clone(), errors.join("; "));
        }

        let is_changed = serde_json::to_value(&catalog.packages[name])
            .map_err(|e| e.to_string())?
            != serde_json::to_value(&recipe.package).map_err(|e| e.to_string())?;
        if is_changed {
            report
                .defaults
                .insert(name.clone(), platform_defaults(&recipe.package));
            report.updated.push(name.clone());
        } else if errors.is_empty() && !definition.upstream.is_empty() {
            report.unchanged.push(name.clone());
        }
        if has_rule_changes {
            report.rules_changed.push(name.clone());
        }
        if is_changed || has_rule_changes {
            fs::write(
                destination.join("packages").join(format!("{name}.lua")),
                recipe.to_lua()?,
            )
            .map_err(|e| e.to_string())?;
        }
    }
    if !report.updated.is_empty() || !report.rules_changed.is_empty() {
        for (name, definition) in definitions {
            let path = destination.join("packages").join(format!("{name}.lua"));
            if path.exists() {
                continue;
            }
            fs::write(path, definition.to_lua()?).map_err(|error| error.to_string())?;
        }
        let candidates = PackageCatalog::from_directory(&destination.join("packages"))?;
        if candidates.sha256() != combined.sha256() {
            return Err("candidate catalog differs from resolved updates".into());
        }
    }
    write_report(&destination, &report)?;
    fs::rename(destination, output).map_err(|e| e.to_string())?;
    Ok(report)
}

fn platform_defaults(package: &CatalogPackage) -> BTreeMap<String, String> {
    package
        .default_versions
        .iter()
        .map(|(system, version)| (system.clone(), version.clone()))
        .collect()
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('`', "&#96;")
        .replace(['\n', '\r'], " ")
}

fn write_report(output: &Path, report: &UpdateReport) -> Result<(), String> {
    fs::write(
        output.join("report.json"),
        serde_json::to_vec_pretty(report).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let mut summary = format!("# Upstream discovery\n\n{} changed; {} unchanged; {} errors; {} untracked.\n\nMetadata: {} fetched, {} not modified.\n", report.updated.len(), report.unchanged.len(), report.errors.len(), report.untracked.len(), report.metadata.fetched, report.metadata.not_modified);
    for (name, defaults) in &report.defaults {
        summary.push_str(&format!(
            "\n- **{}**: {}\n",
            escape(name),
            defaults
                .iter()
                .map(|(system, version)| format!("{} {}", escape(system), escape(version)))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for (name, error) in &report.errors {
        summary.push_str(&format!(
            "\n- **{} failed**: {}\n",
            escape(name),
            escape(error)
        ));
    }
    if !report.untracked.is_empty() {
        summary.push_str(&format!(
            "\nNot tracked by discovery: {}.\n",
            report
                .untracked
                .iter()
                .map(|name| escape(name))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    summary.push_str("\nCandidates require platform qualification and review before promotion.\n");
    fs::write(output.join("summary.md"), summary).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(seed: &str) -> String {
        seed.repeat(64)[..64].to_string()
    }

    fn authored(name: &str, versions: &str) -> String {
        format!(
            r#"return {{
                name = "{name}",
                description = "Tool",
                homepage = "https://example.com",
                default_license = "MIT",
                platforms = {{
                    ["aarch64-macos"] = {{
                        default_version = "1",
                        upstream = {{ github = "owner/{name}", tag = "v{{version}}" }},
                        prebuilt = {{ github = "owner/{name}", asset = "{name}-{{tag}}-darwin-arm64.tar.gz" }},
                        outputs = {{ bins = {{ "{name}" }}, checks = {{ {{ "{name}", "--version" }} }} }},
                    }},
                }},
                versions = {versions},
            }}"#
        )
    }

    fn definitions(sources: &[String]) -> BTreeMap<String, PackageDefinition> {
        sources
            .iter()
            .map(|source| {
                let definition = PackageDefinition::from_lua(source).unwrap();
                (definition.package.name.clone(), definition)
            })
            .collect()
    }

    fn no_downloads(url: &str) -> Result<String, String> {
        panic!("a prebuilt must not download {url}")
    }

    fn releases(name: &str, tags: &[&str]) -> Value {
        Value::Array(
            tags.iter()
                .enumerate()
                .map(|(index, tag)| {
                    serde_json::json!({
                        "id": index + 1,
                        "tag_name": tag,
                        "assets": [{
                            "name": format!("{name}-{tag}-darwin-arm64.tar.gz"),
                            "browser_download_url": format!("https://example.com/{name}"),
                            "digest": format!("sha256:{}", digest(tag.trim_start_matches('v'))),
                        }],
                    })
                })
                .collect(),
        )
    }

    /// An `ls-refs` response tagging every release.
    fn refs(releases: &Value) -> Vec<u8> {
        let mut body = String::new();
        for release in releases.as_array().unwrap() {
            let line = format!(
                "{} refs/tags/{}\n",
                "e".repeat(40),
                release["tag_name"].as_str().unwrap()
            );
            body.push_str(&format!("{:04x}{line}", line.len() + 4));
        }
        body.push_str("0000");
        body.into_bytes()
    }

    /// Answers a `releases/tags/{tag}` request from a release list.
    fn release_by_tag(releases: &Value, url: &str) -> Option<Option<Value>> {
        let (_, tag) = url.split_once("/releases/tags/")?;
        let found = releases
            .as_array()
            .unwrap()
            .iter()
            .find(|release| release["tag_name"] == tag);
        Some(found.cloned())
    }

    #[test]
    fn a_failing_upstream_does_not_stop_the_other_packages() {
        let root = tempfile::tempdir().unwrap();
        let authored = [
            authored(
                "tool",
                r#"{ ["1"] = { digests = { ["aarch64-macos"] = "aa" } } }"#,
            )
            .replace("\"aa\"", &format!("\"{}\"", digest("1"))),
            authored(
                "broken",
                r#"{ ["1"] = { digests = { ["aarch64-macos"] = "aa" } } }"#,
            )
            .replace("\"aa\"", &format!("\"{}\"", digest("1"))),
        ];
        let definitions = definitions(&authored);
        let fetch = |url: &str| {
            if url.contains("/broken") {
                return Err("rate limited".into());
            }
            if let Some(release) = release_by_tag(&releases("tool", &["v1", "v2"]), url) {
                return Ok(release);
            }
            Ok(Some(
                serde_json::json!({"id": 42, "full_name": "owner/tool", "description": "Tool"}),
            ))
        };
        let ls_refs = |_: &str| Ok(refs(&releases("tool", &["v1", "v2"])));
        let output = root.path().join("first");
        let report =
            discover_with_fetch(&definitions, &output, fetch, ls_refs, no_downloads).unwrap();
        assert_eq!(report.updated, ["tool"]);
        assert_eq!(report.errors["broken"], "owner/broken: rate limited");
        assert_eq!(report.defaults["tool"]["aarch64-macos"], "2");

        let candidates = PackageDefinition::from_directory(&output.join("packages")).unwrap();
        let discovered = PackageCatalog::from_definitions(&candidates).unwrap();
        let tool = &discovered.packages["tool"];
        assert_eq!(tool.default_version_for("aarch64-macos"), Some("2"));
        assert_eq!(
            tool.versions["2"].platforms["aarch64-macos"].sha256,
            Some(digest("2"))
        );
        assert_eq!(
            discovered.packages["broken"].default_version_for("aarch64-macos"),
            Some("1")
        );

        let repeat = root.path().join("repeat");
        let report =
            discover_with_fetch(&candidates, &repeat, fetch, ls_refs, no_downloads).unwrap();
        assert!(report.updated.is_empty());
        assert_eq!(report.unchanged, ["tool"]);
    }

    #[test]
    fn discovery_preserves_authored_templates_and_version_overrides() {
        let root = tempfile::tempdir().unwrap();
        let source = authored(
            "tool",
            r#"{ ["1"] = {
                digests = { ["aarch64-macos"] = "DIGEST" },
                revision = 3,
                outputs = { checks = { { "tool", "--help" } } },
            } }"#,
        )
        .replace("DIGEST", &digest("1"));
        let definitions = definitions(std::slice::from_ref(&source));
        let catalog = PackageCatalog::from_definitions(&definitions).unwrap();
        let output = root.path().join("output");
        let report = discover_with_fetch(
            &definitions,
            &output,
            |url| {
                if let Some(release) = release_by_tag(&releases("tool", &["v1", "v2"]), url) {
                    return Ok(release);
                }
                Ok(Some(
                    serde_json::json!({"id": 42, "full_name": "owner/tool", "description": "Tool"}),
                ))
            },
            |_| Ok(refs(&releases("tool", &["v1", "v2"]))),
            no_downloads,
        )
        .unwrap();
        assert_eq!(report.updated, ["tool"]);

        let original: Value = rootbeer_package::definition::lua::read(&source).unwrap();
        let saved_source = fs::read_to_string(output.join("packages/tool.lua")).unwrap();
        let saved: Value = rootbeer_package::definition::lua::read(&saved_source).unwrap();
        assert_eq!(saved["versions"]["1"], original["versions"]["1"]);
        assert_eq!(
            saved["platforms"]["aarch64-macos"]["prebuilt"],
            original["platforms"]["aarch64-macos"]["prebuilt"]
        );
        assert_eq!(
            saved["platforms"]["aarch64-macos"]["outputs"],
            original["platforms"]["aarch64-macos"]["outputs"]
        );
        assert_eq!(saved["platforms"]["aarch64-macos"]["default_version"], "2");

        let expanded = PackageDefinition::from_lua(&saved_source).unwrap();
        let retained = &expanded.package.versions["1"];
        assert_eq!(retained.revision, 3);
        assert_eq!(
            retained.platforms["aarch64-macos"].checks,
            vec![vec!["tool".to_string(), "--help".into()]]
        );
        assert_eq!(
            serde_json::to_value(retained).unwrap(),
            serde_json::to_value(&catalog.packages["tool"].versions["1"]).unwrap()
        );
    }

    /// The upstream collapse: every platform used to be discovered from the first platform's
    /// repository, so helium's macOS build searched helium-linux for a DMG and never moved.
    #[test]
    fn each_platform_group_discovers_from_its_own_repository() {
        let root = tempfile::tempdir().unwrap();
        let source = format!(
            r#"return {{
                name = "helium", description = "Browse", homepage = "https://helium.computer",
                default_license = "GPL-3.0-only",
                platforms = {{
                    ["aarch64-macos"] = {{
                        default_version = "1",
                        upstream = {{ github = "imputnet/helium-macos", repository_id = 1 }},
                        prebuilt = {{ github = "imputnet/helium-macos", asset = "helium_{{version}}_arm64-macos.dmg",
                                      mirror = true }},
                        outputs = {{ apps = {{ ["Helium.app"] = "Helium.app" }} }},
                    }},
                    ["x86_64-linux"] = {{
                        default_version = "1",
                        upstream = {{ github = "imputnet/helium-linux", repository_id = 2 }},
                        prebuilt = {{ github = "imputnet/helium-linux", asset = "helium-{{version}}-x86_64.AppImage" }},
                        outputs = {{ bins = {{ "helium" }}, checks = {{ {{ "helium", "--version" }} }} }},
                    }},
                }},
                versions = {{ ["1"] = {{ digests = {{ ["aarch64-macos"] = "{a}", ["x86_64-linux"] = "{a}" }} }} }},
            }}"#,
            a = digest("a")
        );
        let definitions = definitions(&[source]);
        let release = |tag: &str, asset: String, seed: &str| {
            serde_json::json!({
                "id": 1, "tag_name": tag,
                "assets": [{ "name": asset, "browser_download_url": "https://example.com",
                             "digest": format!("sha256:{}", digest(seed)) }],
            })
        };
        let macos = Value::Array(vec![release("3", "helium_3_arm64-macos.dmg".into(), "3")]);
        let linux = Value::Array(vec![release("2", "helium-2-x86_64.AppImage".into(), "2")]);
        let fetch = |url: &str| match url {
            "https://api.github.com/repos/imputnet/helium-macos" => Ok(Some(
                serde_json::json!({"id": 1, "full_name": "imputnet/helium-macos"}),
            )),
            "https://api.github.com/repos/imputnet/helium-linux" => Ok(Some(
                serde_json::json!({"id": 2, "full_name": "imputnet/helium-linux"}),
            )),
            url if url.contains("helium-macos/") => Ok(release_by_tag(&macos, url).unwrap()),
            url if url.contains("helium-linux/") => Ok(release_by_tag(&linux, url).unwrap()),
            url => Err(format!("unexpected {url}")),
        };
        let ls_refs = |url: &str| match url {
            "https://github.com/imputnet/helium-macos.git" => Ok(refs(&macos)),
            "https://github.com/imputnet/helium-linux.git" => Ok(refs(&linux)),
            url => Err(format!("unexpected {url}")),
        };
        let report = discover_with_fetch(
            &definitions,
            &root.path().join("output"),
            fetch,
            ls_refs,
            no_downloads,
        )
        .unwrap();

        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let defaults = &report.defaults["helium"];
        assert_eq!(defaults["aarch64-macos"], "3");
        assert_eq!(defaults["x86_64-linux"], "2");
    }
}
