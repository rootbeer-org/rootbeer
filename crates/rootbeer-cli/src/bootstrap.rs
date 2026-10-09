use std::fs;
use std::io::{self, IsTerminal};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use rootbeer_core::package::{profile, roots, standalone, PackageRequest};
use rootbeer_core::store::{
    helper, layout, root_dir, state_dir, Store, StoreManifest, DEFAULT_ROOT,
};

use crate::update::{self, Installation};

/// Brings the machine-wide root up to date before a command touches the store.
/// The common case is a few `stat`s and no output.
pub fn ensure() -> Result<(), String> {
    let root = root_dir();
    let layout = layout::read(&root).map_err(|error| error.to_string())?;
    let version = layout.as_ref().map(|layout| layout.version);
    if let Some(version) = version.filter(|version| *version > layout::VERSION) {
        let reason = format!(
            "{} uses layout v{version}, newer than this rootbeer understands (v{}); update rootbeer",
            root.display(),
            layout::VERSION
        );

        crate::newer::relaunch(&reason);
        return Err(reason);
    }

    let installed = layout.as_ref().and_then(|layout| layout.helper.as_ref());
    let is_current =
        version == Some(layout::VERSION) && (!is_shared(&root) || helper::is_sufficient(installed));
    if !is_current {
        provision(&root, version)?;
    }

    let lock = match fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(".lock"))
    {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", root.display())),
    };

    lock.lock().map_err(|error| error.to_string())?;
    let _ = migrate_legacy_store(&root.join("store"));
    let _ = roots::seed(&Store::new(root.join("store")));
    Ok(())
}

fn is_shared(root: &Path) -> bool {
    root == Path::new(DEFAULT_ROOT) && unsafe { libc::geteuid() } != 0
}

fn provision(root: &Path, version: Option<u32>) -> Result<(), String> {
    if is_shared(root) {
        let is_converting = version != Some(layout::VERSION);
        let lines = shared_setup(root, &helper_source()?, is_converting);
        let reason = if is_converting {
            "set up its store for this machine"
        } else {
            "update its store helper"
        };
        return run_as_root(reason, &lines);
    }

    let store = root.join("store");
    match fs::create_dir_all(&store) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
            run_as_root(
                "set up its store for this machine",
                &[format!(
                    "install -d -o {uid} -g {gid} {} {}",
                    quote(root),
                    quote(&store)
                )],
            )?;
        }
        Err(error) => return Err(format!("{}: {error}", store.display())),
    }

    layout::write(root).map_err(|error| error.to_string())
}

fn shared_setup(root: &Path, helper: &Path, is_converting: bool) -> Vec<String> {
    let bin = root.join("bin");
    let installed = quote(&bin.join("rb-store"));
    let lock = quote(&root.join(".lock"));
    let mut lines = vec![format!(
        "install -d -m 755 {} {} {}",
        quote(root),
        quote(&root.join("store")),
        quote(&bin)
    )];

    if is_converting {
        lines.push(format!("chown -R 0:0 {}", quote(root)));
    }

    lines.push(format!("touch {lock} && chmod 666 {lock}"));
    lines.push(format!(
        "install -m 4755 -o 0 -g 0 {} {installed}",
        quote(helper)
    ));

    lines.push(format!(
        "printf '{{\"version\":{},\"helper\":%s}}\\n' \"$({installed} version)\" > {}",
        layout::VERSION,
        quote(&root.join("layout.json"))
    ));

    lines
}

fn helper_source() -> Result<PathBuf, String> {
    let executable = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| error.to_string())?;

    let helper = executable.with_file_name("rb-store");
    if !helper.is_file() {
        return Err(format!(
            "{} is missing; reinstall rootbeer to set up the shared store",
            helper.display()
        ));
    }

    Ok(helper)
}

