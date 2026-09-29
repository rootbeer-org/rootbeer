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
        let is_same_repository = package
            .versions
            .values()
            .flat_map(|entry| entry.platforms.values())
            .filter_map(|recipe| recipe.source.as_deref())
            .map(PackageRequest::parse)
            .any(|request| {
                request.resolver.as_deref() == Some("github")
                    && request.name.eq_ignore_ascii_case(repository)
            });
        if is_same_repository && package.name != name {
            return Err(format!(
                "{repository} is already canonicalized as `{}`",
                package.name
            ));
        }
        if package.name == name
            && !is_same_repository
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
