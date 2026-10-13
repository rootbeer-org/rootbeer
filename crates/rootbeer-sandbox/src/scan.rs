use crate::{Error, io_at};
use goblin::elf::Elf;
use goblin::mach::header::MH_EXECUTE;
use goblin::mach::{Mach, MachO, SingleArch};
use memchr::memmem;
use rootbeer_drv::{Key, STORE_ROOT};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::iter;
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
const SYSTEM_LIBRARIES: [&str; 4] = [
    "/usr/lib",
    "/System/Library/Frameworks",
    "/System/Library/PrivateFrameworks",
    "/System/iOSSupport",
];

const LOADER_PATH: [&str; 1] = ["@loader_path"];
const EXECUTABLE_PATH: [&str; 1] = ["@executable_path"];
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

struct Problems<'a> {
    host: &'a BTreeSet<String>,
    acknowledged: BTreeSet<&'a str>,
    list: Vec<String>,
}

impl<'a> Problems<'a> {
    fn host(&mut self, reference: &str, problem: String) {
        match self.host.get(reference) {
            Some(reference) => {
                self.acknowledged.insert(reference);
            }
            None => self.list.push(problem),
        }
    }

    fn unused(&self) -> impl Iterator<Item = &'a String> {
        self.host
            .iter()
            .filter(|reference| !self.acknowledged.contains(reference.as_str()))
    }
}

