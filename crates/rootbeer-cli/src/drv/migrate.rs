use super::configuration::{configure, link_default_apps};
use super::install::Source;
use super::profile::{self, profile_error, USER};
use rootbeer_core::{Options, PackageLockOptions, Pipeline};
use rootbeer_profile::Profile;
use rustix::process::getuid;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

const LEGACY_ROOT: &str = "/opt/rootbeer";

#[derive(clap::Args, Debug)]
pub(super) struct Args {
    /// The configuration whose packages move, instead of `init.lua` in the
    /// config directory
    #[arg(short, long)]
    script: Option<PathBuf>,
    /// The configuration profile `rb apply` uses
    #[arg(short = 'p', long)]
    profile: Option<String>,
    #[command(flatten)]
    source: Source,
}

#[derive(Deserialize)]
struct LegacyRequest {
    name: String,
    version: Option<String>,
    resolver: Option<String>,
    source: Option<Value>,
    asset: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
struct Ownership {
    owners: BTreeMap<String, BTreeMap<String, PathBuf>>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

/// Installs what the old store's profiles hold into /opt/rb, then releases the
/// old store's roots. Anything that can't move keeps every root, so the old
/// binaries keep working until a rerun moves it.
pub(super) fn migrate(args: &Args) -> Result<(), String> {
    let mut stuck = Vec::new();
    if let Err(error) = configuration(args) {
        stuck.push(format!("the configuration's packages: {error}"));
    }

    let requests = legacy_requests(&mut stuck)?;
    if !requests.is_empty() {
        let directory = profile::directory(true)?;
        let user = Profile::open(&directory, USER).map_err(profile_error)?;
        let generation = profile::current(&user)?;
        let requests = profile::planned(&generation.packages, requests, false);
        if !requests.is_empty() {
            profile::install_requests(&user, generation, requests, &args.source, |name, error| {
                stuck.push(format!("{name}: {error}"));
                Ok(())
            })?;
        }
    }

    if !stuck.is_empty() {
        for problem in &stuck {
            eprintln!("  {problem}");
        }

        return Err(
            "the old store keeps what it holds for you until a rerun moves the rest".into(),
        );
    }

    release_roots()?;
    eprintln!("everything moved, so the old store no longer holds anything for you");
    Ok(())
}

/// Runs only the package step of `rb apply --drv`, which also moves
/// `rootbeer.lock` to the schema that step writes.
fn configuration(args: &Args) -> Result<(), String> {
    let script = args
        .script
        .clone()
        .unwrap_or_else(rootbeer_core::script_path);
    if args.script.is_none() && !script.exists() {
        return match legacy_roots().join("configuration").exists() {
            true => Err(format!(
                "`rb apply` ran a configuration that isn't at {}, so pass it with --script",
                script.display()
            )),
            false => Ok(()),
        };
    }

    let mut options =
        Options::from_script(&script).map_err(|error| format!("{}: {error}", script.display()))?;
    options.profile = args.profile.clone();
    let script_dir = options.script_dir.clone();

    let mut planned = Pipeline::new(options)
        .plan()
        .map_err(|error| error.to_string())?;
    if planned.has_package_sources() {
        return Err(
            "it uses rb.package_catalog or rb.package_repository, which the signed index can't serve"
                .into(),
        );
    }

    let packages = planned.take_packages();
    if configure(
        packages,
        &script_dir,
        &PackageLockOptions::default(),
        &args.source,
    )? {
        link_default_apps()?;
    }

    Ok(())
}

/// What `rb use` asked for in the old `user` profile. A request the signed
/// index can't serve, like a GitHub release or a branch, goes to `stuck`.
fn legacy_requests(stuck: &mut Vec<String>) -> Result<BTreeMap<String, Option<String>>, String> {
    let path = rootbeer_core::state_dir().join("profiles/user/current/requests.json");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };

    let requests: BTreeMap<String, LegacyRequest> =
        serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(servable(requests.into_values(), stuck))
}

fn servable(
    requests: impl IntoIterator<Item = LegacyRequest>,
    stuck: &mut Vec<String>,
) -> BTreeMap<String, Option<String>> {
    let mut servable = BTreeMap::new();
    for request in requests {
        let is_catalog = request
            .resolver
            .as_deref()
            .is_none_or(|resolver| resolver == "rootbeer");
        if !is_catalog || request.source.is_some() || request.asset.is_some() {
            stuck.push(format!(
                "{}: the signed index only has catalog releases",
                request.name
            ));
            continue;
        }

        servable.insert(request.name, request.version);
    }

    servable
}

/// The old store's app links in `applications` that still point where its
/// profiles left them, by name.
pub(super) fn legacy_apps(applications: &Path) -> Result<BTreeSet<String>, String> {
    owned(&ownership_path(), applications)
}

fn owned(ownership: &Path, applications: &Path) -> Result<BTreeSet<String>, String> {
    let ownership = read_ownership(ownership)?;
    let apps = ownership.owners.into_values().flatten();
    let apps = apps.filter(|(name, target)| {
        fs::read_link(applications.join(name)).is_ok_and(|link| link == *target)
    });

    Ok(apps.map(|(name, _)| name).collect())
}

/// Takes `apps` from the old store's profiles, removing their links where they
/// still point where it left them, so a profile can link its own.
pub(super) fn release_legacy_apps<'a>(
    applications: &Path,
    apps: impl IntoIterator<Item = &'a String>,
) -> Result<(), String> {
    release(&ownership_path(), applications, apps)
}

