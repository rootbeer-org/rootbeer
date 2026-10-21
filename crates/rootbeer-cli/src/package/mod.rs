pub(crate) mod cache;
pub(crate) mod github;
pub(crate) mod install;
pub(crate) mod local;
pub(crate) mod setup;

use rootbeer_drv::{fetch_path, output_path, Derivation, Key, Platform};
use rootbeer_eval::{Catalog, Host, Target};
use rootbeer_sandbox::Request;
use rootbeer_store::{Store, ROOT};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

pub(crate) const HELPER: &str = "/opt/rb/libexec/rb-helper";

pub(crate) fn store_error(error: rootbeer_store::Error) -> String {
    format!(
        "{error} (set the store up with `sudo rb-helper setup <group>`, or after a \
         crash let any `rb-helper` command recover it)"
    )
}

/// Realizes a derivation whose inputs the store has, logging to var/log, and
/// seals what it made. Returns the output's references.
pub(crate) fn realize(
    store: &mut Store,
    is_root: bool,
    graph: &BTreeMap<Key, Derivation>,
    references: &BTreeMap<Key, BTreeSet<Key>>,
    key: &Key,
    is_verbose: bool,
) -> Result<BTreeSet<Key>, String> {
    let jobs = std::thread::available_parallelism().map_err(|error| error.to_string())?;

    // var/log is shared, so the name may be another user's link to a file
    let log = Path::new(ROOT).join("var/log").join(key.as_str());
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
        graph,
        references,
        jobs,
        log: &mut output,
    };

    let derivation = graph.get(key);
    eprintln!("realizing {key} {}", label(derivation));
    let found = rootbeer_sandbox::realize(&mut request)
        .map_err(|error| format!("{error}\nlog: {}", log.display()))?;

    let path = match derivation {
        Some(Derivation::Fetch(_)) => fetch_path(key),
        Some(Derivation::Build(build)) => output_path(key, &build.name, &build.version, "out"),
        Some(Derivation::Unpack(unpack)) => output_path(key, &unpack.name, &unpack.version, "out"),
        Some(Derivation::Check(_)) | None => return Ok(found),
    };

    let entry = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{} has no entry name", path.display()))?;

    seal(store, is_root, key, entry, &found)?;
    Ok(found)
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

pub(crate) fn present_references(
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
        Some(Derivation::Unpack(unpack)) => format!("{}@{}", unpack.name, unpack.version),
        Some(Derivation::Fetch(_)) | None => "fetch".to_string(),
    }
}

/// `name` or `name@version` on `platform`, defaulting to its default version.
pub(crate) fn target(
    catalog: &Catalog,
    package: &str,
    platform: Platform,
) -> Result<Target, String> {
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

pub(crate) fn load_from(
    directory: &Path,
    host: &Path,
) -> Result<(Catalog, BTreeMap<Platform, Host>), String> {
    let read = |path: &Path| {
        fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))
    };

    let mut paths = fs::read_dir(directory)
        .and_then(|entries| {
            entries
                .map(|entry| Ok(entry?.path()))
                .collect::<std::io::Result<Vec<_>>>()
        })
        .map_err(|error| format!("{}: {error}", directory.display()))?;
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

    let hosts = read(host)?;
    let hosts =
        Host::parse(&host.display().to_string(), &hosts).map_err(|error| error.to_string())?;

    Ok((catalog, hosts))
}

pub(crate) fn this_platform() -> Result<Platform, String> {
    parse_platform(&format!(
        "{}-{}",
        std::env::consts::ARCH,
        std::env::consts::OS
    ))
}

pub(crate) fn parse_platform(name: &str) -> Result<Platform, String> {
    Platform::try_from(name.to_string()).map_err(|error| error.to_string())
}
