//! rb-helper is used to allow trusted users to write into the shared store.
//! It seals what is built in place, pulls archives from stdin, and routes
//! garbage collection operations. It's pretty much a root helper.

use rootbeer_drv::Key;
use rootbeer_store::{Layout, ROOT, Store};
use rustix::fs::{Access, Mode, OFlags};
use rustix::process::{getgid, getuid};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{
    DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, chown, fchown,
};
use std::path::Path;

const SHARED: [&str; 3] = ["store", "var/build", "var/log"];
const LEGACY: &str = "/opt/rootbeer";
const USAGE: &str = "usage: rb-helper setup <group> | seal <key> <entry> [reference...] | \
                     pull <key> <entry> <digest> [reference...] < archive | profile | gc | \
                     remove-legacy";

fn main() {
    for (name, _) in std::env::vars_os() {
        unsafe { std::env::remove_var(name) };
    }

    let result = run();
    let written = match &result {
        Ok(output) => writeln!(io::stdout(), "{output}"),
        Err(error) => writeln!(io::stderr(), "rb-helper: {error}"),
    };

    if result.is_err() || written.is_err() {
        std::process::exit(1);
    }
}

fn run() -> Result<String, String> {
    rustix::process::umask(Mode::from_raw_mode(0o022));
    let root = Path::new(ROOT);
    let arguments = std::env::args_os()
        .skip(1)
        .map(|argument| argument.into_string())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| USAGE.to_string())?;

    match arguments.as_slice() {
        [command, group] if command == "setup" => setup(root, group),
        [command] if command == "remove-legacy" => remove_legacy(Path::new(LEGACY)),
        [command, key, entry, references @ ..] if command == "seal" => {
            authorize(root)?;
            let (key, references) = parse(key, references)?;
            let mut store = Store::open(root).map_err(|error| error.to_string())?;
            let path = store
                .seal(&key, entry, getuid().as_raw(), &references)
                .map_err(|error| error.to_string())?;

            Ok(path.display().to_string())
        }
        [command, key, entry, digest, references @ ..] if command == "pull" => {
            authorize(root)?;
            let (key, references) = parse(key, references)?;
            let mut store = Store::open(root).map_err(|error| error.to_string())?;
            let path = store
                .pull(&key, entry, digest, &references, io::stdin().lock())
                .map_err(|error| error.to_string())?;

            Ok(path.display().to_string())
        }
        [command] if command == "profile" => {
            authorize(root)?;
            profile(root)
        }
        [command] if command == "gc" => {
            authorize(root)?;
            collect(root)
        }
        _ => Err(USAGE.into()),
    }
}

/// Removes the old store once `rb migrate` has emptied every user's roots in
/// it, so nothing anyone runs comes from it.
fn remove_legacy(legacy: &Path) -> Result<String, String> {
    if !getuid().is_root() {
        return Err("remove-legacy must run as root".into());
    }

    if fs::symlink_metadata(legacy).is_err() {
        return Ok(format!("{} is already gone", legacy.display()));
    }

    let users = legacy.join("var/roots");
    let at = |path: &Path| {
        let path = path.to_path_buf();
        move |error: io::Error| format!("{}: {error}", path.display())
    };

    // Clients from before roots were recorded leave no trace rb can check.
    let entries = fs::read_dir(&users).map_err(|error| {
        format!(
            "{}: {error}, so rb can't tell whether anyone still uses {}",
            users.display(),
            legacy.display()
        )
    })?;

    let mut holding = Vec::new();
    for entry in entries {
        let user = entry.map_err(at(&users))?;
        let path = user.path();
        let is_empty = fs::read_dir(&path).map_err(at(&path))?.next().is_none();
        if !is_empty {
            holding.push(user.file_name().to_string_lossy().into_owned());
        }
    }

    if !holding.is_empty() {
        return Err(format!(
            "{} still holds packages for uid {}, so each must run `rb migrate` first",
            legacy.display(),
            holding.join(", ")
        ));
    }

    fs::remove_dir_all(legacy).map_err(at(legacy))?;
    Ok(format!("removed {}", legacy.display()))
}

fn collect(root: &Path) -> Result<String, String> {
    let profiles = root.join("profiles");
    let roots = || {
        rootbeer_profile::roots(&profiles).map_err(|error| match error {
            rootbeer_profile::Error::Newer { .. } => {
                format!("{error}, so run `rb-helper setup` from a newer rb")
            }
            error => error.to_string(),
        })
    };

    let mut store = Store::open(root).map_err(|error| error.to_string())?;
    let removed = store
        .collect(roots, || {
            eprintln!("waiting for installs and builds to finish")
        })
        .map_err(|error| error.to_string())?;

    let count = format!("removed {} store entries", removed.len());
    let lines = removed.iter().map(|entry| entry.escape_debug().to_string());
    Ok(lines.chain([count]).collect::<Vec<_>>().join("\n"))
}

