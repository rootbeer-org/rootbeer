//! rb-helper is used to allow trusted users to write into the shared store.
//! It seals what is build in place and pulls archives from stdin. It's pretty
//! much a root helper.

use rootbeer_drv::Key;
use rootbeer_store::{ROOT, Store};
use rustix::fs::{Access, Mode};
use rustix::process::getuid;
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, chown, fchown};
use std::path::Path;

const SHARED: [&str; 3] = ["store", "var/build", "var/log"];
const USAGE: &str = "usage: rb-helper setup <group> | seal <key> <entry> [reference...] | \
                     pull <key> <entry> <digest> [reference...] < archive";

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
        _ => Err(USAGE.into()),
    }
}

fn authorize(root: &Path) -> Result<(), String> {
    let store = root.join("store");
    rustix::fs::access(&store, Access::WRITE_OK).map_err(|_| {
        format!(
            "only users who may write to {} may add to it",
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

/// Creates the store for `group` and installs this binary setuid in it. Run
/// once with sudo.
fn setup(root: &Path, group: &str) -> Result<String, String> {
    // The real id, since anyone running the installed copy has root's
    // effective id.
    if !getuid().is_root() {
        return Err("setup must run as root".into());
    }

    // Parents first, so once one is root's nobody else can swap what's in it.
    let trusted = group_id(group)?;
    let private = [root.to_path_buf(), root.join("var"), root.join("libexec")];
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

    Store::open(root).map_err(|error| error.to_string())?;
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
    Ok(format!(
        "installed {}; members of {group} may add to the store",
        helper.display()
    ))
}

/// The id of `name` in `/etc/group`, which setup reads once as root.
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
