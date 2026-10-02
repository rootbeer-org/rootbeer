use rootbeer_drv::{output_path, Build, DependencyKind, Derivation, Key, Platform};
use rootbeer_eval::{Catalog, Graph, Host, Target};
use rootbeer_sandbox::Request;
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
    /// Fetch, build, and check a package and its dependencies on this machine
    Build {
        /// `name` or `name@version`; defaults to the platform's default version
        package: String,
        /// Show build output as it happens, as well as logging it
        #[arg(long = "verbose")]
        is_verbose: bool,
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
        Command::Build {
            package,
            is_verbose,
            sources,
        } => build(&sources, &package, is_verbose),
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

// Stand-in for rootbeer-store until phase 3: a marker per realized key, and logs.
const VAR: &str = "/opt/rb/var";

fn build(sources: &Sources, package: &str, is_verbose: bool) -> Result<(), String> {
    let (graph, target) = evaluate(sources, package, None)?;
    let package = graph
        .packages
        .get(&target)
        .ok_or_else(|| format!("{target} did not evaluate"))?;

    let mut order = Vec::new();
    for key in std::iter::once(&package.build).chain(&package.check) {
        dependencies_first(&graph.derivations, key, &mut order);
    }

    let realized = Path::new(VAR).join("realized");
    let logs = Path::new(VAR).join("log");
    for directory in [&realized, &logs] {
        fs::create_dir_all(directory)
            .map_err(|error| format!("{}: {error}", directory.display()))?;
    }

    let jobs = std::thread::available_parallelism().map_err(|error| error.to_string())?;
    let mut references = BTreeMap::new();

    for key in &order {
        let marker = realized.join(key.as_str());
        if marker.exists() {
            let text = fs::read_to_string(&marker)
                .map_err(|error| format!("{}: {error}", marker.display()))?;

            let keys = text
                .lines()
                .map(str::parse)
                .collect::<Result<BTreeSet<Key>, _>>()
                .map_err(|error| format!("{}: {error}", marker.display()))?;

            references.insert(key.clone(), keys);
            continue;
        }

        let log = logs.join(key.as_str());
        let mut output = Output {
            log: File::create(&log).map_err(|error| format!("{}: {error}", log.display()))?,
            terminal: is_verbose.then(io::stderr),
        };

        let mut request = Request {
            key,
            graph: &graph.derivations,
            references: &references,
            jobs,
            log: &mut output,
        };

        eprintln!("realizing {key} {}", label(graph.derivations.get(key)));
        let found = rootbeer_sandbox::realize(&mut request)
            .map_err(|error| format!("{error}\nlog: {}", log.display()))?;

        let text = found
            .iter()
            .map(|key| format!("{key}\n"))
            .collect::<String>();

        let staged = marker.with_extension("tmp");
        File::create(&staged)
            .and_then(|mut file| {
                file.write_all(text.as_bytes())?;
                file.sync_all()
            })
            .and_then(|()| fs::rename(&staged, &marker))
            .map_err(|error| format!("{}: {error}", marker.display()))?;

        references.insert(key.clone(), found);
    }

    let Some(Derivation::Build(build)) = graph.derivations.get(&package.build) else {
        return Err(format!("{target} has no build derivation"));
    };

    let out = output_path(&package.build, &build.name, &build.version, "out");
    println!("{}", out.display());
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

fn dependencies_first(derivations: &BTreeMap<Key, Derivation>, key: &Key, order: &mut Vec<Key>) {
    if order.contains(key) {
        return;
    }

    let (named, dependencies) = match derivations.get(key) {
        Some(Derivation::Build(build)) => (
            build.inputs.values().collect(),
            build.dependencies.as_slice(),
        ),
        Some(Derivation::Check(check)) => (vec![&check.target], check.dependencies.as_slice()),
        Some(Derivation::Fetch(_)) | None => (Vec::new(), [].as_slice()),
    };

    // Runtime dependencies aren't visible to the build.
    let dependencies = dependencies
        .iter()
        .filter(|dependency| dependency.kind != DependencyKind::Runtime)
        .map(|dependency| &dependency.key);

    for named in named.into_iter().chain(dependencies) {
        dependencies_first(derivations, named, order);
    }

    order.push(key.clone());
}

fn label(derivation: Option<&Derivation>) -> String {
    match derivation {
        Some(Derivation::Build(build)) => format!("{}@{}", build.name, build.version),
        Some(Derivation::Check(check)) => format!("check {}", check.name),
        Some(Derivation::Fetch(_)) | None => "fetch".to_string(),
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
        None => parse_platform(&format!(
            "{}-{}",
            std::env::consts::ARCH,
            std::env::consts::OS
        ))
        .map_err(|error| format!("{error}; pass --platform"))?,
    };

    let (name, version) = match package.split_once('@') {
        Some((name, version)) => (name, version),
        None => {
            let version = catalog.default_version(package, platform).ok_or_else(|| {
                format!("{package} is not in the catalog or has no default version on {platform}")
            })?;
            (package, version)
        }
    };

    let target = Target {
        name: name.to_string(),
        version: version.to_string(),
        platform,
    };
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
