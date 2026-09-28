use crate::{BuildDependency, DependencyKind};
use crate::{
    CatalogPackage, CatalogRecipe, CatalogVersion, PackageCatalog, PackageRequest, ResolveContext,
};
use std::collections::{BTreeMap, BTreeSet};

pub fn find_recipe_definition<'a>(
    catalog: &'a PackageCatalog,
    request: &str,
) -> Result<(&'a CatalogPackage, &'a str, &'a CatalogVersion), String> {
    let request = PackageRequest::parse(request);
    if request.source.is_some() {
        return Err("build graphs require resolved source revisions; use the consumer source resolver to pin HEAD or Git refs first".into());
    }
    if request
        .resolver
        .as_deref()
        .is_some_and(|resolver| resolver != "rootbeer")
    {
        return Err("source builds use canonical catalog names".into());
    }
    let package = catalog
        .find(&request.name)
        .ok_or_else(|| format!("unknown package `{}`", request.name))?;
    let system = ResolveContext::current().system;
    let version = match request.version.as_deref() {
        Some(version) => version,
        None => package
            .default_version_for(&system)
            .ok_or_else(|| format!("{} does not support {system}", package.name))?,
    };
    let (version, recipe) = package
        .versions
        .get_key_value(version)
        .ok_or_else(|| format!("{}@{version} is not in the catalog", package.name))?;
    Ok((package, version, recipe))
}

/// Selects the package recipe for the host platform.
pub fn find_recipe<'a>(
    catalog: &'a PackageCatalog,
    request: &str,
) -> Result<(&'a CatalogPackage, &'a str, CatalogRecipe), String> {
    find_recipe_for_system(catalog, request, &ResolveContext::current().system)
}

/// Selects a platform's exact recipe without unrelated platform contracts.
pub fn find_recipe_for_system<'a>(
    catalog: &'a PackageCatalog,
    request: &str,
    system: &str,
) -> Result<(&'a CatalogPackage, &'a str, CatalogRecipe), String> {
    let parsed = PackageRequest::parse(request);
    let package = catalog
        .find(&parsed.name)
        .ok_or_else(|| format!("unknown package `{}`", parsed.name))?;
    let exact = match parsed.version {
        None => format!(
            "{request}@{}",
            package
                .default_version_for(system)
                .ok_or_else(|| format!("{} does not support {system}", package.name))?
        ),
        Some(_) => request.into(),
    };
    let (package, version, entry) = find_recipe_definition(catalog, &exact)?;
    let recipe = entry
        .for_system(system)
        .ok_or_else(|| format!("{}@{version} does not build {system}", package.name))?
        .clone();
    Ok((package, version, recipe))
}

fn visit(
    catalog: &PackageCatalog,
    request: &str,
    system: &str,
    active: &mut BTreeSet<String>,
    done: &mut BTreeSet<String>,
    order: &mut Vec<String>,
) -> Result<(), String> {
    let (package, version, recipe) = find_recipe_for_system(catalog, request, system)?;
    recipe.validate(system)?;
    let key = format!("{}@{version}", package.name);
    if done.contains(&key) {
        return Ok(());
    }
    if !active.insert(key.clone()) {
        return Err(format!("build dependency cycle at `{key}`"));
    }
    if let Some(build) = &recipe.build {
        for dependency in &build.dependencies {
            let dependency = dependency.package();
            if !catalog
                .packages
                .contains_key(&PackageRequest::parse(dependency).name)
            {
                return Err(format!(
                    "build dependency `{dependency}` must use a canonical package name"
                ));
            }
            visit(catalog, dependency, system, active, done, order)?;
        }
    }
    active.remove(&key);
    done.insert(key.clone());
    order.push(key);
    Ok(())
}

pub(crate) fn validate_dependencies(catalog: &PackageCatalog) -> Result<(), String> {
    for system in ["aarch64-macos", "aarch64-linux", "x86_64-linux"] {
        let mut done = BTreeSet::new();
        for package in catalog.packages.values() {
            for (version, entry) in &package.versions {
                if !entry.platforms.contains_key(system) {
                    continue;
                }
                visit(
                    catalog,
                    &format!("{}@{version}", package.name),
                    system,
                    &mut BTreeSet::new(),
                    &mut done,
                    &mut Vec::new(),
                )?;
            }
        }
    }
    Ok(())
}

/// A validated dependency closure in deterministic execution order.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DependencyGraph {
    pub system: String,
    pub order: Vec<String>,
    pub nodes: std::collections::BTreeMap<String, DependencyNode>,
}

/// Direct edges and the ordered transitive inputs visible to a recipe.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DependencyNode {
    pub dependencies: Vec<BuildDependency>,
    pub closure: Vec<String>,
    pub runtime_closure: Vec<String>,
    pub exports: BTreeMap<String, DependencyExports>,
}

