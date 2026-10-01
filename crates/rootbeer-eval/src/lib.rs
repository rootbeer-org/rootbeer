//! rootbeer-eval turns recipes into derivations and package metadata.
//!
//! Evaluation is a pure function of recipe text, platform, veresion, and host
//! toolchains. The only "IO" is running the Lua sandbox which translates the
//! package definition into a structured recipe that can be used to render out
//! a derivation.

mod backend;
mod recipe;
mod template;

use recipe::{DependencyKind as RecipeKind, Recipe};
use rootbeer_drv::{Dependency, DependencyKind, Derivation, Key, Platform};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// A catalog of recipes, keyed by their name.
pub struct Catalog {
    recipes: BTreeMap<String, Recipe>,
}

/// One version of a package on one platform.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Target {
    pub name: String,
    pub version: String,
    pub platform: Platform,
}

/// The pinned host toolchain stand-in for one platform, until toolchains are
/// catalog packages. A change rekeys every source build on that platform.
#[derive(Debug, Clone)]
pub struct Host {
    /// The Linux builder image or the macOS Xcode and SDK versions.
    /// This isn't ideal, but we'll be getting rid of this soon.
    pub system: String,
    pub rust: String,
}

#[derive(Debug, Default)]
pub struct Graph {
    pub derivations: BTreeMap<Key, Derivation>,
    /// The requested targets and everything they depend on.
    pub packages: BTreeMap<Target, Package>,
}

#[derive(Debug, Clone)]
pub struct Package {
    pub build: Key,
    pub check: Option<Key>,
    pub metadata: Metadata,
}

/// Metadata published alongside a package, used for search and display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub description: String,
    pub homepage: String,
    pub license: String,
    pub aliases: Vec<String>,
    pub maintainers: Vec<String>,
    /// Commands, each at `bin/<name>` in the output.
    pub bins: BTreeSet<String>,
    /// Application bundles by name, at their path in the output.
    pub apps: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Parse { file: String, message: String },
    Recipe { target: Target, message: String },
    Cycle(Vec<Target>),
}

impl Catalog {
    /// Parses `(file name, recipe source)` pairs.
    pub fn parse<'a>(
        files: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Catalog, Error> {
        let mut recipes = BTreeMap::new();
        for (file, source) in files {
            let parse_error = |message: String| Error::Parse {
                file: file.to_string(),
                message,
            };

            let recipe =
                Recipe::parse(file, source).map_err(|error| parse_error(error.to_string()))?;
            if recipes.contains_key(&recipe.name) {
                return Err(parse_error(format!("{} is declared twice", recipe.name)));
            }

            recipes.insert(recipe.name.clone(), recipe);
        }

        Ok(Catalog { recipes })
    }

    /// Every declared version on every platform it has a digest for.
    pub fn targets(&self) -> Vec<Target> {
        self.recipes
            .values()
            .flat_map(|recipe| {
                recipe.versions.iter().flat_map(move |(version, entry)| {
                    entry.digests.keys().map(move |platform| Target {
                        name: recipe.name.clone(),
                        version: version.clone(),
                        platform: *platform,
                    })
                })
            })
            .collect()
    }

    pub fn default_version(&self, name: &str, platform: Platform) -> Option<&str> {
        let recipe = self.recipes.get(name)?;
        recipe
            .platforms
            .get(&platform)
            .map(|spec| spec.default_version.as_str())
    }

    /// Evaluates each target and its dependencies, sharing work across them.
    pub fn evaluate(
        &self,
        hosts: &BTreeMap<Platform, Host>,
        targets: &[Target],
    ) -> Result<Graph, Error> {
        let mut evaluation = Evaluation {
            catalog: self,
            hosts,
            graph: Graph::default(),
            stack: Vec::new(),
        };

        targets
            .iter()
            .try_for_each(|target| evaluation.package(target).map(drop))?;

        Ok(evaluation.graph)
    }
}

struct Evaluation<'a> {
    catalog: &'a Catalog,
    hosts: &'a BTreeMap<Platform, Host>,
    graph: Graph,
    stack: Vec<Target>,
}

impl<'a> Evaluation<'a> {
    fn package(&mut self, target: &Target) -> Result<Key, Error> {
        if let Some(package) = self.graph.packages.get(target) {
            return Ok(package.build.clone());
        }

        if let Some(start) = self.stack.iter().position(|entered| entered == target) {
            let cycle = self
                .stack
                .iter()
                .skip(start)
                .chain([target])
                .cloned()
                .collect();
            return Err(Error::Cycle(cycle));
        }

        let recipe_error = |message: String| Error::Recipe {
            target: target.clone(),
            message,
        };

        let catalog = self.catalog;
        let recipe = catalog
            .recipes
            .get(&target.name)
            .ok_or_else(|| recipe_error("no such package".into()))?;

        let host = self
            .hosts
            .get(&target.platform)
            .ok_or_else(|| recipe_error("no host toolchain for this platform".into()))?;

        let resolved = recipe
            .resolve(&target.version, target.platform)
            .map_err(recipe_error)?;

        self.stack.push(target.clone());
        let dependencies = self.dependencies(&resolved, target.platform);
        self.stack.pop();

        let package = backend::Package {
            name: &recipe.name,
            host,
            resolved,
        };

        let package = self
            .lower(recipe, &package, dependencies?)
            .map_err(recipe_error)?;

        let key = package.build.clone();
        self.graph.packages.insert(target.clone(), package);
        Ok(key)
    }

