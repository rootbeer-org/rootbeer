mod cache;
mod plan;

use rootbeer_cache::Cache;
use rootbeer_drv::{
    fetch_path, output_path, Build, DependencyKind, Derivation, Key, Platform, STORE_ROOT,
};
use rootbeer_eval::{Catalog, Graph, Host, Target};
use rootbeer_sandbox::Request;
use rootbeer_store::{Store, ROOT};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Write};
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
    /// Push a package's built output and its runtime references to a registry
    Push {
        /// `name` or `name@version`; defaults to the platform's default version
        package: String,
        #[command(flatten)]
        registry: cache::Registry,
        #[command(flatten)]
        sources: Sources,
    },
    /// Install an output by key from a registry, after what it references
    Install {
        /// The package name the output was pushed under
        name: String,
        key: Key,
        #[command(flatten)]
        registry: cache::Registry,
    },
    /// Print, as JSON levels, the packages CI must build because no cache has them
    Plan {
        /// Each `name` or `name@version`; versions default to the platform's
        #[arg(required = true)]
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
            caches,
            sources,
        } => build(&sources, &package, is_verbose, &caches),
        Command::Push {
            package,
            registry,
            sources,
        } => cache::push(&sources, &package, &registry),
        Command::Install {
            name,
            key,
            registry,
        } => cache::install(&name, &key, &registry),
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

const HELPER: &str = "/opt/rb/libexec/rb-helper";

fn store_error(error: rootbeer_store::Error) -> String {
    format!(
        "{error} (set the store up with `sudo rb-helper setup <group>`, or after a \
         crash let any `rb-helper` command recover it)"
    )
}

fn build(
    sources: &Sources,
    package: &str,
    is_verbose: bool,
    caches: &cache::Caches,
) -> Result<(), String> {
    let (graph, target) = evaluate(sources, package, None)?;
    let package = graph
        .packages
        .get(&target)
        .ok_or_else(|| format!("{target} did not evaluate"))?;

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

    let Resolver { order, seen, .. } = resolver;
    let mut references = BTreeMap::new();
    for key in &seen {
        present_references(&store, key, &mut references)?;
    }

    let jobs = std::thread::available_parallelism().map_err(|error| error.to_string())?;

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

        // var/log is shared, so the name may be another user's link to a file
        let log = logs.join(key.as_str());
        let file = match fs::remove_file(&log) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => File::options().write(true).create_new(true).open(&log),
        };

        let mut output = Output {
            log: file.map_err(|error| format!("{}: {error}", log.display()))?,
            terminal: is_verbose.then(io::stderr),
        };

        let mut request = Request {
            key,
            graph: &graph.derivations,
            references: &references,
            jobs,
            log: &mut output,
        };

        let derivation = graph.derivations.get(key);
        eprintln!("realizing {key} {}", label(derivation));
        let found = rootbeer_sandbox::realize(&mut request)
            .map_err(|error| format!("{error}\nlog: {}", log.display()))?;

        let path = match derivation {
            Some(Derivation::Fetch(_)) => fetch_path(key),
            Some(Derivation::Build(build)) => output_path(key, &build.name, &build.version, "out"),
            Some(Derivation::Check(_)) | None => continue,
        };

        let entry = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("{} has no entry name", path.display()))?;

        seal(&mut store, is_root, key, entry, &found)?;
        references.insert(key.clone(), found);
    }

    let Some(Derivation::Build(build)) = graph.derivations.get(&package.build) else {
        return Err(format!("{target} has no build derivation"));
    };

    let out = output_path(&package.build, &build.name, &build.version, "out");
    println!("{}", out.display());
    Ok(())
}

/// Registers a new output either as root or with the setuid helper.
fn seal(
    store: &mut Store,
    is_root: bool,
    key: &Key,
    entry: &str,
    references: &BTreeSet<Key>,
) -> Result<(), String> {
    if is_root {
        let builder = rustix::process::getuid().as_raw();
        store
            .seal(key, entry, builder, references)
            .map_err(|error| error.to_string())?;
        return Ok(());
    }

    let status = std::process::Command::new(HELPER)
        .args(["seal", key.as_str(), entry])
        .args(references.iter().map(Key::as_str))
        .stdout(std::process::Stdio::null())
        .status()
        .map_err(|error| format!("{HELPER}: {error}"))?;

    if !status.success() {
        return Err(format!("{HELPER} seal {key} {status}"));
    }

    Ok(())
}