/// Exports visible to a consumer, after propagating dependency roles.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DependencyExports {
    pub has_bins: bool,
    pub has_libraries: bool,
    pub libraries: Vec<std::path::PathBuf>,
    /// Linked as shared libraries, so version aliases and cmake packages are exported too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_shared: bool,
}

fn exports(
    package: &str,
    kind: DependencyKind,
    nodes: &BTreeMap<String, DependencyNode>,
    visible: &mut BTreeMap<String, DependencyExports>,
    visited: &mut BTreeSet<(String, DependencyKind)>,
) {
    if kind == DependencyKind::Runtime || !visited.insert((package.into(), kind)) {
        return;
    }
    let entry = visible.entry(package.into()).or_default();
    entry.has_bins |= matches!(kind, DependencyKind::All | DependencyKind::Build);
    entry.has_libraries |= kind != DependencyKind::Build;
    entry.is_shared |= kind == DependencyKind::LinkRuntime;
    if kind == DependencyKind::Build {
        return;
    }
    for dependency in &nodes[package].dependencies {
        if dependency.kind() == DependencyKind::Runtime
            || (matches!(kind, DependencyKind::Link | DependencyKind::LinkRuntime)
                && dependency.kind() == DependencyKind::Build)
        {
            continue;
        }
        // A runtime link stays one only through runtime links: nothing else carries the consumer
        // a runtime closure, so it links that library statically.
        let kind = match (kind, dependency.kind()) {
            (DependencyKind::LinkRuntime, DependencyKind::LinkRuntime) => {
                DependencyKind::LinkRuntime
            }
            (DependencyKind::Link | DependencyKind::LinkRuntime, _) => DependencyKind::Link,
            (_, DependencyKind::LinkRuntime) => DependencyKind::Link,
            (_, kind) => kind,
        };
        exports(dependency.package(), kind, nodes, visible, visited);
    }
}

