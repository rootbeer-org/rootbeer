mod artifact;
mod bump;
mod catalog;
mod git;
mod index;
mod plan;
mod updates;

use crate::package::{
    cache, install, load_from, parse_platform, present_references, realize, setup, store_error,
    target, this_platform,
};
use rootbeer_cache::Cache;
use rootbeer_drv::{output_path, Build, DependencyKind, Derivation, Key, Platform, STORE_ROOT};
use rootbeer_eval::{Catalog, Graph, Host, Target};
use rootbeer_store::{Store, ROOT};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(clap::Args, Debug)]
pub struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Print a package's derivations and their keys
    Show {
        /// `name` or `name@version`; defaults to the platform's default version
        package: String,
        /// Defaults to this machine's platform
        #[arg(long, value_parser = parse_platform)]
        platform: Option<Platform>,
        #[command(flatten)]
        sources: Sources,
    },
    /// Print every declared version on every platform with its keys, as JSON lines
    Keys {
        #[command(flatten)]
        sources: Sources,
    },
    /// Compare two `keys` outputs: added, removed, and changed packages, and why
    Diff { base: PathBuf, head: PathBuf },
    /// Export a package's built output and its runtime references to a directory
    Export {
        /// `name` or `name@version`; defaults to the platform's default version
        package: String,
        directory: PathBuf,
        #[command(flatten)]
        sources: Sources,
    },
    /// Install every output exported to a directory, references first
    Import { directory: PathBuf },
    /// Push every output exported to a directory to a registry
    Publish {
        directory: PathBuf,
        #[command(flatten)]
        registry: cache::Registry,
    },
    /// Write a package index entry for each package with outputs in a namespace
    Index {
        directory: PathBuf,
        #[command(flatten)]
        registry: cache::Registry,
        #[command(flatten)]
        sources: Sources,
    },
    /// Merge index entries into a TUF repository and sign it with the online
    /// key
    Sign {
        repository: PathBuf,
        /// The online key as PKCS#8 DER
        #[arg(long)]
        key: PathBuf,
        /// Entries from `rb drv index`, or none to only renew the signatures
        entries: Option<PathBuf>,
    },
    /// Install a package from the signed index, after what it references
    Install(install::Args),
    /// Add a version to a recipe as its default, pinning each platform's
    /// download, and drop versions beyond --keep
    Bump(bump::Args),
    /// Find newer upstream versions for recipes, as JSON lines for rb drv bump
    Updates(updates::Args),
    /// Print the verified package index as JSON, for tools like the package
    /// browser
    Catalog {
        #[command(flatten)]
        source: install::Source,
    },
    /// Print, as JSON levels, the packages CI must build because no cache has them
    Plan {
        /// Each `name` or `name@version`, or every declared version when none
        /// are named. A name without a default version on the platform is
        /// left out
        packages: Vec<String>,
        #[arg(long, value_parser = parse_platform)]
        platform: Platform,
        #[command(flatten)]
        caches: cache::Caches,
        #[command(flatten)]
        sources: Sources,
    },
    /// Fetch, build, and check a package and its dependencies on this machine
    Build {
        /// `name` or `name@version`; defaults to the platform's default version
        package: String,
        /// Show build output as it happens, as well as logging it
        #[arg(long = "verbose")]
        is_verbose: bool,
        /// Fail instead of building a dependency the store and caches lack
        #[arg(long = "require-dependencies")]
        should_require_dependencies: bool,
        #[command(flatten)]
        caches: cache::Caches,
        #[command(flatten)]
        sources: Sources,
    },
}

#[derive(clap::Args, Debug)]
struct Sources {
    /// Directory of recipe files
    #[arg(long, default_value = "packages")]
    catalog: PathBuf,
    /// Lua file of host toolchain stand-ins by platform
    #[arg(long, default_value = "host.lua")]
    host: PathBuf,
}

/// One line of `rb drv keys`. The build is kept so `diff` can say what changed.
#[derive(Serialize, Deserialize)]
struct Line {
    name: String,
    version: String,
    platform: Platform,
    key: Key,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    check: Option<Key>,
    build: Build,
}

