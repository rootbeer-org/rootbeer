use std::collections::BTreeMap;

use rootbeer_package::distribution::{input_key, DependencyInputs};
use rootbeer_package::graph::{find_recipe, find_recipe_for_system, DependencyGraph};
use rootbeer_package::{PackageCatalog, ResolveContext};

use rootbeer_package::repository::RepositoryResolver;

use crate::{BuildOptions, PublishedDependencies};
use rootbeer_build::Generation;

/// One package to qualify on the current platform.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PackageTask {
    pub package: String,
    pub name: String,
    pub system: String,
    pub key: String,
    /// The PDR root whose published dependencies the key names, so a builder reads the same one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pdr_root: Option<String>,
    /// Keys the same inputs had under reviewed predecessor engines, whose results remain valid.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub compatible_keys: Vec<String>,
    /// Source dependencies with no published build, which this build compiles unless another
    /// job hands it their builds.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub builds: Vec<String>,
}

/// What identifies every package in a build's closure, so a change to any of them is a new build.
/// A dependency taken from the PDR is identified by the build it was published from.
pub(crate) fn dependency_inputs(
    catalog: &PackageCatalog,
    id: &str,
    system: &str,
    published: &PublishedDependencies,
) -> Result<BTreeMap<String, DependencyInputs>, String> {
    generation_dependency_inputs(catalog, id, system, published, &Generation::current())
}

fn generation_dependency_inputs(
    catalog: &PackageCatalog,
    id: &str,
    system: &str,
    published: &PublishedDependencies,
    generation: &Generation,
) -> Result<BTreeMap<String, DependencyInputs>, String> {
    let graph = DependencyGraph::new(catalog, &[id.to_string()], system)?;
    graph.nodes[id]
        .closure
        .iter()
        .map(|dependency| {
            if let Some(inputs) = published.inputs(dependency) {
                return Ok((dependency.clone(), inputs));
            }
            let (package, version, recipe) = find_recipe_for_system(catalog, dependency, system)?;
            let inputs = DependencyInputs {
                revision: package.versions[version].revision,
                recipe_sha256: recipe.sha256(),
                engine_sha256: generation
                    .engine_identity(recipe.build.as_ref().map(|build| &build.backend)),
            };
            Ok((dependency.clone(), inputs))
        })
        .collect()
}

/// Plans explicitly selected packages, and the closure each builds with, without building them.
/// With a PDR, dependencies it has published builds of are keyed as those builds.
pub fn plan_packages(
    catalog: &PackageCatalog,
    requests: &[String],
    options: &BuildOptions,
    context: &str,
    pdr: Option<&RepositoryResolver>,
) -> Result<Vec<PackageTask>, String> {
    catalog.validate()?;
    if requests.is_empty() || context.trim().is_empty() {
        return Err(
            "package planning requires exact package requests and an environment context".into(),
        );
    }
    let system = ResolveContext::current().system;
    let mut tasks = std::collections::BTreeMap::new();
    let mut environments = std::collections::BTreeMap::new();
    for request in requests {
        let (package, version, recipe) = find_recipe(catalog, request)?;
        let revision = package.versions[version].revision;
        let id = format!("{}@{version}", package.name);
        if *request != id {
            return Err(format!("use the exact canonical request {id}"));
        }
        let backend = recipe.build.as_ref().map(|build| &build.backend);
        let engine = rootbeer_build::engine_identity(backend);
        let published = match pdr {
            Some(pdr) if recipe.build.is_some() => {
                PublishedDependencies::find(catalog, &id, &system, pdr)?
            }
            _ => PublishedDependencies::default(),
        };
        let environment = match environments.get(&engine) {
            Some(environment) => environment,
            None => {
                let identity = options.environment_identity(catalog, request, context)?;
                environments.entry(engine.clone()).or_insert(identity)
            }
        };
        let mut builds = Vec::new();
        for dependency in
            &DependencyGraph::new(catalog, std::slice::from_ref(&id), &system)?.nodes[&id].closure
        {
            let (_, _, dependency_recipe) = find_recipe_for_system(catalog, dependency, &system)?;
            if dependency_recipe.build.is_some() && published.inputs(dependency).is_none() {
                builds.push(dependency.clone());
            }
        }
        let compatible_keys = rootbeer_build::compatible_generations()
            .iter()
            .map(|generation| {
                Ok(input_key(
                    &id,
                    &system,
                    revision,
                    &recipe,
                    &generation.engine_identity(backend),
                    environment,
                    &generation_dependency_inputs(catalog, &id, &system, &published, generation)?,
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        tasks.insert(
            id.clone(),
            PackageTask {
                key: input_key(
                    &id,
                    &system,
                    revision,
                    &recipe,
                    &engine,
                    environment,
                    &dependency_inputs(catalog, &id, &system, &published)?,
                ),
                package: id,
                name: package.name.clone(),
                system: system.clone(),
                pdr_root: pdr
                    .filter(|_| recipe.build.is_some())
                    .map(|pdr| pdr.pin().root.clone()),
                compatible_keys,
                builds,
            },
        );
    }
    Ok(tasks.into_values().collect())
}
