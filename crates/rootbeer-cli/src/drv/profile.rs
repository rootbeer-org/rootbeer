use super::cache::{self, Signed};
use super::github;
use super::install::{self, Source, Wanted};
use super::{migrate, this_platform, HELPER};
use rootbeer_core::env::{profiles, DEFAULT, USER};
use rootbeer_profile::{Generation, Member, Profile};
use rootbeer_store::{Store, ROOT};
use rustix::process::getuid;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(clap::Args, Debug)]
pub(crate) struct UseArgs {
    /// Each `name[@version]`, or `github:owner/repo[@tag]` for a release's
    /// asset. A version pins the package, and otherwise it follows the
    /// platform's default or the latest release
    #[arg(required_unless_present = "should_update")]
    packages: Vec<String>,
    /// Move these packages, or every package, to what the index has now
    #[arg(long = "update")]
    should_update: bool,
    #[command(flatten)]
    source: Source,
}

pub(crate) fn add(args: &UseArgs) -> Result<(), String> {
    let mut requests = BTreeMap::new();
    for request in &args.packages {
        let (name, wanted) = Wanted::parse(request)?;
        let asked = requests.insert(name.clone(), wanted.clone());
        if asked.is_some_and(|asked| asked != wanted) {
            return Err(format!("{name} is asked for twice"));
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
    requests: BTreeMap<String, Wanted>,
    source: &Source,
    mut unmatched: impl FnMut(&str, String) -> Result<(), String>,
) -> Result<(), String> {
    let (mut store, is_root) = install::open_store()?;
    let _held = store.hold().map_err(|error| error.to_string())?;
    let mut index = None;
    let mut signed = Signed::new(|name| {
        let index = match &mut index {
            Some(index) => index,
            None => index.insert(install::open_index(source, is_root)?),
        };
        index.package(name).map_err(|error| error.to_string())
    });
    let cache = source.cache();
    let platform = this_platform()?;

    let mut members = Vec::new();
    for (name, wanted) in requests {
        let member = match &wanted {
            Wanted::Index(version) => signed
                .package(&name)
                .and_then(|package| install::select(package, version.as_deref(), platform))
                .map(|(resolved, output)| Member {
                    version: resolved,
                    is_pinned: version.is_some(),
                    key: output.key.clone(),
                    manifest: Some(output.manifest.clone()),
                    release: None,
                    bins: output.bins.clone(),
                    apps: output.apps.clone(),
                    extra: BTreeMap::new(),
                }),
            Wanted::Release(request) => {
                let previous = generation
                    .packages
                    .get(&name)
                    .and_then(|member| member.release.as_ref());
                github::resolve(&mut store, is_root, &name, request, platform, previous).map(
                    |resolved| Member {
                        version: resolved.version,
                        is_pinned: request.tag.is_some(),
                        key: resolved.key,
                        manifest: None,
                        bins: resolved.release.paths.keys().cloned().collect(),
                        release: Some(resolved.release),
                        apps: BTreeMap::new(),
                        extra: BTreeMap::new(),
                    },
                )
            }
        };

        let member = match member {
            Ok(member) => member,
            Err(error) => {
                unmatched(&name, error)?;
                continue;
            }
        };

        let existing = generation.packages.get(&name);
        if !existing.is_some_and(|existing| is_same(existing, &member)) {
            members.push((name, member));
        }
    }

    if members.is_empty() {
        eprintln!("{USER} already has the newest of what it asks for");
        return link_apps(profile);
    }

    if let Some(applications) = applications() {
        let apps = members.iter().flat_map(|(_, member)| member.apps.keys());
        check_apps(profile, &applications, apps)?;
    }

    // A release is in the store once it resolves.
    for (name, member) in members {
        if member.release.is_none() {
            cache::install_into(
                &cache,
                &mut store,
                is_root,
                &name,
                &member.key,
                Some(&mut signed),
            )?;
        }
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
    mut requests: BTreeMap<String, Wanted>,
    should_update: bool,
) -> BTreeMap<String, Wanted> {
    if should_update && requests.is_empty() {
        requests = packages
            .iter()
            .map(|(name, member)| (name.clone(), unpinned(member)))
            .collect();
    }

    for (name, wanted) in &mut requests {
        let pinned = packages.get(name).filter(|member| member.is_pinned);
        if let (true, None, Some(member)) = (should_update, wanted.version(), pinned) {
            wanted.pin(member.version.clone());
        }
    }

    requests.retain(|name, wanted| {
        let member = packages.get(name);
        should_update
            || !member.is_some_and(|member| {
                member.is_pinned == wanted.version().is_some()
                    && wanted.is_answered_by(&member.version, member.release.as_ref())
            })
    });

    requests
}

/// What a member was asked for, without its version.
fn unpinned(member: &Member) -> Wanted {
    match &member.release {
        Some(release) => Wanted::Release(github::Request::again(release)),
        None => Wanted::Index(None),
    }
}

/// Ignores fields a newer rb added, which never change what's linked.
pub(super) fn is_same(a: &Member, b: &Member) -> bool {
    let strip = |member: &Member| Member {
        extra: BTreeMap::new(),
        ..member.clone()
    };

    strip(a) == strip(b)
}

pub(crate) fn remove(names: &[String]) -> Result<(), String> {
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

pub(crate) fn rollback() -> Result<(), String> {
    let directory = directory(false)?;
    let profile = Profile::open(&directory, USER).map_err(profile_error)?;
    let number = profile.rollback().map_err(profile_error)?;
    eprintln!("{USER} is now generation {number}");
    link_apps(&profile)
}

pub(crate) fn generations() -> Result<(), String> {
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

/// Updates the rootbeer in `user`, or installs it there. A configuration that
/// installs rootbeer updates it instead.
pub(crate) fn self_update() -> Result<(), String> {
    let directory = directory(true)?;
    let user = Profile::open(&directory, USER).map_err(profile_error)?;
    let generation = current(&user)?;
    if !generation.packages.contains_key("rootbeer") && has_rootbeer(&directory, DEFAULT)? {
        return Err("your configuration installs rootbeer, so run `rb apply --update`".into());
    }

    let requests = BTreeMap::from([("rootbeer".to_string(), Wanted::Index(None))]);
    let requests = planned(&generation.packages, requests, true);
    install_requests(
        &user,
        generation,
        requests,
        &Source::default(),
        |_, error| Err(error),
    )
}

/// While the old profiles stay on PATH, an rb from before /opt/rb can still be
/// the one a shell finds. Installing rootbeer into `user` shadows it, and puts
/// `rb migrate` within reach even when a fetched rb ran this command.
pub(crate) fn keep_rootbeer() -> Result<(), String> {
    if !rootbeer_core::env::is_legacy_live() {
        return Ok(());
    }

    let directory = directory(true)?;
    if has_rootbeer(&directory, DEFAULT)? || has_rootbeer(&directory, USER)? {
        return Ok(());
    }

    let user = Profile::open(&directory, USER).map_err(profile_error)?;
    let generation = current(&user)?;
    let requests = BTreeMap::from([("rootbeer".to_string(), Wanted::Index(None))]);
    install_requests(
        &user,
        generation,
        requests,
        &Source::default(),
        |_, error| Err(error),
    )
}

fn has_rootbeer(directory: &Path, name: &str) -> Result<bool, String> {
    let profile = Profile::open_shared(directory, name).map_err(profile_error)?;
    let current = profile.current().map_err(profile_error)?;
    Ok(current.is_some_and(|generation| generation.packages.contains_key("rootbeer")))
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
            super::setup::ensure()?;
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
            manifest: Some(format!("sha256:{}", "0".repeat(64))),
            release: None,
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
                .map(|(name, version)| {
                    let version = version.map(str::to_string);
                    (name.to_string(), Wanted::Index(version))
                })
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
    fn an_update_keeps_a_release_pinned_to_its_tag() {
        let release = rootbeer_profile::Release {
            repository: "BurntSushi/ripgrep".into(),
            asset: "rg.tar.gz".into(),
            url: String::new(),
            sha256: "0".repeat(64).try_into().unwrap(),
            format: rootbeer_drv::Format::TarGz,
            paths: BTreeMap::from([("rg".into(), "rg".into())]),
            libc: None,
            should_keep_asset: false,
            should_keep_paths: false,
        };
        let ripgrep = |is_pinned| Member {
            manifest: None,
            release: Some(release.clone()),
            bins: ["rg".to_string()].into(),
            ..member("14.1.1", is_pinned)
        };
        let latest = github::Request::new("BurntSushi/ripgrep", None).unwrap();
        let pinned = github::Request {
            tag: Some("14.1.1".into()),
            ..latest.clone()
        };

        let packages = BTreeMap::from([("ripgrep".to_string(), ripgrep(true))]);
        let everything = planned(&packages, BTreeMap::new(), true);
        assert_eq!(everything["ripgrep"], Wanted::Release(pinned));

        let packages = BTreeMap::from([("ripgrep".to_string(), ripgrep(false))]);
        let asked = BTreeMap::from([("ripgrep".to_string(), Wanted::Release(latest.clone()))]);
        assert!(planned(&packages, asked.clone(), false).is_empty());
        assert_eq!(
            planned(&packages, asked, true)["ripgrep"],
            Wanted::Release(latest)
        );

        let index = BTreeMap::from([("ripgrep".to_string(), Wanted::Index(None))]);
        assert_eq!(planned(&packages, index.clone(), false), index);

        let mut kept = ripgrep(false);
        if let Some(release) = &mut kept.release {
            release.should_keep_asset = true;
        }
        let packages = BTreeMap::from([("ripgrep".to_string(), kept)]);
        let Wanted::Release(again) = &planned(&packages, BTreeMap::new(), true)["ripgrep"] else {
            panic!("ripgrep is a release");
        };
        assert_eq!(again.asset.as_deref(), Some("rg.tar.gz"));
    }

    #[test]
    fn fields_from_a_newer_rb_never_make_a_member_differ() {
        let mut newer = member("0.30", true);
        newer.extra.insert("signed".into(), true.into());
        assert!(is_same(&newer, &member("0.30", true)));
        assert!(!is_same(&newer, &member("0.31", true)));
    }
}