pub(crate) fn scan(
    key: &Key,
    out: &Path,
    allowed: &BTreeSet<Key>,
    host: &BTreeSet<String>,
    is_runnable: bool,
    log: &mut dyn Write,
) -> Result<BTreeSet<Key>, Error> {
    let mut references = BTreeSet::new();
    let mut problems = Problems {
        host,
        acknowledged: BTreeSet::new(),
        list: Vec::new(),
    };
    let mut images = Vec::new();
    let mut objects = Vec::new();
    let stored = Path::new(STORE_ROOT).join(out.file_name().unwrap_or_default());
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
        if !is_runnable {
            continue;
        }

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

        let mut is_mentioned = false;
        for needle in FORBIDDEN {
            if memmem::find(&bytes, needle.as_bytes()).is_none() {
                continue;
            }

            is_mentioned = true;
            let problem = format!("{name} mentions {needle}");
            if is_strict {
                problems.list.push(problem);
            } else {
                writeln!(log, "scan warning: {problem}").map_err(io_at(path))?;
            }
        }

        if is_symlink
            && !is_mentioned
            && let Some(target) =
                host_target(&stored.join(path.strip_prefix(out).unwrap_or(path)), &bytes)
        {
            let problem = format!("{name} links to {target}, which is outside the store");
            problems.host(&target, problem);
        }

        if is_script && let Some((interpreter, problem)) = shebang(&name, &bytes) {
            problems.host(&interpreter, problem);
        }

        match Mach::parse(&bytes) {
            Ok(Mach::Binary(macho)) => images.push(Image::new(&name, path, &macho)),
            Ok(Mach::Fat(fat)) => {
                for arch in &fat {
                    if let Ok(SingleArch::MachO(macho)) = arch {
                        images.push(Image::new(&name, path, &macho));
                    }
                }
            }
            Err(_) => {}
        }

        if let Ok(elf) = Elf::parse(&bytes) {
            objects.push(Object::new(name, path, &elf));
        }
    }

    for (library, problem) in mach_linkage(&images)
        .into_iter()
        .chain(elf_linkage(&objects))
    {
        problems.host(&library, problem);
    }

    problems.list.extend(
        references
            .iter()
            .filter(|reference| *reference != key && !allowed.contains(reference))
            .map(|reference| format!("references {reference}, which is not in its build closure")),
    );

    if is_runnable {
        for reference in problems.unused() {
            writeln!(
                log,
                "scan warning: host reference {reference:?} matches nothing in the output"
            )
            .map_err(io_at(out))?;
        }
    }

    if !problems.list.is_empty() {
        return Err(Error::Scan {
            key: key.clone(),
            out: out.to_path_buf(),
            problems: problems.list,
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

// A symlink outside the store points at the host, or at a build directory
// that is gone once the build ends. A relative one resolves from where the
// link lands in the store.
fn host_target(link: &Path, bytes: &[u8]) -> Option<String> {
    let target = String::from_utf8_lossy(bytes);
    let resolved = link.parent()?.join(target.as_ref());
    let is_host = !normalize(&resolved).starts_with(STORE_ROOT);
    is_host.then(|| target.into_owned())
}

fn shebang(name: &str, bytes: &[u8]) -> Option<(String, String)> {
    let line = bytes.get(2..)?.split(|byte| *byte == b'\n').next()?;
    let interpreter = String::from_utf8_lossy(line);
    let interpreter = interpreter.split_whitespace().next().unwrap_or_default();

    let is_allowed =
        interpreter.starts_with(&format!("{STORE_ROOT}/")) || INTERPRETERS.contains(&interpreter);
    if is_allowed {
        return None;
    }

    Some((
        interpreter.to_string(),
        format!("{name} runs {interpreter:?}, which is outside the store"),
    ))
}

struct Image {
    name: String,
    path: PathBuf,
    arch: (u32, u32),
    libraries: Vec<String>,
    rpaths: Vec<String>,
    is_executable: bool,
    install_name: Option<String>,
}

impl Image {
    fn new(name: &str, path: &Path, macho: &MachO) -> Image {
        Image {
            name: name.to_string(),
            path: path.to_path_buf(),
            arch: (macho.header.cputype(), macho.header.cpusubtype()),
            libraries: macho.libs.iter().skip(1).map(ToString::to_string).collect(),
            rpaths: macho.rpaths.iter().map(ToString::to_string).collect(),
            is_executable: macho.header.filetype == MH_EXECUTE,
            install_name: macho.name.map(ToString::to_string),
        }
    }

    fn directory(&self) -> &Path {
        self.path.parent().unwrap_or(&self.path)
    }

    fn is_public(&self) -> bool {
        self.install_name
            .as_deref()
            .is_some_and(|name| Path::new(name).is_absolute())
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Process {
    executable: Option<PathBuf>,
    rpaths: Vec<PathBuf>,
}

fn mach_linkage(images: &[Image]) -> Vec<(String, String)> {
    images
        .iter()
        .zip(mach_processes(images))
        .flat_map(|(image, searches)| {
            image
                .libraries
                .iter()
                .filter(move |library| {
                    !searches
                        .is_resolvable(|process| is_loadable(library, image.directory(), process))
                })
                .map(|library| {
                    let problem =
                        format!("{} loads {library}, which is outside the store", image.name);
                    (library.clone(), problem)
                })
        })
        .collect()
}

fn mach_processes(images: &[Image]) -> Vec<Searches<Process>> {
    let paths = images
        .iter()
        .enumerate()
        .filter_map(|(index, image)| {
            let path = fs::canonicalize(&image.path).ok()?;
            Some(((path, image.arch), index))
        })
        .collect::<HashMap<_, _>>();

    let executables = images
        .iter()
        .enumerate()
        .filter(|(_, image)| image.is_executable)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();

    let mains = |root: usize| {
        let is_private = images.get(root).is_some_and(|image| !image.is_public());
        iter::once(None)
            .chain(executables.iter().copied().filter(|_| is_private).map(Some))
            .collect()
    };

    processes(images.len(), &executables, mains, |root, main| {
        load(
            root,
            main,
            |chain| process(images, chain, main),
            |index, process| {
                let Some(image) = images.get(index) else {
                    return Vec::new();
                };

                image
                    .libraries
                    .iter()
                    .filter_map(|library| resolve(library, image.directory(), process))
                    .filter_map(|path| fs::canonicalize(path).ok())
                    .filter_map(|path| paths.get(&(path, image.arch)).copied())
                    .collect()
            },
        )
    })
}

fn process(images: &[Image], chain: &[usize], main: Option<usize>) -> Process {
    let executable = main
        .and_then(|main| images.get(main))
        .map(|main| main.directory().to_path_buf());

    let rpaths = chain
        .iter()
        .filter_map(|at| images.get(*at))
        .flat_map(|image| {
            image
                .rpaths
                .iter()
                .map(|rpath| expand_mach(rpath, image.directory(), executable.as_deref()))
        })
        .collect();

    Process { executable, rpaths }
}

fn resolve(library: &str, loader: &Path, process: &Process) -> Option<PathBuf> {
    let executable = process.executable.as_deref();
    let Some(rest) = library.strip_prefix("@rpath/") else {
        return Some(expand_mach(library, loader, executable));
    };

    search_rpaths(rest, &process.rpaths, |path| path.exists())
}

fn search_rpaths(
    rest: &str,
    rpaths: &[PathBuf],
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let mut system = None;
    for directory in rpaths {
        let path = directory.join(rest);
        let is_system = is_system(directory);
        let is_host = !is_system && !is_store_directory(directory);
        if is_host || exists(&path) {
            return Some(path);
        }

        if is_system {
            system.get_or_insert(path);
        }
    }

    system
}

fn is_loadable(library: &str, loader: &Path, process: &Process) -> bool {
    resolve(library, loader, process).is_some_and(|path| is_system(&path) || is_stored(&path))
}

fn expand_mach(path: &str, loader: &Path, executable: Option<&Path>) -> PathBuf {
    match executable {
        Some(executable) if path.starts_with(EXECUTABLE_PATH[0]) => {
            expand(path, executable, &EXECUTABLE_PATH)
        }
        _ => expand(path, loader, &LOADER_PATH),
    }
}

struct Object {
    name: String,
    path: PathBuf,
    needed: Vec<String>,
    rpath: Vec<PathBuf>,
    runpath: Option<Vec<PathBuf>>,
    is_executable: bool,
    has_soname: bool,
}

impl Object {
    fn new(name: String, path: &Path, elf: &Elf) -> Object {
        let origin = path.parent().unwrap_or(path);
        let directories = |entries: &[&str]| {
            entries
                .iter()
                .flat_map(|entry| entry.split(':'))
                .map(|directory| expand(directory, origin, &ELF_ORIGINS))
                .collect::<Vec<_>>()
        };

        let (rpath, runpath) = search_lists(&elf.rpaths, &elf.runpaths);
        Object {
            name,
            path: path.to_path_buf(),
            needed: elf.libraries.iter().map(ToString::to_string).collect(),
            rpath: directories(rpath),
            runpath: runpath.map(directories),
            is_executable: elf.interpreter.is_some(),
            has_soname: elf.soname.is_some(),
        }
    }
}

// glibc ignores RPATH when RUNPATH is set, also for the objects it loads
fn search_lists<'a>(
    rpath: &'a [&'a str],
    runpath: &'a [&'a str],
) -> (&'a [&'a str], Option<&'a [&'a str]>) {
    match runpath.is_empty() {
        true => (rpath, None),
        false => (&[], Some(runpath)),
    }
}

