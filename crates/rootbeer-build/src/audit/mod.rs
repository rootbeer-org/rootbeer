use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;

mod parse;
pub(crate) use parse::is_native;
mod policy;
#[cfg(test)]
mod tests;

/// Static inspection of the native binaries in an installed package tree.
#[derive(Debug, Serialize)]
pub struct AuditReport {
    pub schema: u32,
    pub binaries: Vec<Binary>,
    pub violations: Vec<Violation>,
}

/// Loader facts for one architecture of an ELF or Mach-O file.
#[derive(Debug, Serialize)]
pub struct Binary {
    pub path: PathBuf,
    pub format: Format,
    pub architecture: u32,
    pub bits: u8,
    pub little_endian: bool,
    pub is_executable: bool,
    pub interpreter: Option<String>,
    pub identity: Option<String>,
    pub libraries: Vec<String>,
    pub rpaths: Vec<String>,
    pub runpaths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Elf,
    MachO,
}

/// A reference that cannot be justified by this package or the OS-runtime baseline.
#[derive(Debug, Serialize)]
pub struct Violation {
    pub path: PathBuf,
    pub architecture: u32,
    pub reference: String,
    pub reason: String,
}

impl AuditReport {
    /// Rejects a package containing unresolved or non-relocatable native references.
    pub fn validate(&self) -> Result<(), String> {
        if self.violations.is_empty() {
            return Ok(());
        }
        Err(format!(
            "runtime audit failed:\n{}",
            self.violations
                .iter()
                .map(|issue| {
                    format!(
                        "{} (architecture {}): {}: {}",
                        issue.path.display(),
                        issue.architecture,
                        issue.reference,
                        issue.reason
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}

/// Reads loader metadata without executing binaries or consulting host library paths.
pub fn audit(root: &Path) -> Result<AuditReport, String> {
    let root = root.canonicalize().map_err(|error| error.to_string())?;
    if !root.is_dir() {
        return Err("runtime audit requires an installed package directory".into());
    }
    let mut binaries = Vec::new();
    visit(&root, &root, &mut binaries)?;
    binaries.sort_by(|a, b| (&a.path, a.architecture).cmp(&(&b.path, b.architecture)));
    let violations = policy::check(
        &std::collections::BTreeMap::from([(PathBuf::new(), root)]),
        &binaries,
    );
    Ok(AuditReport {
        schema: 1,
        binaries,
        violations,
    })
}

fn visit(root: &Path, directory: &Path, binaries: &mut Vec<Binary>) -> Result<(), String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        let path = entry.path();
        if kind.is_dir() {
            visit(root, &path, binaries)?;
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let mut magic = [0; 4];
        let read = fs::File::open(&path)
            .and_then(|file| file.take(4).read(&mut magic))
            .map_err(|error| error.to_string())?;
        if read != 4 || !parse::is_native(magic) {
            continue;
        }
        let relative = path.strip_prefix(root).unwrap();
        let bytes = fs::read(&path).map_err(|error| error.to_string())?;
        binaries.extend(parse::parse(&bytes, relative).map_err(|error| {
            format!("{}: malformed native binary: {error}", relative.display())
        })?);
    }
    Ok(())
}

/// Audits a package and its declared runtime closure in the store's sibling layout.
/// Keys are content-addressed store directory names; values are realized package roots.
pub fn audit_with_runtime(
    root: &Path,
    runtime: &std::collections::BTreeMap<PathBuf, PathBuf>,
) -> Result<AuditReport, String> {
    if runtime.is_empty() {
        return audit(root);
    }
    let mut roots = std::collections::BTreeMap::from([(
        PathBuf::from("package"),
        root.canonicalize().map_err(|e| e.to_string())?,
    )]);
    for (directory, path) in runtime {
        rootbeer_package::realize::validate_relative_path("runtime store directory", directory)
            .map_err(|e| e.to_string())?;
        if directory.components().count() != 1 || directory == Path::new("package") {
            return Err("runtime roots must use distinct sibling store directories".into());
        }
        roots.insert(
            directory.clone(),
            path.canonicalize().map_err(|e| e.to_string())?,
        );
    }
    let mut binaries = Vec::new();
    for (directory, path) in &roots {
        let mut entries = Vec::new();
        visit(path, path, &mut entries)?;
        for binary in &mut entries {
            binary.path = directory.join(&binary.path);
        }
        binaries.extend(entries);
    }
    binaries.sort_by(|a, b| (&a.path, a.architecture).cmp(&(&b.path, b.architecture)));
    let violations = policy::check(&roots, &binaries);
    Ok(AuditReport {
        schema: 1,
        binaries,
        violations,
    })
}

/// Verifies installed output hashes and audits the pinned sibling runtime entries.
pub fn audit_installed(
    root: &Path,
    package: &rootbeer_package::LockedPackage,
) -> Result<AuditReport, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let parent = root
        .parent()
        .ok_or("installed package has no store parent")?;
    let mut runtime = std::collections::BTreeMap::new();
    for dependency in rootbeer_package::runtime::closure(package)? {
        let directory = rootbeer_package::runtime::store_directory(dependency)?;
        let path = parent.join(&directory);
        if rootbeer_store_legacy::hash_tree(&path).map_err(|e| e.to_string())?
            != dependency.output_sha256.as_deref().unwrap()
        {
            return Err(format!("{}: runtime output hash mismatch", dependency.id()));
        }
        runtime.insert(directory, path);
    }
    if package.output_sha256.as_deref()
        != Some(&rootbeer_store_legacy::hash_tree(&root).map_err(|e| e.to_string())?)
    {
        return Err("installed package output hash mismatch".into());
    }
    audit_with_runtime(&root, &runtime)
}
