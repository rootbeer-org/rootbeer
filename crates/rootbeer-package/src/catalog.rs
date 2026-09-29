use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{
    PackageRequest, PackageRequestResolver, PackageResolution, PackageResolver,
    PackageResolverInputs, ResolutionProof, ResolveContext, ResolverStack,
};

mod recipe;

pub(crate) use recipe::{validate_apps, validate_commands, validate_systems, SYSTEMS};
pub use recipe::{Bins, CatalogPackage, CatalogRecipe, CatalogVersion, ExtraFields};

/// A versioned snapshot of Rootbeer's canonical package definitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageCatalog {
    pub packages: BTreeMap<String, CatalogPackage>,
    /// Fields published by a newer engine, retained so they survive a round trip.
    #[serde(
        flatten,
        default,
        skip_serializing_if = "recipe::ExtraFields::is_empty"
    )]
    pub extra: recipe::ExtraFields,
}

/// Connects canonical identity to the exact backend resolution used to install it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogProof {
    pub catalog_sha256: String,
    pub name: String,
    pub version: String,
    pub revision: u32,
    pub source: PackageRequest,
    pub source_proof: Box<ResolutionProof>,
}

impl PackageCatalog {
    /// Loads a recipe directory without rebuilding the executable.
    pub fn from_directory(directory: &Path) -> Result<Self, String> {
        Self::from_definitions(&super::PackageDefinition::from_directory(directory)?)
    }

    #[cfg(test)]
    fn from_lua(recipes: &[(&str, &str)]) -> Result<Self, String> {
        let mut definitions = BTreeMap::new();
        for (name, source) in recipes {
            let definition =
                super::PackageDefinition::from_lua(source).map_err(|e| format!("{name}: {e}"))?;
            if definitions.insert(name.to_string(), definition).is_some() {
                return Err(format!("duplicate canonical package `{name}`"));
            }
        }
        Self::from_definitions(&definitions)
    }

    /// Builds a validated catalog from already evaluated package definitions.
    pub fn from_definitions(
        definitions: &BTreeMap<String, super::PackageDefinition>,
    ) -> Result<Self, String> {
        let mut packages = BTreeMap::new();
        for (name, definition) in definitions {
            if definition.package.name != *name {
                return Err(format!("{name}: canonical name must match the filename"));
            }
            packages.insert(name.clone(), definition.package.clone());
        }
        let catalog = Self {
            extra: Default::default(),
            packages,
        };
        catalog.validate()?;
        for (name, definition) in definitions {
            for (upstream, _) in definition.upstreams() {
                let Some(repository) = upstream.github() else {
                    continue;
                };
                let is_source = definition
                    .package
                    .versions
                    .values()
                    .flat_map(|entry| entry.platforms.values())
                    .all(|recipe| recipe.build.is_some());
                super::upstream::check_identity(&catalog, name, repository, is_source)?;
            }
        }
        Ok(catalog)
    }

    /// Loads local recipes, allowing dependencies supplied by the selected registry.
    pub fn from_local_directory(directory: &Path) -> Result<Self, String> {
        let definitions = super::PackageDefinition::from_directory(directory)?;
        let catalog = Self {
            extra: Default::default(),
            packages: definitions
                .into_iter()
                .map(|(name, definition)| (name, definition.package))
                .collect(),
        };
        catalog.validate_recipes()?;
        if !catalog.requires_pdr() {
            catalog.validate()?;
        }
        Ok(catalog)
    }

    /// Whether any declared dependency must be supplied by the PDR.
    pub fn requires_pdr(&self) -> bool {
        self.packages
            .values()
            .flat_map(|package| package.versions.values())
            .flat_map(|version| version.platforms.values())
            .filter_map(|recipe| recipe.build.as_ref())
            .flat_map(|build| &build.dependencies)
            .any(|dependency| {
                self.find(&PackageRequest::parse(dependency.package()).name)
                    .is_none()
            })
    }

