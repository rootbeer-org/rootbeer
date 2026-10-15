use super::cache::{self, Signed};
use super::install::{self, Source};
use super::{migrate, this_platform, HELPER};
use rootbeer_profile::{Generation, Member, Profile};
use rootbeer_store::{Store, ROOT};
use rustix::process::getuid;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(super) const USER: &str = "user";

/// Later ones take precedence on PATH.
pub(super) const ON_PATH: [&str; 2] = ["default", USER];

#[derive(clap::Args, Debug)]
pub(super) struct UseArgs {
    /// Each `name` or `name@version`. A version pins the package, and
    /// otherwise it follows the platform's default
    #[arg(required_unless_present = "should_update")]
    packages: Vec<String>,
    /// Move these packages, or every package, to what the index has now
    #[arg(long = "update")]
    should_update: bool,
    #[command(flatten)]
    source: Source,
}

pub(super) fn add(args: &UseArgs) -> Result<(), String> {
    let mut requests = BTreeMap::new();
    for request in &args.packages {
        let (name, version) = install::request(request);
        let asked = requests.insert(name.to_string(), version.map(str::to_string));
        if asked.is_some_and(|asked| asked.as_deref() != version) {
            return Err(format!("{name} is asked for at two versions"));
        }
    }

    let directory = directory(true)?;
    let profile = Profile::open(&directory, USER).map_err(profile_error)?;
    let generation = current(&profile)?;
    let requests = planned(&generation.packages, requests, args.should_update);
    if requests.is_empty() {
        eprintln!("{USER} already has everything asked for");
        return link_apps(&profile);
    }

    install_requests(&profile, generation, requests, &args.source, |_, error| {
        Err(error)
    })
}

pub(super) fn install_requests(
    profile: &Profile,
    mut generation: Generation,
    requests: BTreeMap<String, Option<String>>,
    source: &Source,
    mut unmatched: impl FnMut(&str, String) -> Result<(), String>,
) -> Result<(), String> {
    let (mut store, is_root) = install::open_store()?;
    let _held = store.hold().map_err(|error| error.to_string())?;
    let index = install::open_index(source, is_root)?;
    let mut signed = Signed::new(|name| index.package(name).map_err(|error| error.to_string()));
    let cache = source.cache();
    let platform = this_platform()?;

    let mut members = Vec::new();
    for (name, version) in requests {
        let selected = signed
            .package(&name)
            .and_then(|package| install::select(package, version.as_deref(), platform));
        let (resolved, output) = match selected {
            Ok(selected) => selected,
            Err(error) => {
                unmatched(&name, error)?;
                continue;
            }
        };

        let member = Member {
            version: resolved,
            is_pinned: version.is_some(),
            key: output.key.clone(),
            manifest: output.manifest.clone(),
            bins: output.bins.clone(),
            apps: output.apps.clone(),
            extra: BTreeMap::new(),
        };

        let existing = generation.packages.get(&name);
        if !existing.is_some_and(|existing| is_same(existing, &member)) {
            members.push((name, member));
        }
    }

    if members.is_empty() {
        eprintln!("{USER} already has what the index has now");
        return link_apps(profile);
    }

    if let Some(applications) = applications() {
        let apps = members.iter().flat_map(|(_, member)| member.apps.keys());
        check_apps(profile, &applications, apps)?;
    }

    for (name, member) in members {
        cache::install_into(
            &cache,
            &mut store,
            is_root,
            &name,
            &member.key,
            Some(&mut signed),
        )?;
        generation.packages.insert(name, member);
    }

    switch(profile, USER, &store, generation)?;
    link_apps(profile)
}

/// Drops what the generation already has, unless updating. An update keeps a
/// pin when the request names no version, and with nothing named it updates
/// everything.
pub(super) fn planned(
    packages: &BTreeMap<String, Member>,
    mut requests: BTreeMap<String, Option<String>>,
    should_update: bool,
) -> BTreeMap<String, Option<String>> {
    if should_update && requests.is_empty() {
        requests = packages.keys().map(|name| (name.clone(), None)).collect();
    }

    for (name, version) in &mut requests {
        let pinned = packages.get(name).filter(|member| member.is_pinned);
        if let (true, None, Some(member)) = (should_update, &version, pinned) {
            *version = Some(member.version.clone());
        }
    }

    requests.retain(|name, version| {
        let member = packages.get(name);
        should_update
            || !member.is_some_and(|member| match version {
                Some(version) => member.is_pinned && member.version == *version,
                None => !member.is_pinned,
            })
    });

    requests
}

