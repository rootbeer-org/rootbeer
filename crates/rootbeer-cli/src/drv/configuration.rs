use super::cache::{self, Signed};
use super::github;
use super::install::{self, Source, Wanted};
use super::local;
use super::profile::{
    applications, check_apps, current, directory, is_same, link_apps, profile_error, switch,
};
use super::this_platform;
use rootbeer_core::env::{profiles, DEFAULT};
use rootbeer_core::package::PackageRequest;
use rootbeer_core::PackageLockOptions;
use rootbeer_drv::{output_path, Platform};
use rootbeer_profile::{Error, Generation, Lock, Locked, Member, Profile, Release};
use rootbeer_store::Store;
use rootbeer_trust::Package;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

type Requests = BTreeMap<String, Wanted>;

pub(crate) struct Configured {
    /// Whether apps need linking once the rest of the configuration runs.
    pub(crate) should_link_apps: bool,
}

/// Installs what the configuration asks for from the signed index, GitHub
/// releases, and the local catalog into `default`. A name the catalog defines
/// comes from its recipe.
pub(crate) fn configure(
    packages: Vec<PackageRequest>,
    catalog: Option<&Path>,
    script_dir: &Path,
    flags: &PackageLockOptions,
    source: &Source,
) -> Result<Configured, String> {
    let mut requests = requests(packages)?;
    let path = script_dir.join("rootbeer.lock");
    let existing = read(&path, flags.is_locked)?;
    let platform = this_platform()?;
    let name = platform.to_string();
    let evaluated = match catalog {
        // A machine sharing the configuration may not have the checkout.
        Some(root) if !root.exists() => {
            eprintln!(
                "warning: {} doesn't exist, so the index serves its packages",
                root.display()
            );
            None
        }
        Some(root) => local::shadow(&mut requests, root, platform)?,
        None => None,
    };

    let mut lock = existing.clone().unwrap_or_default();
    lock.retain(&name, |package| requests.contains_key(package));
    let kept = reconcile(&lock, &name, &requests, flags).map_err(|error| match error {
        Stale::Missing(package) => format!(
            "{package} isn't locked for {name} in {}, so run without --locked",
            path.display()
        ),
        Stale::Moved(package, version) => format!(
            "{package}@{version} in {} no longer answers what's asked for, so run without \
             --locked",
            path.display()
        ),
    })?;

    if flags.is_locked && existing.unwrap_or_default() != lock {
        return Err(format!(
            "{} is stale, so run without --locked",
            path.display()
        ));
    }

    // Nothing asked for and nothing installed before, so no directory yet.
    let has_profile = !requests.is_empty() || fs::symlink_metadata(profiles()).is_ok();
    if !has_profile {
        if !flags.is_locked && path.exists() {
            lock.write(&path).map_err(profile_error)?;
        }

        return Ok(Configured {
            should_link_apps: false,
        });
    }

    let (mut store, is_root) = install::open_store()?;
    let _held = store.hold().map_err(|error| error.to_string())?;
    let cache = source.cache();
    let is_index_open = Cell::new(false);
    let mut index = None;
    let mut signed = Signed::new(|package| {
        if flags.is_offline {
            return Err(format!(
                "{package} needs the signed index, which --offline doesn't allow"
            ));
        }

        let index = match &mut index {
            Some(index) => index,
            None => index.insert(install::open_index(source, is_root)?),
        };
        is_index_open.set(true);
        index.package(package).map_err(|error| error.to_string())
    });

    let mut resolved = BTreeMap::new();
    for (package, wanted) in &requests {
        if kept.contains_key(package) {
            continue;
        }

        let locked = match wanted {
            Wanted::Index(version) => {
                let entry = signed.package(package)?;
                let (version, output) = install::select(entry, version.as_deref(), platform)?;
                Locked {
                    version,
                    key: output.key.clone(),
                    manifest: Some(output.manifest.clone()),
                    release: None,
                    is_local: false,
                    bins: output.bins.clone(),
                    apps: output.apps.clone(),
                    extra: BTreeMap::new(),
                }
            }
            Wanted::Release(_) if flags.is_offline => {
                return Err(format!(
                    "{package} needs GitHub's release API, which --offline doesn't allow"
                ));
            }
            Wanted::Release(request) => {
                let previous = existing_release(&lock, package, &name);
                let resolved =
                    github::resolve(&mut store, is_root, package, request, platform, previous)?;
                Locked {
                    version: resolved.version,
                    key: resolved.key,
                    manifest: None,
                    bins: resolved.release.paths.keys().cloned().collect(),
                    release: Some(resolved.release),
                    is_local: false,
                    apps: BTreeMap::new(),
                    extra: BTreeMap::new(),
                }
            }
            Wanted::Local { .. } => {
                let (target, built) =
                    local_evaluated(evaluated.as_ref(), package)?.package(package)?;
                Locked {
                    version: target.version.clone(),
                    key: built.build.clone(),
                    manifest: None,
                    release: None,
                    is_local: true,
                    bins: built.metadata.bins.clone(),
                    apps: built.metadata.apps.clone(),
                    extra: BTreeMap::new(),
                }
            }
        };
        resolved.insert(package.clone(), locked);
    }

    if is_index_open.get() {
        for (package, locked) in kept.iter().filter(|(_, locked)| locked.manifest.is_some()) {
            verify(signed.package(package)?, locked, platform)?;
        }
    }

    let profile = Profile::open(&directory(true)?, DEFAULT).map_err(profile_error)?;
    let current = current(&profile)?;
    if let Some(applications) = applications() {
        let apps = kept.values().chain(resolved.values());
        check_apps(
            &profile,
            &applications,
            apps.flat_map(|locked| locked.apps.keys()),
        )?;
    }

    let mut members = BTreeMap::new();
    for (package, locked) in kept.into_iter().chain(resolved) {
        match &locked.release {
            None if locked.is_local => {
                let is_new = !current
                    .packages
                    .get(&package)
                    .is_some_and(|member| member.key == locked.key);
                local_evaluated(evaluated.as_ref(), &package)?.install(
                    &package,
                    &mut store,
                    is_root,
                    &cache,
                    &mut signed,
                    flags.is_offline,
                    is_new,
                )?;
            }
            Some(release) => {
                let key = github::install(
                    &mut store,
                    is_root,
                    &package,
                    &locked.version,
                    release,
                    platform,
                    flags.is_offline,
                )?;
                if key != locked.key {
                    return Err(format!(
                        "{package} unpacks to {key}, not {} as {} says",
                        locked.key,
                        path.display()
                    ));
                }
            }
            None => cache::install_into(
                &cache,
                &mut store,
                is_root,
                &package,
                &locked.key,
                Some(&mut signed),
            )?,
        }
        installed(&store, &package, &locked)?;

        let member = Member {
            version: locked.version.clone(),
            is_pinned: requests.get(&package).and_then(Wanted::version).is_some(),
            key: locked.key.clone(),
            manifest: locked.manifest.clone(),
            release: locked.release.clone(),
            is_local: locked.is_local,
            bins: locked.bins.clone(),
            apps: locked.apps.clone(),
            extra: BTreeMap::new(),
        };

        members.insert(package.clone(), member);
        lock.insert(&package, &name, locked);
    }

    if !flags.is_locked && (path.exists() || !lock.packages.is_empty()) {
        lock.write(&path).map_err(profile_error)?;
    }

    let configured = Configured {
        should_link_apps: true,
    };

    if same(&current.packages, &members) {
        return Ok(configured);
    }

    let generation = Generation {
        packages: members,
        extra: current.extra,
        ..Generation::default()
    };
    switch(&profile, DEFAULT, &store, generation)?;
    Ok(configured)
}