    /// Checks complete recipes and their dependency graph.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_recipes()?;
        crate::graph::validate_dependencies(self)
    }

    /// Checks recipe identities and contracts without requiring a complete dependency graph.
    pub fn validate_recipes(&self) -> Result<(), String> {
        if self.packages.is_empty() {
            return Err("catalog must contain packages".into());
        }
        let mut names = BTreeSet::new();
        for (name, package) in &self.packages {
            if name != &package.name
                || package.description.trim().is_empty()
                || !package.homepage.starts_with("https://")
            {
                return Err(format!("{name}: invalid package identity"));
            }
            for identity in std::iter::once(name).chain(package.aliases.iter()) {
                if !valid_name(identity) || !names.insert(identity) {
                    return Err(format!("invalid or duplicate package name `{identity}`"));
                }
            }
            package.validate()?;
            for version in package.versions.keys() {
                if version.is_empty()
                    || matches!(version.as_str(), "latest" | "HEAD")
                    || version
                        .bytes()
                        .any(|c| c.is_ascii_whitespace() || matches!(c, b'/' | b':' | b'@'))
                {
                    return Err(format!("{name}: invalid version `{version}`"));
                }
            }
        }
        Ok(())
    }

    /// Finds a package by canonical name or an explicitly declared alias.
    pub fn find(&self, name: &str) -> Option<&CatalogPackage> {
        self.packages.get(name).or_else(|| {
            self.packages
                .values()
                .find(|package| package.aliases.iter().any(|alias| alias == name))
        })
    }

    /// Returns a stable digest of the evaluated catalog data.
    pub fn sha256(&self) -> String {
        rootbeer_catalog::canonical_sha256(self).expect("catalog serialization cannot fail")
    }

    /// Exports the validated catalog as deterministic JSON for inspection or CI.
    pub fn to_json(&self) -> Result<String, String> {
        self.validate()?;
        serde_json::to_string_pretty(self).map_err(|e| e.to_string())
    }
}

