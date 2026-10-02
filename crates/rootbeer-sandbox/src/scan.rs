use crate::{Error, io_at};
use goblin::elf::Elf;
use goblin::mach::{Mach, MachO, SingleArch};
use memchr::memmem;
use rootbeer_drv::{Key, STORE_ROOT};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

const FORBIDDEN: [&str; 5] = [
    "/usr/local/",
    "/opt/homebrew/",
    "/home/runner/",
    "/Users/runner/",
    "/build/",
];

const INTERPRETERS: [&str; 2] = ["/bin/sh", "/usr/bin/env"];
const SYSTEM_LIBRARIES: [&str; 2] = ["/usr/lib", "/System/Library/Frameworks"];
const MACH_ORIGINS: [&str; 2] = ["@loader_path", "@executable_path"];
const ELF_ORIGINS: [&str; 2] = ["$ORIGIN", "${ORIGIN}"];

const RUNTIME: [&str; 12] = [
    "ld-linux-aarch64.so.1",
    "ld-linux-x86-64.so.2",
    "libc.so.6",
    "libm.so.6",
    "libpthread.so.0",
    "libdl.so.2",
    "librt.so.1",
    "libutil.so.1",
    "libresolv.so.2",
    "libanl.so.1",
    "libstdc++.so.6",
    "libgcc_s.so.1",
];

const METADATA: [&str; 2] = [".pc", ".la"];
const PACKAGE_CONFIGS: [&str; 3] = ["lib/cmake/", "lib64/cmake/", "share/cmake/"];

/// The store keys `out` references. Fails when one is outside `allowed`, when a
/// symlink or build metadata names a host path, when an executable script's
/// interpreter or a binary's library is outside the store. Host paths anywhere
/// else (binaries' built-in defaults, docs, script comments) are only logged.
pub(crate) fn scan(
    key: &Key,
    out: &Path,
    allowed: &BTreeSet<Key>,
    log: &mut dyn Write,
) -> Result<BTreeSet<Key>, Error> {
    let mut references = BTreeSet::new();
    let mut problems = Vec::new();
    for entry in WalkDir::new(out)
        .follow_root_links(false)
        .sort_by_file_name()
    {
        let entry = entry.map_err(|error| io_at(out)(error.into()))?;
        let path = entry.path();
        let is_symlink = entry.path_is_symlink();

        let bytes = match is_symlink {
            true => fs::read_link(path).map(|target| target.into_os_string().into_vec()),
            false if entry.file_type().is_file() => fs::read(path),
            false => continue,
        }
        .map_err(io_at(path))?;

        let name = path.strip_prefix(out).unwrap_or(path).display().to_string();
        references.extend(keys(&bytes));

        // A shebang only matters on a file that can run
        let mode = entry
            .metadata()
            .map_err(|error| io_at(path)(error.into()))?
            .permissions()
            .mode();

        let is_script = !is_symlink && mode & 0o111 != 0 && bytes.starts_with(b"#!");
        let is_strict = is_symlink
            || METADATA.iter().any(|extension| name.ends_with(extension))
            || PACKAGE_CONFIGS
                .iter()
                .any(|directory| name.starts_with(directory));

        for needle in FORBIDDEN {
            if memmem::find(&bytes, needle.as_bytes()).is_none() {
                continue;
            }

            let problem = format!("{name} mentions {needle}");
            if is_strict {
                problems.push(problem);
            } else {
                writeln!(log, "scan warning: {problem}").map_err(io_at(path))?;
            }
        }

        if is_script && let Some(problem) = shebang(&name, &bytes) {
            problems.push(problem);
        }

        match Mach::parse(&bytes) {
            Ok(Mach::Binary(macho)) => problems.extend(linkage(&name, path, &macho)),
            Ok(Mach::Fat(fat)) => {
                for arch in &fat {
                    if let Ok(SingleArch::MachO(macho)) = arch {
                        problems.extend(linkage(&name, path, &macho));
                    }
                }
            }
            Err(_) => {}
        }

        if let Ok(elf) = Elf::parse(&bytes) {
            problems.extend(elf_linkage(&name, path, &elf));
        }
    }

    problems.extend(
        references
            .iter()
            .filter(|reference| *reference != key && !allowed.contains(reference))
            .map(|reference| format!("references {reference}, which is not in its build closure")),
    );

    if !problems.is_empty() {
        return Err(Error::Scan {
            key: key.clone(),
            out: out.to_path_buf(),
            problems,
        });
    }

    references.remove(key);
    Ok(references)
}