fn local_evaluated<'a>(
    evaluated: Option<&'a local::Evaluated>,
    package: &str,
) -> Result<&'a local::Evaluated, String> {
    evaluated.ok_or_else(|| format!("{package} comes from rb.package_catalog, which isn't set"))
}

fn existing_release<'a>(lock: &'a Lock, package: &str, platform: &str) -> Option<&'a Release> {
    lock.get(package, platform)?.release.as_ref()
}

/// Stops before the configuration runs when a newer rb wrote its lock, and
/// hands the command to that rb.
pub(crate) fn check_lock(script_dir: &Path) -> Result<(), String> {
    match Lock::read(&script_dir.join("rootbeer.lock")) {
        Err(error @ Error::Newer { .. }) => Err(profile_error(error)),
        _ => Ok(()),
    }
}

pub(crate) fn link_default_apps() -> Result<(), String> {
    let profile = Profile::open(&directory(false)?, DEFAULT).map_err(profile_error)?;
    link_apps(&profile)
}

fn requests(packages: Vec<PackageRequest>) -> Result<Requests, String> {
    let mut requests = Requests::new();
    for package in packages {
        let (name, wanted) = match package {
            PackageRequest::Index { name, version } => (name, Wanted::Index(version)),
            PackageRequest::Release {
                repository,
                tag,
                asset,
                bins,
            } => {
                let mut release = github::Request::new(&repository, tag)?;
                release.asset = asset;
                release.paths = paths(&bins)?;
                (install::named(&repository)?, Wanted::Release(release))
            }
        };

        let Some(asked) = requests.insert(name.clone(), wanted.clone()) else {
            continue;
        };

        if asked != wanted {
            let reason = match (&asked, &wanted) {
                (Wanted::Index(_), Wanted::Index(_)) => "at two versions",
                _ => "from two places",
            };
            return Err(format!("{name} is asked for {reason}"));
        }
    }

    Ok(requests)
}

