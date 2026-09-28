//! Reading a PDR: select its signed root, then resolve a request through the package's
//! document to the signed record that approves it.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::distribution::{verify_record, PackageRecord, RECORD_LIMIT};
use crate::download::DownloadCache;
use crate::pdr::{PackageDocument, Root, RootPackage, DOCUMENT_LIMIT, ROOT_LIMIT};
use crate::store::hash_bytes;
use crate::{
    CatalogPackage, CatalogVersion, PackageCatalog, PackageRequest, PackageResolution,
    PackageResolver, ResolutionProof, ResolveContext,
};
use rootbeer_catalog::decode_hex;

/// The highest `min_engine_level` this build can install. Level 2 adds records with runtime
/// dependencies.
pub const ENGINE_LEVEL: u32 = 2;

/// A PDR as a client knows it: where its root is published and who signs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    pub url: String,
    pub public_key: String,
}

/// The exact root a resolution used, so a lock replays against the same packages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryPin {
    pub url: String,
    pub public_key: String,
    /// Digest of the signed root.
    pub root: String,
}

/// The signed record an installation used, independent of later publications.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageRecordProof {
    /// Digest of the signed record.
    pub record: String,
    pub public_key: String,
    pub system: String,
}

/// A selected root, and why it is not the freshest one when the network was unavailable.
#[derive(Debug)]
pub struct Selection {
    pub pin: RepositoryPin,
    pub notice: Option<String>,
}

enum FetchError {
    Unavailable(String),
    Invalid(String),
}

impl Repository {
    /// The official PDR a release build is configured to trust, if any.
    pub fn official() -> Result<Option<Self>, String> {
        match (
            option_env!("ROOTBEER_PDR_URL"),
            option_env!("ROOTBEER_PDR_PUBLIC_KEY"),
        ) {
            (None, None) => Ok(None),
            (Some(url), Some(public_key)) => {
                let repository = Self {
                    url: url.into(),
                    public_key: public_key.into(),
                };
                repository.validate()?;
                Ok(Some(repository))
            }
            _ => Err(
                "release must configure both ROOTBEER_PDR_URL and ROOTBEER_PDR_PUBLIC_KEY".into(),
            ),
        }
    }

    /// The repository a configuration names, or else the one this build trusts.
    pub fn chosen(configured: Option<&Self>) -> Result<Self, String> {
        if let Some(configured) = configured {
            configured.validate()?;
            return Ok(configured.clone());
        }
        Self::official()?.ok_or_else(|| {
            "this build trusts no PDR; build with ROOTBEER_PDR_URL and ROOTBEER_PDR_PUBLIC_KEY, \
             or call rb.package_repository()"
                .into()
        })
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.url.starts_with("file:///") {
            rootbeer_catalog::validate_https(&self.url)?;
        }
        if !self.url.ends_with(".json") || !self.url.contains('/') {
            return Err("a repository URL names its root document".into());
        }
        decode_hex::<32>(&self.public_key)?;
        Ok(())
    }

    /// Fetches, verifies and caches the current root. When the network is unavailable the
    /// last verified root is used instead, unless the caller asked for a refresh.
    pub fn select(&self, state: &Path, should_refresh: bool) -> Result<Selection, String> {
        self.select_with(state, should_refresh, fetch)
    }

    /// Selects the last verified root without contacting the repository.
    pub fn select_offline(&self, state: &Path) -> Result<Selection, String> {
        self.validate()?;
        let cached = self.cached(state)?.ok_or(
            "no verified package repository is cached; run once with network access first",
        )?;
        Ok(Selection {
            pin: self.pin(&cached),
            notice: None,
        })
    }