fn run_as_root(reason: &str, lines: &[String]) -> Result<(), String> {
    let listing: String = lines.iter().map(|line| format!("\n  {line}")).collect();
    if !io::stdin().is_terminal() {
        return Err(format!("rootbeer must {reason}; run as root:{listing}"));
    }

    eprintln!("rootbeer needs sudo to {reason}:{listing}");
    let script = format!("set -e\n{}\n", lines.join("\n"));
    let status = Command::new("sudo")
        .args(["sh", "-c", &script])
        .status()
        .map_err(|error| format!("running sudo: {error}"))?;
    if !status.success() {
        return Err(format!("failed to {reason} as root"));
    }
    Ok(())
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// Installs the published `rootbeer` into the user profile when running from a
/// downloaded copy, so `rb` owns and updates itself from then on.
pub fn adopt() {
    // An overridden root is a test, CI, or dev store; only a real install adopts itself.
    if cfg!(debug_assertions) || std::env::var_os("ROOTBEER_ROOT").is_some() {
        return;
    }
    let Ok((executable, Installation::Standalone)) = update::detect() else {
        return;
    };

    let bin = profile::user_dir().join("bin");
    let is_installed = bin.join("rb").exists();
    if !is_installed {
        eprintln!("installing rootbeer into your profile...");
        let request = PackageRequest::parse("rootbeer");
        let result = standalone::prepare_with_resolver(
            &[request],
            true,
            false,
            false,
            rootbeer_build::consumer::self_update_resolver_stack,
        );
        if let Err(error) = result {
            eprintln!("warning: could not install rootbeer into your profile: {error}");
            return;
        }
    }

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let is_linked = home
        .and_then(|home| link_legacy_bin(&home, &executable, &bin).ok())
        .unwrap_or(false);
    if !is_installed && !is_linked && !is_on_path(&bin) {
        eprintln!(
            "Run eval \"$({}/rb env)\" to add installed commands to this shell.",
            bin.display()
        );
    }
}

fn is_on_path(directory: &Path) -> bool {
    let Ok(directory) = fs::canonicalize(directory) else {
        return false;
    };
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|entry| fs::canonicalize(entry).is_ok_and(|entry| entry == directory))
}

// TODO(legacy-install): remove once `rb.sh` installs into ~/.rootbeer/bin are gone.
/// Replaces the old `rb.sh` install directory with a link to the profile's `bin`,
/// so existing PATH lines keep working and run the profile's `rb`.
fn link_legacy_bin(home: &Path, executable: &Path, bin: &Path) -> io::Result<bool> {
    let legacy = home.join(".rootbeer/bin");
    let Ok(metadata) = fs::symlink_metadata(&legacy) else {
        return Ok(false);
    };
    if !metadata.is_dir()
        || fs::canonicalize(legacy.join("rb")).ok().as_deref() != Some(executable)
        || fs::read_dir(&legacy)?.count() != 1
    {
        return Ok(false);
    }

    let link = home.join(".rootbeer/.bin.link");
    let _ = fs::remove_file(&link);
    symlink(bin, &link)?;
    fs::remove_file(legacy.join("rb"))?;
    fs::remove_dir(&legacy)?;
    fs::rename(link, legacy)?;
    Ok(true)
}

/// Bumped when migration learns to move entries it used to leave behind, so
/// stores migrated by an older `rb` are scanned again.
const MIGRATION_VERSION: u32 = 2;

// TODO(legacy-store): remove once stores from before /opt/rootbeer are gone.
fn migrate_legacy_store(store: &Path) -> io::Result<()> {
    let legacy = state_dir().join("store");
    let marker = legacy.join(".migrated");
    let migrated = fs::read_to_string(&marker)
        .ok()
        .and_then(|version| version.trim().parse::<u32>().ok());
    let is_current = migrated.is_some_and(|version| version >= MIGRATION_VERSION);
    if !legacy.is_dir() || is_current || fs::canonicalize(&legacy)? == fs::canonicalize(store)? {
        return Ok(());
    }

    let source = Store::new(&legacy);
    let target = Store::new(store);
    for entry in fs::read_dir(&legacy)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if let Some(pending) = name
            .strip_prefix('.')
            .and_then(|name| name.strip_suffix(".link"))
        {
            finish_interrupted(&path, &legacy.join(pending))?;
            continue;
        }
        if name.starts_with('.') || entry.file_type()?.is_symlink() {
            continue;
        }

        remove_finder_metadata(&path)?;
        let Ok(manifest) = source.verify_entry(&path) else {
            continue;
        };

        // The link is staged first so an interrupted move can still be finished.
        let destination = store.join(&name);
        let link = legacy.join(format!(".{name}.link"));
        let _ = fs::remove_file(&link);
        symlink(&destination, &link)?;
        if destination.exists() {
            target.verify_entry(&destination)?;
            fs::remove_dir_all(&path)?;
        } else {
            move_entry(&path, &destination, &target, manifest)?;
        }
        fs::rename(&link, &path)?;
    }

    fs::write(marker, format!("{MIGRATION_VERSION}\n"))
}

