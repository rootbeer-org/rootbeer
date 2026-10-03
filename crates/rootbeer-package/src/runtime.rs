use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::LockedPackage;
use rootbeer_store_legacy::Store;

/// Returns the verified transitive runtime inputs in dependency-first order.
pub fn closure(package: &LockedPackage) -> Result<Vec<&LockedPackage>, String> {
    let mut ordered = Vec::new();
    visit(
        package,
        &mut BTreeSet::new(),
        &mut BTreeMap::new(),
        &mut ordered,
    )?;
    ordered.pop();
    Ok(ordered)
}

fn visit<'a>(
    package: &'a LockedPackage,
    active: &mut BTreeSet<String>,
    seen: &mut BTreeMap<String, &'a LockedPackage>,
    ordered: &mut Vec<&'a LockedPackage>,
) -> Result<(), String> {
    let id = package.id();
    if active.len() >= 64 || active.contains(&id) {
        return Err(format!(
            "{id}: runtime dependency cycle or depth exceeds 64"
        ));
    }
    if let Some(previous) = seen.get(&id) {
        if previous.output_sha256 != package.output_sha256
            || previous.provides != package.provides
            || previous
                .runtime_dependencies
                .iter()
                .map(|(key, package)| (key, &package.output_sha256))
                .collect::<Vec<_>>()
                != package
                    .runtime_dependencies
                    .iter()
                    .map(|(key, package)| (key, &package.output_sha256))
                    .collect::<Vec<_>>()
        {
            return Err(format!("{id}: conflicting runtime dependency facts"));
        }
    }
    active.insert(id.clone());
    for (key, dependency) in &package.runtime_dependencies {
        if key != &dependency.id() {
            return Err(format!("{key}: runtime dependency identity mismatch"));
        }
        store_directory(dependency)?;
        visit(dependency, active, seen, ordered)?;
    }
    active.remove(&id);
    if seen.insert(id, package).is_none() {
        ordered.push(package);
    }
    Ok(())
}

/// Stable sibling directory used by loader-relative runtime references.
pub fn store_directory(package: &LockedPackage) -> Result<PathBuf, String> {
    let hash = package
        .output_sha256
        .as_deref()
        .filter(|hash| rootbeer_catalog::is_sha256(hash))
        .ok_or_else(|| format!("{}: runtime output requires a locked SHA-256", package.id()))?;
    Ok(Store::new("").store_path(hash, &package.name, &package.version))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LockedInstall, LockedSource, Provides};

    fn package(name: &str) -> LockedPackage {
        LockedPackage {
            name: name.into(),
            version: "1".into(),
            source: LockedSource::Url {
                url: "https://example.org/package".into(),
                sha256: "a".repeat(64),
            },
            install: LockedInstall::Binary {
                path: "bin/tool".into(),
            },
            provides: Provides::default(),
            output_sha256: Some("b".repeat(64)),
            runtime_dependencies: BTreeMap::new(),
        }
    }

    #[test]
    fn validates_runtime_pins_cycles_and_conflicting_diamonds() {
        let mut root = package("root");
        let base = package("base");
        let mut left = package("left");
        left.runtime_dependencies.insert(base.id(), base.clone());
        let mut right = package("right");
        right.runtime_dependencies.insert(base.id(), base.clone());
        root.runtime_dependencies = BTreeMap::from([(left.id(), left), (right.id(), right)]);
        assert_eq!(
            closure(&root)
                .unwrap()
                .iter()
                .map(|package| package.id())
                .collect::<Vec<_>>(),
            ["base@1", "left@1", "right@1"]
        );
        let mut changed = root.clone();
        changed
            .runtime_dependencies
            .get_mut("right@1")
            .unwrap()
            .runtime_dependencies
            .get_mut("base@1")
            .unwrap()
            .output_sha256 = Some("c".repeat(64));
        assert!(closure(&changed).unwrap_err().contains("conflicting"));
        let mut changed = root.clone();
        changed
            .runtime_dependencies
            .get_mut("left@1")
            .unwrap()
            .output_sha256 = None;
        assert!(closure(&changed).unwrap_err().contains("SHA-256"));
        let mut changed = root.clone();
        changed.runtime_dependencies.insert("wrong@1".into(), base);
        assert!(closure(&changed).unwrap_err().contains("identity mismatch"));
        let mut changed = root.clone();
        changed
            .runtime_dependencies
            .get_mut("left@1")
            .unwrap()
            .runtime_dependencies
            .insert(root.id(), package("root"));
        assert!(closure(&changed).unwrap_err().contains("cycle"));
    }

    #[test]
    fn runtime_locks_reject_schema_downgrades_and_track_realization_inputs() {
        let mut root = package("root");
        let before = root.clone();
        root.runtime_dependencies
            .insert("base@1".into(), package("base"));
        assert!(!root.same_realization_input(&before));
        let mut lock = crate::lockfile::RootbeerLock::from_packages([root]).unwrap();
        assert_eq!(lock.schema, 3);
        assert_eq!(lock.store_paths(&Store::new("store")).unwrap().len(), 2);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lock.json");
        lock.schema = 1;
        lock.write(&path).unwrap();
        assert!(crate::lockfile::RootbeerLock::read(path)
            .unwrap_err()
            .to_string()
            .contains("schema 3"));
    }
}
