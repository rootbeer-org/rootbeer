use std::collections::BTreeSet;

use rootbeer_package::{CatalogVersion, PackageCatalog};

/// The exact package versions whose build inputs differ from `before` on `system`, or on any
/// system when `None`, plus every version that builds with one of them, transitively.
///
/// A license is published metadata, not a build input, so changing only a license selects nothing.
pub fn changed_requests(
    before: &PackageCatalog,
    after: &PackageCatalog,
    system: Option<&str>,
) -> Vec<String> {
    let mut changed = BTreeSet::new();
    for (name, package) in &after.packages {
        for (version, recipe) in &package.versions {
            let previous = before
                .packages
                .get(name)
                .and_then(|package| package.versions.get(version));
            if build_inputs(previous, system) != build_inputs(Some(recipe), system) {
                changed.insert(format!("{name}@{version}"));
            }
        }
    }
    loop {
        let mut affected = changed.clone();
        for (name, package) in &after.packages {
            for (version, recipe) in &package.versions {
                let is_built = system.is_none_or(|system| recipe.platforms.contains_key(system));
                if is_built
                    && dependencies(recipe, system).any(|dependency| changed.contains(dependency))
                {
                    affected.insert(format!("{name}@{version}"));
                }
            }
        }
        if affected == changed {
            return changed.into_iter().collect();
        }
        changed = affected;
    }
}

/// Whether `request` has a recipe for `system`.
pub fn is_supported(catalog: &PackageCatalog, request: &str, system: &str) -> bool {
    request.split_once('@').is_some_and(|(name, version)| {
        catalog
            .packages
            .get(name)
            .and_then(|package| package.versions.get(version))
            .is_some_and(|recipe| recipe.platforms.contains_key(system))
    })
}

fn build_inputs<'a>(
    recipe: Option<&'a CatalogVersion>,
    system: Option<&str>,
) -> Option<impl PartialEq + 'a> {
    let recipe = recipe?;
    let platforms: Vec<_> = recipe
        .platforms
        .iter()
        .filter(|(platform, _)| system.is_none_or(|system| *platform == system))
        .collect();
    if system.is_some() && platforms.is_empty() {
        return None;
    }
    Some((recipe.revision, &recipe.extra, platforms))
}

fn dependencies<'a>(
    recipe: &'a CatalogVersion,
    system: Option<&'a str>,
) -> impl Iterator<Item = &'a str> {
    recipe
        .platforms
        .iter()
        .filter(move |(platform, _)| system.is_none_or(|system| *platform == system))
        .filter_map(|(_, recipe)| recipe.build.as_ref())
        .flat_map(|build| {
            build
                .dependencies
                .iter()
                .map(|dependency| dependency.package())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> PackageCatalog {
        crate::test_catalog::catalog().clone()
    }

    #[test]
    fn an_unchanged_catalog_selects_nothing_and_a_new_one_selects_everything() {
        let catalog = catalog();
        assert!(changed_requests(&catalog, &catalog, None).is_empty());
        let empty = PackageCatalog {
            packages: Default::default(),
            extra: Default::default(),
        };
        let everything = changed_requests(&empty, &catalog, None);
        let versions: usize = catalog
            .packages
            .values()
            .map(|package| package.versions.len())
            .sum();
        assert_eq!(everything.len(), versions);
    }

    #[test]
    fn a_license_alone_is_not_a_build_input() {
        let before = catalog();
        let (name, package) = before.packages.iter().next().unwrap();
        let version = package.versions.keys().next().unwrap();
        let mut after = before.clone();
        let recipe = |catalog: &mut PackageCatalog| {
            catalog
                .packages
                .get_mut(name)
                .unwrap()
                .versions
                .get_mut(version)
                .unwrap()
                .clone()
        };
        let mut changed = recipe(&mut after);
        changed.license = "LicenseRef-Changed".into();
        after
            .packages
            .get_mut(name)
            .unwrap()
            .versions
            .insert(version.clone(), changed.clone());
        assert!(changed_requests(&before, &after, None).is_empty());
        changed.revision += 1;
        after
            .packages
            .get_mut(name)
            .unwrap()
            .versions
            .insert(version.clone(), changed);
        assert!(changed_requests(&before, &after, None).contains(&format!("{name}@{version}")));
    }

    fn recipe(name: &str, dependencies: &str, systems: &[&str], revision: u32) -> String {
        let platforms: String = systems
            .iter()
            .map(|system| format!("[\"{system}\"] = {{ default_version = \"1\" }},"))
            .collect();
        let digests: String = systems
            .iter()
            .map(|system| format!("[\"{system}\"] = \"{}\",", "0".repeat(64)))
            .collect();
        format!(
            r#"return {{
                name = "{name}", description = "A package", homepage = "https://example.org",
                default_license = "MIT",
                source = {{ url = "https://example.org/{name}-{{version}}.tar.gz",
                           archive = "tar.gz", strip_prefix = "{name}-{{version}}" }},
                build = {{
                    backend = "custom",
                    {dependencies}
                    steps = {{ build = {{{{ "make" }}}}, check = {{{{ "make", "test" }}}},
                              install = {{{{ "make", "DESTDIR={{prefix}}", "install" }}}} }},
                }},
                outputs = {{ bins = {{ "{name}" }}, checks = {{ {{ "{name}", "--version" }} }} }},
                platforms = {{ {platforms} }},
                versions = {{ ["1"] = {{ digests = {{ {digests} }}, revision = {revision} }} }},
            }}"#
        )
    }

    #[test]
    fn dependents_rebuild_with_their_dependency_on_the_systems_that_build_them() {
        let directory = tempfile::tempdir().unwrap();
        let load = |library_revision| {
            let systems = ["aarch64-linux", "aarch64-macos", "x86_64-linux"];
            let library = recipe("library", "", &systems, library_revision);
            std::fs::write(directory.path().join("library.lua"), library).unwrap();
            let app = recipe("app", "dependencies = { \"library@1\" },", &systems[1..], 1);
            std::fs::write(directory.path().join("app.lua"), app).unwrap();
            PackageCatalog::from_directory(directory.path()).unwrap()
        };
        let before = load(1);
        let after = load(2);
        assert_eq!(
            changed_requests(&before, &after, None),
            ["app@1", "library@1"]
        );
        assert_eq!(
            changed_requests(&before, &after, Some("aarch64-linux")),
            ["library@1"]
        );
        assert_eq!(
            changed_requests(&before, &after, Some("x86_64-linux")),
            ["app@1", "library@1"]
        );
        assert!(!is_supported(&after, "app@1", "aarch64-linux"));
        assert!(is_supported(&after, "app@1", "aarch64-macos"));
    }
}