fn release<'a>(
    path: &Path,
    applications: &Path,
    apps: impl IntoIterator<Item = &'a String>,
) -> Result<(), String> {
    let mut ownership = read_ownership(path)?;
    let mut is_changed = false;
    for app in apps {
        for exports in ownership.owners.values_mut() {
            let Some(target) = exports.remove(app) else {
                continue;
            };

            is_changed = true;
            let link = applications.join(app);
            if fs::read_link(&link).is_ok_and(|current| current == target) {
                fs::remove_file(&link).map_err(|error| format!("{}: {error}", link.display()))?;
            }
        }
    }

    if !is_changed {
        return Ok(());
    }

    let bytes = serde_json::to_vec(&ownership).map_err(|error| error.to_string())?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let at = |error: io::Error| format!("{}: {error}", tmp.display());
    let mut file = File::create(&tmp).map_err(at)?;
    file.write_all(&bytes).map_err(at)?;
    file.sync_all().map_err(at)?;
    fs::rename(&tmp, path).map_err(|error| format!("{}: {error}", path.display()))
}

fn ownership_path() -> PathBuf {
    rootbeer_core::state_dir().join("applications/owners.json")
}

/// Drops names that aren't a single path component, so a link outside the
/// applications directory is never touched.
fn read_ownership(path: &Path) -> Result<Ownership, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Ownership::default()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };

    let mut ownership: Ownership =
        serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.display()))?;
    for exports in ownership.owners.values_mut() {
        exports.retain(|name, _| {
            let mut components = Path::new(name).components();
            matches!(
                (components.next(), components.next()),
                (Some(Component::Normal(_)), None)
            )
        });
    }

    Ok(ownership)
}

/// Once this user's directory under `var/roots` is empty, the old store holds
/// nothing for them, and it can go when every user's is.
fn release_roots() -> Result<(), String> {
    let directory = legacy_roots();
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", directory.display())),
    };

    for entry in entries {
        let path = entry
            .map_err(|error| format!("{}: {error}", directory.display()))?
            .path();
        fs::remove_file(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    }

    Ok(())
}

fn legacy_roots() -> PathBuf {
    std::env::var_os("ROOTBEER_ROOT")
        .map_or(PathBuf::from(LEGACY_ROOT), PathBuf::from)
        .join("var/roots")
        .join(getuid().as_raw().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(name: &str, version: Option<&str>, resolver: Option<&str>) -> LegacyRequest {
        LegacyRequest {
            name: name.into(),
            version: version.map(str::to_string),
            resolver: resolver.map(str::to_string),
            source: None,
            asset: None,
        }
    }

    #[test]
    fn only_catalog_requests_are_servable() {
        let mut stuck = Vec::new();
        let mut branch = request("zig", Some("branch:master"), None);
        branch.source = Some(Value::String("Head".into()));
        let servable = servable(
            [
                request("jq", None, None),
                request("fd", Some("10.2.0"), Some("rootbeer")),
                request("cli", None, Some("github")),
                branch,
            ],
            &mut stuck,
        );

        assert_eq!(
            servable,
            BTreeMap::from([
                ("fd".to_string(), Some("10.2.0".to_string())),
                ("jq".to_string(), None),
            ])
        );
        assert_eq!(stuck.len(), 2);
    }

    #[test]
    fn only_links_still_pointing_where_the_old_store_left_them_make_way() {
        let state = tempfile::tempdir().unwrap();
        let applications = tempfile::tempdir().unwrap();
        let ownership = state.path().join("owners.json");
        let ours = state.path().join("Ours.app");
        let moved = state.path().join("Moved.app");
        fs::write(
            &ownership,
            serde_json::json!({
                "owners": {
                    "user": { "Ours.app": ours, "Moved.app": moved, "../Escape.app": ours },
                    "default": { "Kept.app": ours },
                },
                "schema": 1,
            })
            .to_string(),
        )
        .unwrap();
        std::os::unix::fs::symlink(&ours, applications.path().join("Ours.app")).unwrap();
        std::os::unix::fs::symlink(&ours, applications.path().join("Moved.app")).unwrap();
        std::os::unix::fs::symlink(&ours, applications.path().join("Kept.app")).unwrap();

        let owned = owned(&ownership, applications.path()).unwrap();
        assert_eq!(
            owned.into_iter().collect::<Vec<_>>(),
            ["Kept.app", "Ours.app"]
        );

        let apps = ["Ours.app".to_string(), "Moved.app".to_string()];
        release(&ownership, applications.path(), &apps).unwrap();
        assert!(fs::symlink_metadata(applications.path().join("Ours.app")).is_err());
        assert!(fs::symlink_metadata(applications.path().join("Moved.app")).is_ok());

        let left: Value = serde_json::from_slice(&fs::read(&ownership).unwrap()).unwrap();
        assert_eq!(
            left,
            serde_json::json!({
                "owners": { "user": {}, "default": { "Kept.app": ours } },
                "schema": 1,
            })
        );
    }
}