fn keys(bytes: &[u8]) -> Vec<Key> {
    let prefix = format!("{STORE_ROOT}/");
    memmem::find_iter(bytes, prefix.as_bytes())
        .filter_map(|at| {
            let start = at.checked_add(prefix.len())?;
            let key = bytes.get(start..start.checked_add(32)?)?;
            std::str::from_utf8(key).ok()?.parse().ok()
        })
        .collect()
}

fn shebang(name: &str, bytes: &[u8]) -> Option<String> {
    let line = bytes.get(2..)?.split(|byte| *byte == b'\n').next()?;
    let interpreter = String::from_utf8_lossy(line);
    let interpreter = interpreter.split_whitespace().next().unwrap_or_default();

    let is_allowed =
        interpreter.starts_with(&format!("{STORE_ROOT}/")) || INTERPRETERS.contains(&interpreter);
    if is_allowed {
        return None;
    }

    Some(format!(
        "{name} runs {interpreter:?}, which is outside the store"
    ))
}

fn linkage(name: &str, path: &Path, macho: &MachO) -> Vec<String> {
    let loader = path.parent().unwrap_or(path);
    macho
        .libs
        .iter()
        .skip(1) // The first entry is the binary itself
        .filter(|library| !is_loadable(library, &macho.rpaths, loader))
        .map(|library| format!("{name} loads {library}, which is outside the store"))
        .collect()
}

fn is_loadable(library: &str, rpaths: &[&str], loader: &Path) -> bool {
    if is_system(Path::new(library)) {
        return true;
    }

    let Some(rest) = library.strip_prefix("@rpath/") else {
        return is_stored(&expand(library, loader, &MACH_ORIGINS));
    };

    let directories = rpaths
        .iter()
        .map(|rpath| expand(rpath, loader, &MACH_ORIGINS));
    search(rest, directories, is_system).unwrap_or(false)
}

fn elf_linkage(name: &str, path: &Path, elf: &Elf) -> Vec<String> {
    let loader = path.parent().unwrap_or(path);

    // RUNPATH replaces RPATH when both are set.
    let search_path = match elf.runpaths.is_empty() {
        true => &elf.rpaths,
        false => &elf.runpaths,
    };

    let directories = search_path
        .iter()
        .flat_map(|entry| entry.split(':'))
        .map(|directory| expand(directory, loader, &ELF_ORIGINS))
        .collect::<Vec<_>>();

    elf.libraries
        .iter()
        .filter(|library| !is_resolvable(library, &directories))
        .map(|library| format!("{name} needs {library}, which is outside the store"))
        .collect()
}

fn is_resolvable(library: &str, directories: &[PathBuf]) -> bool {
    if library.contains('/') {
        return is_stored(Path::new(library));
    }

    let is_runtime = RUNTIME.contains(&library);
    search(library, directories, |directory| {
        is_runtime && is_system(directory)
    })
    .unwrap_or(is_runtime)
}

/// Whether the loader finds `name` in the store, searching `directories` in
/// its order. Any other directory ahead of the store fails unless `is_allowed`.
/// None means no directory has it.
fn search(
    name: &str,
    directories: impl IntoIterator<Item = impl AsRef<Path>>,
    is_allowed: impl Fn(&Path) -> bool,
) -> Option<bool> {
    directories.into_iter().find_map(|directory| {
        let directory = directory.as_ref();
        if is_stored(&directory.join(name)) {
            return Some(true);
        }

        let is_host = !directory.starts_with(STORE_ROOT) && !is_allowed(directory);
        is_host.then_some(false)
    })
}

fn is_system(path: &Path) -> bool {
    let is_lexical = !path.components().any(|part| part == Component::ParentDir);
    is_lexical
        && SYSTEM_LIBRARIES
            .iter()
            .any(|prefix| path.starts_with(prefix))
}

