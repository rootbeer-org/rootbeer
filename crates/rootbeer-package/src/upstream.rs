use std::collections::BTreeMap;

use super::{PackageCatalog, PackageDefinition, PackageRequest};

/// Rejects two packages discovering from one repository, which would race each other's
/// versions. One package may use several repositories, one per platform group.
pub fn validate_upstreams(definitions: &BTreeMap<String, PackageDefinition>) -> Result<(), String> {
    let mut repositories = BTreeMap::new();
    let mut ids = BTreeMap::new();
    for (name, definition) in definitions {
        for (upstream, _) in definition.upstreams() {
            let owners = [
                repositories.insert(upstream.identity(), name),
                upstream.repository_id.and_then(|id| ids.insert(id, name)),
            ];
            if owners.into_iter().flatten().any(|owner| owner != name) {
                return Err(format!(
                    "{}: upstream already belongs to another package",
                    upstream.label()
                ));
            }
        }
    }
    Ok(())
}

pub fn check_identity(
    catalog: &PackageCatalog,
    name: &str,
    repository: &str,
    is_source: bool,
) -> Result<(), String> {
    for package in catalog.packages.values() {
        let repositories: Vec<String> = package
            .versions
            .values()
            .flat_map(|entry| entry.platforms.values())
            .filter_map(|recipe| recipe.source.as_deref())
            .map(PackageRequest::parse)
            .filter(|request| request.resolver.as_deref() == Some("github"))
            .map(|request| request.name)
            .collect();
        let is_same_repository = repositories
            .iter()
            .any(|known| known.eq_ignore_ascii_case(repository));
        if is_same_repository && package.name != name {
            return Err(format!(
                "{repository} is already canonicalized as `{}`",
                package.name
            ));
        }
        // A package downloading from no GitHub repository takes only versions from it.
        if package.name == name
            && !is_same_repository
            && !repositories.is_empty()
            && !(is_source
                && package
                    .versions
                    .values()
                    .flat_map(|entry| entry.platforms.values())
                    .all(|recipe| recipe.build.is_some()))
        {
            return Err(format!(
                "{name}: existing package has a different upstream; resolve identity manually",
            ));
        }
    }
    if let Some(existing) = catalog.find(name) {
        if existing.name != name {
            return Err(format!("name `{name}` belongs to `{}`", existing.name));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(prebuilt: &str) -> PackageCatalog {
        let definition = PackageDefinition::from_lua(&format!(
            r#"return {{
                name = "tool", description = "Tool", homepage = "https://example.com",
                default_license = "MIT",
                prebuilt = {prebuilt},
                outputs = {{ bins = {{ "tool" }}, checks = {{ {{ "tool", "--version" }} }} }},
                platforms = {{ ["aarch64-macos"] = {{ default_version = "1" }} }},
                versions = {{ ["1"] = {{ digests = {{ ["aarch64-macos"] = "{}" }} }} }},
            }}"#,
            "a".repeat(64)
        ))
        .unwrap();
        PackageCatalog::from_definitions(&BTreeMap::from([("tool".to_string(), definition)]))
            .unwrap()
    }

    #[test]
    fn a_vendor_download_may_discover_versions_from_any_repository() {
        let vendor = catalog(
            r#"{ url = "https://example.com/tool-{version}.tar.xz",
                 install = { Archive = { format = "TarXz" } } }"#,
        );
        assert!(check_identity(&vendor, "tool", "owner/tool", false).is_ok());

        let released = catalog(r#"{ github = "owner/tool", asset = "tool-{version}.tar.gz" }"#);
        assert!(check_identity(&released, "tool", "owner/tool", false).is_ok());
        assert!(check_identity(&released, "tool", "other/tool", false)
            .unwrap_err()
            .contains("different upstream"));
    }
}
