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
pub use recipe::{LineBy, Lines, Rolling, Upstream};
use rootbeer_drv::{
    AppSettings, Dependency, DependencyKind, Derivation, Extract, Format, Key, Platform, Sha256,
    output_path,
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// A catalog of recipes, keyed by their name.
pub struct Catalog {
    recipes: BTreeMap<String, Recipe>,
}

/// Each recipe's patch files by recipe name, as file names and their diffs.
pub type Patches = BTreeMap<String, BTreeMap<String, String>>;

/// Where a version downloads from on one platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub url: String,
    /// The GitHub release asset `url` names, whose digest GitHub publishes
    pub release: Option<ReleaseAsset>,
    /// How an extract reads a prebuilt archive, whose tree a version can
    /// record. A source or a raw binary has none.
    pub extract: Option<Format>,
}

/// What a declared version records for one platform.
#[derive(Debug)]
pub struct Pinned {
    pub download: Download,
    pub sha256: Sha256,
    /// The digest of the unpacked archive, once recorded.
    pub tree: Option<Sha256>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub repository: String,
    pub tag: String,
    pub asset: String,
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
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    /// The Linux builder image or the macOS Xcode and SDK versions.
    /// This isn't ideal, but we'll be getting rid of this soon.
    pub system: String,
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    /// How each app that differs from the default is installed.
    pub app_settings: BTreeMap<String, AppSettings>,
    /// The upstream repository's web address, when the recipe names one.
    pub repository: Option<String>,
    /// The tag upstream released this version under.
    pub tag: Option<String>,
    /// How the output is made. Either `prebuilt` or the source build's
    /// backend.
    pub build: String,
    /// Packages the output needs when it runs.
    pub runtime_dependencies: BTreeSet<String>,
    /// What the output expects the host to provide, such as a soname.
    pub host: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Parse { file: String, message: String },
    Recipe { target: Target, message: String },
    Cycle(Vec<Target>),
}

impl Host {
    /// Parses a Lua file returning a table of hosts keyed by platform.
    pub fn parse(file: &str, source: &str) -> Result<BTreeMap<Platform, Host>, Error> {
        recipe::load(file, source).map_err(|error| Error::Parse {
            file: file.to_string(),
            message: error.to_string(),
        })
    }
}

impl Graph {
    pub fn derivations_of<'a>(&'a self, package: &'a Package) -> Vec<(&'a Key, &'a Derivation)> {
        let mut keys = Vec::new();
        self.inputs_first(&package.build, &mut keys);
        keys.extend(package.check.iter());

        keys.into_iter()
            .filter_map(|key| Some((key, self.derivations.get(key)?)))
            .collect()
    }

    fn inputs_first<'a>(&'a self, key: &'a Key, keys: &mut Vec<&'a Key>) {
        if keys.contains(&key) {
            return;
        }

        match self.derivations.get(key) {
            Some(Derivation::Build(build)) => {
                for input in build.inputs.values() {
                    self.inputs_first(input, keys);
                }
            }
            Some(Derivation::Extract(extract)) => self.inputs_first(&extract.source, keys),
            _ => {}
        }

        keys.push(key);
    }
}

impl Catalog {
    /// Parses `(file name, recipe source)` pairs whose recipes name no patch
    /// files.
    pub fn parse<'a>(
        files: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Catalog, Error> {
        Catalog::parse_with_patches(files, &Patches::new())
    }