struct Searches<C> {
    loaded: BTreeSet<C>,
    opened: BTreeSet<C>,
}

impl<C> Searches<C> {
    fn is_resolvable(&self, resolves: impl Fn(&C) -> bool) -> bool {
        if !self.loaded.is_empty() {
            return self.loaded.iter().all(&resolves);
        }

        self.opened.iter().any(resolves)
    }
}

fn elf_linkage(objects: &[Object]) -> Vec<(String, String)> {
    objects
        .iter()
        .zip(search_paths(objects))
        .flat_map(|(object, searches)| {
            object
                .needed
                .iter()
                .filter(move |library| {
                    !searches.is_resolvable(|directories| is_resolvable(library, directories))
                })
                .map(|library| {
                    let problem = format!(
                        "{} needs {library}, which is outside the store",
                        object.name
                    );
                    (library.clone(), problem)
                })
        })
        .collect()
}

fn search_paths(objects: &[Object]) -> Vec<Searches<Vec<PathBuf>>> {
    let paths = objects
        .iter()
        .enumerate()
        .filter_map(|(index, object)| Some((fs::canonicalize(&object.path).ok()?, index)))
        .collect::<HashMap<_, _>>();

    let executables = objects
        .iter()
        .enumerate()
        .filter(|(_, object)| object.is_executable)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();

    // Any process may dlopen an object no executable links. A plugin without
    // a SONAME belongs to the package, so the RPATH of any bundled executable
    // may apply. A library with a SONAME is public, and searches only its own
    // RPATH or RUNPATH.
    let hosts = executables
        .iter()
        .copied()
        .filter(|index| {
            objects
                .get(*index)
                .is_some_and(|main| !main.rpath.is_empty())
        })
        .collect::<Vec<_>>();
    let mains = |root: usize| {
        let is_plugin = objects.get(root).is_some_and(|object| !object.has_soname);
        iter::once(None)
            .chain(hosts.iter().copied().filter(|_| is_plugin).map(Some))
            .collect()
    };

    processes(objects.len(), &executables, mains, |root, main| {
        load(
            root,
            main,
            |chain| directories(objects, chain),
            |index, directories| {
                objects
                    .get(index)
                    .map(|object| {
                        object
                            .needed
                            .iter()
                            .filter_map(|library| locate(library, directories, &paths))
                            .collect()
                    })
                    .unwrap_or_default()
            },
        )
    })
}