pub fn valid_name(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// Where each command a direct download declares by name lives in it, found as the GitHub
/// backend finds them: by the executables the verified artifact actually contains.
fn downloaded_bins(
    downloads: &super::download::DownloadCache,
    package: &str,
    url: &str,
    sha256: &str,
    install: &super::LockedInstall,
    declared: &super::Bins,
) -> Result<BTreeMap<String, std::path::PathBuf>, String> {
    let names = declared.names();
    if names.is_empty() {
        return Ok(BTreeMap::new());
    }
    let found = match install {
        super::LockedInstall::Archive {
            format,
            strip_prefix,
        } => {
            let archive = downloads
                .materialize_verified(url, sha256)
                .map_err(|error| error.to_string())?;
            let extracted = tempfile::tempdir().map_err(|error| error.to_string())?;
            super::realize::extract_archive(&archive, *format, extracted.path())
                .map_err(|error| error.to_string())?;
            let root = match strip_prefix {
                Some(prefix) => extracted.path().join(prefix),
                None => extracted.path().to_path_buf(),
            };
            super::github::discover_bins(&root)?
        }
        super::LockedInstall::Binary { path } => {
            let [name] = names.iter().collect::<Vec<_>>()[..] else {
                return Err(format!(
                    "{package}: a raw binary provides exactly one command"
                ));
            };
            BTreeMap::from([((*name).clone(), path.clone())])
        }
        _ => {
            return Err(format!(
                "{package}: this install format needs each command's path in outputs.bins"
            ))
        }
    };
    names
        .into_iter()
        .map(|name| {
            let path = found.get(name).ok_or_else(|| {
                format!(
                    "{package}: the download has no executable `{name}`; map it in outputs.bins"
                )
            })?;
            Ok((name.clone(), path.clone()))
        })
        .collect()
}

pub struct CatalogResolver {
    catalog: PackageCatalog,
    inputs: PackageResolverInputs,
    backends: ResolverStack,
    /// Resolves names this catalog does not define.
    repository: Option<super::RepositoryResolver>,
}

impl CatalogResolver {
    pub fn new(
        catalog: &PackageCatalog,
        inputs: &PackageResolverInputs,
        backends: ResolverStack,
    ) -> Self {
        Self {
            catalog: catalog.clone(),
            inputs: inputs.clone(),
            backends,
            repository: inputs.repository().map(super::RepositoryResolver::new),
        }
    }
}

impl PackageResolver for CatalogResolver {
    fn name(&self) -> &str {
        "rootbeer"
    }

    fn resolve(
        &self,
        request: &PackageRequest,
        context: &ResolveContext,
    ) -> Result<Option<PackageResolution>, String> {
        let catalog = &self.catalog;
        let Some(package) = catalog.find(&request.name) else {
            if let Some(repository) = &self.repository {
                return repository.resolve(request, context);
            }
            return Err(format!("unknown catalog package `{}`; use `rootbeer-forge list` or an explicit backend request", request.name));
        };
        if request.source.is_some() {
            return Err("source requests require a source-build resolver".into());
        }
        if request.asset.is_some() || !request.bins.is_empty() {
            return Err("catalog recipes own assets and commands; use an explicit backend request for overrides".into());
        }
        let digest = catalog.sha256();
        if self
            .inputs
            .catalog_sha256()
            .is_some_and(|expected| expected != digest)
        {
            return Err("the locked catalog differs from the selected catalog; select the matching catalog or explicitly refresh with --update".into());
        }
        let version = match request.version.as_deref() {
            Some(version) => version,
            None => package
                .default_version_for(&context.system)
                .ok_or_else(|| format!("{} does not support {}", package.name, context.system))?,
        };
        let entry = package.versions.get(version).ok_or_else(|| {
            format!(
                "{}@{version} is not in the catalog; available: {}",
                package.name,
                package
                    .versions
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        let recipe = entry.for_system(&context.system).ok_or_else(|| {
            format!(
                "{}@{version} has no recipe for {}",
                package.name, context.system
            )
        })?;
        let source = recipe.source.as_deref().ok_or_else(|| format!(
            "{}@{version} has a source recipe but no published binary; use `rootbeer-forge build {} --output <directory>` explicitly",
            package.name, package.name
        ))?;
        if source.starts_with("https://") {
            let sha256 = recipe
                .sha256
                .clone()
                .ok_or("direct download requires a checksum")?;
            let install = recipe
                .install
                .clone()
                .ok_or("direct download requires an install format")?;
            let bins = match recipe.bins.paths() {
                Some(paths) => paths.clone(),
                None => downloaded_bins(
                    &super::download::DownloadCache::default(),
                    &package.name,
                    source,
                    &sha256,
                    &install,
                    &recipe.bins,
                )?,
            };
            return Ok(Some(PackageResolution::new(
                super::LockedPackage {
                    name: package.name.clone(),
                    version: version.into(),
                    source: super::LockedSource::Url {
                        url: source.into(),
                        sha256: sha256.clone(),
                    },
                    install,
                    provides: super::Provides {
                        bins,
                        apps: recipe.apps.clone(),
                    },
                    runtime_dependencies: BTreeMap::new(),
                    output_sha256: None,
                },
                ResolutionProof::Download {
                    url: source.into(),
                    sha256,
                },
            )));
        }
        let mut source = PackageRequest::parse(source);
        source.asset = recipe.asset.clone();
        source.bins = recipe.bins.paths().cloned().unwrap_or_default();
        let resolution = self
            .backends
            .resolve_package(&source, context)
            .map_err(|e| e.to_string())?;
        let mut locked = resolution.package;
        locked.provides.apps = recipe.apps.clone();
        if locked.install == super::LockedInstall::Dmg && recipe.apps.is_empty() {
            return Err("DMG recipes must declare application bundles in outputs.apps".into());
        }
        if let (Some("github"), super::LockedInstall::Binary { path }, true) = (
            source.resolver.as_deref(),
            &mut locked.install,
            recipe.bins.paths().is_none(),
        ) {
            let [command] = recipe.bins.names().into_iter().collect::<Vec<_>>()[..] else {
                return Err(format!(
                    "{}: raw GitHub assets must declare exactly one command",
                    package.name
                ));
            };
            // Raw assets have no internal filename; the recipe owns their installed command.
            *path = command.clone().into();
            locked.provides.bins = BTreeMap::from([(command.clone(), path.clone())]);
        }
        if let Some(expected) = recipe.sha256.as_ref() {
            if !matches!(&locked.source, super::LockedSource::Url { sha256, .. } if sha256 == expected)
            {
                return Err(format!(
                    "{}@{version}: GitHub asset checksum differs from the recipe",
                    package.name
                ));
            }
        }
        for bin in recipe.bins.names() {
            if !locked.provides.bins.contains_key(bin) {
                return Err(format!(
                    "{}: backend did not provide declared command `{bin}`",
                    package.name
                ));
            }
        }
        locked
            .provides
            .bins
            .retain(|name, _| recipe.bins.names().contains(name));
        locked.name = package.name.clone();
        locked.version = version.to_string();
        Ok(Some(PackageResolution::new(
            locked,
            ResolutionProof::Catalog(CatalogProof {
                catalog_sha256: digest,
                name: package.name.clone(),
                version: version.to_string(),
                revision: entry.revision,
                source,
                source_proof: Box::new(resolution.proof),
            }),
        )))
    }
}

#[cfg(test)]
mod tests {
    /// A tar.gz whose executables live below a versioned directory, as zig's releases do.
    fn nested_archive(root: &Path) -> (String, String) {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let tree = root.join("tree/zig-linux-1.0");
        fs::create_dir_all(tree.join("lib")).unwrap();
        for (name, mode) in [("zig", 0o755), ("lib/README", 0o644)] {
            fs::write(tree.join(name), b"file").unwrap();
            fs::set_permissions(tree.join(name), fs::Permissions::from_mode(mode)).unwrap();
        }
        let archive = root.join("zig.tar.gz");
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            fs::File::create(&archive).unwrap(),
            flate2::Compression::default(),
        ));
        builder.append_dir_all(".", root.join("tree")).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        (
            format!("file://{}", archive.display()),
            crate::store::hash_file(&archive).unwrap(),
        )
    }

    #[test]
    fn a_direct_download_finds_the_commands_it_declares_by_name() {
        let root = tempfile::tempdir().unwrap();
        let (url, sha256) = nested_archive(root.path());
        let downloads = crate::download::DownloadCache::new(root.path().join("downloads"));
        let install = crate::LockedInstall::Archive {
            format: crate::ArchiveFormat::TarGz,
            strip_prefix: None,
        };
        let found = downloaded_bins(
            &downloads,
            "zig",
            &url,
            &sha256,
            &install,
            &crate::Bins::Names(vec!["zig".into()]),
        )
        .unwrap();
        assert_eq!(found["zig"], std::path::PathBuf::from("zig-linux-1.0/zig"));

        let error = downloaded_bins(
            &downloads,
            "zig",
            &url,
            &sha256,
            &install,
            &crate::Bins::Names(vec!["zls".into()]),
        )
        .unwrap_err();
        assert!(error.contains("no executable `zls`"), "{error}");
    }

    #[test]
    fn a_direct_download_without_commands_is_not_searched() {
        let downloads = crate::download::DownloadCache::new(tempfile::tempdir().unwrap().path());
        let found = downloaded_bins(
            &downloads,
            "google-chrome",
            "https://example.invalid/googlechrome.dmg",
            &"0".repeat(64),
            &crate::LockedInstall::Dmg,
            &crate::Bins::Names(vec![]),
        )
        .unwrap();
        assert!(found.is_empty());
    }

    use std::path::PathBuf;

    use super::*;
    use crate::store::Store;
    use crate::{
        GitReleaseProof, LockedInstall, LockedPackage, LockedSource, PackageRealizer, Provides,
        ResolverInput,
    };

    #[test]
    fn catalog_has_canonical_names_and_stable_roundtrip() {
        let catalog = crate::test_catalog::catalog();
        assert_eq!(catalog.find("rg").unwrap().name, "ripgrep");
        assert!(catalog.find("BurntSushi/ripgrep").is_none());
        let decoded: PackageCatalog = serde_json::from_str(&catalog.to_json().unwrap()).unwrap();
        decoded.validate().unwrap();
        assert_eq!(catalog.sha256(), decoded.sha256());
    }

    #[test]
    fn default_stack_does_not_fall_back_for_unqualified_names() {
        let error = crate::default_resolver_stack()
            .resolve(
                &PackageRequest::parse("BurntSushi/ripgrep"),
                &ResolveContext::new("aarch64-macos"),
            )
            .unwrap_err();
        let crate::ResolveError::UnknownExplicitResolver { resolver, .. } = error else {
            panic!("expected an unconfigured canonical resolver")
        };
        assert_eq!(resolver, "rootbeer");
    }

    #[test]
    fn rejects_alias_collisions_and_invalid_recipes() {
        let original = crate::test_catalog::catalog();
        let mut catalog = original.clone();
        catalog
            .packages
            .get_mut("fd")
            .unwrap()
            .aliases
            .push("rg".into());
        assert!(catalog
            .validate()
            .unwrap_err()
            .contains("duplicate package name"));

        let mut catalog = original.clone();
        catalog
            .packages
            .get_mut("fd")
            .unwrap()
            .versions
            .get_mut("10.4.2")
            .unwrap()
            .platforms
            .get_mut("x86_64-linux")
            .unwrap()
            .source = Some("github:sharkdp/fd@latest".into());
        assert!(catalog.validate().unwrap_err().contains("exact"));

        let mut catalog = original.clone();
        catalog
            .packages
            .get_mut("fd")
            .unwrap()
            .versions
            .get_mut("10.4.2")
            .unwrap()
            .platforms
            .get_mut("x86_64-linux")
            .unwrap()
            .checks = vec![vec!["sh".into(), "-c".into(), "true".into()]];
        assert!(catalog
            .validate()
            .unwrap_err()
            .contains("declared commands"));
    }

    #[test]
    fn a_prebuilt_platform_needs_exactly_one_named_asset() {
        let catalog = crate::test_catalog::catalog();
        let original = catalog.packages["ripgrep"].versions["15.2.0"]
            .for_system("x86_64-linux")
            .unwrap()
            .clone();
        assert!(original.validate("x86_64-linux").is_ok());

        let mut recipe = original.clone();
        recipe.asset = None;
        assert!(recipe
            .validate("x86_64-linux")
            .unwrap_err()
            .contains("one release asset"));

        let mut recipe = original.clone();
        recipe.asset = Some(" ".into());
        assert!(recipe.validate("x86_64-linux").is_err());

        let mut recipe = original;
        recipe.source = Some("gitlab:BurntSushi/ripgrep@15.2.0".into());
        assert!(recipe.validate("x86_64-linux").is_err());
    }

    #[test]
    fn platform_defaults_preserve_exact_requests_and_validate_support() {
        let mut catalog = crate::test_catalog::catalog().clone();
        let package = catalog.packages.get_mut("ripgrep").unwrap();
        let recipe = package.versions["15.2.0"].clone();
        package.versions.insert("15.1.0".into(), recipe);
        package
            .default_versions
            .insert("x86_64-linux".into(), "15.1.0".into());
        catalog.validate().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("rg");
        std::fs::write(&source, "test executable").unwrap();
        let resolver = local_resolver(&catalog, source);
        for (system, request, expected) in [
            ("x86_64-linux", "rg", "15.1.0"),
            ("aarch64-macos", "rg", "15.2.0"),
            ("x86_64-linux", "rg@15.2.0", "15.2.0"),
        ] {
            let result = resolver
                .resolve(
                    &PackageRequest::parse(request),
                    &ResolveContext::new(system),
                )
                .unwrap()
                .unwrap();
            assert_eq!(result.package.version, expected);
        }
        let package = catalog.packages.get_mut("ripgrep").unwrap();
        package
            .default_versions
            .insert("x86_64-linux".into(), "missing".into());
        assert!(catalog.validate().unwrap_err().contains("has no recipe"));
        let package = catalog.packages.get_mut("ripgrep").unwrap();
        package.default_versions.clear();
        package
            .default_versions
            .insert("unsupported-platform".into(), "15.2.0".into());
        assert!(catalog
            .validate()
            .unwrap_err()
            .contains("unique supported systems"));
    }

    #[test]
    fn recipe_evaluation_has_no_host_access_and_rejects_unknown_fields() {
        assert!(PackageCatalog::from_lua(&[("bad", "return os.getenv('HOME')")]).is_err());
        assert!(PackageCatalog::from_lua(&[("bad", "return require('rootbeer')")]).is_err());
        let source = r#"return {
            typo = true,
            name = "age", description = "Encrypt", homepage = "https://example.org",
            prebuilt = { github = "FiloSottile/age", asset = "age-{version}.tar.gz" },
            outputs = { bins = { "age" }, checks = { { "age", "--version" } } },
            platforms = { ["aarch64-macos"] = { default_version = "1.3.1" } },
            versions = { ["1.3.1"] = { license = "BSD-3-Clause", digests = { ["aarch64-macos"] = "aa" } } },
        }"#;
        assert!(PackageCatalog::from_lua(&[("age", source)])
            .unwrap_err()
            .contains("unknown field"));
    }

    struct LocalBackend {
        source: PathBuf,
    }

    impl PackageResolver for LocalBackend {
        fn name(&self) -> &str {
            "github"
        }

        fn resolve(
            &self,
            request: &PackageRequest,
            _: &ResolveContext,
        ) -> Result<Option<PackageResolution>, String> {
            assert_eq!(request.name, "BurntSushi/ripgrep");
            assert_eq!(request.version.as_deref(), Some("15.2.0"));
            Ok(Some(PackageResolution::new(
                LockedPackage {
                    name: request.name.clone(),
                    version: "15.2.0".into(),
                    source: LockedSource::File {
                        path: self.source.clone(),
                        sha256: crate::store::hash_file(&self.source).unwrap(),
                    },
                    install: LockedInstall::Binary {
                        path: PathBuf::from("rg"),
                    },
                    provides: Provides {
                        apps: Default::default(),
                        bins: BTreeMap::from([("rg".into(), "rg".into())]),
                    },
                    runtime_dependencies: Default::default(),
                    output_sha256: None,
                },
                ResolutionProof::GitRelease(GitReleaseProof {
                    host: "github.com".into(),
                    owner: "BurntSushi".into(),
                    repo: "ripgrep".into(),
                    tag: Some("15.2.0".into()),
                    target_commit: None,
                    release_id: None,
                    documents: vec![],
                }),
            )))
        }
    }

    fn local_resolver(catalog: &PackageCatalog, source: PathBuf) -> CatalogResolver {
        let mut backends = ResolverStack::new();
        backends.push(LocalBackend { source });
        CatalogResolver::new(catalog, &PackageResolverInputs::default(), backends)
    }

    #[test]
    fn canonical_and_alias_requests_share_identity_and_preserve_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("rg");
        std::fs::write(&source, "#!/bin/sh\nexit 0\n").unwrap();
        let resolver = local_resolver(crate::test_catalog::catalog(), source);
        let context = ResolveContext::new("aarch64-macos");
        let canonical = resolver
            .resolve(&PackageRequest::parse("ripgrep"), &context)
            .unwrap()
            .unwrap();
        let alias = resolver
            .resolve(&PackageRequest::parse("rg@15.2.0"), &context)
            .unwrap()
            .unwrap();
        assert_eq!(canonical, alias);
        assert_eq!(canonical.package.name, "ripgrep");
        let ResolutionProof::Catalog(proof) = &canonical.proof else {
            panic!("missing catalog provenance")
        };
        assert_eq!(proof.revision, 3);
        assert_eq!(
            proof.catalog_sha256,
            crate::test_catalog::catalog().sha256()
        );
        assert_eq!(proof.source.name, "BurntSushi/ripgrep");
        assert_eq!(
            proof.source.asset.as_deref(),
            Some("ripgrep-15.2.0-aarch64-apple-darwin.tar.gz")
        );

        let realizer = PackageRealizer::with_dirs(
            Store::new(dir.path().join("store")),
            dir.path().join("downloads"),
            dir.path().join("tmp"),
        );
        let realized = realizer.realize(&canonical.package).unwrap();
        assert!(realized.bins["rg"].is_file());
    }

    #[test]
    fn resolves_new_catalog_names_without_rebuilding_the_binary() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("rg");
        std::fs::write(&source, "test executable").unwrap();
        let mut catalog = crate::test_catalog::catalog().clone();
        let mut package = catalog.packages.remove("ripgrep").unwrap();
        package.name = "new-search".into();
        package.aliases.clear();
        catalog.packages.insert(package.name.clone(), package);
        catalog.validate().unwrap();
        let resolver = local_resolver(&catalog, source);
        let resolution = resolver
            .resolve(
                &PackageRequest::parse("new-search"),
                &ResolveContext::new("aarch64-linux"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(resolution.package.name, "new-search");
        let ResolutionProof::Catalog(proof) = resolution.proof else {
            panic!("expected catalog proof")
        };
        assert_eq!(proof.catalog_sha256, catalog.sha256());
    }

    #[test]
    fn loads_directory_recipes_and_rejects_a_hostile_file() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("age.lua"),
            r#"return {
                name = "age", description = "Encrypt", homepage = "https://example.org",
                prebuilt = { github = "FiloSottile/age", asset = "age-{version}-{target}.tar.gz" },
                outputs = { bins = { "age" }, checks = { { "age", "--version" } } },
                platforms = {
                    ["aarch64-macos"] = { target = "darwin-arm64", default_version = "1.3.1" },
                    ["x86_64-linux"] = { target = "linux-amd64", default_version = "1.3.1" },
                },
                versions = { ["1.3.1"] = { license = "BSD-3-Clause", digests = {
                    ["aarch64-macos"] = "aa", ["x86_64-linux"] = "bb",
                } } },
            }"#,
        )
        .unwrap();
        std::fs::write(directory.path().join("README.md"), "ignored").unwrap();

        let catalog = PackageCatalog::from_directory(directory.path()).unwrap();
        let age = &catalog.packages["age"];
        assert_eq!(age.default_version_for("aarch64-macos"), Some("1.3.1"));
        assert_eq!(
            age.versions["1.3.1"]
                .for_system("x86_64-linux")
                .unwrap()
                .asset
                .as_deref(),
            Some("age-1.3.1-linux-amd64.tar.gz")
        );

        std::fs::write(
            directory.path().join("invalid.lua"),
            "return os.execute('false')",
        )
        .unwrap();
        assert!(PackageCatalog::from_directory(directory.path()).is_err());
    }

    #[test]
    fn rejects_unknown_names_versions_platforms_overrides_and_catalog_changes_before_download() {
        let mut resolver = local_resolver(
            crate::test_catalog::catalog(),
            PathBuf::from("must-not-read"),
        );
        let context = ResolveContext::new("aarch64-macos");
        for request in ["git", "ripgrep@0.0.0", "ripgrep@latest"] {
            assert!(resolver
                .resolve(&PackageRequest::parse(request), &context)
                .is_err());
        }
        assert!(resolver
            .resolve(
                &PackageRequest::parse("ripgrep"),
                &ResolveContext::new("x86_64-windows")
            )
            .unwrap_err()
            .contains("does not support"));
        let mut request = PackageRequest::parse("ripgrep");
        request.asset = Some("different.tar.gz".into());
        assert!(resolver
            .resolve(&request, &context)
            .unwrap_err()
            .contains("overrides"));
        resolver.inputs.resolvers.insert(
            "rootbeer".into(),
            ResolverInput::Catalog {
                sha256: "different".into(),
            },
        );
        assert!(resolver
            .resolve(&PackageRequest::parse("ripgrep"), &context)
            .unwrap_err()
            .contains("--update"));
    }
}
