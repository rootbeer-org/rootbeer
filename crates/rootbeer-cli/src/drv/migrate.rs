use super::configuration::{configure, link_default_apps};
use super::github;
use super::install::{self, Source, Wanted};
use super::profile::{self, profile_error};
use rootbeer_core::env::{self, legacy_roots, USER};
use rootbeer_core::{Options, PackageLockOptions, Pipeline};
use rootbeer_profile::Profile;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

#[derive(clap::Args, Debug)]
pub(crate) struct Args {
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
    #[serde(default)]
    bins: BTreeMap<String, String>,
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
pub(crate) fn migrate(args: &Args) -> Result<(), String> {
    if !env::is_legacy_live() {
        eprintln!("the old store holds nothing for you");
        return Ok(());
    }

    let mut stuck = Vec::new();
    match configuration(args) {
        Ok(skipped) => stuck.extend(skipped),
        Err(error) => stuck.push(format!("the configuration's packages: {error}")),
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

    // The old profiles are about to leave PATH, and the rb they had with them.
    if let Err(error) = profile::keep_rootbeer() {
        stuck.push(format!("rootbeer: {error}"));
    }

    if !stuck.is_empty() {
        for problem in &stuck {
            eprintln!("  {problem}");
        }

        return Err(
            "the old store keeps what it holds for you until a rerun moves the rest".into(),
        );
    }

    // The old profiles leave PATH first, since releasing the roots lets the old
    // store collect what they link.
    relink_legacy_user()?;
    rewrite_env_files()?;
    release_roots()?;
    write(&env::migrated_path(), b"")?;
    eprintln!("everything moved, so the old store no longer holds anything for you");
    Ok(())
}

/// Printed while the old store may still hold something for the caller. It
/// names the new profiles' rb, since an older one may come first on PATH.
pub(crate) fn hint() {
    if !env::is_legacy_live() {
        return;
    }

    let profiles = env::profiles();
    let rb = [USER, env::DEFAULT]
        .map(|name| rootbeer_profile::bin(&profiles, name).join("rb"))
        .into_iter()
        .find(|rb| rb.exists());
    let rb = rb.map_or("rb".into(), |rb| rb.display().to_string());
    eprintln!("/opt/rootbeer still holds packages for you, so run `{rb} migrate` to move them");
}

/// Runs only the package step of `rb apply`, which also moves
/// `rootbeer.lock` to the schema that step writes. Returns what it skipped.
fn configuration(args: &Args) -> Result<Vec<String>, String> {
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
            false => Ok(Vec::new()),
        };
    }

    let mut options =
        Options::from_script(&script).map_err(|error| format!("{}: {error}", script.display()))?;
    options.profile = args.profile.clone();
    let script_dir = options.script_dir.clone();

    super::check_lock(&script_dir)?;
    let mut planned = Pipeline::new(options)
        .plan()
        .map_err(|error| error.to_string())?;

    let has_sources = planned.has_package_sources();
    let configured = configure(
        planned.take_packages(),
        has_sources,
        &script_dir,
        &PackageLockOptions::default(),
        &args.source,
    )?;
    if configured.should_link_apps {
        link_default_apps()?;
    }

    Ok(configured.skipped)
}

/// What `rb use` asked for in the old `user` profile. A request rb can't
/// serve, like a branch, goes to `stuck`.
fn legacy_requests(stuck: &mut Vec<String>) -> Result<BTreeMap<String, Wanted>, String> {
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
) -> BTreeMap<String, Wanted> {
    let mut servable = BTreeMap::new();
    for request in requests {
        let wanted = match (request.resolver.as_deref(), &request.source) {
            (None | Some("rootbeer"), None) if request.asset.is_none() => {
                Ok((request.name.clone(), Wanted::Index(request.version)))
            }
            (Some("github"), None) => github(&request),
            _ => Err("rb installs only index packages and GitHub releases".to_string()),
        };

        match wanted {
            Ok((name, wanted)) => {
                servable.insert(name, wanted);
            }
            Err(error) => stuck.push(format!("{}: {error}", request.name)),
        }
    }

    servable
}

fn github(request: &LegacyRequest) -> Result<(String, Wanted), String> {
    let mut release = github::Request::new(&request.name, request.version.clone())?;
    release.asset.clone_from(&request.asset);
    release.paths.clone_from(&request.bins);
    Ok((install::named(&request.name)?, Wanted::Release(release)))
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
    write(path, &bytes)
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let at = |error: io::Error| format!("{}: {error}", tmp.display());
    let mut file = File::create(&tmp).map_err(at)?;
    file.write_all(bytes).map_err(at)?;
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

/// Shell profiles from before /opt/rb run `rb env` through the old `user`
/// profile, and old `rb.sh` installs link `~/.rootbeer/bin` into it, so its
/// path now leads to the new `user` profile.
fn relink_legacy_user() -> Result<(), String> {
    let link = rootbeer_core::state_dir().join("profiles/user/current");
    if !fs::symlink_metadata(&link).is_ok_and(|metadata| metadata.is_symlink()) {
        return Ok(());
    }

    let staged = link.with_file_name(".current.link");
    let at = |path: &Path, error: io::Error| format!("{}: {error}", path.display());
    match fs::remove_file(&staged) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(at(&staged, error)),
        _ => {}
    }

    let user = env::profiles().join(USER);
    std::os::unix::fs::symlink(&user, &staged).map_err(|error| at(&staged, error))?;
    fs::rename(&staged, &link).map_err(|error| at(&link, error))
}

/// Points `env.sh` files that shells or older configurations source at the
/// new profiles, now that the old ones may lose what they link.
fn rewrite_env_files() -> Result<(), String> {
    let contents = env::migrated_contents();
    for path in [env::path(), env::legacy_env_path()] {
        if path.is_file() {
            write(&path, contents.as_bytes())?;
        }
    }

    Ok(())
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
            bins: BTreeMap::new(),
        }
    }

    #[test]
    fn index_packages_and_releases_are_servable() {
        let mut stuck = Vec::new();
        let mut branch = request("zig", Some("branch:master"), None);
        branch.source = Some(Value::String("Head".into()));
        let servable = servable(
            [
                request("jq", None, None),
                request("fd", Some("10.2.0"), Some("rootbeer")),
                request("cli/cli", Some("v2.80.0"), Some("github")),
                request("cli", None, Some("github")),
                branch,
            ],
            &mut stuck,
        );

        let release = github::Request::new("cli/cli", Some("v2.80.0".into())).unwrap();
        assert_eq!(
            servable,
            BTreeMap::from([
                ("cli".to_string(), Wanted::Release(release)),
                ("fd".to_string(), Wanted::Index(Some("10.2.0".to_string()))),
                ("jq".to_string(), Wanted::Index(None)),
            ])
        );
        assert_eq!(stuck.len(), 2, "{stuck:?}");
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
