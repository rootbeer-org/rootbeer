use super::cache::{self, Signed};
use super::github;
use super::install::{self, Source, Wanted};
use super::profile::{directory, profile_error};
use super::this_platform;
use rootbeer_drv::Key;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

#[derive(clap::Args, Debug)]
pub(crate) struct Args {
    /// `name[@version]`, defaulting to the platform's default version, or
    /// `github:owner/repo[@tag]` for a release's asset
    package: String,
    /// The command to run, when the package has several
    #[arg(long)]
    bin: Option<String>,
    /// Open one of the package's macOS apps, such as `Ghostty.app`, instead
    #[arg(long, conflicts_with = "bin")]
    app: Option<String>,
    /// Add another package's commands to the command's PATH
    #[arg(short = 'p', long = "package")]
    packages: Vec<String>,
    /// Arguments passed unchanged to the command, after --
    #[arg(last = true)]
    args: Vec<OsString>,
}

struct Installed {
    name: String,
    key: Key,
    path: PathBuf,
    bins: BTreeSet<String>,
    apps: BTreeMap<String, String>,
}

/// Installs the packages without a profile and runs the command, which keeps
/// them from GC until it exits.
pub(crate) fn run(args: Args) -> Result<(), String> {
    if args.app.is_some() && !cfg!(target_os = "macos") {
        return Err("--app works only on macOS".into());
    }

    let (mut store, is_root) = install::open_store()?;
    let _held = store.hold().map_err(|error| error.to_string())?;
    let source = Source::default();
    let mut index = None;
    let mut signed = Signed::new(|name| {
        let index = match &mut index {
            Some(index) => index,
            None => index.insert(install::open_index(&source, is_root)?),
        };
        index.package(name).map_err(|error| error.to_string())
    });
    let cache = source.cache();
    let platform = this_platform()?;

    let mut installed = Vec::new();
    for request in std::iter::once(&args.package).chain(&args.packages) {
        let (name, wanted) = Wanted::parse(request)?;
        let (key, bins, apps) = match &wanted {
            Wanted::Release(request) => {
                let resolved =
                    github::resolve(&mut store, is_root, &name, request, platform, None)?;
                let bins = resolved.release.paths.into_keys().collect();
                (resolved.key, bins, BTreeMap::new())
            }
            Wanted::Local { .. } => return Err(format!("{name} isn't runnable from a catalog")),
            Wanted::Index(version) => {
                let package = signed.package(&name)?;
                let (_, output) = install::select(package, version.as_deref(), platform)?;
                let (key, bins, apps) =
                    (output.key.clone(), output.bins.clone(), output.apps.clone());
                cache::install_into(&cache, &mut store, is_root, &name, &key, Some(&mut signed))?;
                (key, bins, apps)
            }
        };

        let path = store
            .path(&key)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("{key} isn't present after installing"))?;

        installed.push(Installed {
            name,
            key,
            path,
            bins,
            apps,
        });
    }

    let path = search_path(&installed)?;
    let Some(package) = installed.first() else {
        return Err("nothing to run".into());
    };

    let mut command = match &args.app {
        Some(app) => {
            let relative = package
                .apps
                .get(app)
                .ok_or_else(|| exports(&package.name, "apps", package.apps.keys()))?;

            // Waits for the app to quit, so the process holding its keys
            // lives as long as the app does.
            let mut command = Command::new("/usr/bin/open");
            command.arg("-W").arg(package.path.join(relative));
            if !args.args.is_empty() {
                command.arg("--args");
            }

            command
        }
        None => Command::new(
            package
                .path
                .join("bin")
                .join(bin(package, args.bin.as_deref())?),
        ),
    };

    let keys = installed
        .iter()
        .map(|package| package.key.clone())
        .collect();
    rootbeer_profile::hold_while_running(&directory(true)?, &keys).map_err(profile_error)?;
    let error = command.args(args.args).env("PATH", path).exec();
    Err(format!(
        "running {}: {error}",
        command.get_program().display()
    ))
}

fn search_path(installed: &[Installed]) -> Result<OsString, String> {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let bins = installed.iter().map(|package| package.path.join("bin"));
    std::env::join_paths(bins.chain(std::env::split_paths(&existing)))
        .map_err(|error| error.to_string())
}

fn bin<'a>(package: &'a Installed, asked: Option<&str>) -> Result<&'a str, String> {
    let named = match asked {
        Some(asked) => package.bins.get(asked),
        None => package.bins.get(&package.name),
    };

    let only = (asked.is_none() && package.bins.len() == 1)
        .then(|| package.bins.first())
        .flatten();

    named
        .or(only)
        .map(String::as_str)
        .ok_or_else(|| exports(&package.name, "commands", &package.bins))
}

fn exports<'a>(name: &str, kind: &str, names: impl IntoIterator<Item = &'a String>) -> String {
    let names: Vec<_> = names.into_iter().map(String::as_str).collect();
    match names.is_empty() {
        true => format!("{name} has no {kind}"),
        false => format!(
            "{name} has these {kind}, so choose one: {}",
            names.join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(bins: &[&str]) -> Installed {
        Installed {
            name: "ripgrep".into(),
            key: "a".repeat(32).parse().unwrap(),
            path: "/opt/rb/store/x".into(),
            bins: bins.iter().map(|bin| bin.to_string()).collect(),
            apps: BTreeMap::new(),
        }
    }

    #[test]
    fn a_command_is_asked_for_named_after_the_package_or_the_only_one() {
        assert_eq!(bin(&package(&["rg"]), None), Ok("rg"));
        assert_eq!(bin(&package(&["rg", "ripgrep"]), None), Ok("ripgrep"));
        assert_eq!(bin(&package(&["a", "rg"]), Some("a")), Ok("a"));
        assert_eq!(
            bin(&package(&["a", "rg"]), None),
            Err("ripgrep has these commands, so choose one: a, rg".into())
        );
        assert_eq!(
            bin(&package(&["rg"]), Some("x")),
            Err("ripgrep has these commands, so choose one: rg".into())
        );
        assert_eq!(
            bin(&package(&[]), None),
            Err("ripgrep has no commands".into())
        );
    }
}