fn authorize(root: &Path) -> Result<(), String> {
    let store = root.join("store");
    rustix::fs::access(&store, Access::WRITE_OK).map_err(|_| {
        format!(
            "only users who may write to {} may change it",
            store.display()
        )
    })
}

fn parse(key: &str, references: &[String]) -> Result<(Key, BTreeSet<Key>), String> {
    let key = key
        .parse()
        .map_err(|error: rootbeer_drv::Error| error.to_string())?;
    let references = references
        .iter()
        .map(|reference| reference.parse())
        .collect::<Result<BTreeSet<Key>, _>>()
        .map_err(|error| error.to_string())?;

    Ok((key, references))
}

fn profile(root: &Path) -> Result<String, String> {
    let (uid, gid) = (getuid().as_raw(), getgid().as_raw());
    let profiles = root.join("profiles");
    let directory = profiles.join(uid.to_string());
    let at = |path: &Path| {
        let path = path.to_path_buf();
        move |error: io::Error| format!("{}: {error}", path.display())
    };

    let is_root_owned = fs::symlink_metadata(&profiles)
        .is_ok_and(|metadata| metadata.is_dir() && metadata.uid() == 0);

    if !is_root_owned {
        return Err(format!(
            "{} isn't root's directory, so run `rb-helper setup` again",
            profiles.display()
        ));
    }

    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.is_dir() && metadata.uid() == uid => {
            return Ok(directory.display().to_string());
        }
        Ok(_) => return Err(format!("{} isn't your directory", directory.display())),
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            return Err(at(&directory)(error));
        }
        Err(_) => {}
    }

    let staged = profiles.join(format!(".{uid}"));
    match fs::remove_dir_all(&staged) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(at(&staged)(error)),
        _ => {}
    }

    fs::DirBuilder::new()
        .mode(0o700)
        .create(&staged)
        .map_err(at(&staged))?;

    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let opened = rustix::fs::open(&staged, flags, Mode::empty())
        .map_err(|error| at(&staged)(error.into()))?;

    fchown(&opened, Some(uid), Some(gid)).map_err(at(&staged))?;
    fs::rename(&staged, &directory).map_err(at(&directory))?;
    Ok(directory.display().to_string())
}

fn setup(root: &Path, group: &str) -> Result<String, String> {
    if !getuid().is_root() {
        return Err("setup must run as root".into());
    }

    let trusted = group_id(group)?;
    let private = [
        root.to_path_buf(),
        root.join("var"),
        root.join("libexec"),
        root.join("profiles"),
    ];

    let directories = private
        .map(|path| (path, 0, 0o755))
        .into_iter()
        .chain(SHARED.map(|shared| (root.join(shared), trusted, 0o1775)));

    for (path, gid, mode) in directories {
        let at = |error: io::Error| format!("{}: {error}", path.display());
        fs::create_dir_all(&path).map_err(at)?;
        if fs::symlink_metadata(&path).map_err(at)?.is_symlink() {
            return Err(format!("{} is a symlink", path.display()));
        }

        chown(&path, Some(0), Some(gid)).map_err(at)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).map_err(at)?;
    }

    let store = Store::open(root).map_err(|error| error.to_string())?;
    drop(store.hold().map_err(|error| error.to_string())?);
    let database = root.join("var/db.sqlite");
    let at = |error: io::Error| format!("{}: {error}", database.display());
    chown(&database, Some(0), Some(trusted)).map_err(at)?;
    fs::set_permissions(&database, fs::Permissions::from_mode(0o640)).map_err(at)?;

    let libexec = root.join("libexec");
    let helper = libexec.join("rb-helper");
    let staged = libexec.join(".rb-helper");
    let source = std::env::current_exe().map_err(|error| error.to_string())?;
    let install = || -> io::Result<()> {
        match fs::remove_file(&staged) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&staged)?;

        io::copy(&mut fs::File::open(&source)?, &mut file)?;
        fchown(&file, Some(0), Some(0))?;
        file.set_permissions(fs::Permissions::from_mode(0o4755))?;
        file.sync_all()?;

        fs::rename(&staged, &helper)
    };

    install().map_err(|error| format!("{}: {error}", helper.display()))?;
    Layout::write(root).map_err(|error| error.to_string())?;
    Ok(format!(
        "installed {}; members of {group} may add to the store",
        helper.display()
    ))
}

// TODO: This /etc/group parsing is safe across both macOS and Linux but we
// should probably consider switching to a libc call in the future.
fn group_id(name: &str) -> Result<u32, String> {
    let groups = fs::read_to_string("/etc/group").map_err(|error| error.to_string())?;
    groups
        .lines()
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| {
            let mut fields = line.split(':');
            let found = fields.next()? == name;
            let gid = fields.nth(1)?.parse().ok()?;
            found.then_some(gid)
        })
        .ok_or_else(|| format!("no group {name} in /etc/group"))
}
