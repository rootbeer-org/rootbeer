use super::{Binary, Format, Violation};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

const UNBUNDLED: &str = "no compatible bundled library at the loader's search paths; external runtime dependencies are not supported yet";

/// Loader references the OS runtime provides, so packages may use them without bundling.
struct Baseline {
    interpreters: &'static [&'static str],
    search_dirs: &'static [&'static str],
    search_prefixes: &'static [&'static str],
    libraries: &'static [&'static str],
    library_prefixes: &'static [&'static str],
}

const ELF: Baseline = Baseline {
    interpreters: &[
        "/lib64/ld-linux-x86-64.so.2",
        "/lib/ld-linux-aarch64.so.1",
        "/lib/ld-linux.so.2",
        "/lib/ld-linux-armhf.so.3",
        "/lib/ld-musl-x86_64.so.1",
        "/lib/ld-musl-aarch64.so.1",
    ],
    search_dirs: &[
        "/lib",
        "/lib64",
        "/usr/lib",
        "/usr/lib64",
        "/lib/x86_64-linux-gnu",
        "/usr/lib/x86_64-linux-gnu",
        "/lib/aarch64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
    ],
    search_prefixes: &[],
    libraries: &[
        "libc.so.6",
        "libm.so.6",
        "libdl.so.2",
        "libpthread.so.0",
        "librt.so.1",
        "libgcc_s.so.1",
        "libstdc++.so.6",
        "libc.musl-x86_64.so.1",
        "libc.musl-aarch64.so.1",
        "ld-linux-x86-64.so.2",
        "ld-linux-aarch64.so.1",
    ],
    library_prefixes: &[],
};

const MACHO: Baseline = Baseline {
    interpreters: &["/usr/lib/dyld"],
    search_dirs: &["/usr/lib"],
    search_prefixes: &["/System/Library/Frameworks"],
    libraries: &[],
    library_prefixes: &["/usr/lib", "/System/Library/Frameworks"],
};

impl Baseline {
    fn of(format: Format) -> &'static Self {
        match format {
            Format::Elf => &ELF,
            Format::MachO => &MACHO,
        }
    }

    fn allows_interpreter(&self, interpreter: &str) -> bool {
        self.interpreters.contains(&interpreter)
    }

    fn allows_search_path(&self, path: &str) -> bool {
        is_plain(path) && allows(path, self.search_dirs, self.search_prefixes)
    }

    fn allows_library(&self, library: &str) -> bool {
        is_plain(library) && allows(library, self.libraries, self.library_prefixes)
    }
}

fn allows(reference: &str, exact: &[&str], prefixes: &[&str]) -> bool {
    exact.contains(&reference)
        || prefixes
            .iter()
            .any(|prefix| Path::new(reference).starts_with(prefix))
}

fn is_plain(reference: &str) -> bool {
    !Path::new(reference)
        .components()
        .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
}