impl DependencyGraph {
    /// Resolves aliases and defaults before constructing one graph for all roots.
    pub fn new(
        catalog: &PackageCatalog,
        requests: &[String],
        system: &str,
    ) -> Result<Self, String> {
        let mut order = Vec::new();
        let mut done = BTreeSet::new();
        for request in requests {
            let parsed = PackageRequest::parse(request);
            let package = catalog
                .find(&parsed.name)
                .ok_or_else(|| format!("unknown package `{}`", parsed.name))?;
            if parsed
                .resolver
                .as_deref()
                .is_some_and(|resolver| resolver != "rootbeer")
            {
                return Err("build graphs require canonical catalog requests".into());
            }
            let Some(version) = parsed
                .version
                .as_deref()
                .or_else(|| package.default_version_for(system))
            else {
                continue;
            };
            visit(
                catalog,
                &format!("{}@{version}", package.name),
                system,
                &mut BTreeSet::new(),
                &mut done,
                &mut order,
            )?;
        }
        let mut nodes = std::collections::BTreeMap::<String, DependencyNode>::new();
        for key in &order {
            let (_, _, recipe) = find_recipe_for_system(catalog, key, system)?;
            let dependencies = recipe
                .build
                .as_ref()
                .map(|build| build.dependencies.clone())
                .unwrap_or_default();
            let mut closure = BTreeSet::new();
            let mut runtime_closure = BTreeSet::new();
            let mut visible = BTreeMap::new();
            let mut visited = BTreeSet::new();
            for dependency in &dependencies {
                let package = dependency.package();
                if dependency.kind().is_runtime() {
                    runtime_closure.insert(package.to_string());
                    runtime_closure.extend(nodes[package].runtime_closure.iter().cloned());
                }
                closure.insert(package.to_string());
                closure.extend(nodes[package].closure.iter().cloned());
                exports(
                    package,
                    dependency.kind(),
                    &nodes,
                    &mut visible,
                    &mut visited,
                );
            }
            for (name, exports) in &mut visible {
                if exports.has_libraries {
                    exports.libraries = find_recipe_for_system(catalog, name, system)?
                        .2
                        .build
                        .as_ref()
                        .map(|build| build.libraries.clone())
                        .unwrap_or_default();
                }
            }
            nodes.insert(
                key.clone(),
                DependencyNode {
                    dependencies,
                    runtime_closure: order
                        .iter()
                        .filter(|key| runtime_closure.contains(*key))
                        .cloned()
                        .collect(),
                    exports: visible,
                    closure: order
                        .iter()
                        .filter(|dependency| closure.contains(*dependency))
                        .cloned()
                        .collect(),
                },
            );
        }
        Ok(Self {
            system: system.into(),
            order,
            nodes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_edges_are_separate_from_build_and_link_exports() {
        let template = crate::test_catalog::catalog().packages["xz"].clone();
        let mut catalog = PackageCatalog {
            extra: Default::default(),
            packages: BTreeMap::new(),
        };
        for (name, edges) in [
            ("data", vec![]),
            ("base", vec![]),
            ("tool", vec![("data", DependencyKind::Runtime)]),
            ("static", vec![]),
            ("shared", vec![("base", DependencyKind::LinkRuntime)]),
            ("bundled", vec![("shared", DependencyKind::All)]),
            (
                "root",
                vec![
                    ("tool", DependencyKind::Build),
                    ("static", DependencyKind::Link),
                    ("shared", DependencyKind::LinkRuntime),
                    ("data", DependencyKind::Runtime),
                ],
            ),
        ] {
            let mut package = template.clone();
            package.name = name.into();
            package.aliases.clear();
            let mut recipe = package.versions.values().next().unwrap().clone();
            package.default_versions = recipe
                .platforms
                .keys()
                .map(|system| (system.clone(), "1".to_string()))
                .collect();
            let dependencies: Vec<BuildDependency> = edges
                .into_iter()
                .map(|(name, kind)| BuildDependency::Scoped {
                    package: format!("{name}@1"),
                    kind,
                })
                .collect();
            for platform in recipe.platforms.values_mut() {
                platform.build.as_mut().unwrap().dependencies = dependencies.clone();
            }
            package.versions = BTreeMap::from([("1".into(), recipe)]);
            catalog.packages.insert(name.into(), package);
        }
        let graph = DependencyGraph::new(
            &catalog,
            &["root".into(), "bundled".into()],
            &ResolveContext::current().system,
        )
        .unwrap();
        let root = &graph.nodes["root@1"];
        assert_eq!(
            root.runtime_closure
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["base@1".into(), "shared@1".into(), "data@1".into()])
        );
        assert!(!root.exports.contains_key("data@1"));
        assert!(root.exports["tool@1"].has_bins);
        assert!(!root.exports["tool@1"].has_libraries);
        for name in ["static@1", "shared@1", "base@1"] {
            assert!(root.exports[name].has_libraries);
            assert!(!root.exports[name].has_bins);
        }
        assert!(!root.exports["static@1"].is_shared);
        assert!(root.exports["shared@1"].is_shared);
        assert!(root.exports["base@1"].is_shared);
        let bundled = &graph.nodes["bundled@1"];
        assert!(bundled.runtime_closure.is_empty());
        assert!(!bundled.exports["base@1"].is_shared);
    }

    #[test]
    fn diamond_dependencies_share_nodes_and_keep_direct_edges() {
        let template = crate::test_catalog::catalog().packages["xz"].clone();
        let version = template
            .default_version_for("aarch64-linux")
            .unwrap()
            .to_string();
        let mut catalog = PackageCatalog {
            extra: Default::default(),
            packages: Default::default(),
        };
        for (name, dependencies) in [
            ("base", vec![]),
            ("left", vec!["base"]),
            ("right", vec!["base"]),
            ("root", vec!["left", "right"]),
        ] {
            let mut package = template.clone();
            package.name = name.into();
            package.aliases.clear();
            for recipe in package.versions.values_mut() {
                for platform in recipe.platforms.values_mut() {
                    platform.build.as_mut().unwrap().dependencies = dependencies
                        .iter()
                        .map(|name| format!("{name}@{version}").into())
                        .collect();
                }
            }
            catalog.packages.insert(name.into(), package);
        }
        catalog
            .packages
            .get_mut("root")
            .unwrap()
            .aliases
            .push("alias".into());
        let graph = DependencyGraph::new(
            &catalog,
            &["alias".into(), "right".into()],
            &ResolveContext::current().system,
        )
        .unwrap();
        let keys = ["base", "left", "right", "root"].map(|name| format!("{name}@{version}"));
        assert_eq!(graph.order, keys);
        assert_eq!(
            graph.nodes[&keys[3]]
                .dependencies
                .iter()
                .map(BuildDependency::package)
                .collect::<Vec<_>>(),
            keys[1..3]
        );
        assert_eq!(graph.nodes[&keys[3]].closure, keys[..3]);
        assert!(graph.nodes[&keys[0]].closure.is_empty());
        catalog
            .packages
            .get_mut("base")
            .unwrap()
            .versions
            .get_mut(&version)
            .unwrap()
            .platforms
            .values_mut()
            .flat_map(|platform| platform.build.as_mut())
            .for_each(|build| build.dependencies.push(keys[3].clone().into()));
        assert!(DependencyGraph::new(
            &catalog,
            &["root".into()],
            &ResolveContext::current().system
        )
        .unwrap_err()
        .contains("cycle"));
    }
}
