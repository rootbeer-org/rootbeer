use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

pub fn validate(root: &Path, libraries: &[PathBuf]) -> Result<(), String> {
    let root = root.canonicalize().map_err(|error| error.to_string())?;
    for library in libraries {
        let path = contained_file(&root, library)?;
        let mut magic = [0; 8];
        fs::File::open(&path)
            .and_then(|mut file| file.read_exact(&mut magic))
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if &magic == b"!<arch>\n" {
            continue;
        }
        let report = crate::audit::audit(&root)?;
        if !report
            .binaries
            .iter()
            .any(|binary| root.join(&binary.path) == path && !binary.is_executable)
        {
            return Err(format!(
                "{} is not a static archive or shared library",
                path.display()
            ));
        }
    }
    Ok(())
}

/// Links a dependency's exports into `output`. Shared exports also carry each library's version
/// aliases and cmake packages, since consumers find those rather than the declared file.
pub(crate) fn stage(
    root: &Path,
    libraries: &[PathBuf],
    is_shared: bool,
    output: &Path,
) -> Result<(), String> {
    if libraries.is_empty() {
        return Ok(());
    }
    validate(root, libraries)?;
    for library in libraries {
        link(root, library, output)?;
        if is_shared {
            for alias in aliases(root, library)? {
                link(root, &alias, output)?;
            }
        }
    }

    let mut directories = vec![
        "include",
        "lib/pkgconfig",
        "lib64/pkgconfig",
        "share/pkgconfig",
    ];
    if is_shared {
        directories.extend(["lib/cmake", "lib64/cmake", "share/cmake"]);
    }
    for directory in directories {
        let path = root.join(directory);
        if path.try_exists().map_err(|error| error.to_string())? {
            tree(root, Path::new(directory), output)?;
        }
    }
    Ok(())
}

/// Entries beside `library` that resolve to the same file, such as `libz.so.1` for `libz.so`.
fn aliases(root: &Path, library: &Path) -> Result<Vec<PathBuf>, String> {
    let target = contained_file(root, library)?;
    let parent = library.parent().unwrap_or(Path::new(""));
    let mut entries = fs::read_dir(root.join(parent))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    entries.sort_by_key(|entry| entry.file_name());

    let mut aliases = Vec::new();
    for entry in entries {
        let relative = parent.join(entry.file_name());
        if relative != library && fs::canonicalize(entry.path()).ok() == Some(target.clone()) {
            aliases.push(relative);
        }
    }
    Ok(aliases)
}

fn contained_file(root: &Path, relative: &Path) -> Result<PathBuf, String> {
    let root = root.canonicalize().map_err(|error| error.to_string())?;
    let path = root
        .join(relative)
        .canonicalize()
        .map_err(|error| format!("missing dependency export {}: {error}", relative.display()))?;
    if !path.starts_with(&root) || !path.is_file() {
        return Err(format!(
            "dependency export {} escapes its package or is not a file",
            relative.display()
        ));
    }
    Ok(path)
}

fn link(root: &Path, relative: &Path, output: &Path) -> Result<(), String> {
    let source = contained_file(root, relative)?;
    let target = output.join(relative);
    if target.symlink_metadata().is_ok() {
        if fs::canonicalize(&target).ok().as_ref() == Some(&source) {
            return Ok(());
        }
        return Err(format!(
            "dependency export collision at {}",
            relative.display()
        ));
    }
    fs::create_dir_all(target.parent().unwrap()).map_err(|error| error.to_string())?;
    symlink(source, target).map_err(|error| error.to_string())
}

fn tree(root: &Path, relative: &Path, output: &Path) -> Result<(), String> {
    let directory = root.join(relative);
    let metadata = directory
        .symlink_metadata()
        .map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "dependency directory {} must be a real directory",
            relative.display()
        ));
    }
    let mut entries = fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = relative.join(entry.file_name());
        if entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            tree(root, &path, output)?;
        } else {
            link(root, &path, output)?;
        }
    }
    Ok(())
}

pub(crate) fn environment(
    prefix: &Path,
    environment: &mut BTreeMap<&str, String>,
) -> Result<(), String> {
    environment.insert("PKG_CONFIG_PATH", String::new());
    environment.insert(
        "PKG_CONFIG_LIBDIR",
        ["lib/pkgconfig", "lib64/pkgconfig", "share/pkgconfig"]
            .map(|directory| prefix.join(directory).to_string_lossy().into_owned())
            .join(":"),
    );
    environment.insert(
        "PKG_CONFIG_SYSROOT_DIR",
        prefix.to_string_lossy().into_owned(),
    );
    if !prefix.try_exists().map_err(|error| error.to_string())? {
        return Ok(());
    }
    for (name, directories) in [
        ("CPATH", vec![prefix.join("include")]),
        (
            "LIBRARY_PATH",
            vec![prefix.join("lib"), prefix.join("lib64")],
        ),
    ] {
        let existing = environment.get(name).filter(|value| !value.is_empty());
        let paths = directories
            .into_iter()
            .chain(existing.into_iter().flat_map(std::env::split_paths));
        let value = std::env::join_paths(paths).map_err(|error| error.to_string())?;
        environment.insert(name, value.to_string_lossy().into_owned());
    }
    Ok(())
}
