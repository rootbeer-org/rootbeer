use rootbeer_package::github::Release;
use rootbeer_package::{PackageDefinition, PackageUpstream};
use serde::Deserialize;
use serde_json::Value;

mod generate;
mod git;
mod metadata;
mod updates;
pub use updates::{discover_updates, UpdateReport};

#[derive(Debug, Deserialize)]
struct Repository {
    id: u64,
    full_name: String,
}

/// What one upstream contributed to a package, beyond the versions it recorded.
struct Discovery {
    /// Failures confined to single platforms; the package's other platforms still advance.
    errors: Vec<String>,
    /// The GitHub repository's ID, for a github upstream.
    repository_id: Option<u64>,
    is_newly_pinned: bool,
}

fn discover_upstream(
    upstream: &PackageUpstream,
    systems: &[String],
    recipe: &mut PackageDefinition,
    fetch: &mut impl FnMut(&str) -> Result<Option<Value>, String>,
    ls_refs: &mut impl FnMut(&str) -> Result<Vec<u8>, String>,
    hash: &mut impl FnMut(&str) -> Result<String, String>,
) -> Result<Discovery, String> {
    let repository_id = match upstream.github() {
        Some(repository) => Some(check_repository(upstream, repository, fetch)?),
        None => None,
    };
    let tags = git::parse_tags(&ls_refs(&upstream.git_url())?)?;

    let release_of = |tag: &str| -> Result<Option<Release>, String> {
        let Some(repository) = upstream.github() else {
            return Ok(None);
        };
        let url = format!(
            "https://api.github.com/repos/{repository}/releases/tags/{}",
            path_segment(tag)
        );
        fetch(&url)?
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| e.to_string())
    };
    let errors = generate::discover(upstream, systems, &tags, recipe, hash, release_of)?;

    let is_newly_pinned = repository_id.is_some() && upstream.repository_id.is_none();
    if let (true, Some(repository), Some(id)) = (is_newly_pinned, upstream.github(), repository_id)
    {
        recipe.pin_repository_id(repository, id)?;
    }
    Ok(Discovery {
        errors,
        repository_id,
        is_newly_pinned,
    })
}

/// The repository's ID, once it matches the one pinned and has not moved.
fn check_repository(
    upstream: &PackageUpstream,
    repository: &str,
    fetch: &mut impl FnMut(&str) -> Result<Option<Value>, String>,
) -> Result<u64, String> {
    let found: Repository = fetch(&format!("https://api.github.com/repos/{repository}"))?
        .ok_or("GitHub repository not found; review upstream ownership")
        .and_then(|value| serde_json::from_value(value).map_err(|_| "invalid GitHub repository"))?;
    if found.id == 0 || upstream.repository_id.is_some_and(|id| id != found.id) {
        return Err("GitHub repository ID changed; review upstream ownership".into());
    }
    if !repository.eq_ignore_ascii_case(&found.full_name) {
        return Err(format!(
            "repository moved to {}; review the identity mapping",
            found.full_name
        ));
    }
    Ok(found.id)
}

/// Percent-encodes a tag for one URL path segment, since tags may contain `/` or `+`.
fn path_segment(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rootbeer_package::upstream::{check_identity, validate_upstreams};
    use rootbeer_package::PackageDefinition;

    fn tracked(name: &str, repositories: [&str; 2]) -> (String, PackageDefinition) {
        let definition = PackageDefinition::from_lua(&format!(
            r#"return {{
                name = "{name}", description = "Tool", homepage = "https://example.com",
                default_license = "MIT",
                prebuilt = {{ github = "owner/{name}", asset = "{name}-{{version}}.tar.gz" }},
                outputs = {{ bins = {{ "{name}" }}, checks = {{ {{ "{name}", "--version" }} }} }},
                platforms = {{
                    ["aarch64-macos"] = {{ default_version = "1", upstream = {{ github = "{}" }} }},
                    ["x86_64-linux"] = {{ default_version = "1", upstream = {{ github = "{}" }} }},
                }},
                versions = {{ ["1"] = {{ digests = {{ ["aarch64-macos"] = "{digest}", ["x86_64-linux"] = "{digest}" }} }} }},
            }}"#,
            repositories[0],
            repositories[1],
            digest = "a".repeat(64)
        ))
        .unwrap();
        (name.to_string(), definition)
    }

    #[test]
    fn one_package_may_span_repositories_but_two_may_not_share_one() {
        let split = BTreeMap::from([tracked("helium", ["owner/mac", "owner/linux"])]);
        assert!(validate_upstreams(&split).is_ok());

        let shared = BTreeMap::from([
            tracked("one", ["owner/tool", "owner/tool"]),
            tracked("two", ["owner/other", "OWNER/Tool"]),
        ]);
        let error = validate_upstreams(&shared).unwrap_err();
        assert!(error.contains("another package"), "{error}");
    }

    #[test]
    fn recognizes_existing_upstreams_and_prevents_alias_takeover() {
        let catalog = crate::test_catalog::catalog();
        let identity =
            |name: &str, repository: &str| check_identity(catalog, name, repository, false);
        assert!(identity("encryption", "filosottile/AGE")
            .unwrap_err()
            .contains("canonicalized as `age`"));
        assert!(identity("age", "other/tool")
            .unwrap_err()
            .contains("different upstream"));
        assert!(identity("rg", "other/tool")
            .unwrap_err()
            .contains("belongs to"));
    }
}
