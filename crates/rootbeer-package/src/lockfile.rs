use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{
    LockedPackage, PackageIntent, PackageRequest, PackageResolution, PackageResolverInputs,
    ResolutionProof, ResolveContext,
};
use crate::deterministic::DeterministicInput;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootbeerLock {
    pub schema: u32,
    #[serde(default, skip_serializing_if = "PackageResolverInputs::is_empty")]
    pub inputs: PackageResolverInputs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub resolutions: BTreeMap<String, PackageLockResolution>,
    pub packages: BTreeMap<String, LockedPackage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageLockResolution {
    pub package: String,
    pub proof: ResolutionProof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageLockEntry {
    pub package: LockedPackage,
    pub id: Option<String>,
    pub resolution_fingerprint: Option<String>,
    pub proof: Option<ResolutionProof>,
}

impl PackageLockEntry {
    pub fn locked(package: LockedPackage) -> Self {
        Self {
            package,
            id: None,
            resolution_fingerprint: None,
            proof: None,
        }
    }

    pub fn resolved(
        request: &PackageRequest,
        context: &ResolveContext,
        resolution: PackageResolution,
    ) -> Result<Self, LockError> {
        let id = package_id_for_request(request, &resolution.package);
        Ok(Self {
            package: resolution.package,
            id: Some(id),
            resolution_fingerprint: Some(resolution_fingerprint(request, context)?),
            proof: Some(resolution.proof),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockError {
    DuplicatePackage { id: String },
    DuplicateResolution { fingerprint: String },
    MissingResolutionProof { id: String },
    MissingPackage { id: String },
    PackageChanged { id: String },
    MissingLockfile { path: std::path::PathBuf },
    StaleLockfile { path: std::path::PathBuf },
    Fingerprint { kind: &'static str, error: String },
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockError::DuplicatePackage { id } => write!(f, "duplicate package `{id}`"),
            LockError::DuplicateResolution { fingerprint } => {
                write!(f, "duplicate package resolution `{fingerprint}`")
            }
            LockError::MissingResolutionProof { id } => {
                write!(f, "package resolution `{id}` is missing a proof")
            }
            LockError::MissingPackage { id } => {
                write!(f, "package `{id}` is not present in rootbeer.lock")
            }
            LockError::PackageChanged { id } => write!(
                f,
                "package `{id}` differs from the facts recorded in rootbeer.lock"
            ),
            LockError::MissingLockfile { path } => {
                write!(f, "package lockfile {} is missing", path.display())
            }
            LockError::StaleLockfile { path } => write!(
                f,
                "package lockfile {} is stale for the current plan",
                path.display()
            ),
            LockError::Fingerprint { kind, error } => {
                write!(f, "failed to fingerprint {kind}: {error}")
            }
        }
    }
}

impl std::error::Error for LockError {}

fn invalid_lock(path: &Path, error: serde_json::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid package lock {}: {error}", path.display()),
    )
}

fn retain_parseable<T: serde::de::DeserializeOwned>(map: &mut serde_json::Value) -> bool {
    let Some(map) = map.as_object_mut() else {
        return false;
    };
    let before = map.len();
    map.retain(|_, entry| T::deserialize(&*entry).is_ok());
    map.len() != before
}

impl RootbeerLock {
    pub fn from_packages(
        packages: impl IntoIterator<Item = LockedPackage>,
    ) -> Result<Self, LockError> {
        Self::from_package_entries(packages.into_iter().map(PackageLockEntry::locked))
    }

    pub fn from_package_entries(
        entries: impl IntoIterator<Item = PackageLockEntry>,
    ) -> Result<Self, LockError> {
        let mut map = BTreeMap::new();
        let mut resolutions = BTreeMap::new();
        for entry in entries {
            let id = entry.id.unwrap_or_else(|| entry.package.id());
            if map.insert(id.clone(), entry.package).is_some() {
                return Err(LockError::DuplicatePackage { id });
            }

            if let Some(fingerprint) = entry.resolution_fingerprint {
                let proof = entry
                    .proof
                    .ok_or_else(|| LockError::MissingResolutionProof { id: id.clone() })?;
                let resolution = PackageLockResolution {
                    package: id.clone(),
                    proof,
                };
                if let Some(previous) = resolutions.insert(fingerprint.clone(), resolution) {
                    if previous.package != id {
                        return Err(LockError::DuplicateResolution { fingerprint });
                    }
                }
            }
        }

        for package in map.values() {
            crate::runtime::closure(package).map_err(|error| LockError::Fingerprint {
                kind: "package.runtime",
                error,
            })?;
        }
        let schema = if map
            .values()
            .any(|package| !package.runtime_dependencies.is_empty())
        {
            3
        } else if map
            .values()
            .any(|package| !package.provides.apps.is_empty())
        {
            2
        } else {
            1
        };
        Ok(Self {
            schema,
            inputs: PackageResolverInputs::default(),
            input_fingerprint: None,
            resolutions,
            packages: map,
        })
    }

    pub fn with_input_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.input_fingerprint = Some(fingerprint.into());
        self
    }

    pub fn with_resolver_inputs(mut self, inputs: PackageResolverInputs) -> Self {
        self.inputs = inputs;
        self
    }

    pub fn matches_input_fingerprint(&self, fingerprint: &str) -> bool {
        self.input_fingerprint.as_deref() == Some(fingerprint)
    }

    pub fn package_for_intent(
        &self,
        intent: &PackageIntent,
        context: &ResolveContext,
    ) -> Result<&LockedPackage, LockError> {
        match intent {
            PackageIntent::Request(request) => self.package_for_request(request, context),
            PackageIntent::Locked(package) => self.package_for_locked_spec(package),
        }
    }

    pub fn package_for_request(
        &self,
        request: &PackageRequest,
        context: &ResolveContext,
    ) -> Result<&LockedPackage, LockError> {
        let fingerprint = resolution_fingerprint(request, context)?;
        if let Some(resolution) = self.resolutions.get(&fingerprint) {
            let id = &resolution.package;
            return self
                .packages
                .get(id)
                .ok_or_else(|| LockError::MissingPackage { id: id.clone() });
        }

        if let Some(version) = request.version.as_ref().filter(|_| {
            request.source.is_none() && request.asset.is_none() && request.bins.is_empty()
        }) {
            let id = package_id_for_request_version(request, version);
            if let Some(package) = self.packages.get(&id) {
                return Ok(package);
            }
        }

        Err(LockError::MissingPackage {
            id: request.to_string(),
        })
    }

    /// Returns a resolution only for an identical request and platform.
    pub fn resolution_for_request(
        &self,
        request: &PackageRequest,
        context: &ResolveContext,
    ) -> Result<Option<PackageResolution>, LockError> {
        let fingerprint = resolution_fingerprint(request, context)?;
        let Some(resolution) = self.resolutions.get(&fingerprint) else {
            return Ok(None);
        };
        let package =
            self.packages
                .get(&resolution.package)
                .ok_or_else(|| LockError::MissingPackage {
                    id: resolution.package.clone(),
                })?;
        Ok(Some(PackageResolution::new(
            package.clone(),
            resolution.proof.clone(),
        )))
    }

    fn package_for_locked_spec(
        &self,
        package: &LockedPackage,
    ) -> Result<&LockedPackage, LockError> {
        let id = package.id();
        let Some(locked) = self.packages.get(&id) else {
            return Err(LockError::MissingPackage { id });
        };

        if !locked.same_realization_input(package) {
            return Err(LockError::PackageChanged { id });
        }

        Ok(locked)
    }

    /// Store entries retained by these roots, including their complete runtime closure.
    pub fn store_paths(
        &self,
        store: &rootbeer_store_legacy::Store,
    ) -> Result<std::collections::BTreeSet<std::path::PathBuf>, String> {
        let mut paths = std::collections::BTreeSet::new();
        for package in self.packages.values() {
            for entry in crate::runtime::closure(package)?
                .into_iter()
                .chain(std::iter::once(package))
            {
                paths.insert(store.root().join(crate::runtime::store_directory(entry)?));
            }
        }
        Ok(paths)
    }

    pub fn read(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let bytes = fs::read(path)?;
        let lock: Self =
            serde_json::from_slice(&bytes).map_err(|error| invalid_lock(path, error))?;
        lock.validate()
    }

    /// Reads a lock another rootbeer wrote, dropping the resolver inputs,
    /// resolutions, and packages this build no longer understands so they
    /// resolve again. Returns whether anything was dropped. JSON that does not
    /// parse at all, such as a merge conflict, is still an error.
    pub fn read_compatible(path: impl AsRef<Path>) -> io::Result<(Self, bool)> {
        let path = path.as_ref();
        let bytes = fs::read(path)?;
        let mut value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|error| invalid_lock(path, error))?;

        let mut is_salvaged = false;
        if let Some(resolvers) = value.pointer_mut("/inputs/resolvers") {
            is_salvaged |= retain_parseable::<super::ResolverInput>(resolvers);
        }
        if let Some(resolutions) = value.get_mut("resolutions") {
            is_salvaged |= retain_parseable::<PackageLockResolution>(resolutions);
        }
        if let Some(packages) = value.get_mut("packages") {
            is_salvaged |= retain_parseable::<LockedPackage>(packages);
        }

        let lock: Self =
            serde_json::from_value(value).map_err(|error| invalid_lock(path, error))?;
        Ok((lock.validate()?, is_salvaged))
    }

    fn validate(self) -> io::Result<Self> {
        if !matches!(self.schema, 1..=3) {
            return Err(io::Error::other(
                "unsupported package lock schema; update Rootbeer",
            ));
        }
        for package in self.packages.values() {
            if self.schema < 3 && !package.runtime_dependencies.is_empty() {
                return Err(io::Error::other(
                    "runtime dependencies require package lock schema 3",
                ));
            }
            crate::runtime::closure(package).map_err(io::Error::other)?;
        }
        Ok(self)
    }

    pub fn write(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        let bytes = format!("{json}\n").into_bytes();
        match fs::read(path) {
            Ok(existing) if existing == bytes => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        use io::Write;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        Ok(())
    }
}

fn package_id_for_request(request: &PackageRequest, package: &LockedPackage) -> String {
    package_id_for_request_version(request, &package.version)
}

fn package_id_for_request_version(request: &PackageRequest, version: &str) -> String {
    match &request.resolver {
        Some(resolver) => format!("{resolver}:{}@{version}", request.name),
        None => format!("{}@{version}", request.name),
    }
}

fn resolution_fingerprint(
    request: &PackageRequest,
    context: &ResolveContext,
) -> Result<String, LockError> {
    request
        .resolution_input(context)
        .fingerprint()
        .map(|fingerprint| fingerprint.into_string())
        .map_err(|e| LockError::Fingerprint {
            kind: "package.resolution",
            error: e.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_json_reports_lock_path_and_location() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("rootbeer.lock");
        let contents = format!("{{\n{}not-json\n}}", "\n".repeat(16));
        fs::write(&path, &contents).unwrap();
        let error = RootbeerLock::read(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains(&path.display().to_string()));
        assert!(error.to_string().contains("line 18 column 1"));
        assert_eq!(fs::read_to_string(path).unwrap(), contents);
    }
    #[test]
    fn writes_are_atomic_and_unchanged_locks_keep_their_inode() {
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("rootbeer.lock");
        let mut lock = RootbeerLock::from_packages([]).unwrap();
        lock.write(&path).unwrap();
        let inode = fs::metadata(&path).unwrap().ino();
        lock.write(&path).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        let original = root.path().join("original");
        fs::hard_link(&path, &original).unwrap();
        lock.input_fingerprint = Some("changed".into());
        lock.write(&path).unwrap();
        assert_ne!(fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(
            RootbeerLock::read(&original).unwrap().input_fingerprint,
            None
        );
        assert_eq!(RootbeerLock::read(&path).unwrap(), lock);
    }
}