fn is_stored(path: &Path) -> bool {
    path.is_absolute()
        && fs::canonicalize(path).is_ok_and(|resolved| resolved.starts_with(STORE_ROOT))
}

fn expand(path: &str, loader: &Path, origins: &[&str]) -> PathBuf {
    let relative = origins
        .iter()
        .filter_map(|origin| path.strip_prefix(origin))
        .find(|rest| rest.is_empty() || rest.starts_with('/'));

    match relative {
        Some(rest) => loader.join(rest.trim_start_matches('/')),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn scan_fails_on_host_dependencies_and_foreign_references() {
        let own: Key = "a".repeat(32).parse().unwrap();
        let allowed: Key = "b".repeat(32).parse().unwrap();
        let foreign: Key = "c".repeat(32).parse().unwrap();
        let store = |key: &Key| format!("{STORE_ROOT}/{key}-x-1");

        let out = tempfile::tempdir().unwrap();
        let file = |name: &str, text: String| fs::write(out.path().join(name), text).unwrap();
        file("own", store(&own));
        file("allowed", store(&allowed));
        file("foreign", store(&foreign));
        file("script", "#!/usr/bin/python3\n".into());
        file(
            "env-script",
            "#! /usr/bin/env perl -w\n# installs to /usr/local/bin\n".into(),
        );
        file("module.pm", "#!perl -w\n".into());
        for script in ["script", "env-script"] {
            fs::set_permissions(out.path().join(script), fs::Permissions::from_mode(0o755))
                .unwrap();
        }

        file("zlib.pc", "prefix=/opt/homebrew/opt/zlib\n".into());
        file(
            "README",
            "Installs into /usr/local/lib by default.\n".into(),
        );
        file(
            "FindY.cmake",
            "find_path(Y PATHS /usr/local/include)\n".into(),
        );
        fs::create_dir_all(out.path().join("lib/cmake/x")).unwrap();
        file(
            "lib/cmake/x/x-config.cmake",
            "set(X_DIR /opt/rb/var/build/k/src)\n".into(),
        );
        symlink("/usr/local/bin/tool", out.path().join("link")).unwrap();

        let mut log = Vec::new();
        let error = scan(&own, out.path(), &BTreeSet::from([allowed]), &mut log).unwrap_err();

        let Error::Scan { problems, .. } = error else {
            panic!("{error}");
        };
        assert_eq!(
            problems,
            [
                "lib/cmake/x/x-config.cmake mentions /build/",
                "link mentions /usr/local/",
                "script runs \"/usr/bin/python3\", which is outside the store",
                "zlib.pc mentions /opt/homebrew/",
                &format!("references {foreign}, which is not in its build closure"),
            ]
        );
        assert_eq!(
            String::from_utf8(log).unwrap(),
            "scan warning: FindY.cmake mentions /usr/local/\nscan warning: README mentions /usr/local/\n\
             scan warning: env-script mentions /usr/local/\n"
        );
    }

    #[test]
    fn elf_libraries_resolve_in_loader_order() {
        let cases: [(&str, &[&str], bool); 6] = [
            ("libc.so.6", &[], true),
            ("libc.so.6", &["/usr/lib/x86_64-linux-gnu"], true),
            ("libc.so.6", &["/usr/local/lib"], false),
            ("libc.so.6", &[""], false),
            ("libz.so.1", &[], false),
            ("/usr/lib/libc.so.6", &[], false),
        ];

        for (library, directories, expected) in cases {
            let directories = directories.iter().map(PathBuf::from).collect::<Vec<_>>();
            assert_eq!(is_resolvable(library, &directories), expected, "{library}");
        }
    }

    #[test]
    fn origins_expand_only_as_whole_tokens() {
        let loader = Path::new("/opt/rb/store/k/bin");
        let cases = [
            ("$ORIGIN/../lib", "/opt/rb/store/k/bin/../lib"),
            ("${ORIGIN}", "/opt/rb/store/k/bin"),
            ("$ORIGINAL/lib", "$ORIGINAL/lib"),
        ];

        for (path, expected) in cases {
            assert_eq!(expand(path, loader, &ELF_ORIGINS), Path::new(expected));
        }
    }
}