/// Where the loader would find one library reference.
enum Resolution<'a> {
    System,
    Bundled(&'a Binary),
    Unresolved(&'static str),
}

/// Directories a binary's libraries are searched in, and those its dependencies inherit.
struct SearchPaths {
    own: Vec<PathBuf>,
    inherited: Vec<PathBuf>,
}

pub(super) fn check(roots: &BTreeMap<PathBuf, PathBuf>, binaries: &[Binary]) -> Vec<Violation> {
    let mut audit = Checker {
        roots,
        binaries,
        violations: Vec::new(),
        reached: BTreeSet::new(),
    };
    for binary in binaries.iter().filter(|binary| binary.is_executable) {
        audit.visit(binary, Some(&binary.path), &[], &mut BTreeSet::new());
    }
    for binary in binaries {
        if !audit.reached.contains(&(&binary.path, binary.architecture)) {
            audit.visit(binary, None, &[], &mut BTreeSet::new());
        }
    }

    let key = |violation: &Violation| {
        (
            violation.path.clone(),
            violation.architecture,
            violation.reference.clone(),
            violation.reason.clone(),
        )
    };
    audit.violations.sort_by_key(key);
    audit.violations.dedup_by(|a, b| key(a) == key(b));
    audit.violations
}

struct Checker<'a> {
    roots: &'a BTreeMap<PathBuf, PathBuf>,
    binaries: &'a [Binary],
    violations: Vec<Violation>,
    reached: BTreeSet<(&'a PathBuf, u32)>,
}

impl<'a> Checker<'a> {
    fn reject(&mut self, binary: &Binary, reference: &str, reason: &str) {
        self.violations.push(Violation {
            path: binary.path.clone(),
            architecture: binary.architecture,
            reference: reference.into(),
            reason: reason.into(),
        });
    }

    fn visit(
        &mut self,
        binary: &'a Binary,
        executable: Option<&Path>,
        inherited: &[PathBuf],
        active: &mut BTreeSet<(&'a PathBuf, u32)>,
    ) {
        let key = (&binary.path, binary.architecture);
        self.reached.insert(key);
        if active.len() > 128 {
            self.reject(
                binary,
                "dependency chain",
                "runtime dependency depth exceeds 128",
            );
            return;
        }
        if !active.insert(key) {
            return;
        }

        self.check_interpreter(binary);
        self.check_identity(binary);

        let search = self.search_paths(binary, executable, inherited);
        for library in &binary.libraries {
            match self.resolve(binary, library, &search.own, executable) {
                Resolution::System => {}
                Resolution::Bundled(target) => {
                    self.visit(target, executable, &search.inherited, active)
                }
                Resolution::Unresolved(reason) => self.reject(binary, library, reason),
            }
        }

        active.remove(&key);
    }

    fn check_interpreter(&mut self, binary: &Binary) {
        let Some(interpreter) = &binary.interpreter else {
            return;
        };
        if !Baseline::of(binary.format).allows_interpreter(interpreter) {
            self.reject(
                binary,
                interpreter,
                "interpreter is outside the OS-runtime baseline",
            );
        }
    }

    fn check_identity(&mut self, binary: &Binary) {
        let Some(identity) = &binary.identity else {
            return;
        };
        let is_relocatable = match binary.format {
            Format::Elf => !identity.contains('/') && !identity.is_empty(),
            Format::MachO => ["@rpath/", "@loader_path/"].iter().any(|prefix| {
                identity
                    .strip_prefix(prefix)
                    .is_some_and(|path| normalize(Path::new(path)).is_ok())
            }),
        };
        if !is_relocatable {
            self.reject(binary, identity, "library identity is not relocatable");
        }
    }

    /// ELF RUNPATH replaces inherited rpaths for this binary alone; otherwise rpaths accumulate.
    fn search_paths(
        &mut self,
        binary: &Binary,
        executable: Option<&Path>,
        inherited: &[PathBuf],
    ) -> SearchPaths {
        let rpaths = self.expand_all(binary, &binary.rpaths, executable);
        let runpaths = self.expand_all(binary, &binary.runpaths, executable);

        let mut accumulated = rpaths;
        accumulated.extend_from_slice(inherited);
        if binary.format == Format::Elf && !binary.runpaths.is_empty() {
            return SearchPaths {
                own: runpaths,
                inherited: inherited.to_vec(),
            };
        }
        SearchPaths {
            own: accumulated.clone(),
            inherited: accumulated,
        }
    }

    fn expand_all(
        &mut self,
        binary: &Binary,
        paths: &[String],
        executable: Option<&Path>,
    ) -> Vec<PathBuf> {
        let baseline = Baseline::of(binary.format);
        let mut expanded = Vec::new();
        for path in paths {
            if baseline.allows_search_path(path) {
                continue;
            }
            match expand(path, binary, executable) {
                Ok(path) => expanded.push(path),
                Err(reason) => self.reject(binary, path, reason),
            }
        }
        expanded
    }

    fn resolve(
        &self,
        binary: &Binary,
        library: &str,
        search: &[PathBuf],
        executable: Option<&Path>,
    ) -> Resolution<'a> {
        if Baseline::of(binary.format).allows_library(library) {
            return Resolution::System;
        }
        let candidates = match binary.format {
            Format::Elf if !library.contains('/') => {
                search.iter().map(|path| path.join(library)).collect()
            }
            Format::MachO if library.starts_with("@rpath/") => {
                let suffix = &library["@rpath/".len()..];
                search.iter().map(|path| path.join(suffix)).collect()
            }
            _ => match expand(library, binary, executable) {
                Ok(path) => vec![path],
                Err(reason) => return Resolution::Unresolved(reason),
            },
        };
        candidates
            .iter()
            .filter_map(|candidate| normalize(candidate).ok())
            .filter_map(|candidate| self.locate(&candidate))
            .find_map(|relative| self.library_at(&relative, binary))
            .map_or(Resolution::Unresolved(UNBUNDLED), Resolution::Bundled)
    }

    /// Maps a loader path onto the audited binaries through the realized package roots.
    fn locate(&self, candidate: &Path) -> Option<PathBuf> {
        let (directory, root) = self
            .roots
            .iter()
            .find(|(directory, _)| candidate.starts_with(directory))?;
        let relative = candidate.strip_prefix(directory).ok()?;
        let path = root.join(relative).canonicalize().ok()?;
        let relative = path.strip_prefix(root).ok()?;
        Some(directory.join(relative))
    }

    fn library_at(&self, path: &Path, binary: &Binary) -> Option<&'a Binary> {
        self.binaries.iter().find(|other| {
            other.path == path
                && other.format == binary.format
                && other.architecture == binary.architecture
                && other.bits == binary.bits
                && other.little_endian == binary.little_endian
                && !other.is_executable
        })
    }
}

fn expand(
    reference: &str,
    binary: &Binary,
    executable: Option<&Path>,
) -> Result<PathBuf, &'static str> {
    let (base, suffix) = match binary.format {
        Format::Elf => {
            let suffix = reference
                .strip_prefix("$ORIGIN")
                .or_else(|| reference.strip_prefix("${ORIGIN}"))
                .ok_or(
                    "reference must be relative to $ORIGIN, not a host or working-directory path",
                )?;
            (binary.path.parent().unwrap(), suffix)
        }
        Format::MachO => {
            if let Some(suffix) = reference.strip_prefix("@loader_path") {
                (binary.path.parent().unwrap(), suffix)
            } else if let Some(suffix) = reference.strip_prefix("@executable_path") {
                (
                    executable
                        .ok_or("@executable_path requires an executable dependency context")?
                        .parent()
                        .unwrap(),
                    suffix,
                )
            } else {
                return Err("reference must use @loader_path, @executable_path, or @rpath rather than a host path");
            }
        }
    };
    if !suffix.is_empty() && !suffix.starts_with('/') {
        return Err("invalid loader-relative token");
    }
    if suffix.contains('$') || suffix.contains('@') {
        return Err("unsupported loader token");
    }
    normalize(&base.join(suffix.trim_start_matches('/')))
}

fn normalize(path: &Path) -> Result<PathBuf, &'static str> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => result.push(value),
            Component::CurDir => {}
            Component::ParentDir if result.pop() => {}
            _ => return Err("reference escapes the package"),
        }
    }
    Ok(result)
}
