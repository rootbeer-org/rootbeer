use super::cache::{self, Signed};
use super::install::{self, Source};
use super::profile::{
    applications, current, directory, is_same, link_apps, profile_error, profiles, switch,
};
use super::this_platform;
use rootbeer_core::package::PackageIntent;
use rootbeer_core::PackageLockOptions;
use rootbeer_drv::{output_path, Platform};
use rootbeer_profile::{Error, Generation, Lock, Locked, Member, Profile};
use rootbeer_store::Store;
use rootbeer_trust::Package;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

const DEFAULT: &str = "default";

type Requests = BTreeMap<String, Option<String>>;

/// Returns whether apps need linking once the rest of the configuration runs.
pub(crate) fn configure(
    intents: Vec<PackageIntent>,
    script_dir: &Path,
    flags: &PackageLockOptions,
) -> Result<bool, String> {
    let requests = requests(intents)?;
    let path = script_dir.join("rootbeer.lock");
    let existing = read(&path, flags.is_locked)?;
    let platform = this_platform()?;
    let name = platform.to_string();

    let mut lock = existing.clone().unwrap_or_default();
    lock.retain(&name, |package| requests.contains_key(package));
    let kept = reconcile(&lock, &name, &requests, flags).map_err(|error| match error {
        Stale::Missing(package) => format!(
            "{package} isn't locked for {name} in {}, so run without --locked",
            path.display()
        ),
        Stale::Moved(package, version) => format!(
            "{package} is locked at {version} in {}, so run without --locked",
            path.display()
        ),
    })?;

    if flags.is_locked && existing.unwrap_or_default() != lock {
        return Err(format!(
            "{} is stale, so run without --locked",
            path.display()
        ));
    }

    let (mut store, is_root) = install::open_store()?;
    let _held = store.hold().map_err(|error| error.to_string())?;
    let source = Source::default();
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
            None => index.insert(install::open_index(&source, is_root)?),
        };
        is_index_open.set(true);
        index.package(package).map_err(|error| error.to_string())
    });

    let mut resolved = BTreeMap::new();
    for (package, version) in &requests {
        if kept.contains_key(package) {
            continue;
        }

        let entry = signed.package(package)?;
        let (version, output) = install::select(entry, version.as_deref(), platform)?;
        let locked = Locked {
            version,
            key: output.key.clone(),
            manifest: output.manifest.clone(),
            bins: output.bins.clone(),
            apps: output.apps.clone(),
            extra: BTreeMap::new(),
        };
        resolved.insert(package.clone(), locked);
    }

    if is_index_open.get() {
        for (package, locked) in &kept {
            verify(signed.package(package)?, locked, platform)?;
        }
    }

    // Nothing asked for and nothing installed before, so no directory yet.
    let has_profile = !requests.is_empty() || fs::symlink_metadata(profiles()).is_ok();
    let profile = match has_profile {
        true => Some(Profile::open(&directory(true)?, DEFAULT).map_err(profile_error)?),
        false => None,
    };

    if let (Some(profile), Some(applications)) = (&profile, applications()) {
        let apps = kept.values().chain(resolved.values());
        let apps = apps.flat_map(|locked| locked.apps.keys());
        profile
            .check_apps(&applications, apps.map(String::as_str))
            .map_err(profile_error)?;
    }

    let mut members = BTreeMap::new();
    for (package, locked) in kept.into_iter().chain(resolved) {
        cache::install_into(
            &cache,
            &mut store,
            is_root,
            &package,
            &locked.key,
            Some(&mut signed),
        )?;
        installed(&store, &package, &locked)?;

        let member = Member {
            version: locked.version.clone(),
            is_pinned: requests[&package].is_some(),
            key: locked.key.clone(),
            manifest: locked.manifest.clone(),
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

    let Some(profile) = profile else {
        return Ok(false);
    };

    let current = current(&profile)?;
    if same(&current.packages, &members) {
        return Ok(true);
    }

    let generation = Generation {
        packages: members,
        extra: current.extra,
        ..Generation::default()
    };
    switch(&profile, DEFAULT, &store, generation)?;
    Ok(true)
}

pub(crate) fn link_default_apps() -> Result<(), String> {
    let profile = Profile::open(&directory(false)?, DEFAULT).map_err(profile_error)?;
    link_apps(&profile)
}

fn requests(intents: Vec<PackageIntent>) -> Result<Requests, String> {
    let mut requests = Requests::new();
    for intent in intents {
        let request = match intent {
            PackageIntent::Request(request) => request,
            PackageIntent::Locked(package) => {
                return Err(format!(
                    "{}@{} is an exact package spec, which --drv can't install",
                    package.name, package.version
                ))
            }
        };

        let is_indexed = request
            .resolver
            .as_deref()
            .is_none_or(|name| name == "rootbeer")
            && request.source.is_none()
            && request.asset.is_none()
            && request.bins.is_empty();
        if !is_indexed {
            let resolver = request.resolver.map(|name| format!("{name}:"));
            return Err(format!(
                "{}{} doesn't come from the signed index, so --drv can't install it",
                resolver.unwrap_or_default(),
                request.name
            ));
        }

        let asked = requests.insert(request.name.clone(), request.version.clone());
        if asked.is_some_and(|asked| asked != request.version) {
            return Err(format!("{} is asked for at two versions", request.name));
        }
    }

    Ok(requests)
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
    for (package, version) in requests {
        let locked = lock.get(package, platform);
        let is_kept = locked.is_some_and(|locked| {
            !flags.should_update
                && version
                    .as_ref()
                    .is_none_or(|version| locked.version == *version)
        });

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
    let is_signed = match outputs.and_then(|outputs| outputs.get(&platform)) {
        Some(output) if output.key == locked.key => {
            output.manifest == locked.manifest
                && output.bins == locked.bins
                && output.apps == locked.apps
        }
        _ => package.retired.get(&locked.key) == Some(&locked.manifest),
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
    use rootbeer_core::package::{PackageRequest, SourceSelection};
    use rootbeer_trust::Output;

    fn locked(version: &str) -> Locked {
        Locked {
            version: version.into(),
            key: "a".repeat(32).parse().unwrap(),
            manifest: format!("sha256:{}", "0".repeat(64)),
            bins: Default::default(),
            apps: Default::default(),
            extra: Default::default(),
        }
    }

    #[test]
    fn only_requests_the_index_can_serve_are_accepted() {
        let parse = |requests: &[&str]| {
            let intents = requests
                .iter()
                .map(|request| PackageIntent::Request(PackageRequest::parse(request)))
                .collect();
            super::requests(intents)
        };

        let accepted = parse(&["jq", "fd@10.2.0", "jq"]).unwrap();
        assert_eq!(accepted["jq"], None);
        assert_eq!(accepted["fd"].as_deref(), Some("10.2.0"));

        let error = parse(&["jq", "jq@1.7"]).unwrap_err();
        assert_eq!(error, "jq is asked for at two versions");

        let error = parse(&["github:owner/tool@1.0"]).unwrap_err();
        assert!(error.starts_with("github:"), "{error}");

        let mut source = PackageRequest::parse("jq");
        source.source = Some(SourceSelection::Head);
        assert!(super::requests(vec![PackageIntent::Request(source)]).is_err());
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
            let requests = Requests::from([("jq".into(), version.map(Into::into))]);
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

        let requests = Requests::from([("fd".into(), None)]);
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
            retired: BTreeMap::new(),
        };
        let platform = Platform::Aarch64Macos;
        let entry = locked("1.8.2");
        assert!(verify(&package, &entry, platform).is_err());

        package
            .retired
            .insert(entry.key.clone(), entry.manifest.clone());
        assert!(verify(&package, &entry, platform).is_ok());

        let output = Output {
            key: entry.key.clone(),
            manifest: entry.manifest.clone(),
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