/// Build output, logged and also shown with `--verbose`.
struct Output {
    log: File,
    terminal: Option<io::Stderr>,
}

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.log.write_all(bytes)?;
        if let Some(terminal) = &mut self.terminal {
            terminal.write_all(bytes)?;
        }

        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.log.flush()
    }
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
                    return cache::install_into(cache, self.store, self.is_root, &build.name, key);
                }

                (
                    build.inputs.values().collect(),
                    build.dependencies.as_slice(),
                )
            }
            Some(Derivation::Check(check)) => (vec![&check.target], check.dependencies.as_slice()),
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

fn present_references(
    store: &Store,
    key: &Key,
    references: &mut BTreeMap<Key, BTreeSet<Key>>,
) -> Result<(), String> {
    if references.contains_key(key)
        || store
            .path(key)
            .map_err(|error| error.to_string())?
            .is_none()
    {
        return Ok(());
    }

    let found = store.references(key).map_err(|error| error.to_string())?;
    for reference in &found {
        present_references(store, reference, references)?;
    }

    references.insert(key.clone(), found);
    Ok(())
}

fn label(derivation: Option<&Derivation>) -> String {
    match derivation {
        Some(Derivation::Build(build)) => format!("{}@{}", build.name, build.version),
        Some(Derivation::Check(check)) => format!("check {}", check.name),
        Some(Derivation::Fetch(_)) | None => "fetch".to_string(),
    }
}

/// `name` or `name@version` on `platform`, defaulting to its default version.
fn target(catalog: &Catalog, package: &str, platform: Platform) -> Result<Target, String> {
    let (name, version) = match package.split_once('@') {
        Some((name, version)) => (name, version),
        None => {
            let version = catalog.default_version(package, platform).ok_or_else(|| {
                format!("{package} is not in the catalog or has no default version on {platform}")
            })?;
            (package, version)
        }
    };

    Ok(Target {
        name: name.to_string(),
        version: version.to_string(),
        platform,
    })
}

fn evaluate(
    sources: &Sources,
    package: &str,
    platform: Option<Platform>,
) -> Result<(Graph, Target), String> {
    let (catalog, hosts) = load(sources)?;
    let platform = match platform {
        Some(platform) => platform,
        None => parse_platform(&format!(
            "{}-{}",
            std::env::consts::ARCH,
            std::env::consts::OS
        ))
        .map_err(|error| format!("{error}; pass --platform"))?,
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

const LOCATE: &str = "run from a catalog checkout or pass --catalog and --host";

fn load(sources: &Sources) -> Result<(Catalog, BTreeMap<Platform, Host>), String> {
    let read = |path: &Path| {
        fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))
    };

    let mut paths = fs::read_dir(&sources.catalog)
        .and_then(|entries| {
            entries
                .map(|entry| Ok(entry?.path()))
                .collect::<std::io::Result<Vec<_>>>()
        })
        .map_err(|error| format!("{}: {error}; {LOCATE}", sources.catalog.display()))?;
    paths.retain(|path| path.extension().is_some_and(|extension| extension == "lua"));
    paths.sort();

    let files = paths
        .iter()
        .map(|path| Ok((path.display().to_string(), read(path)?)))
        .collect::<Result<Vec<_>, String>>()?;
    let catalog = Catalog::parse(
        files
            .iter()
            .map(|(file, text)| (file.as_str(), text.as_str())),
    )
    .map_err(|error| error.to_string())?;

    let host = read(&sources.host).map_err(|error| format!("{error}; {LOCATE}"))?;
    let hosts = Host::parse(&sources.host.display().to_string(), &host)
        .map_err(|error| error.to_string())?;

    Ok((catalog, hosts))
}

fn parse_platform(name: &str) -> Result<Platform, String> {
    Platform::try_from(name.to_string()).map_err(|error| error.to_string())
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