fn paths(bins: &BTreeMap<String, PathBuf>) -> Result<BTreeMap<String, String>, String> {
    bins.iter()
        .map(|(bin, path)| match path.to_str() {
            Some(path) => Ok((bin.clone(), path.to_string())),
            None => Err(format!("{bin}'s path {} isn't UTF-8", path.display())),
        })
        .collect()
}

fn read(path: &Path, is_locked: bool) -> Result<Option<Lock>, String> {
    match Lock::read(path) {
        Ok(lock) => Ok(lock),
        Err(Error::Older { .. }) if is_locked => Err(format!(
            "{} is from an older rb, so run without --locked to replace it",
            path.display()
        )),
        Err(Error::Older { .. }) => {
            eprintln!("replacing {} from an older rb", path.display());
            Ok(None)
        }
        Err(error) => Err(profile_error(error)),
    }
}

enum Stale {
    Missing(String),
    Moved(String, String),
}

/// Picks the entries `rootbeer.lock` keeps. A pin keeps its entry only at its
/// exact version, and `--update` moves pins too, onto a rebuilt key.
fn reconcile(
    lock: &Lock,
    platform: &str,
    requests: &Requests,
    flags: &PackageLockOptions,
) -> Result<BTreeMap<String, Locked>, Stale> {
    let mut kept = BTreeMap::new();
    for (package, wanted) in requests {
        let locked = lock.get(package, platform);
        let is_kept = locked
            .is_some_and(|locked| !flags.should_update && wanted.is_answered_by(locked.into()));

        if let Some(locked) = locked.filter(|_| is_kept) {
            kept.insert(package.clone(), locked.clone());
            continue;
        }

        if !flags.is_locked {
            continue;
        }

        return Err(match locked {
            Some(locked) => Stale::Moved(package.clone(), locked.version.clone()),
            None => Stale::Missing(package.clone()),
        });
    }

    Ok(kept)
}

/// A rebuild retires the old key, which stays installable though only its
/// manifest is still signed.
fn verify(package: &Package, locked: &Locked, platform: Platform) -> Result<(), String> {
    let outputs = package.versions.get(&locked.version);
    let manifest = locked.manifest.as_ref();
    let is_signed = match outputs.and_then(|outputs| outputs.get(&platform)) {
        Some(output) if output.key == locked.key => {
            Some(&output.manifest) == manifest
                && output.bins == locked.bins
                && output.apps == locked.apps
        }
        _ => manifest.is_some() && package.retired.get(&locked.key) == manifest,
    };

    if !is_signed {
        return Err(format!(
            "{}@{} {} in rootbeer.lock doesn't match the signed index",
            package.name, locked.version, locked.key
        ));
    }

    Ok(())
}

/// Entries are named after what built them, which a lock can't change.
fn installed(store: &Store, package: &str, locked: &Locked) -> Result<(), String> {
    let path = store
        .path(&locked.key)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("{} isn't in the store", locked.key))?;

    let expected = output_path(&locked.key, package, &locked.version, "out");
    if path.file_name() != expected.file_name() {
        return Err(format!(
            "rootbeer.lock says {} is {package}@{}, but the store has {}",
            locked.key,
            locked.version,
            path.display()
        ));
    }

    Ok(())
}

