use std::collections::BTreeMap;

use rootbeer_package::distribution::{
    verify_record, DependencyInputs, PackageProvenance, PackageRecord,
};
use rootbeer_package::graph::{find_recipe_definition, find_recipe_for_system, DependencyGraph};
use rootbeer_package::repository::RepositoryResolver;
use rootbeer_package::{BuildArtifact, LockedPackage, PackageCatalog, ResolveContext};

use crate::BuildPlan;

/// Source dependencies a build takes from their signed PDR record instead of compiling.
///
/// A dependency qualifies only when its record was built from the exact recipe and revision
/// the catalog approves now; anything else, including a dependency changed alongside its
/// dependent, is compiled as before.
#[derive(Debug, Default)]
pub struct PublishedDependencies {
    records: BTreeMap<String, Published>,
}

#[derive(Debug)]
struct Published {
    record: PackageRecord,
    bytes: String,
}

impl PublishedDependencies {
    /// Looks up every source dependency in `request`'s closure among the PDR's published builds.
    pub fn find(
        catalog: &PackageCatalog,
        request: &str,
        system: &str,
        pdr: &RepositoryResolver,
    ) -> Result<Self, String> {
        let graph = DependencyGraph::new(catalog, &[request.to_string()], system)?;
        let root = graph
            .order
            .last()
            .ok_or_else(|| format!("{request} has no recipe for {system}"))?;
        let context = ResolveContext::new(system);
        let mut records = BTreeMap::new();
        for key in &graph.nodes[root].closure {
            let (package, version, recipe) = find_recipe_for_system(catalog, key, system)?;
            if recipe.build.is_none() {
                continue;
            }
            let Ok((name, entry)) = pdr.package(&package.name) else {
                continue;
            };
            let is_published = pdr
                .document(name, entry)?
                .versions
                .get(version)
                .is_some_and(|published| published.platforms.contains_key(system));
            if !is_published {
                continue;
            }
            let (record, _, bytes) = pdr.signed_record_of(name, entry, Some(version), &context)?;
            if !builds(catalog, key, system, &record)? {
                continue;
            }
            let bytes = String::from_utf8(bytes).map_err(|error| error.to_string())?;
            records.insert(key.clone(), Published { record, bytes });
        }
        let packages: BTreeMap<_, _> = records
            .iter()
            .map(|(key, published)| (key.clone(), &published.record.artifact.package))
            .collect();
        let is_source = |key: &str| {
            find_recipe_for_system(catalog, key, system)
                .is_ok_and(|(_, _, recipe)| recipe.build.is_some())
        };
        let consistent = consistent_closures(&packages, is_source);
        records.retain(|key, _| consistent.contains(key));
        Ok(Self { records })
    }

    /// Re-verifies the published builds a receipt claims, so a release trusts only the PDR's key.
    pub(crate) fn from_receipt(
        catalog: &PackageCatalog,
        receipt: &BuildArtifact,
        public_key: &str,
    ) -> Result<Self, String> {
        let mut records = BTreeMap::new();
        for (key, bytes) in &receipt.published_dependencies {
            let record = verify_record(bytes.as_bytes(), public_key, key, &receipt.system)?;
            if !builds(catalog, key, &receipt.system, &record)? {
                return Err(format!(
                    "{key}: published build differs from the approved recipe"
                ));
            }
            let used = receipt
                .dependencies
                .get(key)
                .ok_or_else(|| format!("{key}: published build is not a dependency"))?;
            let artifact = &record.artifact.package;
            let is_same_output = artifact
                .output_sha256
                .as_ref()
                .is_none_or(|output| used.output_sha256.as_ref() == Some(output));
            if used.source != artifact.source || !is_same_output {
                return Err(format!("{key}: receipt used another build than its record"));
            }
            let bytes = bytes.clone();
            records.insert(key.clone(), Published { record, bytes });
        }
        Ok(Self { records })
    }

    /// Each published dependency's package, as its record publishes it.
    pub(crate) fn packages(&self) -> impl Iterator<Item = (&String, &LockedPackage)> {
        self.records
            .iter()
            .map(|(key, published)| (key, &published.record.artifact.package))
    }

    /// What a published dependency contributes to its dependents' inputs: the inputs it was
    /// built from, including the engine that built it.
    pub(crate) fn inputs(&self, key: &str) -> Option<DependencyInputs> {
        let record = &self.records.get(key)?.record;
        let PackageProvenance::Source(provenance) = &record.provenance else {
            return None;
        };
        Some(DependencyInputs {
            revision: record.revision,
            recipe_sha256: record.recipe.sha256(),
            engine_sha256: provenance.engine_sha256.clone(),
        })
    }

    /// Installs each published dependency from its record instead of compiling it.
    pub(crate) fn use_in(&self, plan: &mut BuildPlan) -> Result<(), String> {
        for (key, published) in &self.records {
            plan.use_published(
                key,
                published.record.artifact.package.clone(),
                published.bytes.clone(),
            )?;
        }
        Ok(())
    }
}