    // Sorted by name, then kind, because dependency order is part of the key.
    fn dependencies(
        &mut self,
        resolved: &recipe::Resolved<'a>,
        platform: Platform,
    ) -> Result<Vec<Dependency>, Error> {
        let declared = match (&resolved.spec.prebuilt, &resolved.spec.build) {
            (None, Some(build)) => build.dependencies.as_slice(),
            _ => &[],
        };

        let mut dependencies = Vec::new();
        for declared in declared {
            let key = self.package(&Target {
                name: declared.package.clone(),
                version: declared.version.clone(),
                platform,
            })?;

            let kinds: &[DependencyKind] = match declared.kind {
                RecipeKind::All => &[DependencyKind::Build, DependencyKind::Linked],
                RecipeKind::Build => &[DependencyKind::Build],
                RecipeKind::Link | RecipeKind::LinkRuntime => &[DependencyKind::Linked],
                RecipeKind::Runtime => &[DependencyKind::Runtime],
            };

            dependencies.extend(kinds.iter().map(|kind| Dependency {
                key: key.clone(),
                name: declared.package.clone(),
                kind: *kind,
            }));
        }

        dependencies.sort_by(|a, b| (&a.name, a.kind).cmp(&(&b.name, b.kind)));
        Ok(dependencies)
    }

    fn lower(
        &mut self,
        recipe: &Recipe,
        package: &backend::Package<'_>,
        dependencies: Vec<Dependency>,
    ) -> Result<Package, String> {
        let spec = &package.resolved.spec;
        let build = match (&spec.prebuilt, &spec.source, &spec.build) {
            (Some(prebuilt), None, _) => {
                let (fetch, install) = package.download(prebuilt)?;
                let source = self.insert(Derivation::Fetch(fetch))?;
                package.unpack(install, source)?
            }
            (None, Some(source), Some(build)) => {
                let source_key = self.insert(Derivation::Fetch(package.source(source)?))?;
                let vendor = package.vendor(source, build.backend, &source_key, &dependencies)?;

                let mut inputs = BTreeMap::from([("source".to_string(), source_key)]);
                if let Some(vendor) = vendor {
                    inputs.insert("vendor".into(), self.insert(Derivation::Build(vendor))?);
                }

                package.compile(source, build, inputs, dependencies)?
            }
            (Some(_), Some(_), _) => {
                return Err("a platform is prebuilt or built from source, not both".into());
            }
            (None, Some(_), None) => return Err("a source platform needs a build".into()),
            (None, None, _) => return Err("a platform needs a prebuilt or a source".into()),
        };

        let build = self.insert(Derivation::Build(build))?;
        let check = package
            .check(&build)?
            .map(|check| self.insert(Derivation::Check(check)))
            .transpose()?;

        let outputs = &spec.outputs;
        let metadata = Metadata {
            description: recipe.description.clone(),
            homepage: recipe.homepage.clone(),
            license: package.resolved.license.to_string(),
            aliases: recipe.aliases.clone(),
            maintainers: recipe.recipe_maintainers.clone(),
            bins: outputs
                .bins
                .as_ref()
                .map(|bins| bins.names().into_iter().map(String::from).collect())
                .unwrap_or_default(),
            apps: outputs.apps.clone().unwrap_or_default(),
        };

        Ok(Package {
            build,
            check,
            metadata,
        })
    }

    fn insert(&mut self, derivation: Derivation) -> Result<Key, String> {
        let key = derivation.key().map_err(|error| error.to_string())?;
        self.graph.derivations.insert(key.clone(), derivation);
        Ok(key)
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{} ({})", self.name, self.version, self.platform)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Parse { file, message } => write!(f, "{file}: {message}"),
            Error::Recipe { target, message } => write!(f, "{target}: {message}"),
            Error::Cycle(cycle) => {
                let cycle = cycle
                    .iter()
                    .map(|target| format!("{}@{}", target.name, target.version));
                write!(
                    f,
                    "dependency cycle: {}",
                    cycle.collect::<Vec<_>>().join(" -> ")
                )
            }
        }
    }
}

impl std::error::Error for Error {}