pub fn run(args: Args) {
    let result = match args.command {
        Command::Show {
            package,
            platform,
            sources,
        } => show(&sources, &package, platform),
        Command::Keys { sources } => keys(&sources),
        Command::Diff { base, head } => diff(&base, &head),
        Command::Plan {
            packages,
            platform,
            caches,
            sources,
        } => plan::plan(&sources, &packages, platform, &caches),
        Command::Build {
            package,
            is_verbose,
            should_require_dependencies,
            caches,
            sources,
        } => build(
            &sources,
            &package,
            is_verbose,
            should_require_dependencies,
            &caches,
        ),
        Command::Export {
            package,
            directory,
            sources,
        } => artifact::export(&sources, &package, &directory),
        Command::Import { directory } => artifact::import(&directory),
        Command::Publish {
            directory,
            registry,
        } => artifact::publish(&directory, &registry),
        Command::Index {
            directory,
            registry,
            sources,
        } => index::index(&sources, &directory, &registry),
        Command::Sign {
            repository,
            key,
            entries,
        } => index::sign(&repository, &key, entries.as_deref()),
        Command::Install(args) => install::install(&args),
        Command::Bump(args) => bump::bump(&args),
        Command::Updates(args) => updates::updates(&args),
        Command::Catalog { source } => catalog::catalog(&source),
    };

    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn show(sources: &Sources, package: &str, platform: Option<Platform>) -> Result<(), String> {
    let (graph, target) = evaluate(sources, package, platform)?;
    let package = graph
        .packages
        .get(&target)
        .ok_or_else(|| format!("{target} did not evaluate"))?;

    for (key, derivation) in graph.derivations_of(package) {
        println!("# {key}\n{derivation}");
    }

    Ok(())
}

fn build(
    sources: &Sources,
    package: &str,
    is_verbose: bool,
    should_require_dependencies: bool,
    caches: &cache::Caches,
) -> Result<(), String> {
    let (graph, target) = evaluate(sources, package, None)?;
    let package = graph
        .packages
        .get(&target)
        .ok_or_else(|| format!("{target} did not evaluate"))?;

    setup::ensure()?;
    let root = Path::new(ROOT);
    let logs = root.join("var/log");
    let is_root = rustix::process::geteuid().is_root();
    if is_root {
        for directory in [Path::new(STORE_ROOT), &logs] {
            fs::create_dir_all(directory)
                .map_err(|error| format!("{}: {error}", directory.display()))?;
        }
    }

    let mut store = match is_root {
        true => Store::open(root),
        false => Store::open_read_only(root),
    }
    .map_err(store_error)?;
    let _held = store.hold().map_err(|error| error.to_string())?;

    let caches = caches.open();
    let mut resolver = Resolver {
        derivations: &graph.derivations,
        published: graph
            .packages
            .values()
            .map(|package| &package.build)
            .collect(),
        caches: &caches,
        store: &mut store,
        is_root,
        order: Vec::new(),
        seen: BTreeSet::new(),
    };

    for key in std::iter::once(&package.build).chain(&package.check) {
        resolver.resolve(key)?;
    }

    let Resolver {
        order,
        seen,
        published,
        ..
    } = resolver;
    if should_require_dependencies {
        let missing: Vec<_> = order
            .iter()
            .filter(|key| *key != &package.build && published.contains(key))
            .filter_map(|key| match graph.derivations.get(key) {
                Some(Derivation::Build(build)) => Some(format!("{}@{}", build.name, build.version)),
                _ => None,
            })
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "{target} needs dependencies that aren't in the store or a cache: {}",
                missing.join(", ")
            ));
        }
    }

    let mut references = BTreeMap::new();
    for key in &seen {
        present_references(&store, key, &mut references)?;
    }

    for key in &order {
        // Checks aren't outputs, so they run every time.
        if store
            .path(key)
            .map_err(|error| error.to_string())?
            .is_some()
        {
            let found = store.references(key).map_err(|error| error.to_string())?;
            references.insert(key.clone(), found);
            continue;
        }

        let found = realize(
            &mut store,
            is_root,
            &graph.derivations,
            &references,
            key,
            is_verbose,
        )?;
        references.insert(key.clone(), found);
    }

    let Some(Derivation::Build(build)) = graph.derivations.get(&package.build) else {
        return Err(format!("{target} has no build derivation"));
    };

    let out = output_path(&package.build, &build.name, &build.version, "out");
    println!("{}", out.display());
    Ok(())
}