    /// Parses `(file name, recipe source)` pairs, reading the patch files
    /// their sources name from `patches`.
    pub fn parse_with_patches<'a>(
        files: impl IntoIterator<Item = (&'a str, &'a str)>,
        patches: &Patches,
    ) -> Result<Catalog, Error> {
        let mut recipes = BTreeMap::new();
        for (file, source) in files {
            let parse_error = |message: String| Error::Parse {
                file: file.to_string(),
                message,
            };

            let mut recipe = recipe::load::<Recipe>(file, source)
                .map_err(|error| parse_error(error.to_string()))?;
            recipe.has_commit_placeholder = source.contains("{commit}");
            recipe
                .read_patches(patches.get(&recipe.name))
                .map_err(parse_error)?;
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

    /// Every recipe's name, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.recipes.keys().map(String::as_str)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.recipes.contains_key(name)
    }

    /// The platforms a recipe declares.
    pub fn platforms(&self, name: &str) -> Vec<Platform> {
        self.recipes
            .get(name)
            .map(|recipe| recipe.platforms.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Each upstream a recipe finds new versions in, with the platforms that
    /// look there. A platform's own upstream replaces the recipe's.
    pub fn upstreams(&self, name: &str) -> Vec<(Vec<Platform>, &Upstream)> {
        let Some(recipe) = self.recipes.get(name) else {
            return Vec::new();
        };

        let mut upstreams: Vec<(Vec<Platform>, &Upstream)> = Vec::new();
        for (platform, spec) in &recipe.platforms {
            let Some(upstream) = spec.upstream.as_ref().or(recipe.upstream.as_ref()) else {
                continue;
            };

            match upstreams.iter_mut().find(|(_, seen)| *seen == upstream) {
                Some((platforms, _)) => platforms.push(*platform),
                None => upstreams.push((vec![*platform], upstream)),
            }
        }

        upstreams
    }

    /// Whether a recipe's source names `{commit}`, so each of its versions
    /// records the commit it builds from.
    pub fn has_commit_placeholder(&self, name: &str) -> bool {
        self.recipes
            .get(name)
            .is_some_and(|recipe| recipe.has_commit_placeholder)
    }

    /// Where a version the recipe doesn't declare yet would download from, so
    /// its digest can be pinned before it's added.
    pub fn download(
        &self,
        name: &str,
        version: &str,
        commit: Option<&str>,
        serial: Option<u64>,
        platform: Platform,
    ) -> Result<Download, String> {
        let recipe = self
            .recipes
            .get(name)
            .ok_or_else(|| format!("{name} isn't in the catalog"))?;

        recipe.download(version, commit.map(str::to_string), serial, platform)
    }

    /// Where a declared version downloads from on a platform, with the
    /// digests it records there.
    pub fn pinned(&self, name: &str, version: &str, platform: Platform) -> Result<Pinned, String> {
        let recipe = self
            .recipes
            .get(name)
            .ok_or_else(|| format!("{name} isn't in the catalog"))?;

        recipe.pinned(version, platform)
    }

    /// The paths of the app bundles a version exports on a platform.
    pub fn apps(
        &self,
        name: &str,
        version: &str,
        platform: Platform,
    ) -> Result<Vec<String>, String> {
        let recipe = self
            .recipes
            .get(name)
            .ok_or_else(|| format!("{name} isn't in the catalog"))?;

        recipe.app_paths(version, platform)
    }

    /// The versions of a package any recipe depends on, at any layer.
    pub fn depended_on(&self, name: &str) -> BTreeSet<&str> {
        self.recipes
            .values()
            .flat_map(|recipe| {
                let platforms = recipe.platforms.values().map(|platform| &platform.spec);
                let versions = recipe.versions.values().map(|version| &version.spec);
                std::iter::once(&recipe.shared)
                    .chain(platforms)
                    .chain(versions)
            })
            .flat_map(|spec| {
                let prebuilt = spec
                    .prebuilt
                    .iter()
                    .flat_map(|prebuilt| &prebuilt.dependencies);
                let build = spec.build.iter().flat_map(|build| &build.dependencies);
                prebuilt.chain(build)
            })
            .filter(|dependency| dependency.package == name)
            .filter_map(|dependency| dependency.version.as_deref())
            .collect()
    }

    /// The versions a recipe declares with a download for a platform.
    pub fn versions(&self, name: &str, platform: Platform) -> Vec<&str> {
        let Some(recipe) = self.recipes.get(name) else {
            return Vec::new();
        };

        recipe
            .versions
            .iter()
            .filter(|(_, version)| version.digests.contains_key(&platform))
            .map(|(version, _)| version.as_str())
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
            shared: BTreeSet::new(),
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
    shared: BTreeSet<Key>,
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

        let mut resolved = recipe
            .resolve(&target.version, target.platform)
            .map_err(recipe_error)?;

        self.stack.push(target.clone());
        let dependencies = self.dependencies(&mut resolved, target);
        self.stack.pop();

        let (dependencies, linked) = dependencies?;
        let mut package = backend::Package {
            name: &recipe.name,
            host,
            resolved,
            rpaths: Vec::new(),
        };

        let is_shared = package.is_shared().map_err(recipe_error)?;
        package.rpaths = is_shared
            .then(|| "${out}/lib".to_string())
            .into_iter()
            .chain(linked)
            .collect();

        let package = self
            .lower(recipe, &package, dependencies)
            .map_err(recipe_error)?;

        if is_shared {
            self.shared.insert(package.build.clone());
        }

        let key = package.build.clone();
        self.graph.packages.insert(target.clone(), package);
        Ok(key)
    }

    // Sorted by name, then kind, because dependency order is part of the key.
    // Fills `{dependencies.<name>}` and returns the `lib/` of shared `link_runtime`
    // dependencies: an unused rpath would be a false runtime reference.
    fn dependencies(
        &mut self,
        resolved: &mut recipe::Resolved<'a>,
        target: &Target,
    ) -> Result<(Vec<Dependency>, Vec<String>), Error> {
        let platform = target.platform;
        let declared = match (&resolved.spec.prebuilt, &resolved.spec.build) {
            (Some(prebuilt), _) => prebuilt.dependencies.as_slice(),
            (None, Some(build)) => build.dependencies.as_slice(),
            (None, None) => &[],
        };

        // A prebuilt version that records its tree is extracted by rb, so an
        // unpacker the recipe still declares for its other versions is left
        // out of this one's key.
        let is_extracted = resolved.spec.prebuilt.is_some() && resolved.tree.is_some();
        let declared = declared.iter().filter(|declared| {
            let is_unpacker = matches!(declared.kind, RecipeKind::Build)
                && backend::is_unpacker(&declared.package);
            !(is_extracted && is_unpacker)
        });

        let mut dependencies = Vec::new();
        let mut linked = Vec::new();
        for declared in declared {
            let version = match &declared.version {
                Some(version) => version.clone(),
                None => self
                    .catalog
                    .default_version(&declared.package, platform)
                    .ok_or_else(|| Error::Recipe {
                        target: target.clone(),
                        message: format!(
                            "{} has no default version on {platform} to depend on",
                            declared.package
                        ),
                    })?
                    .to_string(),
            };

            let key = self.package(&Target {
                name: declared.package.clone(),
                version: version.clone(),
                platform,
            })?;

            let kinds: &[DependencyKind] = match declared.kind {
                RecipeKind::All => &[DependencyKind::Build, DependencyKind::Linked],
                RecipeKind::Build => &[DependencyKind::Build],
                RecipeKind::Link | RecipeKind::LinkRuntime => &[DependencyKind::Linked],
                RecipeKind::Runtime => &[DependencyKind::Runtime],
            };

            // Only link_runtime gives the output a run-path to a shared library
            let is_shared = self.shared.contains(&key);
            let misuse = match declared.kind {
                RecipeKind::Link if is_shared => Some(("link", "use link_runtime")),
                RecipeKind::All if is_shared => Some(("all", "list it as build and link_runtime")),
                _ => None,
            };

            if let Some((kind, fix)) = misuse {
                return Err(Error::Recipe {
                    target: target.clone(),
                    message: format!(
                        "{} ships a shared library, so kind {kind} leaves no run-path, {fix} instead",
                        declared.package,
                    ),
                });
            }

            let path = output_path(&key, &declared.package, &version, "out")
                .display()
                .to_string();
            if matches!(declared.kind, RecipeKind::LinkRuntime) && is_shared {
                linked.push(format!("{path}/lib"));
            }

            resolved
                .values
                .dependencies
                .insert(declared.package.clone(), path);

            dependencies.extend(kinds.iter().map(|kind| Dependency {
                key: key.clone(),
                name: declared.package.clone(),
                kind: *kind,
            }));
        }

        dependencies.sort_by(|a, b| (&a.name, a.kind).cmp(&(&b.name, b.kind)));
        Ok((dependencies, linked))
    }

    fn lower(
        &mut self,
        recipe: &Recipe,
        package: &backend::Package<'_>,
        dependencies: Vec<Dependency>,
    ) -> Result<Package, String> {
        let spec = &package.resolved.spec;
        let (build, method) = match (&spec.prebuilt, &spec.source, &spec.build) {
            (Some(prebuilt), None, _) => {
                let (fetch, install) = package.download(prebuilt)?;
                let source = self.insert(Derivation::Fetch(fetch))?;
                let build = match (package.resolved.tree, install) {
                    (Some(tree), Some(install)) => {
                        let unpacked = self.insert(Derivation::Extract(Extract {
                            tree: tree.clone(),
                            source,
                            format: backend::prebuilt::format(&install),
                        }))?;
                        package.copy(&install, prebuilt, unpacked, dependencies)?
                    }
                    (Some(_), None) => {
                        return Err("a raw binary has no tree, so it records no trees".into());
                    }
                    (None, install) => package.unpack(install, prebuilt, source, dependencies)?,
                };
                (build, "prebuilt")
            }
            (None, Some(source), Some(build)) => {
                let source_key = self.insert(Derivation::Fetch(package.source(source)?))?;
                let vendor = package.vendor(source, build.backend, &source_key, &dependencies)?;

                let mut inputs = BTreeMap::from([("source".to_string(), source_key)]);
                if let Some(vendor) = vendor {
                    inputs.insert("vendor".into(), self.insert(Derivation::Build(vendor))?);
                }

                let backend = build.backend.name();
                (
                    package.compile(source, build, inputs, dependencies)?,
                    backend,
                )
            }
            (Some(_), Some(_), _) => {
                return Err("a platform is prebuilt or built from source, not both".into());
            }
            (None, Some(_), None) => return Err("a source platform needs a build".into()),
            (None, None, _) => return Err("a platform needs a prebuilt or a source".into()),
        };

        // A check runs what was built, so it needs what the build needs to run
        let runtime = build
            .dependencies
            .iter()
            .filter(|dependency| dependency.kind == DependencyKind::Runtime)
            .cloned()
            .collect();

        // A shared library linked for run time stays referenced when it runs
        let runtime_dependencies = build
            .dependencies
            .iter()
            .filter(|dependency| match dependency.kind {
                DependencyKind::Runtime => true,
                DependencyKind::Linked => self.shared.contains(&dependency.key),
                DependencyKind::Build => false,
            })
            .map(|dependency| dependency.name.clone())
            .collect();
        // A reference the loader resolves from the output asks nothing of the host
        let host = build
            .host
            .iter()
            .filter(|reference| {
                let relative = ["$ORIGIN", "@executable_path", "@loader_path", "@rpath"];
                !relative.iter().any(|prefix| reference.starts_with(prefix))
            })
            .cloned()
            .collect();

        let build = self.insert(Derivation::Build(build))?;
        let check = package
            .check(&build, runtime)?
            .map(|check| self.insert(Derivation::Check(check)))
            .transpose()?;

        let values = &package.resolved.values;
        let upstream = recipe
            .platforms
            .get(&values.platform)
            .and_then(|platform| platform.upstream.as_ref())
            .or(recipe.upstream.as_ref());

        let outputs = &spec.outputs;
        let apps = outputs.apps.clone().unwrap_or_default();
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
            apps: apps
                .iter()
                .map(|(name, app)| (name.clone(), app.path().to_string()))
                .collect(),
            app_settings: apps
                .iter()
                .map(|(name, app)| (name.clone(), app.settings()))
                .filter(|(_, settings)| *settings != AppSettings::default())
                .collect(),
            repository: upstream.and_then(Upstream::repository),
            tag: upstream.map(|_| values.tag.clone()),
            build: method.to_string(),
            runtime_dependencies,
            host,
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
