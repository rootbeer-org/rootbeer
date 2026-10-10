use super::cache::{self, Signed};
use super::install::{self, Source};
use super::{this_platform, HELPER};
use rootbeer_profile::{Generation, Member, Profile};
use rootbeer_store::{Store, ROOT};
use rustix::process::getuid;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const USER: &str = "user";

/// Later ones take precedence on PATH.
const ON_PATH: [&str; 2] = ["default", USER];

#[derive(clap::Args, Debug)]
pub(super) struct UseArgs {
    /// Each `name` or `name@version`. A version pins the package, and
    /// otherwise it follows the platform's default
    #[arg(required = true)]
    packages: Vec<String>,
    #[command(flatten)]
    source: Source,
}

pub(super) fn add(args: &UseArgs) -> Result<(), String> {
    let mut requests = BTreeMap::new();
    for request in &args.packages {
        let (name, version) = install::request(request);
        let asked = requests.insert(name, version);
        if asked.is_some_and(|asked| asked != version) {
            return Err(format!("{name} is asked for at two versions"));
        }
    }

    let directory = directory(true)?;
    let profile = Profile::open(&directory, USER).map_err(profile_error)?;
    let mut generation = current(&profile)?;
    requests.retain(|name, version| {
        let member = generation.packages.get(*name);
        !member.is_some_and(|member| match version {
            Some(version) => member.is_pinned && member.version == *version,
            None => !member.is_pinned,
        })
    });

    if requests.is_empty() {
        eprintln!("{USER} already has everything asked for");
        return link_apps(&profile);
    }

    let (mut store, is_root) = install::open_store()?;
    let index = install::open_index(&args.source, is_root)?;
    let mut signed = Signed::new(|name| index.package(name).map_err(|error| error.to_string()));
    let cache = args.source.cache();
    let platform = this_platform()?;

    let mut members = Vec::new();
    for (name, version) in requests {
        let (resolved, output) = install::select(signed.package(name)?, version, platform)?;
        let member = Member {
            version: resolved,
            is_pinned: version.is_some(),
            key: output.key.clone(),
            manifest: output.manifest.clone(),
            bins: output.bins.clone(),
            apps: output.apps.clone(),
            extra: BTreeMap::new(),
        };

        members.push((name, member));
    }

    if let Some(applications) = applications() {
        let apps = members.iter().flat_map(|(_, member)| member.apps.keys());
        profile
            .check_apps(&applications, apps.map(String::as_str))
            .map_err(profile_error)?;
    }

    for (name, member) in members {
        cache::install_into(
            &cache,
            &mut store,
            is_root,
            name,
            &member.key,
            Some(&mut signed),
        )?;
        generation.packages.insert(name.to_string(), member);
    }

    switch(&profile, &store, generation)
}

pub(super) fn remove(names: &[String]) -> Result<(), String> {
    let directory = directory(false)?;
    let profile = Profile::open(&directory, USER).map_err(profile_error)?;
    let mut generation = current(&profile)?;
    let names = names.iter().collect::<BTreeSet<_>>();
    for name in names {
        if generation.packages.remove(name).is_none() {
            return Err(format!("{name} isn't in {USER}"));
        }
    }

    let (store, _) = install::open_store()?;
    switch(&profile, &store, generation)
}

pub(super) fn rollback() -> Result<(), String> {
    let directory = directory(false)?;
    let profile = Profile::open(&directory, USER).map_err(profile_error)?;
    let number = profile.rollback().map_err(profile_error)?;
    eprintln!("{USER} is now generation {number}");
    link_apps(&profile)
}

pub(super) fn generations() -> Result<(), String> {
    let directory = directory(false)?;
    let profile = Profile::open_shared(&directory, USER).map_err(profile_error)?;
    let current = profile.current_number().map_err(profile_error)?;
    for number in profile.generations().map_err(profile_error)? {
        let marker = if Some(number) == current { '*' } else { ' ' };
        let contents = match profile.read(number) {
            Ok(generation) => generation
                .packages
                .iter()
                .map(|(name, member)| format!("{name}@{}", member.version))
                .collect::<Vec<_>>()
                .join(" "),
            Err(error) => format!("unreadable, {}", profile_error(error)),
        };

        println!("{marker} {number:>4}  {contents}");
    }

    Ok(())
}

pub(super) fn env() -> Result<(), String> {
    let directory = profiles();
    for name in ON_PATH {
        let bin = rootbeer_profile::bin(&directory, name);
        let bin = bin
            .to_str()
            .ok_or_else(|| format!("{} isn't UTF-8", bin.display()))?;

        println!("case \":$PATH:\" in *:{bin}:*) ;; *) export PATH=\"{bin}:$PATH\" ;; esac");
    }

    Ok(())
}

fn switch(profile: &Profile, store: &Store, generation: Generation) -> Result<(), String> {
    let locate = |key: &_| {
        store
            .path(key)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("{key} isn't in the store"))
    };

    let number = profile.create(generation, locate).map_err(profile_error)?;
    eprintln!("{USER} is now generation {number}");
    link_apps(profile)
}

fn link_apps(profile: &Profile) -> Result<(), String> {
    let Some(applications) = applications() else {
        return Ok(());
    };

    for problem in profile.link_apps(&applications).map_err(profile_error)? {
        eprintln!("warning: {problem}");
    }

    Ok(())
}

/// None under sudo with HOME kept, so root never writes into a user's home.
fn applications() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }

    let home = PathBuf::from(std::env::var_os("HOME")?);
    let is_own = home.is_absolute()
        && fs::metadata(&home).is_ok_and(|metadata| metadata.uid() == getuid().as_raw());
    is_own.then(|| home.join("Applications"))
}

fn current(profile: &Profile) -> Result<Generation, String> {
    let current = profile.current().map_err(profile_error)?;
    Ok(current.unwrap_or_default())
}

fn directory(should_create: bool) -> Result<PathBuf, String> {
    let directory = profiles();
    let uid = getuid();
    let at = |error: io::Error| format!("{}: {error}", directory.display());

    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.is_dir() && metadata.uid() == uid.as_raw() => {}
        Ok(_) => return Err(format!("{} isn't your directory", directory.display())),
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(at(error)),
        Err(_) if !should_create => return Err(format!("{USER} has no generations")),
        Err(_) if uid.is_root() => {
            let parent = Path::new(ROOT).join("profiles");
            fs::create_dir_all(&parent)
                .map_err(|error| format!("{}: {error}", parent.display()))?;
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&directory)
                .map_err(at)?;
        }
        Err(_) => {
            let status = Command::new(HELPER)
                .arg("profile")
                .stdout(Stdio::null())
                .status()
                .map_err(|error| format!("{HELPER}: {error}"))?;
            if !status.success() {
                return Err(format!("{HELPER} profile {status}"));
            }
        }
    }

    Ok(directory)
}

fn profiles() -> PathBuf {
    Path::new(ROOT)
        .join("profiles")
        .join(getuid().as_raw().to_string())
}

fn profile_error(error: rootbeer_profile::Error) -> String {
    let reason = error.to_string();
    if let rootbeer_profile::Error::Newer { .. } = error {
        crate::newer::relaunch(&reason);
    }

    reason
}