fn directories(objects: &[Object], chain: &[usize]) -> Vec<PathBuf> {
    let runpath = chain
        .first()
        .and_then(|index| objects.get(*index))
        .and_then(|object| object.runpath.clone());
    if let Some(runpath) = runpath {
        return runpath;
    }

    chain
        .iter()
        .filter_map(|at| objects.get(*at))
        .flat_map(|loader| loader.rpath.iter().cloned())
        .collect()
}

fn processes<C: Ord>(
    count: usize,
    executables: &[usize],
    mains: impl Fn(usize) -> Vec<Option<usize>>,
    load: impl Fn(usize, Option<usize>) -> Vec<(usize, C)>,
) -> Vec<Searches<C>> {
    let mut loaded = HashMap::<_, BTreeSet<_>>::new();
    for &executable in executables {
        for (index, context) in load(executable, Some(executable)) {
            loaded.entry(index).or_default().insert(context);
        }
    }

    let mut opened = HashMap::<_, BTreeSet<_>>::new();
    for root in (0..count).filter(|root| !loaded.contains_key(root)) {
        for main in mains(root) {
            for (index, context) in load(root, main) {
                opened.entry(index).or_default().insert(context);
            }
        }
    }

    (0..count)
        .map(|index| Searches {
            loaded: loaded.remove(&index).unwrap_or_default(),
            opened: opened.remove(&index).unwrap_or_default(),
        })
        .collect()
}

fn load<C>(
    root: usize,
    main: Option<usize>,
    context: impl Fn(&[usize]) -> C,
    dependencies: impl Fn(usize, &C) -> Vec<usize>,
) -> Vec<(usize, C)> {
    let mut loaders = HashMap::from([(root, None)]);
    let mut queue = VecDeque::from([root]);
    let mut loaded = Vec::new();
    while let Some(index) = queue.pop_front() {
        let mut chain = iter::successors(Some(index), |at| loaders.get(at).copied().flatten())
            .collect::<Vec<_>>();
        if let Some(main) = main
            && !chain.contains(&main)
        {
            chain.push(main);
        }

        let context = context(&chain);
        for dependency in dependencies(index, &context) {
            if loaders.contains_key(&dependency) {
                continue;
            }

            loaders.insert(dependency, Some(index));
            queue.push_back(dependency);
        }

        loaded.push((index, context));
    }

    loaded
}

