//! Setuid-root helper that inserts trees into the shared store. It reads a
//! [`rootbeer_store_legacy::stream`] on stdin, never a path, and hashes what it wrote,
//! so it needs no trust in the caller. It also creates each user's roots
//! directory and runs garbage collection, which only ever reads roots.

use std::io;
use std::path::PathBuf;

use rootbeer_store_legacy::Store;

fn main() {
    if let Err(error) = run() {
        eprintln!("rb-store: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let (name, version) = match arguments.as_slice() {
        [command] if command == "version" => {
            let helper = rootbeer_store_legacy::helper::current();
            println!(
                "{}",
                serde_json::to_string(&helper).map_err(|e| e.to_string())?
            );
            return Ok(());
        }
        [command] if command == "roots" => {
            unsafe { libc::umask(0o022) };
            let dir = store().create_user_roots().map_err(|e| e.to_string())?;
            println!("{}", dir.display());
            return Ok(());
        }
        [command] if command == "gc" => {
            let report = store()
                .collect_garbage(rootbeer_store_legacy::gc::GRACE)
                .map_err(|e| e.to_string())?;
            for name in report.removed {
                println!("{name}");
            }
            return Ok(());
        }
        [command, name, version] if command == "add" => (name, version),
        _ => return Err(
            "usage: rb-store add <name> <version> < tree | rb-store roots | rb-store gc | rb-store version"
                .into(),
        ),
    };

    unsafe { libc::umask(0o022) };
    let entry = store()
        .add_stream(name, version, &mut io::stdin().lock())
        .map_err(|error| error.to_string())?;
    println!("{}", entry.path.display());
    Ok(())
}

fn store() -> Store {
    Store::new(root().join("store"))
}

// Elevated, the root is fixed; unprivileged, the caller's own override is harmless.
fn root() -> PathBuf {
    let is_elevated = unsafe { libc::geteuid() != libc::getuid() };
    if is_elevated {
        return PathBuf::from(rootbeer_store_legacy::DEFAULT_ROOT);
    }
    rootbeer_store_legacy::root_dir()
}