struct Resolver<'a> {
    derivations: &'a BTreeMap<Key, Derivation>,
    published: BTreeSet<&'a Key>,
    caches: &'a [Cache],
    store: &'a mut Store,
    is_root: bool,
    order: Vec<Key>,
    seen: BTreeSet<Key>,
}

impl Resolver<'_> {
    fn resolve(&mut self, key: &Key) -> Result<(), String> {
        if !self.seen.insert(key.clone()) {
            return Ok(());
        }

        if self
            .store
            .path(key)
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Ok(());
        }

        let derivations = self.derivations;
        let (named, dependencies) = match derivations.get(key) {
            Some(Derivation::Build(build)) => {
                let cache = match self.published.contains(key) {
                    true => cache::find(self.caches, &build.name, key)?,
                    false => None,
                };

                if let Some(cache) = cache {
                    return cache::install_into(
                        cache,
                        self.store,
                        self.is_root,
                        &build.name,
                        key,
                        None,
                    );
                }

                (
                    build.inputs.values().collect(),
                    build.dependencies.as_slice(),
                )
            }
            Some(Derivation::Check(check)) => (vec![&check.target], check.dependencies.as_slice()),
            Some(Derivation::Unpack(unpack)) => (vec![&unpack.source], [].as_slice()),
            Some(Derivation::Fetch(_)) | None => (Vec::new(), [].as_slice()),
        };

        // Runtime dependencies aren't visible to the build.
        let dependencies = dependencies
            .iter()
            .filter(|dependency| dependency.kind != DependencyKind::Runtime)
            .map(|dependency| &dependency.key);

        for named in named.into_iter().chain(dependencies) {
            self.resolve(named)?;
        }

        self.order.push(key.clone());
        Ok(())
    }
}

fn evaluate(
    sources: &Sources,
    package: &str,
    platform: Option<Platform>,
) -> Result<(Graph, Target), String> {
    let (catalog, hosts) = load(sources)?;
    let platform = match platform {
        Some(platform) => platform,
        None => this_platform().map_err(|error| format!("{error}; pass --platform"))?,
    };

    let target = target(&catalog, package, platform)?;
    let graph = catalog
        .evaluate(&hosts, std::slice::from_ref(&target))
        .map_err(|error| error.to_string())?;

    Ok((graph, target))
}

fn keys(sources: &Sources) -> Result<(), String> {
    let (catalog, hosts) = load(sources)?;
    let targets = catalog.targets();
    let graph = catalog
        .evaluate(&hosts, &targets)
        .map_err(|error| error.to_string())?;

    let mut out = std::io::stdout().lock();
    for target in targets {
        let package = graph
            .packages
            .get(&target)
            .ok_or_else(|| format!("{target} did not evaluate"))?;
        let Some(Derivation::Build(build)) = graph.derivations.get(&package.build) else {
            return Err(format!("{target} has no build derivation"));
        };

        let line = Line {
            name: target.name,
            version: target.version,
            platform: target.platform,
            key: package.build.clone(),
            check: package.check.clone(),
            build: build.clone(),
        };
        let json = serde_json::to_string(&line).map_err(|error| error.to_string())?;
        writeln!(out, "{json}").map_err(|error| error.to_string())?;
    }

    Ok(())
}

fn diff(base: &Path, head: &Path) -> Result<(), String> {
    let base = read_lines(base)?;
    let head = read_lines(head)?;

    for (id, line) in &head {
        let label = format!("{}@{} {}", line.name, line.version, line.platform);
        let Some(old) = base.get(id) else {
            println!("added {label}");
            continue;
        };

        // A build change always rekeys the check, so `check` is named only on its own.
        let reasons = if old.key != line.key {
            reasons(&old.build, &line.build)
        } else if old.check != line.check {
            vec!["check".to_string()]
        } else {
            continue;
        };

        // Identical builds under new keys mean the key encoding itself changed.
        if reasons.is_empty() {
            println!("changed {label}");
        } else {
            println!("changed {label}: {}", reasons.join(", "));
        }
    }

    for (id, line) in &base {
        if !head.contains_key(id) {
            println!("removed {}@{} {}", line.name, line.version, line.platform);
        }
    }

    Ok(())
}