    fn select_with(
        &self,
        state: &Path,
        should_refresh: bool,
        mut fetch: impl FnMut(&str, usize) -> Result<Vec<u8>, FetchError>,
    ) -> Result<Selection, String> {
        self.validate()?;
        let cache = self.cache_path(state);
        fs::create_dir_all(cache.parent().expect("cache has a parent"))
            .map_err(|error| error.to_string())?;
        let guard = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(cache.with_extension("lock"))
            .map_err(|error| error.to_string())?;
        guard.lock().map_err(|error| error.to_string())?;

        let cached = self.cached(state)?;
        let bytes = match fetch(&self.url, ROOT_LIMIT) {
            Ok(bytes) => bytes,
            Err(FetchError::Invalid(reason)) => return Err(reason),
            Err(FetchError::Unavailable(reason)) if should_refresh => {
                return Err(format!("cannot refresh the package repository: {reason}"));
            }
            Err(FetchError::Unavailable(reason)) => {
                let cached = cached
                    .ok_or_else(|| format!("{reason}; no verified package repository is cached"))?;
                return Ok(Selection {
                    pin: self.pin(&cached),
                    notice: Some(format!("{reason}; using the cached package repository")),
                });
            }
        };
        let root = Root::from_bytes(&bytes, &self.public_key)?;
        if let Some(previous) = &cached {
            let previous = Root::from_bytes(previous, &self.public_key)?;
            if root.sequence < previous.sequence
                || (root.sequence == previous.sequence && Some(&bytes) != cached.as_ref())
            {
                return Err("package repository rollback or conflicting sequence detected".into());
            }
        }

        let downloads = state.join("downloads");
        fs::create_dir_all(&downloads).map_err(|error| error.to_string())?;
        atomic_write(
            &downloads.join(format!("sha256-{}", hash_bytes(&bytes))),
            &bytes,
        )?;
        atomic_write(&cache, &bytes)?;
        Ok(Selection {
            pin: self.pin(&bytes),
            notice: None,
        })
    }

    fn pin(&self, root: &[u8]) -> RepositoryPin {
        RepositoryPin {
            url: self.url.clone(),
            public_key: self.public_key.clone(),
            root: hash_bytes(root),
        }
    }

    fn cache_path(&self, state: &Path) -> PathBuf {
        let identity = rootbeer_catalog::canonical_sha256(self).expect("a repository serializes");
        state
            .join("repositories")
            .join(identity)
            .join("current.json")
    }

