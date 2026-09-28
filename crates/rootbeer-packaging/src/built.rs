use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rootbeer_package::graph::DependencyGraph;
use rootbeer_package::{
    BuildArtifact, LockedPackage, LockedSource, PackageCatalog, ResolveContext,
};
use rootbeer_store::hash_file;

use crate::{BuildOptions, BuildPlan};

/// Dependency builds that earlier jobs of the same run qualified, installed instead of compiled.
///
/// Each is a `prepare` output directory: its receipt must match the recipe the catalog approves
/// now and the environment this build runs in, and its archive must match the receipt.
#[derive(Debug, Default)]
pub struct BuiltDependencies {
    builds: BTreeMap<String, Built>,
}

#[derive(Debug)]
struct Built {
    package: LockedPackage,
    dependencies: BTreeMap<String, LockedPackage>,
}

impl BuiltDependencies {
    pub fn load(
        catalog: &PackageCatalog,
        request: &str,
        directories: &[PathBuf],
        options: &BuildOptions,
    ) -> Result<Self, String> {
        let system = ResolveContext::current().system;
        let graph = DependencyGraph::new(catalog, &[request.to_string()], &system)?;
        let closure = &graph.nodes[request].closure;
        let context = options
            .cache
            .as_ref()
            .map_or("", |cache| cache.context.as_str());
        let mut builds = BTreeMap::new();
        for directory in directories {
            let (key, built) = load(catalog, directory, &system, options, context)?;
            if !closure.contains(&key) {
                return Err(format!("{key}: not a dependency of {request}"));
            }
            if builds.insert(key.clone(), built).is_some() {
                return Err(format!("{key}: given more than one build"));
            }
        }
        Ok(Self { builds })
    }

    /// Installs each build, then requires every dependency it was built with to be installed
    /// from that same output, so the whole closure links one build of each package.
    pub fn use_in(&self, plan: &mut BuildPlan) -> Result<(), String> {
        for (key, built) in &self.builds {
            plan.use_built(key, built.package.clone())?;
        }
        for (key, built) in &self.builds {
            for (dependency, used) in &built.dependencies {
                let installed = plan.installed(dependency).ok_or_else(|| {
                    format!("{key} was built with {dependency}; provide that build too")
                })?;
                if installed.output_sha256 != used.output_sha256 {
                    return Err(format!(
                        "{key} was built with another build of {dependency}"
                    ));
                }
            }
        }
        Ok(())
    }
}

fn load(
    catalog: &PackageCatalog,
    directory: &Path,
    system: &str,
    options: &BuildOptions,
    context: &str,
) -> Result<(String, Built), String> {
    let receipt = directory.join("receipt.json");
    let receipt: BuildArtifact = serde_json::from_slice(
        &fs::read(&receipt).map_err(|error| format!("{}: {error}", receipt.display()))?,
    )
    .map_err(|error| error.to_string())?;
    crate::receipt::validate_receipt(catalog, &receipt)?;
    let key = receipt.package.id();
    if receipt.system != system {
        return Err(format!("{key}: built for {}, not {system}", receipt.system));
    }
    if !receipt.package.runtime_dependencies.is_empty() {
        return Err(format!(
            "{key}: builds with runtime dependencies cannot be handed off yet"
        ));
    }
    let LockedSource::File { sha256, .. } = &receipt.package.source else {
        return Err(format!("{key}: receipt does not name a local archive"));
    };
    let archive = directory
        .join("package.tar.gz")
        .canonicalize()
        .map_err(|error| format!("{key}: {error}"))?;
    if hash_file(&archive).map_err(|error| error.to_string())? != *sha256 {
        return Err(format!("{key}: archive differs from its receipt"));
    }
    let environment = options.environment_identity(catalog, &key, context)?;
    if receipt.qualification_environment.as_deref() != Some(environment.as_str()) {
        return Err(format!(
            "{key}: built in another environment than this job's"
        ));
    }
    let mut package = receipt.package.clone();
    package.source = LockedSource::File {
        path: archive,
        sha256: sha256.clone(),
    };
    Ok((
        key,
        Built {
            package,
            dependencies: receipt.dependencies,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(root: &Path) -> (PackageCatalog, PathBuf) {
        let (catalog, receipt) = crate::receipt::tests::fixture(root);
        (catalog, receipt.parent().unwrap().to_path_buf())
    }

    #[test]
    fn a_build_for_another_system_is_not_installed() {
        let root = tempfile::tempdir().unwrap();
        let (catalog, build) = fixture(root.path());
        let error = load(
            &catalog,
            &build,
            "x86_64-linux",
            &BuildOptions::default(),
            "",
        )
        .unwrap_err();
        assert!(error.contains("not x86_64-linux"), "{error}");
    }

    #[test]
    fn an_archive_that_differs_from_its_receipt_is_not_installed() {
        let root = tempfile::tempdir().unwrap();
        let (catalog, build) = fixture(root.path());
        fs::write(build.join("package.tar.gz"), b"replaced").unwrap();
        let error = load(
            &catalog,
            &build,
            "aarch64-linux",
            &BuildOptions::default(),
            "",
        )
        .unwrap_err();
        assert!(error.contains("archive differs"), "{error}");
    }

    #[test]
    fn a_receipt_for_another_recipe_is_not_installed() {
        let root = tempfile::tempdir().unwrap();
        let (catalog, build) = fixture(root.path());
        let path = build.join("receipt.json");
        let mut receipt: BuildArtifact = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        receipt.revision += 1;
        fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        let error = load(
            &catalog,
            &build,
            "aarch64-linux",
            &BuildOptions::default(),
            "",
        )
        .unwrap_err();
        assert!(error.contains("does not match catalog inputs"), "{error}");
    }
}
