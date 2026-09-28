use std::collections::BTreeMap;
use std::path::Path;

use rootbeer_package::{catalog::CatalogResolver, graph::DependencyGraph, *};

use crate::BuildOptions;

/// Exact source recipes and resolved binary inputs, prepared before any build executes.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BuildPlan {
    pub(crate) catalog_sha256: String,
    pub(crate) graph: DependencyGraph,
    pub(crate) binaries: BTreeMap<String, LockedPackage>,
    /// Signed records of the source dependencies taken from the PDR instead of compiled.
    pub(crate) published: BTreeMap<String, String>,
    pub(crate) recipes: BTreeMap<String, CatalogRecipe>,
    /// Catalog revision per key; a recipe is one platform and no longer carries it.
    pub(crate) revisions: BTreeMap<String, u32>,
    pub(crate) inputs: PackageResolverInputs,
}

impl BuildPlan {
    /// Pins the metadata sources needed by this graph and resolves all binary inputs.
    pub fn current(catalog: &PackageCatalog, request: &str) -> Result<Self, String> {
        catalog.validate()?;
        let graph = DependencyGraph::new(
            catalog,
            &[request.into()],
            &ResolveContext::current().system,
        )?;
        let mut inputs = PackageResolverInputs::default();
        inputs.resolvers.insert(
            "rootbeer".into(),
            ResolverInput::Catalog {
                sha256: catalog.sha256(),
            },
        );
        Self::from_graph(catalog, graph, &inputs, backend_stack())
    }

    /// Resolves a build under caller-supplied, pinned metadata inputs.
    pub fn resolve(
        catalog: &PackageCatalog,
        request: &str,
        inputs: &PackageResolverInputs,
    ) -> Result<Self, String> {
        catalog.validate()?;
        let graph = DependencyGraph::new(
            catalog,
            &[request.into()],
            &ResolveContext::current().system,
        )?;
        Self::from_graph(catalog, graph, inputs, backend_stack())
    }

    fn from_graph(
        catalog: &PackageCatalog,
        graph: DependencyGraph,
        inputs: &PackageResolverInputs,
        backends: ResolverStack,
    ) -> Result<Self, String> {
        if inputs.catalog_sha256() != Some(catalog.sha256().as_str()) {
            return Err("build inputs must pin the current catalog".into());
        }
        let root = graph
            .order
            .last()
            .ok_or("build plan needs a root package")?;
        if graph::find_recipe(catalog, root)?.2.build.is_none() {
            return Err("this package uses an upstream binary, not a source build".into());
        }
        let context = ResolveContext::new(&graph.system);
        let mut resolver = ResolverStack::new().with_implicit_resolver("rootbeer");
        resolver.push(CatalogResolver::new(catalog, inputs, backends));
        let mut binaries = BTreeMap::new();
        let mut recipes = BTreeMap::new();
        let mut revisions = BTreeMap::new();
        for key in &graph.order {
            let (_, _, recipe) = graph::find_recipe(catalog, key)?;
            let (_, _, entry) = graph::find_recipe_definition(catalog, key)?;
            revisions.insert(key.clone(), entry.revision);
            let is_prebuilt = key != root && recipe.build.is_none();
            if is_prebuilt {
                let package = resolver
                    .resolve(&PackageRequest::parse(key), &context)
                    .map_err(|error| error.to_string())?
                    .package;
                binaries.insert(key.clone(), package);
            }
            recipes.insert(key.clone(), recipe.clone());
        }
        Ok(Self {
            catalog_sha256: catalog.sha256(),
            graph,
            binaries,
            published: BTreeMap::new(),
            recipes,
            revisions,
            inputs: inputs.clone(),
        })
    }

    /// Installs `key` from its published build instead of compiling it. The caller has verified
    /// `record` against the PDR's key; it travels in the receipt so a release can verify again.
    pub fn use_published(
        &mut self,
        key: &str,
        package: LockedPackage,
        record: String,
    ) -> Result<(), String> {
        self.use_built(key, package)?;
        self.published.insert(key.to_string(), record);
        Ok(())
    }

    /// Installs `key` from a build of its exact recipe that another job already qualified,
    /// instead of compiling it again. The caller has verified the build's receipt and archive.
    pub fn use_built(&mut self, key: &str, package: LockedPackage) -> Result<(), String> {
        let is_dependency = self.graph.order.last().is_some_and(|root| root != key);
        if !is_dependency
            || !self
                .recipes
                .get(key)
                .is_some_and(|recipe| recipe.build.is_some())
        {
            return Err(format!(
                "{key}: only a source-built dependency has a qualified build"
            ));
        }
        if self.binaries.contains_key(key) {
            return Err(format!("{key}: already installed from another build"));
        }
        self.binaries.insert(key.to_string(), package);
        Ok(())
    }

    /// The build a dependency is installed from instead of being compiled, if any.
    pub fn installed(&self, key: &str) -> Option<&LockedPackage> {
        self.binaries.get(key)
    }

    pub fn graph(&self) -> &DependencyGraph {
        &self.graph
    }

    /// Executes the frozen recipes and binary facts without consulting mutable metadata.
    pub fn execute(&self, output: &Path, opts: &BuildOptions) -> Result<BuildArtifact, String> {
        crate::execute(self, output, opts)
    }
}

#[cfg(test)]
#[path = "plan_test.rs"]
mod tests;