/// The published builds whose runtime closures name exactly the builds published now.
///
/// A record links the dependency builds that existed when it was published. Once one of those
/// is republished or compiled anew, installing the record beside the current build would give
/// it a runtime closure it was never linked against, so it is compiled instead, and so are its
/// dependents in turn. Prebuilt runtime dependencies are never rebuilt, so they always match.
fn consistent_closures(
    packages: &BTreeMap<String, &LockedPackage>,
    is_source: impl Fn(&str) -> bool,
) -> std::collections::BTreeSet<String> {
    let mut kept: std::collections::BTreeSet<String> = packages.keys().cloned().collect();
    loop {
        let stale: Vec<String> = kept
            .iter()
            .filter(|key| {
                rootbeer_package::runtime::closure(packages[*key]).map_or(true, |closure| {
                    closure.iter().any(|dependency| {
                        let id = dependency.id();
                        is_source(&id)
                            && (!kept.contains(&id)
                                || packages[&id].output_sha256 != dependency.output_sha256)
                    })
                })
            })
            .cloned()
            .collect();
        if stale.is_empty() {
            return kept;
        }
        for key in stale {
            kept.remove(&key);
        }
    }
}

/// Whether `record` qualifies the recipe and revision the catalog approves for `key` now.
fn builds(
    catalog: &PackageCatalog,
    key: &str,
    system: &str,
    record: &PackageRecord,
) -> Result<bool, String> {
    let (_, _, recipe) = find_recipe_for_system(catalog, key, system)?;
    let (_, _, entry) = find_recipe_definition(catalog, key)?;
    Ok(matches!(record.provenance, PackageProvenance::Source(_))
        && record.revision == entry.revision
        && record.recipe == recipe)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ring::signature::{Ed25519KeyPair, KeyPair};
    use rootbeer_package::distribution::{
        signing_message, BuildProvenance, PackageRecord, SignedPackageRecord,
    };
    use rootbeer_package::{
        ArchiveFormat, BuildEnvironmentLock, LockedInstall, LockedPackage, LockedSource,
        PackageDefinition, Provides, PublishedArtifact,
    };

    use super::*;

    const LINUX: &str = "x86_64-linux";

    fn catalog(lib_configure: &str) -> PackageCatalog {
        let digest = "a".repeat(64);
        let recipe = |name: &str, build: &str| {
            PackageDefinition::from_lua(&format!(
                r#"return {{
                    name = "{name}", description = "A {name}", homepage = "https://example.com",
                    default_license = "MIT",
                    source = {{ url = "https://example.com/{name}-{{version}}.tar.gz", archive = "tar.gz",
                                strip_prefix = "{name}-{{version}}" }},
                    build = {build},
                    outputs = {{ bins = {{ "{name}" }}, checks = {{ {{ "{name}", "--version" }} }} }},
                    platforms = {{ ["{LINUX}"] = {{ default_version = "1" }} }},
                    versions = {{ ["1"] = {{ digests = {{ ["{LINUX}"] = "{digest}" }} }} }},
                }}"#
            ))
            .unwrap()
        };
        let lib = recipe(
            "lib",
            &format!(r#"{{ backend = "autotools", configure = {{ "{lib_configure}" }} }}"#),
        );
        let app = recipe(
            "app",
            r#"{ backend = "autotools", dependencies = { { package = "lib", version = "1", kind = "all" } } }"#,
        );
        PackageCatalog::from_definitions(&BTreeMap::from([
            ("lib".into(), lib),
            ("app".into(), app),
        ]))
        .unwrap()
    }

    fn lib_package() -> LockedPackage {
        LockedPackage {
            name: "lib".into(),
            version: "1".into(),
            source: LockedSource::Url {
                url: format!("ghcr://example/packages/lib@sha256:{}", "c".repeat(64)),
                sha256: "c".repeat(64),
            },
            install: LockedInstall::Archive {
                format: ArchiveFormat::TarGz,
                strip_prefix: None,
            },
            provides: Provides {
                bins: BTreeMap::from([("lib".into(), "bin/lib".into())]),
                apps: BTreeMap::new(),
            },
            output_sha256: Some("d".repeat(64)),
            runtime_dependencies: BTreeMap::new(),
        }
    }

    /// `lib@1` as the PDR publishes it, signed by `seed`, built by an older engine.
    fn signed_lib(catalog: &PackageCatalog, seed: u8) -> (String, String) {
        let record = PackageRecord {
            extra: Default::default(),
            schema: 2,
            published: 1,
            system: LINUX.into(),
            revision: 1,
            recipe: catalog.packages["lib"].versions["1"].platforms[LINUX].clone(),
            artifact: PublishedArtifact {
                revision: 1,
                receipt_sha256: "e".repeat(64),
                package: lib_package(),
            },
            provenance: PackageProvenance::Source(Box::new(BuildProvenance {
                engine_sha256: "0".repeat(64),
                environment_sha256: "b".repeat(64),
                environment: BuildEnvironmentLock {
                    schema: 1,
                    system: LINUX.into(),
                    tools: ["sh", "cc", "make", "patch"]
                        .into_iter()
                        .map(|name| {
                            let input = rootbeer_package::BuildEnvironmentInput {
                                path: format!("/usr/bin/{name}").into(),
                                sha256: "d".repeat(64),
                            };
                            (name.into(), input)
                        })
                        .collect(),
                    inputs: BTreeMap::new(),
                    variables: BTreeMap::new(),
                },
                isolation: "host".into(),
                toolchain: BTreeMap::from([("cc".into(), "test compiler".into())]),
                runtime_audit_sha256: "f".repeat(64),
                dependencies: BTreeMap::new(),
            })),
        };
        let key = Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).unwrap();
        let record = serde_json::value::to_raw_value(&record).unwrap();
        let hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let signature = hex(key.sign(&signing_message(&record)).as_ref());
        let bytes = serde_json::to_string(&SignedPackageRecord { record, signature }).unwrap();
        (bytes, hex(key.public_key().as_ref()))
    }

    fn receipt(catalog: &PackageCatalog, record: &str, used: LockedPackage) -> BuildArtifact {
        let app = &catalog.packages["app"].versions["1"].platforms[LINUX];
        BuildArtifact {
            schema: 1,
            recipe_sha256: app.sha256(),
            qualification_environment: None,
            build_key: None,
            build_environment: None,
            environment: None,
            isolation: None,
            runtime_audit_sha256: None,
            catalog_sha256: catalog.sha256(),
            revision: 1,
            system: LINUX.into(),
            build: app.build.clone().unwrap(),
            dependencies: BTreeMap::from([("lib@1".into(), used)]),
            published_dependencies: BTreeMap::from([("lib@1".into(), record.to_string())]),
            resolver_inputs: Default::default(),
            toolchain: BTreeMap::new(),
            package: LockedPackage {
                name: "app".into(),
                ..lib_package()
            },
        }
    }

    #[test]
    fn a_published_dependency_is_identified_by_the_build_it_was_published_from() {
        let catalog = catalog("--static");
        let (record, key) = signed_lib(&catalog, 7);
        let published = PublishedDependencies::from_receipt(
            &catalog,
            &receipt(&catalog, &record, lib_package()),
            &key,
        )
        .unwrap();

        let inputs =
            crate::package_plan::dependency_inputs(&catalog, "app@1", LINUX, &published).unwrap();
        assert_eq!(inputs["lib@1"].engine_sha256, "0".repeat(64));
        let compiled = crate::package_plan::dependency_inputs(
            &catalog,
            "app@1",
            LINUX,
            &PublishedDependencies::default(),
        )
        .unwrap();
        assert_ne!(compiled["lib@1"].engine_sha256, "0".repeat(64));
        assert_eq!(
            compiled["lib@1"].recipe_sha256,
            inputs["lib@1"].recipe_sha256
        );
    }

    #[test]
    fn a_release_trusts_only_published_builds_the_pdr_signed_for_the_approved_recipe() {
        let approved = catalog("--static");
        let (record, key) = signed_lib(&approved, 7);
        let (_, other_key) = signed_lib(&approved, 8);
        let verify = |catalog: &PackageCatalog, used: LockedPackage, key: &str| {
            PublishedDependencies::from_receipt(catalog, &receipt(catalog, &record, used), key)
                .map(|_| ())
        };
        verify(&approved, lib_package(), &key).unwrap();

        assert!(verify(&approved, lib_package(), &other_key).is_err());

        let error = verify(&catalog("--shared"), lib_package(), &key).unwrap_err();
        assert!(
            error.contains("differs from the approved recipe"),
            "{error}"
        );

        let rebuilt = LockedPackage {
            output_sha256: Some("9".repeat(64)),
            ..lib_package()
        };
        let error = verify(&approved, rebuilt, &key).unwrap_err();
        assert!(error.contains("another build"), "{error}");
    }

    #[test]
    fn stale_runtime_closures_are_compiled_along_with_their_dependents() {
        let package = |name: &str, output: &str, runtime: &[&LockedPackage]| {
            let mut package = lib_package();
            package.name = name.into();
            package.output_sha256 = Some(output.repeat(64));
            package.runtime_dependencies = runtime
                .iter()
                .map(|dependency| (dependency.id(), (*dependency).clone()))
                .collect();
            package
        };
        let iconv_old = package("iconv", "0", &[]);
        let iconv = package("iconv", "1", &[]);
        let idn2 = package("idn2", "2", &[&iconv_old]);
        let psl = package("psl", "3", &[&idn2]);
        let lz4 = package("lz4", "4", &[]);
        let zstd = package("zstd", "5", &[&lz4]);
        let prebuilt = package("prebuilt", "6", &[]);
        let tool = package("tool", "7", &[&prebuilt]);
        let packages: BTreeMap<String, &LockedPackage> = [&iconv, &idn2, &psl, &lz4, &zstd, &tool]
            .into_iter()
            .map(|package| (package.id(), package))
            .collect();

        let kept = consistent_closures(&packages, |key| !key.starts_with("prebuilt@"));
        let kept: Vec<_> = kept.iter().map(String::as_str).collect();
        assert_eq!(kept, ["iconv@1", "lz4@1", "tool@1", "zstd@1"]);
    }
}