/// Finder drops `.DS_Store` into browsed entries, which changes their hash.
fn remove_finder_metadata(path: &Path) -> io::Result<()> {
    for child in fs::read_dir(path)? {
        let child = child?;
        let file_type = child.file_type()?;
        if file_type.is_dir() {
            remove_finder_metadata(&child.path())?;
        } else if file_type.is_file() && child.file_name() == ".DS_Store" {
            fs::remove_file(child.path())?;
        }
    }
    Ok(())
}

fn finish_interrupted(link: &Path, entry: &Path) -> io::Result<()> {
    if fs::symlink_metadata(entry).is_ok() {
        return fs::remove_file(link);
    }
    fs::rename(link, entry)
}

fn move_entry(
    path: &Path,
    destination: &Path,
    target: &Store,
    manifest: StoreManifest,
) -> io::Result<()> {
    match fs::rename(path, destination) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::CrossesDevices | io::ErrorKind::PermissionDenied
            ) =>
        {
            target.add_tree(manifest.name, manifest.version, path)?;
            fs::remove_dir_all(path)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn legacy_install(home: &Path) -> PathBuf {
        let legacy = home.join(".rootbeer/bin");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("rb"), "rb").unwrap();
        fs::canonicalize(legacy.join("rb")).unwrap()
    }

    #[test]
    fn shared_setup_is_valid_shell_and_quotes_paths() {
        for is_converting in [true, false] {
            let root = Path::new("/opt/root'beer");
            let lines = shared_setup(root, Path::new("/tmp/a b/rb-store"), is_converting);
            let script = lines.join("\n");
            let status = Command::new("sh")
                .args(["-n", "-c", &script])
                .status()
                .unwrap();

            assert!(status.success());
            assert!(script.contains("'/opt/root'\\''beer/bin/rb-store' version"));
            assert!(script.contains("install -m 4755 -o 0 -g 0 '/tmp/a b/rb-store'"));
            assert_eq!(script.contains("chown -R"), is_converting);
        }
    }

    #[test]
    fn shared_setup_records_what_the_installed_helper_reports() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(
            bin.join("rb-store"),
            "#!/bin/sh\necho '{\"release\":7,\"protocols\":[1]}'\n",
        )
        .unwrap();
        fs::set_permissions(bin.join("rb-store"), fs::Permissions::from_mode(0o755)).unwrap();

        let record = shared_setup(root.path(), Path::new("unused"), false)
            .pop()
            .unwrap();
        assert!(Command::new("sh")
            .args(["-c", &record])
            .status()
            .unwrap()
            .success());

        let helper = layout::read(root.path()).unwrap().unwrap().helper.unwrap();
        assert_eq!(helper.release, 7);
    }

    #[test]
    fn links_the_legacy_install_directory_to_the_profile() {
        let home = tempfile::tempdir().unwrap();
        let executable = legacy_install(home.path());
        let bin = home.path().join("profile/bin");
        fs::create_dir_all(&bin).unwrap();

        assert!(link_legacy_bin(home.path(), &executable, &bin).unwrap());
        assert_eq!(
            fs::read_link(home.path().join(".rootbeer/bin")).unwrap(),
            bin
        );
    }

    #[test]
    fn leaves_legacy_directories_it_does_not_own() {
        let home = tempfile::tempdir().unwrap();
        let executable = legacy_install(home.path());
        let bin = home.path().join("profile/bin");
        fs::write(home.path().join(".rootbeer/bin/other"), "").unwrap();

        assert!(!link_legacy_bin(home.path(), &executable, &bin).unwrap());
        assert!(!link_legacy_bin(home.path(), &home.path().join("elsewhere"), &bin).unwrap());
        assert!(home.path().join(".rootbeer/bin/rb").is_file());
    }
}