fn same(current: &BTreeMap<String, Member>, members: &BTreeMap<String, Member>) -> bool {
    current.len() == members.len()
        && current
            .iter()
            .zip(members)
            .all(|((name, a), (other, b))| name == other && is_same(a, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rootbeer_trust::Output;

    fn locked(version: &str) -> Locked {
        Locked {
            version: version.into(),
            key: "a".repeat(32).parse().unwrap(),
            manifest: Some(format!("sha256:{}", "0".repeat(64))),
            release: None,
            is_local: false,
            bins: Default::default(),
            apps: Default::default(),
            extra: Default::default(),
        }
    }

    #[test]
    fn requests_are_keyed_by_the_name_they_install_under() {
        let packages = |requests: &[&str]| {
            requests
                .iter()
                .map(|request| PackageRequest::parse(request).unwrap())
                .collect::<Vec<_>>()
        };

        let accepted = super::requests(packages(&[
            "jq",
            "fd@10.2.0",
            "jq",
            "github:owner/Tool_X@1.0",
        ]))
        .unwrap();
        assert_eq!(accepted.len(), 3);
        assert_eq!(accepted["jq"], Wanted::Index(None));
        assert_eq!(accepted["fd"], Wanted::Index(Some("10.2.0".into())));
        let release = github::Request::new("owner/Tool_X", Some("1.0".into())).unwrap();
        assert_eq!(accepted["tool-x"], Wanted::Release(release));

        let error = super::requests(packages(&["jq", "jq@1.7"])).unwrap_err();
        assert_eq!(error, "jq is asked for at two versions");

        let error = super::requests(packages(&["jq", "github:jqlang/jq"])).unwrap_err();
        assert_eq!(error, "jq is asked for from two places");
    }

    #[test]
    fn a_pin_keeps_its_version_until_update() {
        let mut lock = Lock::default();
        lock.insert("jq", "aarch64-macos", locked("1.8.2"));
        let plain = PackageLockOptions::default();
        let update = PackageLockOptions {
            should_update: true,
            ..PackageLockOptions::default()
        };
        let strict = PackageLockOptions {
            is_locked: true,
            ..PackageLockOptions::default()
        };

        let kept = |version: Option<&str>, flags| {
            let requests = Requests::from([("jq".into(), Wanted::Index(version.map(Into::into)))]);
            reconcile(&lock, "aarch64-macos", &requests, flags).map(|kept| kept.contains_key("jq"))
        };
        assert!(matches!(kept(None, &plain), Ok(true)));
        assert!(matches!(kept(Some("1.8.2"), &plain), Ok(true)));
        assert!(matches!(kept(Some("1.7"), &plain), Ok(false)));
        assert!(matches!(kept(None, &update), Ok(false)));
        assert!(matches!(kept(Some("1.8.2"), &update), Ok(false)));
        assert!(matches!(kept(Some("1.8.2"), &strict), Ok(true)));

        let moved = kept(Some("1.7"), &strict);
        assert!(matches!(moved, Err(Stale::Moved(_, version)) if version == "1.8.2"));

        let requests = Requests::from([("fd".into(), Wanted::Index(None))]);
        let missing = reconcile(&lock, "aarch64-macos", &requests, &strict);
        assert!(matches!(missing, Err(Stale::Missing(package)) if package == "fd"));
    }

    #[test]
    fn a_kept_entry_must_match_what_the_index_signed() {
        let mut package = Package {
            name: "jq".into(),
            description: String::new(),
            license: "MIT".into(),
            default: BTreeMap::new(),
            versions: BTreeMap::new(),
            ..Package::default()
        };
        let platform = Platform::Aarch64Macos;
        let entry = locked("1.8.2");
        assert!(verify(&package, &entry, platform).is_err());

        package
            .retired
            .insert(entry.key.clone(), entry.manifest.clone().unwrap());
        assert!(verify(&package, &entry, platform).is_ok());

        let output = Output {
            key: entry.key.clone(),
            manifest: entry.manifest.clone().unwrap(),
            bins: ["jq".to_string()].into(),
            apps: Default::default(),
        };
        let outputs = BTreeMap::from([(platform, output)]);
        package.versions.insert("1.8.2".into(), outputs);
        assert!(verify(&package, &entry, platform).is_err());

        let entry = Locked {
            bins: ["jq".to_string()].into(),
            ..entry
        };
        assert!(verify(&package, &entry, platform).is_ok());
    }

    #[test]
    fn a_dropped_package_loses_only_this_platforms_entry() {
        let mut lock = Lock::default();
        lock.insert("slack", "aarch64-macos", locked("4.0"));
        lock.insert("jq", "aarch64-macos", locked("1.8.2"));
        lock.insert("jq", "x86_64-linux", locked("1.8.2"));

        lock.retain("x86_64-linux", |name| name != "jq");
        assert!(lock.get("slack", "aarch64-macos").is_some());
        assert!(lock.get("jq", "aarch64-macos").is_some());
        assert!(lock.get("jq", "x86_64-linux").is_none());

        lock.retain("aarch64-macos", |name| name == "jq");
        assert!(!lock.packages.contains_key("slack"));
    }
}
