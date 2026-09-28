//! Checking a build receipt against the catalog and turning it into a published artifact.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::{
    ArchiveFormat, BuildArtifact, LockedInstall, LockedSource, PackageCatalog, PackageRealizer,
    PublishedArtifact,
};
use rootbeer_catalog::is_sha256;
use rootbeer_store::{hash_bytes, hash_file};

/// Verifies a source build's receipt and archives, audits the realized tree, and returns the
/// artifact as it will be addressed in `registry`. The destination receives the archives.
pub(crate) fn prepare_artifact(
    catalog: &PackageCatalog,
    receipt_path: &Path,
    registry: &str,
    destination: &Path,
    realizer: &PackageRealizer,
) -> Result<(String, PublishedArtifact, Vec<u8>), String> {
    let bytes = fs::read(receipt_path).map_err(|e| format!("{}: {e}", receipt_path.display()))?;
    let receipt: BuildArtifact = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    validate_receipt(catalog, &receipt)?;
    let key = receipt.package.id();
    let LockedSource::File { sha256, .. } = &receipt.package.source else {
        return Err(format!("{key}: expected a source-build file artifact"));
    };
    let sha256 = sha256.clone();
    // Receipts move between CI runners; never follow the builder's absolute paths.
    let source = receipt_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("package.tar.gz");
    let target = destination
        .join("artifacts")
        .join(format!("{sha256}.tar.gz"));
    fs::copy(source, &target).map_err(|e| e.to_string())?;
    if hash_file(&target).map_err(|e| e.to_string())? != sha256 {
        return Err(format!("{key}: artifact hash mismatch"));
    }
    let mut package = receipt.package;
    copy_runtime(
        &mut package,
        receipt_path.parent().unwrap_or(Path::new(".")),
        destination,
    )?;
    package.source = LockedSource::File {
        path: target,
        sha256: sha256.clone(),
    };
    let realized = realizer
        .realize(&package)
        .map_err(|e| format!("{key}: {e}"))?;
    let mut runtime = BTreeMap::new();
    for dependency in rootbeer_package::runtime::closure(&package)? {
        let realized = realizer.realize(dependency).map_err(|e| e.to_string())?;
        runtime.insert(
            rootbeer_package::runtime::store_directory(dependency)?,
            realized.store_entry.path,
        );
    }
    let report = rootbeer_build::audit::audit_with_runtime(&realized.store_entry.path, &runtime)?;
    report.validate()?;
    if let Some(expected) = &receipt.runtime_audit_sha256 {
        let report = serde_json::to_vec_pretty(&report).map_err(|error| error.to_string())?;
        if hash_bytes(&report) != *expected {
            return Err(format!("{key}: runtime audit digest mismatch"));
        }
    }
    package.source = LockedSource::Url {
        url: format!("ghcr://{registry}@sha256:{sha256}"),
        sha256: sha256.clone(),
    };
    publish_runtime(&mut package, registry)?;
    let receipt_sha256 = hash_bytes(&bytes);
    Ok((
        receipt.system,
        PublishedArtifact {
            revision: receipt.revision,
            receipt_sha256,
            package,
        },
        bytes,
    ))
}

fn copy_runtime(
    package: &mut rootbeer_package::LockedPackage,
    source: &Path,
    destination: &Path,
) -> Result<(), String> {
    for dependency in package.runtime_dependencies.values_mut() {
        copy_runtime(dependency, source, destination)?;
        let LockedSource::File { sha256, .. } = &dependency.source else {
            return Err("runtime receipts require local archives".into());
        };
        let archive = source.join("runtime").join(format!(
            "{}.tar.gz",
            rootbeer_package::runtime::store_directory(dependency)?.display()
        ));
        let target = destination
            .join("artifacts")
            .join(format!("{sha256}.tar.gz"));
        fs::copy(archive, &target).map_err(|e| e.to_string())?;
        if hash_file(&target).map_err(|e| e.to_string())? != *sha256 {
            return Err(format!(
                "{}: runtime archive hash mismatch",
                dependency.id()
            ));
        }
        dependency.source = LockedSource::File {
            path: target,
            sha256: sha256.clone(),
        };
    }
    Ok(())
}

fn publish_runtime(
    package: &mut rootbeer_package::LockedPackage,
    registry: &str,
) -> Result<(), String> {
    for dependency in package.runtime_dependencies.values_mut() {
        publish_runtime(dependency, registry)?;
        let LockedSource::File { sha256, .. } = &dependency.source else {
            return Err("runtime bundle requires local archives".into());
        };
        dependency.source = LockedSource::Url {
            url: format!("ghcr://{registry}@sha256:{sha256}"),
            sha256: sha256.clone(),
        };
    }
    Ok(())
}

