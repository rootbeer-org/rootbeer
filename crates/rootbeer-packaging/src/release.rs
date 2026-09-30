use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rootbeer_package::distribution::{
    verify_record, BuildProvenance, PackageProvenance, PackageRecord,
};

use crate::receipt::ReleasedDependency;
use rootbeer_package::{
    BuildArtifact, CatalogRecipe, LockedSource, PackageCatalog, PackageRealizer, PublishedArtifact,
};
use rootbeer_store::{hash_bytes, Store};

/// Who approves a release, and when it counts as published.
pub struct Signer<'a> {
    /// Publisher's Ed25519 PKCS#8 DER key.
    pub key_der: &'a [u8],
    pub public_key: &'a str,
    /// Unix seconds recorded in the signed record.
    pub published: u64,
}

/// Verifies and signs one qualified package. The caller must trust the receipt's producer.
/// The destination contains only this package's archive, receipt, and signed record.
///
/// `released` names release outputs of this run, by package, that runtime dependencies not yet
/// in the PDR are referenced from.
pub fn release_package(
    catalog: &PackageCatalog,
    receipt: &Path,
    registry: &str,
    output: &Path,
    signer: &Signer,
    expected_inputs: Option<&str>,
    released: &BTreeMap<String, PathBuf>,
) -> Result<String, String> {
    rootbeer_package::ghcr::validate_repository(registry)?;
    let receipt_bytes = fs::read(receipt).map_err(|error| error.to_string())?;
    #[derive(serde::Deserialize)]
    struct Identity {
        package: rootbeer_package::LockedPackage,
        system: String,
    }
    let identity: Identity =
        serde_json::from_slice(&receipt_bytes).map_err(|error| error.to_string())?;
    let definition = catalog
        .packages
        .get(&identity.package.name)
        .ok_or("receipt names a package outside the catalog")?;
    let entry = definition
        .versions
        .get(&identity.package.version)
        .ok_or("no matching package recipe")?;
    let revision = entry.revision;
    let recipe = entry
        .for_system(&identity.system)
        .ok_or("no matching package recipe for this platform")?
        .clone();
    let staging = rootbeer_package::staging::staging(output)?;
    let destination = staging.path().join("release");
    fs::create_dir(&destination).map_err(|error| error.to_string())?;
    let realizer = PackageRealizer::with_dirs(
        Store::new(staging.path().join("store")),
        staging.path().join("downloads"),
        staging.path().join("install"),
    );
    let qualified = if recipe.build.is_some() {
        prepare_source(
            catalog,
            (receipt, &receipt_bytes),
            registry,
            &destination,
            &realizer,
            signer.public_key,
            released,
        )?
    } else {
        prepare_binary(
            &recipe,
            revision,
            receipt,
            &receipt_bytes,
            registry,
            &destination,
            &realizer,
        )?
    };
    let record = PackageRecord {
        extra: Default::default(),
        schema: 2,
        system: qualified.system,
        revision,
        recipe,
        artifact: qualified.artifact,
        provenance: qualified.provenance,
        published: signer.published,
    };
    if expected_inputs.is_some_and(|expected| record.input_key() != expected) {
        return Err("receipt differs from the planned package inputs".into());
    }
    let signed = crate::sign_package_record(&record, signer.key_der, signer.public_key)?;
    let digest = hash_bytes(&signed);
    fs::write(destination.join("package.json"), signed).map_err(|error| error.to_string())?;
    fs::write(destination.join("receipt.json"), receipt_bytes)
        .map_err(|error| error.to_string())?;
    fs::rename(destination, output).map_err(|error| error.to_string())?;
    Ok(format!("ghcr://{registry}@sha256:{digest}"))
}

/// A package checked against its receipt, before anyone has approved it.
struct Qualified {
    system: String,
    artifact: PublishedArtifact,
    provenance: PackageProvenance,
}

