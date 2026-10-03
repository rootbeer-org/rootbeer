use std::fs;
use std::path::{Path, PathBuf};

use rootbeer_package::distribution::UpstreamProvenance;
use rootbeer_package::repository::RepositoryResolver;
use rootbeer_package::{
    LockedPackage, LockedSource, PackageCatalog, PackageRealizer, PackageRequest,
    PackageRequestResolver, PackageResolution, PackageResolverInputs, ResolveContext,
    ResolverInput,
};
use rootbeer_store_legacy::Store;
use serde::{Deserialize, Serialize};

use crate::BuildOptions;

/// An upstream binary's exact bytes, carried beside its receipt so release can verify them
/// without downloading again. Only a mirror publishes them.
pub(crate) const UPSTREAM_FILE: &str = "upstream";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BinaryReceipt {
    pub schema: u32,
    pub system: String,
    pub recipe_sha256: String,
    pub package: LockedPackage,
    pub provenance: UpstreamProvenance,
}

/// Qualifies a source build, or an upstream binary exactly as its vendor publishes it.
/// With a PDR, dependencies it has published builds of are installed rather than compiled, as are
/// the `built` dependency builds earlier jobs qualified.
pub fn prepare_package(
    catalog: &PackageCatalog,
    request: &str,
    output: &Path,
    options: &BuildOptions,
    pdr: Option<&RepositoryResolver>,
    built: &[PathBuf],
) -> Result<LockedPackage, String> {
    catalog.validate()?;
    let (_, _, recipe) = rootbeer_package::graph::find_recipe(catalog, request)?;
    if recipe.build.is_some() {
        let mut plan = crate::BuildPlan::current(catalog, request)?;
        if let Some(pdr) = pdr {
            let system = ResolveContext::current().system;
            crate::PublishedDependencies::find(catalog, request, &system, pdr)?
                .use_in(&mut plan)?;
        }
        crate::BuiltDependencies::load(catalog, request, built, options)?.use_in(&mut plan)?;
        return plan
            .execute(output, options)
            .map(|artifact| artifact.package);
    }
    if !built.is_empty() {
        return Err("an upstream binary is qualified without dependency builds".into());
    }
    let inputs = package_inputs(catalog);
    let mut resolver = rootbeer_package::backend_stack().with_implicit_resolver("rootbeer");
    resolver.push(rootbeer_package::catalog::CatalogResolver::new(
        catalog,
        &inputs,
        rootbeer_package::backend_stack(),
    ));
    let platform = ResolveContext::current();
    let resolution = resolver
        .resolve_package(&PackageRequest::parse(request), &platform)
        .map_err(|error| error.to_string())?;
    if let Some(pdr) = pdr.filter(|_| recipe.mirror) {
        restore_mirrored(pdr, request, &resolution.package, options);
    }
    prepare_binary(catalog, request, output, options, resolution, inputs)
}

/// Requalifying a mirrored binary takes the vendor's bytes from our copy, so a vendor that has
/// since replaced its file cannot fail it. The digest still verifies them either way.
fn restore_mirrored(
    pdr: &RepositoryResolver,
    request: &str,
    upstream: &LockedPackage,
    options: &BuildOptions,
) {
    let Ok((record, _)) = pdr.record(&PackageRequest::parse(request), &ResolveContext::current())
    else {
        return;
    };
    let Some((url, sha256)) = mirror_of(upstream, &record.artifact.package) else {
        return;
    };

    let downloads = rootbeer_package::download::DownloadCache::new(&options.downloads)
        .with_execution(options.execution.clone());
    match downloads.materialize_verified(url, sha256) {
        Ok(_) => eprintln!("restored {request} from its mirror"),
        Err(error) => eprintln!("mirror of {request} is unavailable, using the vendor: {error}"),
    }
}

/// The published copy holding exactly the vendor's bytes. Records from before mirrors kept the
/// vendor's file hold a repackaged archive instead, which cannot stand in for it.
fn mirror_of<'a>(
    upstream: &'a LockedPackage,
    published: &'a LockedPackage,
) -> Option<(&'a str, &'a str)> {
    let LockedSource::Url { sha256, .. } = &upstream.source else {
        return None;
    };
    let LockedSource::Url {
        url,
        sha256: mirrored,
    } = &published.source
    else {
        return None;
    };
    (url.starts_with("ghcr://") && mirrored == sha256).then_some((url.as_str(), sha256.as_str()))
}