type LineId = (String, String, Platform);

fn read_lines(path: &Path) -> Result<BTreeMap<LineId, Line>, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;

    let mut lines = BTreeMap::new();
    for (number, text) in text
        .lines()
        .enumerate()
        .filter(|(_, text)| !text.is_empty())
    {
        let at = format!("{}:{}", path.display(), number.saturating_add(1));
        let line: Line = serde_json::from_str(text).map_err(|error| format!("{at}: {error}"))?;

        let id = (line.name.clone(), line.version.clone(), line.platform);
        if lines.insert(id, line).is_some() {
            return Err(format!("{at}: duplicate name, version and platform"));
        }
    }

    Ok(lines)
}

/// The fields that differ, naming map entries (`env.CFLAGS`) and dependencies
/// (`dependencies.zlib`) so a cascade reads as the dependency that caused it.
fn reasons(old: &Build, new: &Build) -> Vec<String> {
    let (Ok(Value::Object(old)), Ok(Value::Object(new))) =
        (serde_json::to_value(old), serde_json::to_value(new))
    else {
        return vec!["build".to_string()];
    };

    let fields = old.keys().chain(new.keys()).collect::<BTreeSet<_>>();
    fields
        .into_iter()
        .filter(|field| old.get(*field) != new.get(*field))
        .flat_map(
            |field| match (entries(old.get(field)), entries(new.get(field))) {
                (Some(old), Some(new)) => old
                    .symmetric_difference(&new)
                    .map(|(name, _)| format!("{field}.{name}"))
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                _ => vec![field.to_string()],
            },
        )
        .collect()
}

/// A map or a list of named entries as `(name, entry)` pairs. Omitted fields are
/// empty, so a first dependency or env var is still named.
fn entries(value: Option<&Value>) -> Option<BTreeSet<(String, String)>> {
    match value {
        None => Some(BTreeSet::new()),
        Some(Value::Object(map)) => Some(
            map.iter()
                .map(|(name, entry)| (name.clone(), entry.to_string()))
                .collect(),
        ),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| Some((item.get("name")?.as_str()?.to_string(), item.to_string())))
            .collect(),
        Some(_) => None,
    }
}

const LOCATE: &str = "Run from a catalog checkout or pass --catalog and --host";

fn load(sources: &Sources) -> Result<(Catalog, BTreeMap<Platform, Host>), String> {
    load_from(&sources.catalog, &sources.host).map_err(|error| format!("{error}. {LOCATE}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reasons_name_the_entries_that_changed() {
        let build = |fields: Value| -> Build {
            let mut build = json!({
                "name": "curl", "version": "8", "platform": "x86_64-linux", "sandbox": "linux-v1",
                "dependencies": [{ "key": "c".repeat(32), "name": "cmake", "kind": "build" }],
                "env": { "CFLAGS": "-O2", "RB_SYSTEM": "a" },
                "script": "make", "outputs": ["out"],
            });
            if let (Value::Object(build), Value::Object(fields)) = (&mut build, fields) {
                build.extend(fields);
            }
            serde_json::from_value(build).unwrap()
        };

        let old = build(json!({}));
        let new = build(json!({
            "inputs": { "source": "s".repeat(32) },
            "dependencies": [
                { "key": "c".repeat(32), "name": "cmake", "kind": "build" },
                { "key": "z".repeat(32), "name": "zlib", "kind": "linked" },
            ],
            "env": { "CFLAGS": "-O3", "RB_SYSTEM": "a" },
            "script": "make -j",
        }));
        assert_eq!(
            reasons(&old, &new),
            ["dependencies.zlib", "env.CFLAGS", "inputs.source", "script"]
        );
    }
}