fn prepare_source(
    catalog: &PackageCatalog,
    (receipt, receipt_bytes): (&Path, &[u8]),
    registry: &str,
    destination: &Path,
    realizer: &PackageRealizer,
    public_key: &str,
    released: &BTreeMap<String, PathBuf>,
) -> Result<Qualified, String> {
    fs::create_dir(destination.join("artifacts")).map_err(|error| error.to_string())?;
    let build: BuildArtifact =
        serde_json::from_slice(receipt_bytes).map_err(|error| error.to_string())?;
    let id = build.package.id();
    let published = crate::PublishedDependencies::from_receipt(catalog, &build, public_key)?;
    let mut runtime = BTreeMap::new();
    for (key, package) in published.packages() {
        let package = package.clone();
        runtime.insert(
            key.clone(),
            ReleasedDependency {
                package,
                archive: None,
            },
        );
    }
    for (key, directory) in released {
        let bytes =
            fs::read(directory.join("package.json")).map_err(|error| format!("{key}: {error}"))?;
        let record = verify_record(&bytes, public_key, key, &build.system)?;
        // A build or upstream binary released alongside, installed from its local copy.
        let archive = ["package.tar.gz", crate::prepare::UPSTREAM_FILE]
            .into_iter()
            .map(|file| directory.join(file))
            .find(|path| path.is_file());
        let package = record.artifact.package;
        runtime.insert(key.clone(), ReleasedDependency { package, archive });
    }
    let mut inputs =
        crate::package_plan::dependency_inputs(catalog, &id, &build.system, &published)?;
    let mut dependencies = std::collections::BTreeMap::new();
    for (dependency, package) in &build.dependencies {
        let inputs = inputs
            .remove(dependency)
            .ok_or_else(|| format!("{id}: built with {dependency}, outside its closure"))?;
        let output_sha256 = package
            .output_sha256
            .clone()
            .ok_or_else(|| format!("{id}: {dependency} has no output hash"))?;
        let built = rootbeer_package::distribution::BuiltDependency {
            inputs,
            output_sha256,
        };
        dependencies.insert(dependency.clone(), built);
    }
    if let Some(missing) = inputs.keys().next() {
        return Err(format!("{id}: receipt omits {missing} from its closure"));
    }
    let provenance = BuildProvenance {
        engine_sha256: rootbeer_build::engine_identity(Some(&build.build.backend)),
        environment_sha256: build
            .qualification_environment
            .ok_or("build receipt has no qualification environment")?,
        environment: build
            .environment
            .ok_or("build receipt has no pinned environment")?,
        isolation: build
            .isolation
            .ok_or("build receipt has no isolation evidence")?,
        toolchain: build.toolchain,
        runtime_audit_sha256: build
            .runtime_audit_sha256
            .ok_or("build receipt has no runtime audit")?,
        dependencies,
    };
    let (system, artifact, checked_receipt) = crate::receipt::prepare_artifact(
        catalog,
        receipt,
        registry,
        destination,
        realizer,
        &runtime,
    )?;
    if checked_receipt != receipt_bytes {
        return Err("build receipt changed during release".into());
    }
    let LockedSource::Url { sha256, .. } = &artifact.package.source else {
        unreachable!()
    };
    fs::rename(
        destination
            .join("artifacts")
            .join(format!("{sha256}.tar.gz")),
        destination.join("package.tar.gz"),
    )
    .map_err(|error| error.to_string())?;
    fs::remove_dir(destination.join("artifacts")).map_err(|error| error.to_string())?;
    Ok(Qualified {
        system,
        artifact,
        provenance: PackageProvenance::Source(Box::new(provenance)),
    })
}

