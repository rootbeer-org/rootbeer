use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rootbeer_package::{BuildArtifact, CatalogRecipe, LockedPackage, LockedSource};
use rootbeer_store_legacy::hash_file;
use serde::{Deserialize, Serialize};

/// Persistent build results scoped to an explicitly identified host image and toolchain.
#[derive(Debug, Clone)]
pub struct BuildCache {
    pub directory: PathBuf,
    pub context: String,
    pub recheck: bool,
}

/// Shares completed cache entries across builds in one invocation, including forced rechecks.
#[derive(Debug, Clone, Default)]
pub struct BuildSession {
    completed: Arc<Mutex<BTreeSet<PathBuf>>>,
}

#[derive(Serialize, Deserialize)]
struct Record {
    key: String,
    artifact: BuildArtifact,
}

pub(crate) struct Entry {
    directory: PathBuf,
    key: String,
    should_rebuild: bool,
    session: Option<BuildSession>,
    _lock: fs::File,
}

impl BuildCache {
    pub(crate) fn go_compiler_cache(
        &self,
        environment: &crate::environment::Environment,
        dependencies: &BTreeMap<String, LockedPackage>,
    ) -> Result<PathBuf, String> {
        let outputs = dependencies
            .iter()
            .map(|(name, package)| {
                let hash = package
                    .output_sha256
                    .as_ref()
                    .ok_or_else(|| format!("{name}: dependency output is not locked"))?;
                Ok((name, hash))
            })
            .collect::<Result<BTreeMap<_, _>, String>>()?;
        let inputs = serde_json::to_vec(&(environment.identity(&self.context)?, outputs))
            .map_err(|error| error.to_string())?;
        let directory = self
            .directory
            .join("compiler/go")
            .join(rootbeer_store_legacy::hash_bytes(&inputs));
        fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        directory.canonicalize().map_err(|error| error.to_string())
    }

    pub(crate) fn entry(
        &self,
        key: String,
        session: Option<&BuildSession>,
        execution: &rootbeer_package::Execution,
    ) -> Result<Entry, String> {
        if self.context.trim().is_empty() {
            return Err(
                "build cache requires an identity for the host image, SDK, and toolchain".into(),
            );
        }
        fs::create_dir_all(self.directory.join("locks")).map_err(|error| error.to_string())?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.directory.join("locks").join(&key))
            .map_err(|error| error.to_string())?;
        loop {
            execution.check().map_err(|error| error.to_string())?;
            match lock.try_lock() {
                Ok(()) => break,
                Err(fs::TryLockError::WouldBlock) => {
                    std::thread::sleep(std::time::Duration::from_millis(50))
                }
                Err(fs::TryLockError::Error(error)) => return Err(error.to_string()),
            }
        }
        let directory = self.directory.join("results").join(&key);
        let is_completed =
            session.is_some_and(|session| session.completed.lock().unwrap().contains(&directory));
        Ok(Entry {
            directory,
            key,
            should_rebuild: self.recheck && !is_completed,
            session: session.cloned(),
            _lock: lock,
        })
    }
}

pub(crate) fn key(
    name: &str,
    revision: u32,
    recipe: &CatalogRecipe,
    system: &str,
    dependencies: &BTreeMap<String, LockedPackage>,
    context: &str,
    exports: &BTreeMap<String, rootbeer_package::graph::DependencyExports>,
) -> Result<String, String> {
    let outputs = dependencies
        .iter()
        .map(|(name, package)| {
            let hash = package
                .output_sha256
                .as_deref()
                .ok_or_else(|| format!("{name}: dependency output is not locked"))?;
            Ok((name, (hash, &package.provides, exports.get(name))))
        })
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let engine = crate::engine_identity(recipe.build.as_ref().map(|build| &build.backend));
    let mut compilation_recipe = recipe.clone();
    compilation_recipe.checks.clear();
    let bytes = serde_json::to_vec(&(
        "rootbeer-build-v4",
        engine,
        name,
        revision,
        compilation_recipe,
        system,
        outputs,
        context,
    ))
    .map_err(|error| error.to_string())?;
    Ok(rootbeer_store_legacy::hash_bytes(&bytes))
}