    /// The last verified root, re-verified so a tampered cache is never trusted.
    fn cached(&self, state: &Path) -> Result<Option<Vec<u8>>, String> {
        match fs::read(self.cache_path(state)) {
            Ok(bytes) => {
                Root::from_bytes(&bytes, &self.public_key)?;
                Ok(Some(bytes))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
}

impl RepositoryPin {
    /// The repository this root belongs to, without which root it was.
    pub fn repository(&self) -> Repository {
        Repository {
            url: self.url.clone(),
            public_key: self.public_key.clone(),
        }
    }

    /// Where a digest-addressed document lives, beside the root.
    fn locate(&self, directory: &str, digest: &str) -> String {
        let (base, _) = self.url.rsplit_once('/').expect("validated repository URL");
        format!("{base}/{directory}/{digest}.json")
    }
}

/// Resolves requests against one pinned root.
pub struct RepositoryResolver {
    pin: RepositoryPin,
    downloads: DownloadCache,
    root: OnceLock<Result<Root, String>>,
}

impl RepositoryResolver {
    pub fn new(pin: &RepositoryPin) -> Self {
        Self::with_cache(pin, crate::state_dir().join("downloads"), false)
    }

    pub fn with_cache(pin: &RepositoryPin, cache: impl Into<PathBuf>, is_offline: bool) -> Self {
        Self {
            pin: pin.clone(),
            downloads: if is_offline {
                DownloadCache::offline(cache)
            } else {
                DownloadCache::new(cache)
            },
            root: OnceLock::new(),
        }
    }

    /// The exact root this resolver reads.
    pub fn pin(&self) -> &RepositoryPin {
        &self.pin
    }

    pub fn root(&self) -> Result<&Root, String> {
        self.root
            .get_or_init(|| Root::from_bytes(&self.root_bytes()?, &self.pin.public_key))
            .as_ref()
            .map_err(Clone::clone)
    }

    fn root_bytes(&self) -> Result<Vec<u8>, String> {
        self.read(
            &self.pin.locate("roots", &self.pin.root),
            &self.pin.root,
            ROOT_LIMIT,
        )
    }

    /// The package a request names, refusing one this build is too old to install.
    pub fn package(&self, name: &str) -> Result<(&str, &RootPackage), String> {
        let root = self.root()?;
        let Some((name, package)) = root.find(name) else {
            return Err(match root.find_unreadable(name) {
                Some(name) => needs_newer(name),
                None => format!("unknown package {name}"),
            });
        };
        if package
            .min_engine_level
            .is_some_and(|level| level > ENGINE_LEVEL)
        {
            return Err(needs_newer(name));
        }
        Ok((name, package))
    }

    pub fn document(&self, name: &str, package: &RootPackage) -> Result<PackageDocument, String> {
        let bytes = self.read(
            &self.pin.locate("packages", &package.document),
            &package.document,
            DOCUMENT_LIMIT,
        )?;
        let document = PackageDocument::from_bytes(&bytes)?;
        package.check_document(name, &document)?;
        Ok(document)
    }

    /// Published recipes for `names` and every package they build with, as a catalog, for
    /// building from source. Only the documents in that closure are fetched.
    pub fn catalog<'a>(
        &self,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<PackageCatalog, String> {
        let mut packages = BTreeMap::new();
        let mut pending: Vec<String> = names.into_iter().map(str::to_string).collect();
        while let Some(requested) = pending.pop() {
            let (name, package) = self.package(&requested)?;
            if packages.contains_key(name) {
                continue;
            }
            let document = self.document(name, package)?;
            let recipes = document
                .versions
                .values()
                .flat_map(|version| version.platforms.values());
            for platform in recipes {
                let dependencies = platform
                    .recipe
                    .build
                    .iter()
                    .flat_map(|build| &build.dependencies);
                pending.extend(
                    dependencies.map(|dependency| PackageRequest::parse(dependency.package()).name),
                );
            }
            packages.insert(name.to_string(), catalog_package(name, package, document));
        }
        Ok(PackageCatalog {
            extra: Default::default(),
            packages,
        })
    }

    /// The verified record for a request, and its digest.
    pub fn record(
        &self,
        request: &PackageRequest,
        context: &ResolveContext,
    ) -> Result<(PackageRecord, String), String> {
        let (name, package) = self.package(&request.name)?;
        self.record_of(name, package, request.version.as_deref(), context)
    }

    /// The verified record for one package's entry, at `version` or this platform's default.
    pub fn record_of(
        &self,
        name: &str,
        package: &RootPackage,
        version: Option<&str>,
        context: &ResolveContext,
    ) -> Result<(PackageRecord, String), String> {
        let (record, digest, _) = self.signed_record_of(name, package, version, context)?;
        Ok((record, digest))
    }

    /// The verified record, its digest, and the exact signed bytes, for evidence that must be
    /// verifiable again later.
    pub fn signed_record_of(
        &self,
        name: &str,
        package: &RootPackage,
        version: Option<&str>,
        context: &ResolveContext,
    ) -> Result<(PackageRecord, String, Vec<u8>), String> {
        let version = match version {
            Some(version) => version,
            None => package
                .platforms
                .get(&context.system)
                .map(|platform| platform.version.as_str())
                .ok_or_else(|| format!("{name} does not support {}", context.system))?,
        };
        let id = format!("{name}@{version}");
        let document = self.document(name, package)?;
        if document
            .unreadable
            .contains(&(version.to_string(), context.system.clone()))
        {
            return Err(needs_newer(&id));
        }
        let published = document
            .versions
            .get(version)
            .and_then(|entry| entry.platforms.get(&context.system))
            .ok_or_else(|| format!("{id} has no published package for {}", context.system))?;
        let bytes = self.read(
            &self.pin.locate("records", &published.record),
            &published.record,
            RECORD_LIMIT,
        )?;
        let record = verify_record(&bytes, &self.pin.public_key, &id, &context.system)?;
        if record.recipe != published.recipe {
            return Err(format!("{id}: record differs from the published recipe"));
        }
        Ok((record, published.record.clone(), bytes))
    }

    fn read(&self, url: &str, sha256: &str, limit: usize) -> Result<Vec<u8>, String> {
        let path = self
            .downloads
            .materialize_verified(url, sha256)
            .map_err(|error| error.to_string())?;
        if fs::metadata(&path)
            .map_err(|error| error.to_string())?
            .len()
            > limit as u64
        {
            return Err("package metadata exceeds its size limit".into());
        }
        fs::read(path).map_err(|error| error.to_string())
    }
}

impl PackageResolver for RepositoryResolver {
    fn name(&self) -> &str {
        "rootbeer"
    }

    fn resolve(
        &self,
        request: &PackageRequest,
        context: &ResolveContext,
    ) -> Result<Option<PackageResolution>, String> {
        if request.source.is_some() || request.asset.is_some() || !request.bins.is_empty() {
            return Err(
                "published packages do not accept source, asset, or command overrides".into(),
            );
        }
        let (record, digest) = self.record(request, context)?;
        Ok(Some(PackageResolution::new(
            record.artifact.package,
            ResolutionProof::PackageRecord(PackageRecordProof {
                record: digest,
                public_key: self.pin.public_key.clone(),
                system: context.system.clone(),
            }),
        )))
    }
}

fn needs_newer(package: &str) -> String {
    format!("{package} needs a newer rb; run rb self-update")
}

fn catalog_package(name: &str, package: &RootPackage, document: PackageDocument) -> CatalogPackage {
    CatalogPackage {
        name: name.to_string(),
        aliases: package.aliases.clone(),
        description: package.description.clone(),
        homepage: package.homepage.clone(),
        recipe_maintainers: package.maintainers.clone(),
        min_engine_level: package.min_engine_level,
        default_versions: package
            .platforms
            .iter()
            .map(|(system, platform)| (system.clone(), platform.version.clone()))
            .collect(),
        versions: document
            .versions
            .into_iter()
            .map(|(version, entry)| {
                let platforms = entry
                    .platforms
                    .into_iter()
                    .map(|(system, platform)| (system, platform.recipe))
                    .collect();
                let version_entry = CatalogVersion {
                    license: entry.license,
                    revision: entry.revision,
                    platforms,
                    extra: Default::default(),
                };
                (version, version_entry)
            })
            .collect(),
        extra: Default::default(),
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("cache path has no parent")?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    file.write_all(bytes).map_err(|e| e.to_string())?;
    file.as_file().sync_all().map_err(|e| e.to_string())?;
    file.persist(path).map_err(|e| e.to_string())?;
    Ok(())
}

fn fetch(url: &str, limit: usize) -> Result<Vec<u8>, FetchError> {
    if let Some(path) = url.strip_prefix("file://") {
        return fs::read(path).map_err(|error| FetchError::Unavailable(error.to_string()));
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into();
    let mut response = agent.get(url).call().map_err(|error| {
        FetchError::Unavailable(format!("package repository unavailable: {error}"))
    })?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| FetchError::Unavailable(error.to_string()))?;
    if bytes.len() > limit {
        return Err(FetchError::Invalid(
            "package repository root exceeds its size limit".into(),
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ring::signature::Ed25519KeyPair;

    use super::*;
    use crate::pdr::{DocumentPlatform, DocumentVersion, PackageKind, RootPlatform, ROOT_SCHEMA};

    const SYSTEM: &str = "aarch64-linux";

    /// A site laid out as `publish_records` writes it, around one real signed record.
    struct Site {
        directory: tempfile::TempDir,
        repository: Repository,
        name: String,
        version: String,
    }

    impl Site {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let (record, public_key, id) = crate::distribution::tests::signed();
            let (name, version) = id.split_once('@').unwrap();
            let digest = hash_bytes(&record);
            fs::create_dir_all(directory.path().join("records")).unwrap();
            fs::write(
                directory
                    .path()
                    .join("records")
                    .join(format!("{digest}.json")),
                &record,
            )
            .unwrap();
            let site = Self {
                repository: Repository {
                    url: format!("file://{}/current.json", directory.path().display()),
                    public_key,
                },
                directory,
                name: name.into(),
                version: version.into(),
            };
            site.publish(1, |_, _| {});
            site
        }

        fn record(&self) -> PackageRecord {
            let (bytes, key, id) = crate::distribution::tests::signed();
            verify_record(&bytes, &key, &id, SYSTEM).unwrap()
        }

        /// Signs and writes a root at `sequence`, after `change` edits what it will publish.
        fn publish(
            &self,
            sequence: u64,
            change: impl FnOnce(&mut RootPackage, &mut PackageDocument),
        ) {
            self.publish_raw(sequence, change, |_, _| {});
        }

        /// Like [`Site::publish`], then `raw` edits the published JSON as a newer publisher
        /// might, with fields and values this build's types cannot express.
        fn publish_raw(
            &self,
            sequence: u64,
            change: impl FnOnce(&mut RootPackage, &mut PackageDocument),
            raw: impl FnOnce(&mut serde_json::Value, &mut serde_json::Value),
        ) {
            let record = self.record();
            let (bytes, _, _) = crate::distribution::tests::signed();
            let mut document = PackageDocument {
                name: self.name.clone(),
                versions: BTreeMap::from([(
                    self.version.clone(),
                    DocumentVersion {
                        license: "MIT".into(),
                        revision: record.revision,
                        platforms: BTreeMap::from([(
                            SYSTEM.into(),
                            DocumentPlatform {
                                recipe: record.recipe.clone(),
                                record: hash_bytes(&bytes),
                                published: record.published,
                            },
                        )]),
                    },
                )]),
                unreadable: Default::default(),
            };
            let mut package = RootPackage {
                aliases: Vec::new(),
                description: "A package".into(),
                homepage: "https://example.com".into(),
                license: "MIT".into(),
                maintainers: Vec::new(),
                min_engine_level: None,
                added: record.published,
                updated: record.published,
                platforms: BTreeMap::from([(
                    SYSTEM.into(),
                    RootPlatform {
                        version: self.version.clone(),
                        kind: PackageKind::Command,
                        commands: record.recipe.bins.names().into_iter().cloned().collect(),
                    },
                )]),
                document: String::new(),
            };
            change(&mut package, &mut document);
            let mut package = serde_json::to_value(&package).unwrap();
            let mut document = serde_json::to_value(&document).unwrap();
            raw(&mut package, &mut document);

            let document = rootbeer_catalog::canonical_json(&document).unwrap();
            let digest = hash_bytes(&document);
            package["document"] = digest.clone().into();
            let packages = self.directory.path().join("packages");
            fs::create_dir_all(&packages).unwrap();
            fs::write(packages.join(format!("{digest}.json")), document).unwrap();

            let packages = serde_json::json!({ self.name.clone(): package });
            let key = Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap();
            let signature: String = key
                .sign(&crate::pdr::signing_message(sequence, &packages).unwrap())
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let root = serde_json::json!({
                "schema": ROOT_SCHEMA,
                "sequence": sequence,
                "packages": packages,
                "signature": signature,
            });
            let bytes = serde_json::to_vec(&root).unwrap();
            let roots = self.directory.path().join("roots");
            fs::create_dir_all(&roots).unwrap();
            fs::write(roots.join(format!("{}.json", hash_bytes(&bytes))), &bytes).unwrap();
            fs::write(self.directory.path().join("current.json"), &bytes).unwrap();
        }

        fn state(&self) -> PathBuf {
            self.directory.path().join("state")
        }

        fn resolve(&self, pin: &RepositoryPin) -> Result<PackageResolution, String> {
            RepositoryResolver::with_cache(pin, self.state().join("downloads"), false)
                .resolve(
                    &PackageRequest::parse(&self.name),
                    &ResolveContext::new(SYSTEM),
                )
                .map(Option::unwrap)
        }
    }

    #[test]
    fn resolves_through_the_root_the_package_document_and_the_record() {
        let site = Site::new();
        let selection = site.repository.select(&site.state(), false).unwrap();
        assert!(selection.notice.is_none());

        let resolution = site.resolve(&selection.pin).unwrap();
        assert_eq!(resolution.package, site.record().artifact.package);
        let ResolutionProof::PackageRecord(proof) = resolution.proof else {
            panic!("a repository resolution is proven by its record")
        };
        assert_eq!(proof.system, SYSTEM);
        assert!(rootbeer_catalog::is_sha256(&proof.record));
    }

    #[test]
    fn a_locked_root_replays_offline_after_newer_publications() {
        let site = Site::new();
        let pin = site.repository.select(&site.state(), false).unwrap().pin;
        site.resolve(&pin).unwrap();

        site.publish(2, |package, _| package.description = "Changed".into());
        let offline = RepositoryResolver::with_cache(&pin, site.state().join("downloads"), true);
        assert_eq!(
            offline.root().unwrap().sequence,
            1,
            "the lock keeps its exact root"
        );
        assert_eq!(
            site.repository.select_offline(&site.state()).unwrap().pin,
            pin
        );
    }

    #[test]
    fn rejects_rollbacks_and_conflicting_roots() {
        let site = Site::new();
        site.publish(2, |_, _| {});
        site.repository.select(&site.state(), false).unwrap();

        site.publish(1, |_, _| {});
        let error = site.repository.select(&site.state(), false).unwrap_err();
        assert!(error.contains("rollback"), "{error}");

        site.publish(2, |package, _| package.description = "Different".into());
        let error = site.repository.select(&site.state(), false).unwrap_err();
        assert!(error.contains("conflicting"), "{error}");
    }

    #[test]
    fn an_unreachable_repository_falls_back_to_its_verified_cache_unless_refreshing() {
        let site = Site::new();
        let pin = site.repository.select(&site.state(), false).unwrap().pin;
        fs::remove_file(site.directory.path().join("current.json")).unwrap();

        let selection = site.repository.select(&site.state(), false).unwrap();
        assert_eq!(selection.pin, pin);
        assert!(selection.notice.unwrap().contains("cached"));
        let error = site.repository.select(&site.state(), true).unwrap_err();
        assert!(error.contains("cannot refresh"), "{error}");
    }

    #[test]
    fn a_package_above_this_engine_is_named_as_needing_a_newer_rb() {
        let site = Site::new();
        site.publish(1, |package, _| {
            package.min_engine_level = Some(ENGINE_LEVEL + 1)
        });
        let pin = site.repository.select(&site.state(), false).unwrap().pin;
        let error = site.resolve(&pin).unwrap_err();
        assert!(error.contains("needs a newer rb"), "{error}");
    }

    #[test]
    fn a_document_that_misstates_its_record_is_refused() {
        let site = Site::new();
        site.publish(1, |_, document| {
            let platform = document
                .versions
                .values_mut()
                .next()
                .unwrap()
                .platforms
                .get_mut(SYSTEM)
                .unwrap();
            let command = platform
                .recipe
                .bins
                .names()
                .into_iter()
                .next()
                .unwrap()
                .clone();
            platform.recipe.checks.push(vec![command, "--help".into()]);
        });
        let pin = site.repository.select(&site.state(), false).unwrap().pin;
        let error = site.resolve(&pin).unwrap_err();
        assert!(error.contains("differs"), "{error}");
    }

    #[test]
    fn a_package_this_build_cannot_read_needs_a_newer_rb() {
        let site = Site::new();
        site.publish_raw(
            1,
            |_, _| {},
            |package, _| {
                package["platforms"][SYSTEM]["kind"] = "font".into();
            },
        );
        let pin = site.repository.select(&site.state(), false).unwrap().pin;
        let error = site.resolve(&pin).unwrap_err();
        assert!(error.contains("needs a newer rb"), "{error}");
    }

    #[test]
    fn a_retained_version_this_build_cannot_read_costs_only_that_version() {
        let site = Site::new();
        site.publish_raw(
            1,
            |_, _| {},
            |_, document| {
                let versions = document["versions"].as_object_mut().unwrap();
                let mut newer = versions.values().next().unwrap().clone();
                newer["platforms"][SYSTEM]["recipe"]["install"] = "Pkg".into();
                versions.insert("0.0.1".into(), newer);
            },
        );
        let pin = site.repository.select(&site.state(), false).unwrap().pin;
        assert!(
            site.resolve(&pin).is_ok(),
            "the default version is unaffected"
        );

        let error = RepositoryResolver::with_cache(&pin, site.state().join("downloads"), false)
            .resolve(
                &PackageRequest::parse(&format!("{}@0.0.1", site.name)),
                &ResolveContext::new(SYSTEM),
            )
            .unwrap_err();
        assert!(error.contains("needs a newer rb"), "{error}");
    }
}