fn prepare_binary(
    recipe: &CatalogRecipe,
    revision: u32,
    receipt_path: &Path,
    receipt_bytes: &[u8],
    registry: &str,
    destination: &Path,
    realizer: &PackageRealizer,
) -> Result<Qualified, String> {
    let receipt: crate::prepare::BinaryReceipt =
        serde_json::from_slice(receipt_bytes).map_err(|error| error.to_string())?;
    if receipt.schema != 2
        || receipt.recipe_sha256 != recipe.sha256()
        || receipt.provenance.engine_sha256 != rootbeer_build::engine_identity(None)
    {
        return Err("binary receipt does not match the approved recipe or engine".into());
    }
    if receipt.package != receipt.provenance.upstream {
        return Err("binary receipt publishes something other than the upstream download".into());
    }
    let LockedSource::Url { sha256, .. } = &receipt.package.source else {
        return Err("binary receipt requires an upstream download".into());
    };
    let sha256 = sha256.clone();
    let copy = destination.join(crate::prepare::UPSTREAM_FILE);
    fs::copy(
        receipt_path
            .parent()
            .unwrap_or(Path::new("."))
            .join(crate::prepare::UPSTREAM_FILE),
        &copy,
    )
    .map_err(|error| error.to_string())?;
    if rootbeer_store::hash_file(&copy).map_err(|error| error.to_string())? != sha256 {
        return Err("upstream file hash mismatch".into());
    }
    let mut package = receipt.package;
    let mut local = package.clone();
    local.source = LockedSource::File {
        path: copy,
        sha256: sha256.clone(),
    };
    if recipe.mirror {
        package.source = LockedSource::Url {
            url: format!("ghcr://{registry}@sha256:{sha256}"),
            sha256,
        };
    }
    realizer
        .realize(&local)
        .map_err(|error| error.to_string())?;
    Ok(Qualified {
        system: receipt.system,
        artifact: PublishedArtifact {
            revision,
            receipt_sha256: hash_bytes(receipt_bytes),
            package,
        },
        provenance: PackageProvenance::Upstream(Box::new(receipt.provenance)),
    })
}