impl Entry {
    pub(crate) fn complete(&self) {
        if let Some(session) = &self.session {
            session
                .completed
                .lock()
                .unwrap()
                .insert(self.directory.clone());
        }
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    pub(crate) fn restore(&self, output: &Path) -> Result<Option<BuildArtifact>, String> {
        if self.should_rebuild
            || !self
                .directory
                .try_exists()
                .map_err(|error| error.to_string())?
        {
            return Ok(None);
        }
        let bytes =
            fs::read(self.directory.join("receipt.json")).map_err(|error| error.to_string())?;
        let mut record: Record =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        if record.key != self.key {
            return Err("build cache key mismatch".into());
        }
        let LockedSource::File { path, sha256 } = &mut record.artifact.package.source else {
            return Err("build cache artifact must be a local archive".into());
        };
        let archive = self.directory.join("package.tar.gz");
        if hash_file(&archive).map_err(|error| error.to_string())? != *sha256 {
            return Err("build cache archive hash mismatch".into());
        }
        *path = output.join("package.tar.gz");
        fs::copy(archive, path).map_err(|error| error.to_string())?;
        eprintln!("reuse build: {}", record.artifact.package.id());
        Ok(Some(record.artifact))
    }

    pub(crate) fn save(&self, artifact: &BuildArtifact, source: &Path) -> Result<(), String> {
        let parent = self.directory.parent().unwrap();
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let staging = tempfile::tempdir_in(parent).map_err(|error| error.to_string())?;
        fs::copy(
            source.join("package.tar.gz"),
            staging.path().join("package.tar.gz"),
        )
        .map_err(|error| error.to_string())?;
        fs::write(
            staging.path().join("receipt.json"),
            serde_json::to_vec(&Record {
                key: self.key.clone(),
                artifact: artifact.clone(),
            })
            .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        if self.directory.exists() {
            fs::remove_dir_all(&self.directory).map_err(|error| error.to_string())?;
        }
        fs::rename(staging.path(), &self.directory).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rootbeer_package::{LockedInstall, Provides, ResolveContext};

    #[test]
    fn keys_track_build_inputs_without_tracking_dependency_locations() {
        let catalog = crate::test_catalog::catalog();
        let version = catalog.packages["xz"].versions.values().next().unwrap();
        let recipe = &version.platforms[&ResolveContext::current().system];
        let mut dependencies = BTreeMap::from([(
            "compiler@1".into(),
            LockedPackage {
                name: "compiler".into(),
                version: "1".into(),
                source: LockedSource::File {
                    path: "/first/compiler.tar.gz".into(),
                    sha256: "a".repeat(64),
                },
                install: LockedInstall::Directory { strip_prefix: None },
                provides: Provides::default(),
                runtime_dependencies: Default::default(),
                output_sha256: Some("b".repeat(64)),
            },
        )]);
        let directory = tempfile::tempdir().unwrap();
        let mut compiler_cache = BuildCache {
            directory: directory.path().into(),
            context: "sdk-v1".into(),
            recheck: false,
        };
        let mut environment = crate::environment::Environment::resolve(None).unwrap();
        let original_cache = compiler_cache
            .go_compiler_cache(&environment, &dependencies)
            .unwrap();
        assert_eq!(
            original_cache,
            compiler_cache
                .go_compiler_cache(&environment, &dependencies)
                .unwrap()
        );
        compiler_cache.context = "sdk-v2".into();
        assert_ne!(
            original_cache,
            compiler_cache
                .go_compiler_cache(&environment, &dependencies)
                .unwrap()
        );
        compiler_cache.context = "sdk-v1".into();
        environment.lock.tools.get_mut("cc").unwrap().sha256 = "f".repeat(64);
        assert_ne!(
            original_cache,
            compiler_cache
                .go_compiler_cache(&environment, &dependencies)
                .unwrap()
        );
        let environment = crate::environment::Environment::resolve(None).unwrap();
        dependencies.get_mut("compiler@1").unwrap().output_sha256 = Some("c".repeat(64));
        assert_ne!(
            original_cache,
            compiler_cache
                .go_compiler_cache(&environment, &dependencies)
                .unwrap()
        );
        dependencies.get_mut("compiler@1").unwrap().output_sha256 = None;
        assert!(compiler_cache
            .go_compiler_cache(&environment, &dependencies)
            .is_err());
        dependencies.get_mut("compiler@1").unwrap().output_sha256 = Some("b".repeat(64));
        let digest = |revision: u32,
                      recipe: &CatalogRecipe,
                      dependencies: &BTreeMap<String, LockedPackage>| {
            key(
                "xz@1",
                revision,
                recipe,
                "aarch64-macos",
                dependencies,
                "sdk-v1",
                &BTreeMap::new(),
            )
            .unwrap()
        };
        let original = digest(version.revision, recipe, &dependencies);
        let mut checks_changed = recipe.clone();
        checks_changed
            .checks
            .push(vec!["xz".into(), "--help".into()]);
        assert_eq!(
            original,
            digest(version.revision, &checks_changed, &dependencies)
        );
        assert_ne!(
            original,
            digest(version.revision + 1, &checks_changed, &dependencies)
        );
        dependencies.get_mut("compiler@1").unwrap().source = LockedSource::File {
            path: "/second/compiler.tar.gz".into(),
            sha256: "c".repeat(64),
        };
        assert_eq!(original, digest(version.revision, recipe, &dependencies));
        let libraries = BTreeMap::from([(
            "compiler@1".into(),
            rootbeer_package::graph::DependencyExports {
                has_bins: true,
                has_libraries: true,
                libraries: vec![PathBuf::from("lib/libcompiler.a")],
                is_shared: false,
            },
        )]);
        assert_ne!(
            original,
            key(
                "xz@1",
                version.revision,
                recipe,
                "aarch64-macos",
                &dependencies,
                "sdk-v1",
                &libraries
            )
            .unwrap()
        );
        let exported = |libraries| {
            key(
                "xz@1",
                version.revision,
                recipe,
                "aarch64-macos",
                &dependencies,
                "sdk-v1",
                libraries,
            )
            .unwrap()
        };
        let mut shared = libraries.clone();
        shared.get_mut("compiler@1").unwrap().is_shared = true;
        assert_ne!(exported(&libraries), exported(&shared));
        dependencies.get_mut("compiler@1").unwrap().output_sha256 = Some("d".repeat(64));
        assert_ne!(original, digest(version.revision, recipe, &dependencies));
        dependencies.get_mut("compiler@1").unwrap().output_sha256 = None;
        assert!(key(
            "xz@1",
            version.revision,
            recipe,
            "aarch64-macos",
            &dependencies,
            "sdk-v1",
            &BTreeMap::new()
        )
        .is_err());
        let mut changed = recipe.clone();
        changed
            .build
            .as_mut()
            .unwrap()
            .patches
            .push("different patch".into());
        assert_ne!(
            digest(version.revision, recipe, &BTreeMap::new()),
            digest(version.revision, &changed, &BTreeMap::new())
        );
    }

    #[test]
    fn concurrent_builds_hold_one_lock_per_key() {
        let root = tempfile::tempdir().unwrap();
        let cache = BuildCache {
            directory: root.path().into(),
            context: "test".into(),
            recheck: true,
        };
        let session = BuildSession::default();
        let first = cache
            .entry(
                "same".into(),
                Some(&session),
                &rootbeer_package::Execution::default(),
            )
            .unwrap();
        assert!(first.should_rebuild);
        let deadline =
            rootbeer_package::Execution::with_timeout(std::time::Duration::from_millis(50))
                .unwrap();
        assert!(cache
            .entry("same".into(), Some(&session), &deadline)
            .err()
            .unwrap()
            .contains("deadline"));
        let (started, started_receiver) = std::sync::mpsc::channel();
        let (acquired, acquired_receiver) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            started.send(()).unwrap();
            let second = cache
                .entry(
                    "same".into(),
                    Some(&session),
                    &rootbeer_package::Execution::default(),
                )
                .unwrap();
            assert!(!second.should_rebuild);
            acquired.send(()).unwrap();
        });
        started_receiver.recv().unwrap();
        assert!(acquired_receiver
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err());
        first.complete();
        drop(first);
        acquired_receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        thread.join().unwrap();
    }
}
