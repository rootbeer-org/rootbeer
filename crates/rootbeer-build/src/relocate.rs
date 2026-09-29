//! Rewrites absolute build paths in loader metadata into loader-relative ones.
//!
//! Builds link against their runtime dependencies at absolute build-store paths, which needs no
//! `$ORIGIN` escaping through make, libtool, or cmake. After install, every such path is
//! rewritten in place to the sibling store layout the package is realized into. Rewritten paths
//! are never longer, so no load command or string table grows.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use goblin::mach::load_command::CommandVariant;

/// Absolute build paths and where they are found once installed.
pub(crate) struct Relocation<'a> {
    /// The package's own install prefix.
    pub prefix: &'a Path,
    /// Build-time runtime dependency roots, to their sibling store directory names.
    pub runtime: &'a BTreeMap<PathBuf, PathBuf>,
}

impl Relocation<'_> {
    /// Rewrites every native binary under the prefix.
    pub(crate) fn apply(&self) -> Result<(), String> {
        let mut files = Vec::new();
        collect(self.prefix, &mut files)?;
        for path in files {
            let relative = path.strip_prefix(self.prefix).unwrap();
            let depth = relative.components().count() - 1;
            self.file(&path, depth)
                .map_err(|error| format!("{}: {error}", relative.display()))?;
        }
        Ok(())
    }

    fn file(&self, path: &Path, depth: usize) -> Result<(), String> {
        let mut bytes = fs::read(path).map_err(|error| error.to_string())?;
        let is_signed = is_signed(&bytes)?;
        if bytes.starts_with(b"\x7fELF") {
            if !self.elf(&mut bytes, depth)? {
                return Ok(());
            }
            return fs::write(path, &bytes).map_err(|error| error.to_string());
        }

        let edits = self.mach(&bytes, depth)?;
        if edits.is_empty() {
            return Ok(());
        }
        if edits.iter().all(MachEdit::fits)
            && !edits.iter().any(|edit| edit.kind == MachKind::DeleteRpath)
        {
            for edit in &edits {
                bytes[edit.start..edit.end].fill(0);
                bytes[edit.start..edit.start + edit.new.len()].copy_from_slice(edit.new.as_bytes());
            }
            fs::write(path, &bytes).map_err(|error| error.to_string())?;
        } else {
            // Longer names go through the header padding the build linked with.
            let mut arguments: Vec<Vec<&str>> = Vec::new();
            for edit in &edits {
                let argument = match edit.kind {
                    MachKind::Rpath => vec!["-rpath", edit.old.as_str(), edit.new.as_str()],
                    MachKind::DeleteRpath => vec!["-delete_rpath", edit.old.as_str()],
                    MachKind::Identity => vec!["-id", edit.new.as_str()],
                    MachKind::Load => vec!["-change", edit.old.as_str(), edit.new.as_str()],
                };
                if !arguments.contains(&argument) {
                    arguments.push(argument);
                }
            }
            run_tool(
                Command::new("/usr/bin/install_name_tool")
                    .args(arguments.concat())
                    .arg(path),
            )?;
        }

        if is_signed {
            run_tool(
                Command::new("/usr/bin/codesign")
                    .args(["--force", "--sign", "-"])
                    .arg(path),
            )?;
        }
        Ok(())
    }

    /// The loader-relative form of an absolute build path, or `None` when it names neither the
    /// prefix nor a runtime dependency.
    fn rewrite(&self, value: &str, token: &str, depth: usize) -> Option<String> {
        let path = Path::new(value);
        let runtime = self.runtime.iter().find(|(root, _)| path.starts_with(root));
        let (ups, name, rest) = if let Ok(rest) = path.strip_prefix(self.prefix) {
            (depth, None, rest)
        } else if let Some((root, name)) = runtime {
            (depth + 1, Some(name), path.strip_prefix(root).unwrap())
        } else {
            (depth, None, self.installed(path)?)
        };

        let mut relocated = PathBuf::from(token);
        for _ in 0..ups {
            relocated.push("..");
        }
        if let Some(name) = name {
            relocated.push(name);
        }
        relocated.push(rest);
        Some(
            relocated
                .to_string_lossy()
                .trim_end_matches('/')
                .to_string(),
        )
    }

    /// The part of an absolute path that names a file in the prefix, as a `DESTDIR` install
    /// into `/` records it, such as `/lib/libz.1.dylib` for `<prefix>/lib/libz.1.dylib`.
    fn installed<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        let rest = path.strip_prefix("/").ok()?;
        let is_installed = !rest.as_os_str().is_empty()
            && !rest.starts_with("..")
            && self.prefix.join(rest).symlink_metadata().is_ok();
        is_installed.then_some(rest)
    }

    /// Relocates each entry of a search path, dropping entries that relocate to one already
    /// listed, which also leaves room for the longer loader-relative forms.
    fn rewrite_list(&self, value: &str, token: &str, depth: usize) -> Option<String> {
        let mut is_changed = false;
        let mut entries: Vec<String> = Vec::new();
        for entry in value.split(':') {
            let entry = match self.rewrite(entry, token, depth) {
                Some(relocated) => {
                    is_changed = true;
                    relocated
                }
                None => entry.to_string(),
            };
            if !entries.contains(&entry) {
                entries.push(entry);
            }
        }
        is_changed.then(|| entries.join(":"))
    }

    fn elf(&self, bytes: &mut [u8], depth: usize) -> Result<bool, String> {
        let elf = goblin::elf::Elf::parse(bytes).map_err(|error| error.to_string())?;
        let Some(dynamic) = &elf.dynamic else {
            return Ok(false);
        };
        let strtab = file_offset(&elf, dynamic.info.strtab as u64)?;
        let table = elf
            .program_headers
            .iter()
            .find(|header| header.p_type == goblin::elf::program_header::PT_DYNAMIC)
            .ok_or("dynamic section without a program header")?
            .p_offset as usize;
        let (entry_size, value_offset) = if elf.is_64 { (16, 8) } else { (8, 4) };
        let is_little = elf.little_endian;

        let mut edits = Vec::new();
        for (index, entry) in dynamic.dyns.iter().enumerate() {
            if !matches!(
                entry.d_tag,
                goblin::elf::dynamic::DT_RPATH | goblin::elf::dynamic::DT_RUNPATH
            ) {
                continue;
            }
            let start = strtab + entry.d_val as usize;
            let old = c_string(bytes, start)?;
            let Some(new) = self.rewrite_list(old, "$ORIGIN", depth) else {
                continue;
            };
            if new.len() > old.len() {
                return Err(format!("relocated run path {new} does not fit in {old}"));
            }
            edits.push((
                table + index * entry_size + value_offset,
                entry.d_val as usize,
                old.len(),
                new,
            ));
        }
        drop(elf);

        let is_changed = !edits.is_empty();
        for (slot, value, length, new) in edits {
            // Right-aligned, so strings the linker merged into this one's tail still resolve.
            let shift = length - new.len();
            let start = strtab + value;
            bytes[start..start + shift].fill(0);
            bytes[start + shift..start + length].copy_from_slice(new.as_bytes());
            let relocated = (value + shift) as u64;
            match (entry_size, is_little) {
                (16, true) => bytes[slot..slot + 8].copy_from_slice(&relocated.to_le_bytes()),
                (16, false) => bytes[slot..slot + 8].copy_from_slice(&relocated.to_be_bytes()),
                (_, true) => {
                    bytes[slot..slot + 4].copy_from_slice(&(relocated as u32).to_le_bytes())
                }
                (_, false) => {
                    bytes[slot..slot + 4].copy_from_slice(&(relocated as u32).to_be_bytes())
                }
            }
        }
        Ok(is_changed)
    }

    fn mach(&self, bytes: &[u8], depth: usize) -> Result<Vec<MachEdit>, String> {
        let slices = match goblin::mach::Mach::parse(bytes).map_err(|error| error.to_string())? {
            goblin::mach::Mach::Binary(_) => vec![0],
            goblin::mach::Mach::Fat(fat) => fat
                .iter_arches()
                .map(|arch| {
                    arch.map(|arch| arch.offset as usize)
                        .map_err(|error| error.to_string())
                })
                .collect::<Result<_, _>>()?,
        };

        let mut edits = Vec::new();
        for base in slices {
            if bytes[base..].starts_with(b"!<arch>\n") {
                continue;
            }
            let mach =
                goblin::mach::MachO::parse(&bytes[base..], 0).map_err(|error| error.to_string())?;
            let mut rpaths = std::collections::BTreeSet::new();
            for command in &mach.load_commands {
                let (name, size, kind) = match command.command {
                    CommandVariant::Rpath(rpath) => (rpath.path, rpath.cmdsize, MachKind::Rpath),
                    CommandVariant::IdDylib(dylib) => {
                        (dylib.dylib.name, dylib.cmdsize, MachKind::Identity)
                    }
                    CommandVariant::LoadDylib(dylib)
                    | CommandVariant::LoadWeakDylib(dylib)
                    | CommandVariant::ReexportDylib(dylib)
                    | CommandVariant::LazyLoadDylib(dylib)
                    | CommandVariant::LoadUpwardDylib(dylib) => {
                        (dylib.dylib.name, dylib.cmdsize, MachKind::Load)
                    }
                    _ => continue,
                };
                let start = base + command.offset + name as usize;
                let end = base + command.offset + size as usize;
                let old = c_string(&bytes[..end], start)?;
                let new = if kind == MachKind::Identity {
                    let path = Path::new(old);
                    let is_own = path.starts_with(self.prefix) || self.installed(path).is_some();
                    is_own.then(|| format!("@rpath/{}", file_name(old)))
                } else {
                    self.rewrite(old, "@loader_path", depth)
                };
                if kind == MachKind::Rpath {
                    // Two build paths can name the same installed directory, and install_name_tool
                    // refuses to repeat an rpath, so later ones are dropped.
                    let is_repeat = !rpaths.insert(new.as_deref().unwrap_or(old).to_string());
                    if is_repeat && new.is_some() {
                        edits.push(MachEdit {
                            kind: MachKind::DeleteRpath,
                            start,
                            end,
                            old: old.to_string(),
                            new: String::new(),
                        });
                        continue;
                    }
                }
                let Some(new) = new else {
                    continue;
                };
                let old = old.to_string();
                edits.push(MachEdit {
                    kind,
                    start,
                    end,
                    old,
                    new,
                });
            }
        }
        Ok(edits)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MachKind {
    Rpath,
    DeleteRpath,
    Identity,
    Load,
}

/// One load command string and what it becomes.
struct MachEdit {
    kind: MachKind,
    start: usize,
    end: usize,
    old: String,
    new: String,
}

impl MachEdit {
    /// Whether the new string and its terminator fit in the command as linked.
    fn fits(&self) -> bool {
        self.new.len() < self.end - self.start
    }
}

fn run_tool(command: &mut Command) -> Result<(), String> {
    let output = command.output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(())
}

fn collect(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        let path = entry.path();
        if kind.is_dir() {
            collect(&path, files)?;
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let mut magic = [0; 4];
        let read = fs::File::open(&path)
            .and_then(|file| file.take(4).read(&mut magic))
            .map_err(|error| error.to_string())?;
        if read == 4 && crate::audit::is_native(magic) {
            files.push(path);
        }
    }
    Ok(())
}

fn file_offset(elf: &goblin::elf::Elf<'_>, address: u64) -> Result<usize, String> {
    elf.program_headers
        .iter()
        .find(|header| {
            header.p_type == goblin::elf::program_header::PT_LOAD
                && (header.p_vaddr..header.p_vaddr + header.p_filesz).contains(&address)
        })
        .map(|header| (address - header.p_vaddr + header.p_offset) as usize)
        .ok_or_else(|| "string table is outside every loaded segment".into())
}

fn c_string(bytes: &[u8], start: usize) -> Result<&str, String> {
    let tail = bytes.get(start..).ok_or("string offset is out of bounds")?;
    let length = tail
        .iter()
        .position(|byte| *byte == 0)
        .ok_or("unterminated string")?;
    std::str::from_utf8(&tail[..length]).map_err(|error| error.to_string())
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn is_signed(bytes: &[u8]) -> Result<bool, String> {
    if bytes.starts_with(b"\x7fELF") {
        return Ok(false);
    }
    let has_signature = |mach: &goblin::mach::MachO<'_>| {
        mach.load_commands
            .iter()
            .any(|command| matches!(command.command, CommandVariant::CodeSignature(_)))
    };
    match goblin::mach::Mach::parse(bytes).map_err(|error| error.to_string())? {
        goblin::mach::Mach::Binary(mach) => Ok(has_signature(&mach)),
        goblin::mach::Mach::Fat(fat) => {
            for arch in fat.iter_arches() {
                let slice = arch.map_err(|error| error.to_string())?.slice(bytes);
                if slice.starts_with(b"!<arch>\n") {
                    continue;
                }
                let mach =
                    goblin::mach::MachO::parse(slice, 0).map_err(|error| error.to_string())?;
                if has_signature(&mach) {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

/// Linker flags that make runtime dependencies loadable from build-tree and installed binaries,
/// let a package that exports shared libraries load its own, and on macOS leave room for
/// relocated names longer than the ones linked.
pub(crate) fn link_flags(
    prefix: &Path,
    runtime: &BTreeMap<PathBuf, PathBuf>,
    libraries: &[PathBuf],
) -> Result<Vec<String>, String> {
    let is_shared = libraries.iter().any(|library| {
        library
            .extension()
            .is_some_and(|extension| extension == "so" || extension == "dylib")
    });
    let mut flags = Vec::new();
    if cfg!(target_os = "macos") && (is_shared || !runtime.is_empty()) {
        flags.push("-Wl,-headerpad_max_install_names".to_string());
    }
    let mut directories = Vec::new();
    if is_shared {
        directories.push(prefix.join("lib"));
    }
    for root in runtime.keys() {
        for directory in ["lib", "lib64"] {
            let path = root.join(directory);
            if path.is_dir() {
                directories.push(path);
            }
        }
    }
    for path in directories {
        let path = path.to_str().ok_or("runtime path must be UTF-8")?;
        if path.contains(char::is_whitespace) {
            return Err(format!("runtime path {path} contains whitespace"));
        }
        flags.push(format!("-Wl,-rpath,{path}"));
    }
    Ok(flags)
}

/// Whether a declared library is shared rather than a static archive.
pub(crate) fn exports_shared(prefix: &Path, libraries: &[PathBuf]) -> bool {
    libraries.iter().any(|library| {
        let mut magic = [0; 8];
        fs::File::open(prefix.join(library))
            .and_then(|mut file| file.read_exact(&mut magic))
            .is_ok_and(|()| &magic != b"!<arch>\n")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_paths_become_relative_to_the_loader_at_its_depth() {
        let runtime = BTreeMap::from([(
            PathBuf::from("/build/store/sha256-a-base-1"),
            PathBuf::from("sha256-a-base-1"),
        )]);
        let relocation = Relocation {
            prefix: Path::new("/build/prefix"),
            runtime: &runtime,
        };
        assert_eq!(
            relocation.rewrite("/build/store/sha256-a-base-1/lib", "$ORIGIN", 1),
            Some("$ORIGIN/../../sha256-a-base-1/lib".into())
        );
        assert_eq!(
            relocation.rewrite("/build/prefix/lib", "@loader_path", 2),
            Some("@loader_path/../../lib".into())
        );
        assert_eq!(
            relocation.rewrite("/build/prefix", "$ORIGIN", 0),
            Some("$ORIGIN".into())
        );
        assert_eq!(
            relocation.rewrite("/build/prefix-other/lib", "$ORIGIN", 1),
            None
        );
        assert_eq!(relocation.rewrite("$ORIGIN/../lib", "$ORIGIN", 1), None);
        assert_eq!(
            relocation.rewrite_list("$ORIGIN/../lib:/build/prefix/lib", "$ORIGIN", 1),
            Some("$ORIGIN/../lib".into())
        );
        assert_eq!(
            relocation.rewrite_list("$ORIGIN/../lib", "$ORIGIN", 1),
            None
        );
    }

    #[test]
    fn destdir_install_paths_are_relocated_only_when_they_name_the_package() {
        let directory = tempfile::tempdir().unwrap();
        let prefix = directory.path().join("prefix");
        fs::create_dir_all(prefix.join("lib")).unwrap();
        fs::write(prefix.join("lib/libz.1.dylib"), "").unwrap();
        let runtime = BTreeMap::new();
        let relocation = Relocation {
            prefix: &prefix,
            runtime: &runtime,
        };
        assert_eq!(
            relocation.rewrite("/lib/libz.1.dylib", "@loader_path", 1),
            Some("@loader_path/../lib/libz.1.dylib".into())
        );
        assert_eq!(
            relocation.rewrite("/lib", "$ORIGIN", 1),
            Some("$ORIGIN/../lib".into())
        );
        for foreign in [
            "/usr/lib/libSystem.B.dylib",
            "/lib/missing.dylib",
            "/",
            "/../lib",
        ] {
            assert_eq!(
                relocation.rewrite(foreign, "@loader_path", 1),
                None,
                "{foreign}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rpaths_that_relocate_to_the_same_directory_collapse_into_one() {
        let directory = tempfile::tempdir().unwrap();
        let prefix = directory.path().join("prefix");
        fs::create_dir_all(prefix.join("lib")).unwrap();
        fs::create_dir_all(prefix.join("bin")).unwrap();
        let compile = |args: &[&str]| {
            let output = Command::new("/usr/bin/cc")
                .current_dir(directory.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        fs::write(directory.path().join("f.c"), "int f(void) { return 0; }\n").unwrap();
        fs::write(
            directory.path().join("m.c"),
            "int f(void); int main(void) { return f(); }\n",
        )
        .unwrap();
        let library = prefix.join("lib/libf.dylib");
        compile(&[
            "-dynamiclib",
            "-o",
            library.to_str().unwrap(),
            "-install_name",
            "@rpath/libf.dylib",
            "f.c",
        ]);
        let binary = prefix.join("bin/m");
        let lib = prefix.join("lib");
        let rpath = format!("-Wl,-rpath,{}", lib.display());
        compile(&[
            "-o",
            binary.to_str().unwrap(),
            "m.c",
            "-L",
            lib.to_str().unwrap(),
            "-lf",
            &rpath,
            "-Wl,-rpath,/lib",
            "-Wl,-headerpad_max_install_names",
        ]);

        let runtime = BTreeMap::new();
        Relocation {
            prefix: &prefix,
            runtime: &runtime,
        }
        .apply()
        .unwrap();

        let bytes = fs::read(&binary).unwrap();
        let mach = goblin::mach::MachO::parse(&bytes, 0).unwrap();
        let rpaths: Vec<_> = mach.rpaths.iter().map(|rpath| rpath.to_string()).collect();
        assert_eq!(rpaths, ["@loader_path/../lib"]);
        assert!(Command::new(&binary).status().unwrap().success());
    }
}