pub(crate) fn validate_receipt(
    catalog: &PackageCatalog,
    receipt: &BuildArtifact,
) -> Result<(), String> {
    let package = &receipt.package;
    let entry = catalog
        .packages
        .get(&package.name)
        .and_then(|entry| entry.versions.get(&package.version))
        .ok_or_else(|| format!("{}: no matching catalog recipe", package.id()))?;
    let revision = entry.revision;
    let recipe = entry
        .for_system(&receipt.system)
        .ok_or_else(|| format!("{}: no recipe for {}", package.id(), receipt.system))?;
    let Some(build) = &recipe.build else {
        return Err(format!("{}: not a source recipe", package.id()));
    };
    if !matches!(receipt.schema, 1 | 2)
        || (receipt.schema < 2 && !package.runtime_dependencies.is_empty())
        || receipt.revision != revision
        || receipt.recipe_sha256 != recipe.sha256()
        || serde_json::to_value(&receipt.build).map_err(|e| e.to_string())?
            != serde_json::to_value(build).map_err(|e| e.to_string())?
    {
        return Err(format!(
            "{}: receipt does not match catalog inputs",
            package.id()
        ));
    }
    if package.install
        != (LockedInstall::Archive {
            format: ArchiveFormat::TarGz,
            strip_prefix: None,
        })
        || package
            .output_sha256
            .as_deref()
            .is_none_or(|sha| !is_sha256(sha))
        || !matches!(&package.source, LockedSource::File { sha256, .. } if is_sha256(sha256))
        || package.provides.apps != recipe.apps
        || package.provides.bins.len() != recipe.bins.names().len()
        || recipe
            .bins
            .names()
            .iter()
            .any(|bin| package.provides.bins.get(*bin) != Some(&PathBuf::from("bin").join(bin)))
    {
        return Err(format!(
            "{}: invalid artifact hash, layout, or commands",
            package.id()
        ));
    }
    let graph =
        rootbeer_package::graph::DependencyGraph::new(catalog, &[package.id()], &receipt.system)?;
    let closure = &graph.nodes[&package.id()].closure;
    if receipt.dependencies.len() != closure.len()
        || closure.iter().any(|dependency| {
            receipt
                .dependencies
                .get(dependency)
                .is_none_or(|package| package.id() != *dependency)
        })
    {
        return Err(format!(
            "{}: build dependency receipts do not match",
            package.id()
        ));
    }
    let runtime = rootbeer_package::runtime::closure(package)?;
    if runtime
        .iter()
        .map(|package| package.id())
        .collect::<std::collections::BTreeSet<_>>()
        != graph.nodes[&package.id()]
            .runtime_closure
            .iter()
            .cloned()
            .collect()
    {
        return Err("receipt runtime closure does not match dependency roles".into());
    }
    for runtime_package in runtime.into_iter().chain(std::iter::once(package)) {
        let direct: std::collections::BTreeSet<_> = graph.nodes[&runtime_package.id()]
            .dependencies
            .iter()
            .filter(|dependency| dependency.kind().is_runtime())
            .map(|dependency| dependency.package())
            .collect();
        if direct
            != runtime_package
                .runtime_dependencies
                .keys()
                .map(String::as_str)
                .collect()
        {
            return Err("receipt runtime edges do not match dependency roles".into());
        }
        if runtime_package.id() == package.id() {
            continue;
        }
        if !matches!(&runtime_package.source, LockedSource::File { sha256, .. } if is_sha256(sha256))
            || runtime_package.install
                != (LockedInstall::Archive {
                    format: ArchiveFormat::TarGz,
                    strip_prefix: None,
                })
        {
            return Err("runtime receipts require hashed tar.gz archives".into());
        }
        let expected = &receipt.dependencies[&runtime_package.id()];
        if runtime_package.output_sha256 != expected.output_sha256
            || runtime_package.provides != expected.provides
        {
            return Err("receipt runtime output differs from build dependency".into());
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{LockedPackage, PackageResolverInputs, Provides};
    use flate2::{write::GzEncoder, Compression};
    use rootbeer_store::hash_tree;
    use std::os::unix::fs::PermissionsExt;

    pub(crate) fn fixture(root: &Path) -> (PackageCatalog, PathBuf) {
        let catalog = crate::test_catalog::catalog().clone();
        let entry = &catalog.packages["xz"];
        let version = entry.default_version_for("aarch64-linux").unwrap();
        let entry_version = &entry.versions[version];
        let recipe = &entry_version.platforms["aarch64-linux"];
        let tree = root.join("tree");
        fs::create_dir_all(tree.join("bin")).unwrap();
        for bin in recipe.bins.names() {
            let path = tree.join("bin").join(bin);
            fs::write(&path, b"bundle test executable\n").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let build = root.join("build");
        fs::create_dir(&build).unwrap();
        let archive = build.join("package.tar.gz");
        let mut writer = tar::Builder::new(GzEncoder::new(
            fs::File::create(&archive).unwrap(),
            Compression::default(),
        ));
        writer.append_dir_all(".", &tree).unwrap();
        writer.into_inner().unwrap().finish().unwrap();
        let receipt = BuildArtifact {
            schema: 1,
            recipe_sha256: recipe.sha256(),
            qualification_environment: None,
            build_key: None,
            build_environment: None,
            environment: None,
            isolation: None,
            runtime_audit_sha256: None,
            catalog_sha256: catalog.sha256(),
            revision: entry_version.revision,
            system: "aarch64-linux".into(),
            build: recipe.build.clone().unwrap(),
            dependencies: BTreeMap::new(),
            published_dependencies: BTreeMap::new(),
            resolver_inputs: PackageResolverInputs::default(),
            toolchain: BTreeMap::new(),
            package: LockedPackage {
                name: entry.name.clone(),
                version: version.into(),
                source: LockedSource::File {
                    path: PathBuf::from("/unavailable/runner/package.tar.gz"),
                    sha256: hash_file(&archive).unwrap(),
                },
                install: LockedInstall::Archive {
                    format: ArchiveFormat::TarGz,
                    strip_prefix: None,
                },
                provides: Provides {
                    apps: Default::default(),
                    bins: recipe
                        .bins
                        .names()
                        .into_iter()
                        .map(|bin| (bin.clone(), PathBuf::from("bin").join(bin)))
                        .collect(),
                },
                runtime_dependencies: Default::default(),
                output_sha256: Some(hash_tree(&tree).unwrap()),
            },
        };
        let path = build.join("receipt.json");
        fs::write(&path, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();
        (catalog, path)
    }
}