fn prepare_binary(
    catalog: &PackageCatalog,
    request: &str,
    output: &Path,
    options: &BuildOptions,
    resolution: PackageResolution,
    mut inputs: PackageResolverInputs,
) -> Result<LockedPackage, String> {
    let (_, _, recipe) = rootbeer_package::graph::find_recipe(catalog, request)?;
    let context = options
        .cache
        .as_ref()
        .map_or("", |cache| cache.context.as_str());
    let environment = options.environment_identity(catalog, request, context)?;
    let staging = rootbeer_package::staging::staging(output)?;
    let destination = staging.path().join("result");
    fs::create_dir(&destination).map_err(|error| error.to_string())?;
    let realizer = PackageRealizer::with_dirs(
        Store::new(staging.path().join("store")),
        &options.downloads,
        staging.path().join("install"),
    )
    .with_execution(options.execution.clone());
    inputs.resolvers.remove("rootbeer");
    let proof = match resolution.proof {
        rootbeer_package::ResolutionProof::Catalog(proof) => *proof.source_proof,
        proof => proof,
    };
    let mut upstream = resolution.package;
    let realized = realizer
        .realize(&upstream)
        .map_err(|error| error.to_string())?;
    upstream.output_sha256 = Some(realized.store_entry.output_sha256.clone());
    crate::checks::check_package(
        &upstream,
        &realized,
        &realizer,
        &recipe.checks,
        staging.path(),
        options,
    )?;
    if options.environment_identity(catalog, request, context)? != environment {
        return Err("package environment changed during qualification".into());
    }
    // Clients install the vendor's file as published; a mirror only relocates those bytes.
    let LockedSource::Url { url, sha256 } = &upstream.source else {
        return Err("an upstream binary must be a remote download".into());
    };
    let file = rootbeer_package::download::DownloadCache::new(&options.downloads)
        .materialize_verified(url, sha256)
        .map_err(|error| error.to_string())?;
    fs::copy(file, destination.join(UPSTREAM_FILE)).map_err(|error| error.to_string())?;
    let package = upstream.clone();
    let receipt = BinaryReceipt {
        schema: 2,
        system: ResolveContext::current().system,
        recipe_sha256: recipe.sha256(),
        package: package.clone(),
        provenance: UpstreamProvenance {
            engine_sha256: rootbeer_build::engine_identity(None),
            environment_sha256: environment,
            upstream,
            resolution: proof,
            resolver_inputs: inputs,
        },
    };
    fs::write(
        destination.join("receipt.json"),
        serde_json::to_vec(&receipt).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    fs::rename(destination, output).map_err(|error| error.to_string())?;
    Ok(package)
}

/// Resolver inputs that pin this catalog.
fn package_inputs(catalog: &PackageCatalog) -> PackageResolverInputs {
    let mut inputs = PackageResolverInputs::default();
    inputs.resolvers.insert(
        "rootbeer".into(),
        ResolverInput::Catalog {
            sha256: catalog.sha256(),
        },
    );
    inputs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mirror_stands_in_only_for_the_vendors_exact_bytes() {
        let vendor = "a".repeat(64);
        let package = |url: String, sha256: &str| LockedPackage {
            name: "app".into(),
            version: "1".into(),
            source: LockedSource::Url {
                url,
                sha256: sha256.into(),
            },
            install: LockedInstall::Dmg,
            provides: Provides {
                bins: BTreeMap::new(),
                apps: BTreeMap::new(),
            },
            output_sha256: None,
            runtime_dependencies: BTreeMap::new(),
        };
        let upstream = package("https://vendor.example/app.dmg".into(), &vendor);
        let mirrored = format!("ghcr://org/pdr/app@sha256:{vendor}");

        assert_eq!(
            mirror_of(&upstream, &package(mirrored.clone(), &vendor)),
            Some((mirrored.as_str(), vendor.as_str()))
        );
        let repackaged = "b".repeat(64);
        assert_eq!(
            mirror_of(
                &upstream,
                &package(
                    format!("ghcr://org/pdr/app@sha256:{repackaged}"),
                    &repackaged
                )
            ),
            None
        );
        assert_eq!(
            mirror_of(
                &upstream,
                &package("https://vendor.example/app.dmg".into(), &vendor)
            ),
            None
        );
    }
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use rootbeer_package::distribution::{verify_record, PackageProvenance};
    use rootbeer_package::{
        download::DownloadCache, ArchiveFormat, LockedInstall, Provides, ResolutionProof,
        SnapshotProof, SnapshotSource,
    };
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    #[cfg(target_os = "macos")]
    fn a_dmg_is_published_as_the_vendor_ships_it() {
        use std::os::unix::fs::symlink;
        use std::process::Command;

        fn run(command: &mut Command) {
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{command:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let app = source.join("Demo.app");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::create_dir_all(app.join("Contents/Resources")).unwrap();
        fs::write(app.join("Contents/Info.plist"), r#"<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleExecutable</key><string>demo</string><key>CFBundleIdentifier</key><string>org.rootbeer.dmg-test</string><key>CFBundlePackageType</key><string>APPL</string></dict></plist>"#).unwrap();
        fs::write(app.join("Contents/Resources/message"), "preserved").unwrap();
        symlink("Resources", app.join("Contents/LinkedResources")).unwrap();
        let program = root.path().join("demo.c");
        fs::write(&program, "int main(void) { return 0; }\n").unwrap();
        run(Command::new("/usr/bin/cc")
            .arg(&program)
            .arg("-o")
            .arg(app.join("Contents/MacOS/demo")));
        run(Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&app));
        run(Command::new("/usr/bin/xattr")
            .args([
                "-wx",
                "com.apple.FinderInfo",
                "5445535400000000000000000000000000000000000000000000000000000000",
            ])
            .arg(app.join("Contents/MacOS/demo")));
        assert!(!Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&app)
            .output()
            .unwrap()
            .status
            .success());
        let archive = root.path().join("demo.dmg");
        run(Command::new("/usr/bin/hdiutil")
            .args(["create", "-fs", "HFS+", "-format", "UDZO", "-srcfolder"])
            .arg(&source)
            .arg(&archive));
        let options = BuildOptions {
            downloads: root.path().join("downloads"),
            ..Default::default()
        };
        let cached = DownloadCache::new(&options.downloads)
            .materialize(&format!("file://{}", archive.display()), None)
            .unwrap();
        let system = ResolveContext::current().system;
        let catalog: PackageCatalog = serde_json::from_value(serde_json::json!({
            "packages": {"demo": {
                "name": "demo", "description": "DMG qualification fixture", "homepage": "https://example.com",
                "default_versions": {system.clone(): "1"}, "versions": {"1": {
                    "license": "MIT", "revision": 1, "platforms": {system.clone(): {
                        "source": "github:example/demo@v1", "asset": "demo.dmg",
                        "sha256": cached.sha256, "apps": {"Demo.app": "Demo.app"}
                    }}
                }}
            }}
        })).unwrap();
        catalog.validate().unwrap();
        let package = LockedPackage {
            name: "demo".into(),
            version: "1".into(),
            source: LockedSource::Url {
                url: "https://example.com/demo.dmg".into(),
                sha256: cached.sha256,
            },
            install: LockedInstall::Dmg,
            provides: Provides {
                bins: BTreeMap::new(),
                apps: BTreeMap::from([("Demo.app".into(), "Demo.app".into())]),
            },
            output_sha256: None,
            runtime_dependencies: BTreeMap::new(),
        };
        let resolution = PackageResolution::new(
            package,
            ResolutionProof::Snapshot(SnapshotProof {
                resolver: "fixture".into(),
                source: SnapshotSource::Url {
                    url: "https://example.com/metadata.json".into(),
                },
                documents: vec![],
            }),
        );
        let prepared = root.path().join("prepared");
        let package = prepare_binary(
            &catalog,
            "demo@1",
            &prepared,
            &options,
            resolution.clone(),
            PackageResolverInputs::default(),
        )
        .unwrap();
        assert_eq!(package.install, LockedInstall::Dmg);
        assert_eq!(package.source, resolution.package.source);
        assert_eq!(
            fs::read(prepared.join(UPSTREAM_FILE)).unwrap(),
            fs::read(&archive).unwrap(),
            "release verifies the vendor's exact bytes"
        );
        let installed = PackageRealizer::with_dirs(
            Store::new(root.path().join("consumer")),
            root.path().join("downloads"),
            root.path().join("install"),
        )
        .realize(&package)
        .unwrap();
        run(Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&installed.apps["Demo.app"]));
        assert_eq!(
            fs::read(installed.apps["Demo.app"].join("Contents/LinkedResources/message")).unwrap(),
            b"preserved"
        );
        assert_eq!(
            package.output_sha256.as_ref(),
            Some(&installed.store_entry.output_sha256)
        );
        assert!(installed.bins.is_empty());
        fs::write(
            installed.apps["Demo.app"].join("Contents/Resources/message"),
            "tampered",
        )
        .unwrap();
        assert!(crate::checks::check_package(
            &package,
            &installed,
            &PackageRealizer::new(Store::new(root.path().join("unused"))),
            &[],
            root.path(),
            &options
        )
        .is_err());
    }

    #[test]
    fn qualifies_and_signs_upstream_binaries_without_source_build_evidence() {
        for is_archive in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let tree = root.path().join("upstream");
            fs::create_dir_all(tree.join("bin")).unwrap();
            let executable = tree.join("bin/demo");
            fs::write(&executable, b"#!/bin/sh\n[ \"$1\" = --version ]\n").unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
            let (source, install) = if is_archive {
                let archive = root.path().join("upstream.tar.gz");
                rootbeer_build::pack(&tree, &archive).unwrap();
                (
                    archive,
                    LockedInstall::Archive {
                        format: ArchiveFormat::TarGz,
                        strip_prefix: None,
                    },
                )
            } else {
                (
                    executable,
                    LockedInstall::Binary {
                        path: "bin/demo".into(),
                    },
                )
            };
            let options = BuildOptions {
                downloads: root.path().join("downloads"),
                cache: Some(crate::BuildCache {
                    directory: root.path().join("cache"),
                    context: "fixture-image".into(),
                    recheck: false,
                }),
                ..Default::default()
            };
            let cached = DownloadCache::new(&options.downloads)
                .materialize(&format!("file://{}", source.display()), None)
                .unwrap();
            let system = ResolveContext::current().system;
            let catalog: PackageCatalog = serde_json::from_value(serde_json::json!({
                "packages": {"demo": {
                    "name": "demo", "description": "Binary qualification fixture", "homepage": "https://example.com",
                    "default_versions": {system.clone(): "1"}, "versions": {"1": {
                        "license": "MIT", "revision": 1, "platforms": {system.clone(): {
                            "source": "github:example/demo@v1", "asset": "demo",
                            "sha256": cached.sha256,
                            "bins": ["demo"], "checks": [["demo", "--version"]]
                        }}
                    }}
                }}
            })).unwrap();
            let task = crate::plan_packages(
                &catalog,
                &["demo@1".into()],
                &options,
                "fixture-image",
                None,
            )
            .unwrap()
            .remove(0);
            let package = LockedPackage {
                name: "demo".into(),
                version: "1".into(),
                source: LockedSource::Url {
                    url: "https://example.com/demo".into(),
                    sha256: cached.sha256,
                },
                install,
                provides: Provides {
                    bins: BTreeMap::from([("demo".into(), "bin/demo".into())]),
                    apps: BTreeMap::new(),
                },
                output_sha256: None,
                runtime_dependencies: BTreeMap::new(),
            };
            let resolution = PackageResolution::new(
                package,
                ResolutionProof::Snapshot(SnapshotProof {
                    resolver: "fixture".into(),
                    source: SnapshotSource::Url {
                        url: "https://example.com/metadata.json".into(),
                    },
                    documents: vec![],
                }),
            );
            let prepared = root.path().join("prepared");
            prepare_binary(
                &catalog,
                "demo@1",
                &prepared,
                &options,
                resolution.clone(),
                PackageResolverInputs::default(),
            )
            .unwrap();
            let key = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
            let public_key: String = Ed25519KeyPair::from_pkcs8(key.as_ref())
                .unwrap()
                .public_key()
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            let release = root.path().join("release");
            crate::release_package(
                &catalog,
                &prepared.join("receipt.json"),
                "example/demo",
                &release,
                &crate::release::Signer {
                    key_der: key.as_ref(),
                    public_key: &public_key,
                    published: 1,
                },
                Some(&task.key),
                &Default::default(),
            )
            .unwrap();
            let bytes = fs::read(release.join("package.json")).unwrap();
            let record = verify_record(&bytes, &public_key, "demo@1", &system).unwrap();
            assert_eq!(record.input_key(), task.key);
            let PackageProvenance::Upstream(provenance) = &record.provenance else {
                panic!("binary claimed a source build")
            };
            assert_eq!(provenance.upstream.source, resolution.package.source);
            assert_eq!(
                provenance.upstream.output_sha256,
                record.artifact.package.output_sha256
            );
            assert!(!String::from_utf8(bytes).unwrap().contains("toolchain"));
            let mut changed = record.clone();
            changed.recipe.sha256 = Some("f".repeat(64));
            assert!(changed.validate().unwrap_err().contains("checksum"));
            changed = record.clone();
            changed.artifact.package.output_sha256 = Some("f".repeat(64));
            assert!(changed
                .validate()
                .unwrap_err()
                .contains("upstream download"));
            assert_eq!(
                record.artifact.package.source, resolution.package.source,
                "clients download the vendor's file"
            );
            let mut repackaged = record.clone();
            repackaged.artifact.package.source = LockedSource::Url {
                url: format!("ghcr://example/demo@sha256:{}", "e".repeat(64)),
                sha256: "e".repeat(64),
            };
            repackaged.artifact.package.install = LockedInstall::Archive {
                format: ArchiveFormat::TarGz,
                strip_prefix: None,
            };
            repackaged
                .validate()
                .expect("records published as repacks stay valid until superseded");
            assert!(crate::release_package(
                &catalog,
                &prepared.join("receipt.json"),
                "example/demo",
                &root.path().join("wrong-inputs"),
                &crate::release::Signer {
                    key_der: key.as_ref(),
                    public_key: &public_key,
                    published: 1
                },
                Some(&"f".repeat(64)),
                &Default::default()
            )
            .is_err());
            let mut failing = catalog.clone();
            let platform = failing
                .packages
                .get_mut("demo")
                .unwrap()
                .versions
                .get_mut("1")
                .unwrap()
                .platforms
                .get_mut(&ResolveContext::current().system)
                .unwrap();
            platform.checks = vec![vec!["demo".into(), "fail".into()]];
            assert!(prepare_binary(
                &failing,
                "demo@1",
                &root.path().join("failed"),
                &options,
                resolution.clone(),
                PackageResolverInputs::default()
            )
            .is_err());
            assert!(!root.path().join("failed").exists());
            let mut mirrored = catalog.clone();
            let platform = mirrored
                .packages
                .get_mut("demo")
                .unwrap()
                .versions
                .get_mut("1")
                .unwrap()
                .platforms
                .get_mut(&system)
                .unwrap();
            platform.mirror = true;
            let prepared = root.path().join("mirrored");
            prepare_binary(
                &mirrored,
                "demo@1",
                &prepared,
                &options,
                resolution.clone(),
                PackageResolverInputs::default(),
            )
            .unwrap();
            assert_eq!(
                fs::read(prepared.join(UPSTREAM_FILE)).unwrap(),
                fs::read(&source).unwrap(),
                "a mirror holds the vendor's exact bytes"
            );
            let signer = crate::release::Signer {
                key_der: key.as_ref(),
                public_key: &public_key,
                published: 1,
            };
            let release = root.path().join("mirror-release");
            crate::release_package(
                &mirrored,
                &prepared.join("receipt.json"),
                "example/demo",
                &release,
                &signer,
                None,
                &Default::default(),
            )
            .unwrap();
            let bytes = fs::read(release.join("package.json")).unwrap();
            let record = verify_record(&bytes, &public_key, "demo@1", &system).unwrap();
            let LockedSource::Url { sha256, .. } = &resolution.package.source else {
                unreachable!()
            };
            assert_eq!(
                record.artifact.package.source,
                LockedSource::Url {
                    url: format!("ghcr://example/demo@sha256:{sha256}"),
                    sha256: sha256.clone(),
                }
            );
            assert_eq!(record.artifact.package.install, resolution.package.install);

            fs::write(prepared.join(UPSTREAM_FILE), b"tampered").unwrap();
            assert!(crate::release_package(
                &mirrored,
                &prepared.join("receipt.json"),
                "example/demo",
                &root.path().join("tampered"),
                &signer,
                None,
                &Default::default()
            )
            .unwrap_err()
            .contains("hash mismatch"));
        }
    }
}