/// Ignores fields a newer rb added, which never change what's linked.
pub(super) fn is_same(a: &Member, b: &Member) -> bool {
    let strip = |member: &Member| Member {
        extra: BTreeMap::new(),
        ..member.clone()
    };

    strip(a) == strip(b)
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
    switch(&profile, USER, &store, generation)?;
    link_apps(&profile)
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

pub(super) fn switch(
    profile: &Profile,
    name: &str,
    store: &Store,
    generation: Generation,
) -> Result<(), String> {
    let locate = |key: &_| {
        store
            .path(key)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("{key} isn't in the store"))
    };

    let number = profile.create(generation, locate).map_err(profile_error)?;
    eprintln!("{name} is now generation {number}");
    Ok(())
}

pub(super) fn check_apps<'a>(
    profile: &Profile,
    applications: &Path,
    apps: impl IntoIterator<Item = &'a String>,
) -> Result<(), String> {
    let legacy = migrate::legacy_apps(applications)?;
    let apps = apps.into_iter().filter(|app| !legacy.contains(*app));
    profile
        .check_apps(applications, apps.map(String::as_str))
        .map_err(profile_error)
}

pub(super) fn link_apps(profile: &Profile) -> Result<(), String> {
    let Some(applications) = applications() else {
        return Ok(());
    };

    let generation = current(profile)?;
    let apps = generation
        .packages
        .values()
        .flat_map(|member| member.apps.keys());
    migrate::release_legacy_apps(&applications, apps)?;

    for problem in profile.link_apps(&applications).map_err(profile_error)? {
        eprintln!("warning: {problem}");
    }

    Ok(())
}

/// None under sudo with HOME kept, so root never writes into a user's home.
pub(super) fn applications() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }

    let home = PathBuf::from(std::env::var_os("HOME")?);
    let is_own = home.is_absolute()
        && fs::metadata(&home).is_ok_and(|metadata| metadata.uid() == getuid().as_raw());
    is_own.then(|| home.join("Applications"))
}

pub(super) fn current(profile: &Profile) -> Result<Generation, String> {
    let current = profile.current().map_err(profile_error)?;
    Ok(current.unwrap_or_default())
}

pub(super) fn directory(should_create: bool) -> Result<PathBuf, String> {
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

pub(super) fn profiles() -> PathBuf {
    Path::new(ROOT)
        .join("profiles")
        .join(getuid().as_raw().to_string())
}

pub(super) fn profile_error(error: rootbeer_profile::Error) -> String {
    let reason = error.to_string();
    if let rootbeer_profile::Error::Newer { .. } = error {
        crate::newer::relaunch(&reason);
    }

    reason
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(version: &str, is_pinned: bool) -> Member {
        Member {
            version: version.into(),
            is_pinned,
            key: "a".repeat(32).parse().unwrap(),
            manifest: format!("sha256:{}", "0".repeat(64)),
            bins: Default::default(),
            apps: Default::default(),
            extra: Default::default(),
        }
    }

    #[test]
    fn an_update_keeps_pins_and_a_plain_use_skips_what_is_there() {
        let packages = BTreeMap::from([
            ("jj".to_string(), member("0.30", true)),
            ("fd".to_string(), member("10.2.0", false)),
        ]);
        let ask = |requests: &[(&str, Option<&str>)]| {
            requests
                .iter()
                .map(|(name, version)| (name.to_string(), version.map(str::to_string)))
                .collect::<BTreeMap<_, _>>()
        };

        let everything = planned(&packages, BTreeMap::new(), true);
        assert_eq!(everything, ask(&[("fd", None), ("jj", Some("0.30"))]));

        let named = planned(&packages, ask(&[("jj", None)]), true);
        assert_eq!(named, ask(&[("jj", Some("0.30"))]));

        let plain = planned(&packages, ask(&[("fd", None), ("jj", Some("0.30"))]), false);
        assert!(plain.is_empty());

        let unpinned = planned(&packages, ask(&[("jj", None), ("rg", None)]), false);
        assert_eq!(unpinned, ask(&[("jj", None), ("rg", None)]));
    }

    #[test]
    fn fields_from_a_newer_rb_never_make_a_member_differ() {
        let mut newer = member("0.30", true);
        newer.extra.insert("signed".into(), true.into());
        assert!(is_same(&newer, &member("0.30", true)));
        assert!(!is_same(&newer, &member("0.31", true)));
    }
}