/// Uploads a verified package release as an OCI artifact, retaining all three blobs together.
pub fn push_package(release: &Path, registry: &str, public_key: &str) -> Result<String, String> {
    rootbeer_package::ghcr::validate_repository(registry)?;
    let release = release.canonicalize().map_err(|error| error.to_string())?;
    let bytes = fs::read(release.join("package.json")).map_err(|error| error.to_string())?;
    let signed: rootbeer_package::distribution::SignedPackageRecord =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    let record: PackageRecord =
        serde_json::from_str(signed.record.get()).map_err(|error| error.to_string())?;
    let record = rootbeer_package::distribution::verify_record(
        &bytes,
        public_key,
        &record.artifact.package.id(),
        &record.system,
    )?;
    let LockedSource::Url { url, sha256 } = &record.artifact.package.source else {
        unreachable!()
    };
    // Only builds and mirrors live in the registry; other upstream binaries stay the vendor's.
    let hosted = match url.starts_with("ghcr://") {
        true => {
            if rootbeer_package::ghcr::GhcrBlob::parse(url)?.repository != registry {
                return Err("package artifact belongs to another registry".into());
            }
            Some(match &record.provenance {
                PackageProvenance::Source(_) => ("package.tar.gz", "application/gzip"),
                PackageProvenance::Upstream(_) => {
                    (crate::prepare::UPSTREAM_FILE, "application/octet-stream")
                }
            })
        }
        false => None,
    };
    if let Some((file, _)) = hosted {
        if rootbeer_store::hash_file(release.join(file)).map_err(|error| error.to_string())?
            != *sha256
        {
            return Err("package release contents changed".into());
        }
    }
    if rootbeer_store::hash_file(release.join("receipt.json")).map_err(|error| error.to_string())?
        != record.artifact.receipt_sha256
    {
        return Err("package release contents changed".into());
    }
    let digest = hash_bytes(&bytes);
    let mut files = vec![
        "package.json:application/vnd.rootbeer.package.record.v1+json".to_string(),
        "receipt.json:application/json".to_string(),
    ];
    files.extend(hosted.map(|(file, media)| format!("{file}:{media}")));
    let status = Command::new("oras")
        .current_dir(&release)
        .args([
            "push",
            &format!("ghcr.io/{registry}:package-{digest}"),
            "--artifact-type",
            "application/vnd.rootbeer.package.v1",
        ])
        .args(&files)
        .status()
        .map_err(|error| error.to_string())?;
    if !status.success() {
        return Err(format!("package upload failed: {status}"));
    }
    let downloads = tempfile::tempdir().map_err(|error| error.to_string())?;
    let cache = rootbeer_package::download::DownloadCache::new(downloads.path());
    let hashes = [
        Some(&digest),
        Some(&record.artifact.receipt_sha256),
        hosted.map(|_| sha256),
    ];
    for hash in hashes.into_iter().flatten() {
        cache
            .materialize_verified(&format!("ghcr://{registry}@sha256:{hash}"), hash)
            .map_err(|error| format!("cannot verify public package download: {error}"))?;
    }
    let status = Command::new("oras")
        .args([
            "tag",
            &format!("ghcr.io/{registry}:package-{digest}"),
            &format!("inputs-{}", record.input_key()),
        ])
        .status()
        .map_err(|error| error.to_string())?;
    if !status.success() {
        return Err(format!("package input locator upload failed: {status}"));
    }
    Ok(format!("ghcr://{registry}@sha256:{digest}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use rootbeer_package::{BuildEnvironmentInput, BuildEnvironmentLock};
    use std::collections::BTreeMap;

    #[test]
    fn releases_one_build_across_unrelated_catalog_changes_and_rejects_tampering() {
        let root = tempfile::tempdir().unwrap();
        let (mut catalog, receipt) = crate::receipt::tests::fixture(root.path());
        let mut build: BuildArtifact =
            serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
        build.environment = Some(BuildEnvironmentLock {
            schema: 1,
            system: build.system.clone(),
            tools: ["sh", "cc", "make", "patch"]
                .into_iter()
                .map(|name| {
                    (
                        name.into(),
                        BuildEnvironmentInput {
                            path: format!("/usr/bin/{name}").into(),
                            sha256: "d".repeat(64),
                        },
                    )
                })
                .collect(),
            inputs: BTreeMap::new(),
            variables: BTreeMap::new(),
        });
        build.isolation = Some("host".into());
        build.qualification_environment = Some("a".repeat(64));
        build.toolchain.insert("cc".into(), "test compiler".into());
        let report = rootbeer_build::audit::audit(&root.path().join("tree")).unwrap();
        build.runtime_audit_sha256 = Some(hash_bytes(&serde_json::to_vec_pretty(&report).unwrap()));
        fs::write(&receipt, serde_json::to_vec(&build).unwrap()).unwrap();
        catalog
            .packages
            .get_mut(&build.package.name)
            .unwrap()
            .description
            .push_str(" changed");
        assert_ne!(build.catalog_sha256, catalog.sha256());
        let key = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        let public_key: String = Ed25519KeyPair::from_pkcs8(key.as_ref())
            .unwrap()
            .public_key()
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let release = root.path().join("release");
        let reference = release_package(
            &catalog,
            &receipt,
            "example/packages/tool",
            &release,
            &crate::release::Signer {
                key_der: key.as_ref(),
                public_key: &public_key,
                published: 1,
            },
            Some(&rootbeer_package::distribution::input_key(
                &build.package.id(),
                &build.system,
                build.revision,
                &catalog.packages[&build.package.name].versions[&build.package.version].platforms
                    [&build.system],
                &rootbeer_build::engine_identity(Some(&build.build.backend)),
                build.qualification_environment.as_deref().unwrap(),
                &BTreeMap::new(),
            )),
            &Default::default(),
        )
        .unwrap();
        let bytes = fs::read(release.join("package.json")).unwrap();
        assert!(reference.ends_with(&hash_bytes(&bytes)));
        let record = rootbeer_package::distribution::verify_record(
            &bytes,
            &public_key,
            &build.package.id(),
            &build.system,
        )
        .unwrap();
        assert_eq!(record.published, 1);
        assert_eq!(fs::read_dir(&release).unwrap().count(), 3);
        assert!(!String::from_utf8(bytes).unwrap().contains("catalog_sha256"));

        let wrong_inputs = root.path().join("wrong-inputs");
        assert!(release_package(
            &catalog,
            &receipt,
            "example/packages/tool",
            &wrong_inputs,
            &crate::release::Signer {
                key_der: key.as_ref(),
                public_key: &public_key,
                published: 1
            },
            Some(&"0".repeat(64)),
            &Default::default(),
        )
        .unwrap_err()
        .contains("planned package inputs"));
        assert!(!wrong_inputs.exists());

        let mut changed = catalog.clone();
        let platform = changed
            .packages
            .get_mut(&build.package.name)
            .unwrap()
            .versions
            .get_mut(&build.package.version)
            .unwrap()
            .platforms
            .get_mut(&build.system)
            .unwrap();
        let command = platform.bins.names().into_iter().next().unwrap().clone();
        platform.checks.push(vec![command, "--help".into()]);
        assert!(release_package(
            &changed,
            &receipt,
            "example/packages/tool",
            &root.path().join("changed"),
            &crate::release::Signer {
                key_der: key.as_ref(),
                public_key: &public_key,
                published: 1
            },
            None,
            &Default::default()
        )
        .unwrap_err()
        .contains("receipt does not match"));

        fs::write(
            receipt.parent().unwrap().join("package.tar.gz"),
            b"tampered",
        )
        .unwrap();
        let failed = root.path().join("failed");
        assert!(release_package(
            &catalog,
            &receipt,
            "example/packages/tool",
            &failed,
            &crate::release::Signer {
                key_der: key.as_ref(),
                public_key: &public_key,
                published: 1
            },
            None,
            &Default::default()
        )
        .is_err());
        assert!(!failed.exists());
    }

    /// A catalog of `base`, `middle` linking `base`, and `consumer` linking `middle`, all shared.
    fn runtime_chain(root: &Path) -> (PackageCatalog, PathBuf) {
        let sources = root.join("sources");
        for (name, code) in [
            ("base", "int base(void) { return 40; }"),
            (
                "middle",
                "int base(void); int middle(void) { return base() + 2; }",
            ),
            (
                "consumer",
                "int middle(void); int main(void) { return middle() != 42; }",
            ),
        ] {
            fs::create_dir_all(sources.join(name)).unwrap();
            fs::write(sources.join(name).join("main.c"), code).unwrap();
        }
        let archive = root.join("sources.tar.gz");
        rootbeer_build::pack(&sources, &archive).unwrap();
        let downloads = root.join("downloads");
        let cached = rootbeer_package::download::DownloadCache::new(&downloads)
            .materialize(&format!("file://{}", archive.display()), None)
            .unwrap();

        let system = rootbeer_package::ResolveContext::current().system;
        let is_macos = cfg!(target_os = "macos");
        let mut catalog = crate::test_catalog::catalog().clone();
        let template = catalog.packages["xz"].clone();
        catalog.packages.clear();
        for (name, dependency) in [
            ("base", None),
            ("middle", Some("base")),
            ("consumer", Some("middle")),
        ] {
            let is_library = name != "consumer";
            let filename = match (is_library, is_macos) {
                (false, _) => name.to_string(),
                (true, true) => format!("lib{name}.dylib"),
                (true, false) => format!("lib{name}.so"),
            };
            let mut link = format!("cc main.c -o {filename}");
            if is_library && is_macos {
                link.push_str(&format!(
                    " -dynamiclib -Wl,-install_name,{{prefix}}/lib/{filename}"
                ));
            } else if is_library {
                link.push_str(&format!(" -shared -fPIC -Wl,-soname,{filename}"));
            }
            if let Some(dependency) = dependency {
                link.push_str(&format!(" -L{{dependencies}}/lib -l{dependency}"));
            }
            link.push_str(" $LDFLAGS");
            let directory = if is_library { "lib" } else { "bin" };

            let mut package = template.clone();
            package.name = name.into();
            package.aliases.clear();
            let mut recipe = package.versions.values().next().unwrap().clone();
            package.default_versions = BTreeMap::from([(system.clone(), "1".to_string())]);
            recipe.platforms.retain(|platform, _| *platform == system);
            let platform = recipe.platforms.get_mut(&system).unwrap();
            platform.bins = rootbeer_package::Bins::Names(if is_library {
                vec![]
            } else {
                vec![name.into()]
            });
            platform.checks = if is_library {
                vec![]
            } else {
                vec![vec![name.into()]]
            };
            platform.build = Some(serde_json::from_value(serde_json::json!({
                "backend": "custom", "url": "https://source.invalid/runtime.tar.gz",
                "sha256": cached.sha256, "archive": "tar.gz", "strip_prefix": name,
                "dependencies": dependency
                    .map(|name| serde_json::json!({"package": format!("{name}@1"), "kind": "link_runtime"}))
                    .into_iter()
                    .collect::<Vec<_>>(),
                "libraries": if is_library { vec![format!("lib/{filename}")] } else { vec![] },
                "steps": {"configure": [], "build": [["sh", "-c", link]], "check": [["sh", "-c", "exit 0"]],
                    "install": [["mkdir", "-p", format!("{{prefix}}/{directory}")],
                        ["cp", filename, format!("{{prefix}}/{directory}/")]]}
            })).unwrap());
            package.versions = BTreeMap::from([("1".into(), recipe)]);
            catalog.packages.insert(name.into(), package);
        }
        (catalog, downloads)
    }

    #[test]
    fn dependents_reference_runtime_dependencies_as_their_own_records_publish_them() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (catalog, downloads) = runtime_chain(&root);
        let options = crate::BuildOptions {
            downloads,
            cache: Some(crate::BuildCache {
                directory: root.join("cache"),
                context: "runtime-release-test".into(),
                recheck: false,
            }),
            ..Default::default()
        };
        let key = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        let public_key: String = Ed25519KeyPair::from_pkcs8(key.as_ref())
            .unwrap()
            .public_key()
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let signer = Signer {
            key_der: key.as_ref(),
            public_key: &public_key,
            published: 1,
        };

        fs::create_dir_all(root.join("prepared")).unwrap();
        fs::create_dir_all(root.join("releases")).unwrap();
        let mut built = Vec::new();
        let mut released = BTreeMap::new();
        for name in ["base", "middle", "consumer"] {
            let id = format!("{name}@1");
            let prepared = root.join("prepared").join(name);
            crate::prepare_package(&catalog, &id, &prepared, &options, None, &built).unwrap();
            built.push(prepared.clone());

            let release = root.join("releases").join(name);
            if name == "consumer" {
                let error = release_package(
                    &catalog,
                    &prepared.join("receipt.json"),
                    "example/packages/consumer",
                    &root.join("unreleased"),
                    &signer,
                    None,
                    &BTreeMap::new(),
                )
                .unwrap_err();
                assert!(error.contains("not published"), "{error}");
            }
            release_package(
                &catalog,
                &prepared.join("receipt.json"),
                &format!("example/packages/{name}"),
                &release,
                &signer,
                None,
                &released,
            )
            .unwrap();
            released.insert(id, release);
        }

        let record = |name: &str| {
            let bytes = fs::read(root.join("releases").join(name).join("package.json")).unwrap();
            let system = rootbeer_package::ResolveContext::current().system;
            verify_record(&bytes, &public_key, &format!("{name}@1"), &system).unwrap()
        };
        let base = record("base").artifact.package;
        let middle = record("middle").artifact.package;
        let consumer = record("consumer").artifact.package;
        assert_eq!(middle.runtime_dependencies["base@1"], base);
        assert_eq!(consumer.runtime_dependencies["middle@1"], middle);
        let LockedSource::Url { url, .. } = &consumer.runtime_dependencies["middle@1"].source
        else {
            panic!("runtime dependency is not published");
        };
        assert!(
            url.starts_with("ghcr://example/packages/middle@sha256:"),
            "{url}"
        );
    }
}