fn locate(
    library: &str,
    directories: &[PathBuf],
    paths: &HashMap<PathBuf, usize>,
) -> Option<usize> {
    let path = match library.contains('/') {
        true if Path::new(library).is_absolute() => fs::canonicalize(library).ok()?,
        true => return None,
        false => directories
            .iter()
            .filter(|directory| directory.is_absolute())
            .find_map(|directory| fs::canonicalize(directory.join(library)).ok())?,
    };

    paths.get(&path).copied()
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

        let is_host = !is_store_directory(directory) && !is_allowed(directory);
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

fn is_store_directory(directory: &Path) -> bool {
    directory.is_absolute()
        && normalize(directory).starts_with(STORE_ROOT)
        && real_path(directory).is_some_and(|real| real.starts_with(STORE_ROOT))
}

fn real_path(path: &Path) -> Option<PathBuf> {
    let ancestor = path
        .ancestors()
        .find(|ancestor| ancestor.symlink_metadata().is_ok())?;
    let rest = path.strip_prefix(ancestor).ok()?;
    Some(normalize(&fs::canonicalize(ancestor).ok()?.join(rest)))
}

fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normal.pop();
            }
            Component::CurDir => {}
            component => normal.push(component),
        }
    }

    normal
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
    use goblin::mach::constants::cputype::{
        CPU_SUBTYPE_ARM64_ALL, CPU_SUBTYPE_ARM64_E, CPU_TYPE_ARM64,
    };
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
        let error = scan(
            &own,
            out.path(),
            &BTreeSet::from([allowed.clone()]),
            &BTreeSet::new(),
            true,
            &mut log,
        )
        .unwrap_err();

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

        let mut log = Vec::new();
        let error = scan(
            &own,
            out.path(),
            &BTreeSet::from([allowed]),
            &BTreeSet::new(),
            false,
            &mut log,
        )
        .unwrap_err();
        let Error::Scan { problems, .. } = error else {
            panic!("{error}");
        };
        assert_eq!(
            problems,
            [format!(
                "references {foreign}, which is not in its build closure"
            )]
        );
        assert!(log.is_empty());
    }

    // A shared object that needs `needed` and names no SONAME or interpreter.
    // The dynamic section starts at 176 and the string table at 512, in one
    // 4 KiB segment.
    fn elf(needed: &[&str]) -> Vec<u8> {
        let length = |bytes: &[u8]| u64::try_from(bytes.len()).unwrap();
        let mut strings = vec![0u8];
        let mut entries = Vec::new();
        for name in needed {
            entries.push((1u64, length(&strings)));
            strings.extend(name.as_bytes());
            strings.push(0);
        }

        entries.extend([(5, 512), (10, length(&strings)), (0, 0)]);

        let mut bytes = b"\x7fELF\x02\x01\x01".to_vec();
        bytes.resize(16, 0);
        bytes.extend(3u16.to_le_bytes());
        bytes.extend(0xb7u16.to_le_bytes());
        bytes.extend(1u32.to_le_bytes());
        for word in [0u64, 64, 0] {
            bytes.extend(word.to_le_bytes());
        }

        bytes.extend(0u32.to_le_bytes());
        for half in [64u16, 56, 2, 64, 0, 0] {
            bytes.extend(half.to_le_bytes());
        }

        for (kind, offset, size) in [(1u32, 0u64, 4096u64), (2, 176, 336)] {
            bytes.extend(kind.to_le_bytes());
            bytes.extend(4u32.to_le_bytes());
            for word in [offset, offset, offset, size, size, 8] {
                bytes.extend(word.to_le_bytes());
            }
        }

        for (tag, value) in entries {
            bytes.extend(tag.to_le_bytes());
            bytes.extend(value.to_le_bytes());
        }

        bytes.resize(512, 0);
        bytes.extend(strings);
        bytes.resize(4096, 0);
        bytes
    }

    fn problems(out: &Path, host: &[&str], log: &mut Vec<u8>) -> Vec<String> {
        let own: Key = "a".repeat(32).parse().unwrap();
        let host = host.iter().map(ToString::to_string).collect();
        match scan(&own, out, &BTreeSet::new(), &host, true, log) {
            Ok(_) => Vec::new(),
            Err(Error::Scan { problems, .. }) => problems,
            Err(error) => panic!("{error}"),
        }
    }

    #[test]
    fn acknowledged_host_references_pass_and_others_still_fail() {
        let out = tempfile::tempdir().unwrap();
        fs::create_dir_all(out.path().join("bin")).unwrap();
        fs::create_dir_all(out.path().join("lib")).unwrap();
        fs::write(
            out.path().join("lib/plugin.so"),
            elf(&["libX11.so.6", "libxcb.so.1"]),
        )
        .unwrap();

        let script = out.path().join("bin/run");
        fs::write(&script, "#!/bin/bash\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        let mut log = Vec::new();
        assert_eq!(
            problems(out.path(), &[], &mut log),
            [
                "bin/run runs \"/bin/bash\", which is outside the store",
                "lib/plugin.so needs libX11.so.6, which is outside the store",
                "lib/plugin.so needs libxcb.so.1, which is outside the store",
            ]
        );
        assert!(log.is_empty());

        let host = ["/bin/bash", "libX11.so.6", "libunused.so.1"];
        assert_eq!(
            problems(out.path(), &host, &mut log),
            ["lib/plugin.so needs libxcb.so.1, which is outside the store"]
        );
        assert_eq!(
            String::from_utf8(log).unwrap(),
            "scan warning: host reference \"libunused.so.1\" matches nothing in the output\n"
        );

        let host = ["/bin/bash", "libX11.so.6", "libxcb.so.1"];
        assert!(problems(out.path(), &host, &mut Vec::new()).is_empty());
    }

    #[test]
    fn absolute_symlinks_outside_the_store_fail() {
        let out = tempfile::tempdir().unwrap();
        let own = format!("{STORE_ROOT}/{}-x-1/bin/x", "a".repeat(32));
        let links = [
            ("applications", "/Applications"),
            ("build", "/opt/rb/var/build/k/tmp/dmg/Viewer.app"),
            ("escape", "/opt/rb/store/../../../Applications"),
            ("own", own.as_str()),
            ("relative", "../Frameworks"),
        ];
        for (name, target) in links {
            symlink(target, out.path().join(name)).unwrap();
        }

        assert_eq!(
            problems(out.path(), &[], &mut Vec::new()),
            [
                "applications links to /Applications, which is outside the store",
                "build mentions /build/",
                "escape links to /opt/rb/store/../../../Applications, which is outside the store",
            ]
        );

        let host = ["/Applications", "/opt/rb/store/../../../Applications"];
        assert_eq!(
            problems(out.path(), &host, &mut Vec::new()),
            ["build mentions /build/"]
        );
    }

    #[test]
    fn relative_symlinks_that_climb_out_of_the_store_fail() {
        let out = tempfile::tempdir().unwrap();
        fs::create_dir_all(out.path().join("bin")).unwrap();
        fs::create_dir_all(out.path().join("libexec")).unwrap();
        symlink("../libexec/x", out.path().join("bin/x")).unwrap();
        symlink("../../../../Applications", out.path().join("applications")).unwrap();

        assert_eq!(
            problems(out.path(), &[], &mut Vec::new()),
            ["applications links to ../../../../Applications, which is outside the store"]
        );

        let host = ["../../../../Applications"];
        assert!(problems(out.path(), &host, &mut Vec::new()).is_empty());
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
    fn elf_rpaths_follow_the_loader_chain() {
        let out = tempfile::tempdir().unwrap();
        let out = fs::canonicalize(out.path()).unwrap();
        fs::create_dir_all(out.join("bin")).unwrap();
        fs::create_dir_all(out.join("lib")).unwrap();
        let lib = out.join("bin/../lib");
        let own = PathBuf::from("/own");
        let run = out.join("lib/../lib");

        let object =
            |name: &str, needed: &[&str], rpath: &[&PathBuf], runpath: Option<&PathBuf>| {
                fs::write(out.join(name), "").unwrap();
                Object {
                    name: name.into(),
                    path: out.join(name),
                    needed: needed.iter().map(ToString::to_string).collect(),
                    rpath: rpath.iter().map(|path| path.to_path_buf()).collect(),
                    runpath: runpath.map(|path| vec![path.clone()]),
                    is_executable: name.starts_with("bin/"),
                    has_soname: name.starts_with("lib/lib"),
                }
            };

        // app has RPATH, tool has RUNPATH, and nothing links plugin or public
        let objects = [
            object(
                "bin/app",
                &["libinherits.so", "librunpath.so"],
                &[&lib],
                None,
            ),
            object("bin/tool", &["libtool.so"], &[], Some(&lib)),
            object("lib/libinherits.so", &[], &[&own], None),
            object("lib/librunpath.so", &["libbelow.so"], &[], Some(&run)),
            object("lib/libbelow.so", &[], &[], None),
            object("lib/libtool.so", &[], &[], None),
            object("lib/plugin.so", &[], &[], None),
            object("lib/libpublic.so", &[], &[], None),
        ];

        let searches = search_paths(&objects);
        let loaded = |index: usize| searches[index].loaded.iter().cloned().collect::<Vec<_>>();
        let opened = |index: usize| searches[index].opened.iter().cloned().collect::<Vec<_>>();

        assert_eq!(loaded(0), [vec![lib.clone()]]);
        assert_eq!(loaded(1), [vec![lib.clone()]]);
        assert_eq!(loaded(2), [vec![own, lib.clone()]], "own RPATH, then app's");
        assert_eq!(loaded(3), [vec![run]], "RUNPATH blocks inheritance");
        assert_eq!(
            loaded(4),
            [vec![lib.clone()]],
            "app's RPATH, not the RUNPATH"
        );
        assert_eq!(
            loaded(5),
            [Vec::<PathBuf>::new()],
            "tool's RUNPATH is not inherited"
        );
        assert!(opened(5).is_empty());
        assert!(loaded(6).is_empty());
        assert_eq!(opened(6), [vec![], vec![lib]], "dlopened by any process");
        assert_eq!(opened(7), [Vec::<PathBuf>::new()], "a SONAME gets no host");
    }

    #[test]
    fn runpath_hides_rpath() {
        let (rpath, runpath) = search_lists(&["/rpath"], &["/runpath"]);
        assert!(rpath.is_empty());
        assert_eq!(runpath, Some(&["/runpath"][..]));

        let (rpath, runpath) = search_lists(&["/rpath"], &[]);
        assert_eq!(rpath, ["/rpath"]);
        assert_eq!(runpath, None);
    }

    #[test]
    fn linked_objects_resolve_in_every_process_and_dlopened_in_one() {
        let directories = BTreeSet::from([vec![], vec![PathBuf::from("/usr/local/lib")]]);
        let linked = Searches {
            loaded: directories.clone(),
            opened: BTreeSet::new(),
        };
        let dlopened = Searches {
            loaded: BTreeSet::new(),
            opened: directories,
        };

        let resolves = |directories: &Vec<PathBuf>| is_resolvable("libc.so.6", directories);
        assert!(!linked.is_resolvable(resolves));
        assert!(dlopened.is_resolvable(resolves));
    }

    #[test]
    fn mach_rpaths_follow_the_loader_chain_to_the_executable() {
        let out = tempfile::tempdir().unwrap();
        let out = fs::canonicalize(out.path()).unwrap();
        for directory in ["MacOS", "Frameworks/inner", "PlugIns"] {
            fs::create_dir_all(out.join(directory)).unwrap();
        }

        let image = |name: &str, libraries: &[&str], rpaths: &[&str]| {
            fs::write(out.join(name), "").unwrap();
            let file = Path::new(name).file_name().unwrap().to_string_lossy();
            Image {
                name: name.into(),
                path: out.join(name),
                arch: (0, 0),
                libraries: libraries.iter().map(ToString::to_string).collect(),
                rpaths: rpaths.iter().map(ToString::to_string).collect(),
                is_executable: name.starts_with("MacOS/"),
                install_name: match file.strip_prefix("libpublic") {
                    Some(_) => Some(format!("{STORE_ROOT}/k/lib/{file}")),
                    None => name.ends_with(".dylib").then(|| format!("@rpath/{file}")),
                },
            }
        };

        let images = [
            image(
                "MacOS/app",
                &["@rpath/libouter.dylib"],
                &["@executable_path/../Frameworks"],
            ),
            image(
                "Frameworks/libouter.dylib",
                &["@rpath/libinner.dylib"],
                &["@loader_path/inner"],
            ),
            image("Frameworks/inner/libinner.dylib", &[], &[]),
            image("PlugIns/plugin.so", &[], &["@loader_path"]),
            image("Frameworks/libpublic.dylib", &[], &[]),
            image("Frameworks/libprivate.dylib", &[], &[]),
        ];

        let searches = mach_processes(&images);
        let process = |executable: Option<&str>, rpaths: &[&str]| Process {
            executable: executable.map(|directory| out.join(directory)),
            rpaths: rpaths.iter().map(|rpath| out.join(rpath)).collect(),
        };

        let app = Some("MacOS");
        let frameworks = "MacOS/../Frameworks";
        assert_eq!(
            searches[2].loaded.iter().collect::<Vec<_>>(),
            [&process(app, &["Frameworks/inner", frameworks])],
            "the loader's LC_RPATH, then the executable's"
        );
        assert_eq!(
            searches[3].opened.iter().collect::<Vec<_>>(),
            [
                &process(None, &["PlugIns"]),
                &process(app, &["PlugIns", frameworks]),
            ],
            "any executable may load a bundle"
        );
        assert_eq!(
            searches[4].opened.iter().collect::<Vec<_>>(),
            [&process(None, &[])],
            "an absolute install name gets no executable"
        );
        assert_eq!(
            searches[5].opened.iter().collect::<Vec<_>>(),
            [&process(None, &[]), &process(app, &[frameworks])],
            "an @rpath install name is private to the package"
        );

        let slice = |arch: (u32, u32), name: &str, libraries: &[&str]| Image {
            arch,
            ..image(name, libraries, &[])
        };

        let arm64 = (CPU_TYPE_ARM64, CPU_SUBTYPE_ARM64_ALL);
        let arm64e = (CPU_TYPE_ARM64, CPU_SUBTYPE_ARM64_E);
        let images = [
            slice(
                arm64,
                "MacOS/arm64",
                &["@executable_path/../Frameworks/libslice.dylib"],
            ),
            slice(arm64, "Frameworks/libslice.dylib", &[]),
            slice(arm64e, "Frameworks/libslice.dylib", &[]),
        ];

        let searches = mach_processes(&images);
        assert_eq!(
            searches[1].loaded.iter().collect::<Vec<_>>(),
            [&process(app, &[])],
            "the executable loads its own slice"
        );
        assert!(searches[2].loaded.is_empty(), "not the arm64e slice");
        assert!(!searches[2].opened.is_empty());
    }

    #[test]
    fn only_absolute_install_names_are_public() {
        let cases = [
            (Some("/opt/rb/store/k/lib/libx.dylib"), true),
            (Some("@rpath/libx.dylib"), false),
            (Some("@loader_path/libx.dylib"), false),
            (Some("@executable_path/../Frameworks/libx.dylib"), false),
            (Some("libx.dylib"), false),
            (None, false),
        ];

        for (install_name, expected) in cases {
            let image = Image {
                name: String::new(),
                path: PathBuf::new(),
                arch: (0, 0),
                libraries: Vec::new(),
                rpaths: Vec::new(),
                is_executable: false,
                install_name: install_name.map(ToString::to_string),
            };
            assert_eq!(image.is_public(), expected, "{install_name:?}");
        }
    }

    #[test]
    fn real_paths_follow_links_and_refuse_dangling_ones() {
        let root = tempfile::tempdir().unwrap();
        let usr = fs::canonicalize("/usr").unwrap();
        symlink("/usr", root.path().join("linked")).unwrap();
        symlink("/nonexistent/lib", root.path().join("dangling")).unwrap();

        let linked = root.path().join("linked/missing/../lib");
        assert_eq!(real_path(&linked), Some(usr.join("lib")));
        assert_eq!(real_path(&root.path().join("dangling")), None);
        assert_eq!(real_path(&root.path().join("dangling/lib")), None);
    }

    #[test]
    fn rpaths_skip_store_directories_without_the_file() {
        let store = PathBuf::from(format!("{STORE_ROOT}/k/lib"));
        let missing = PathBuf::from(format!("{STORE_ROOT}/m/lib"));
        let swift = PathBuf::from("/usr/lib/swift");
        let host = PathBuf::from("/usr/local/lib");
        let exists = |path: &Path| path.starts_with(&store);

        let inside = PathBuf::from(format!("{STORE_ROOT}/m/../k/lib"));
        let escape = PathBuf::from(format!("{STORE_ROOT}/k/../../../../usr/local/lib"));

        let cases = [
            (vec![missing.clone(), host.clone()], host.join("x.dylib")),
            (vec![inside, host.clone()], host.join("x.dylib")),
            (vec![escape.clone(), store.clone()], escape.join("x.dylib")),
            (vec![swift.clone(), store.clone()], store.join("x.dylib")),
            (vec![swift.clone(), missing], swift.join("x.dylib")),
            (vec![host.clone(), store.clone()], host.join("x.dylib")),
        ];

        for (rpaths, expected) in cases {
            assert_eq!(
                search_rpaths("x.dylib", &rpaths, exists),
                Some(expected),
                "{rpaths:?}"
            );
        }
    }

    #[test]
    fn mach_install_names_resolve_like_dyld() {
        let loader = Path::new("/opt/rb/store/k/Contents/Frameworks");
        let main = Process {
            executable: Some("/opt/rb/store/k/Contents/MacOS".into()),
            rpaths: vec!["/usr/lib/swift".into()],
        };
        let alone = Process {
            executable: None,
            rpaths: vec!["/usr/local/lib".into()],
        };

        let cases = [
            (
                "@loader_path/../lib/x.dylib",
                &main,
                Some("/opt/rb/store/k/Contents/Frameworks/../lib/x.dylib"),
            ),
            (
                "@executable_path/../Frameworks/x.dylib",
                &main,
                Some("/opt/rb/store/k/Contents/MacOS/../Frameworks/x.dylib"),
            ),
            (
                "@executable_path/x.dylib",
                &alone,
                Some("@executable_path/x.dylib"),
            ),
            (
                "@rpath/libswiftCore.dylib",
                &main,
                Some("/usr/lib/swift/libswiftCore.dylib"),
            ),
            (
                "@rpath/libswiftCore.dylib",
                &alone,
                Some("/usr/local/lib/libswiftCore.dylib"),
            ),
        ];

        for (library, process, expected) in cases {
            assert_eq!(
                resolve(library, loader, process).as_deref(),
                expected.map(Path::new),
                "{library}"
            );
        }

        assert!(is_loadable("@rpath/libswiftCore.dylib", loader, &main));
        assert!(!is_loadable("@rpath/libswiftCore.dylib", loader, &alone));
        assert!(!is_loadable("@executable_path/x.dylib", loader, &alone));
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
