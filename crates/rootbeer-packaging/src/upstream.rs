use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;

use base64::Engine;
use rootbeer_package::download::DownloadedFile;
use rootbeer_package::github::Release;
use rootbeer_package::{PackageDefinition, PackageUpstream};
use serde::Deserialize;
use serde_json::Value;

mod generate;
mod git;
mod metadata;
mod sparkle;
mod updates;
use generate::Published;
pub use updates::{discover_updates, UpdateReport};

/// Everything discovery reads from the network, so tests can serve it from memory.
trait Remote {
    /// GitHub API JSON, or None when the resource does not exist.
    fn json(&mut self, url: &str) -> Result<Option<Value>, String>;
    /// A document such as an appcast, or None when it does not exist.
    fn text(&mut self, url: &str) -> Result<Option<String>, String>;
    /// A repository's tags, as an `ls-refs` response.
    fn ls_refs(&mut self, url: &str) -> Result<Vec<u8>, String>;
    /// Downloads a file, returning where it is cached and its SHA-256.
    fn download(&mut self, url: &str) -> Result<DownloadedFile, String>;
}

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
    remote: &mut impl Remote,
) -> Result<Discovery, String> {
    let repository_id = match upstream.github() {
        Some(repository) => Some(check_repository(upstream, repository, remote)?),
        None => None,
    };
    let (published, signatures) = match upstream.git_url() {
        Some(url) => (tags(&url, remote)?, BTreeMap::new()),
        None => appcast(upstream, remote)?,
    };
    let key = upstream.public_key.as_deref().map(decode_key).transpose()?;

    let remote = RefCell::new(remote);
    let hash = |url: &str| -> Result<String, String> {
        let file = remote.borrow_mut().download(url)?;
        if let Some(key) = &key {
            let signature = signatures
                .get(url)
                .and_then(Option::as_deref)
                .ok_or_else(|| format!("{url}: the appcast publishes no edSignature"))?;
            verify(key, signature, &file)?;
        }
        Ok(file.sha256)
    };
    let release_of = |tag: &str| -> Result<Option<Release>, String> {
        let Some(repository) = upstream.github() else {
            return Ok(None);
        };
        let url = format!(
            "https://api.github.com/repos/{repository}/releases/tags/{}",
            path_segment(tag)
        );
        remote
            .borrow_mut()
            .json(&url)?
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| e.to_string())
    };
    let errors = generate::discover(upstream, systems, &published, recipe, hash, release_of)?;

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

fn tags(url: &str, remote: &mut impl Remote) -> Result<BTreeMap<String, Published>, String> {
    let tags = git::parse_tags(&remote.ls_refs(url)?)?;
    Ok(tags
        .into_iter()
        .map(|(tag, commit)| {
            let published = Published {
                commit: Some(commit),
                url: None,
            };
            (tag, published)
        })
        .collect())
}

/// An appcast's versions, and the signature it publishes for each download.
type Appcast = (
    BTreeMap<String, Published>,
    BTreeMap<String, Option<String>>,
);

fn appcast(upstream: &PackageUpstream, remote: &mut impl Remote) -> Result<Appcast, String> {
    let feed = remote.text(upstream.label())?.ok_or("appcast not found")?;
    let mut signatures = BTreeMap::new();
    let published = sparkle::parse(&feed, upstream.channel.as_deref())?
        .into_iter()
        .map(|(version, item)| {
            signatures.insert(item.url.clone(), item.signature);
            let published = Published {
                commit: None,
                url: Some(item.url),
            };
            (version, published)
        })
        .collect();
    Ok((published, signatures))
}

fn decode_key(key: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(key)
        .ok()
        .filter(|key| key.len() == 32)
        .ok_or_else(|| "public_key is not a base64 Ed25519 key".into())
}

/// Checks a download against the app's EdDSA key, as Sparkle does before installing it.
fn verify(key: &[u8], signature: &str, file: &DownloadedFile) -> Result<(), String> {
    let signature = base64::engine::general_purpose::STANDARD
        .decode(signature)
        .map_err(|_| "invalid edSignature")?;
    let bytes = fs::read(&file.path).map_err(|e| e.to_string())?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(&bytes, &signature)
        .map_err(|_| "download is not signed by the app's public_key".to_string())
}

/// The repository's ID, once it matches the one pinned and has not moved.
fn check_repository(
    upstream: &PackageUpstream,
    repository: &str,
    remote: &mut impl Remote,
) -> Result<u64, String> {
    let found: Repository = remote
        .json(&format!("https://api.github.com/repos/{repository}"))?
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
