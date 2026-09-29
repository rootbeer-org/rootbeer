use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rootbeer_packaging::work::{Work, WorkPlan};
use rootbeer_packaging::{BuildArtifact, PackageCatalog};

use crate::ci::summary;
use crate::config::Config;

/// Signs and pushes every downloaded build of `plan`, each on its own, saving each record for
/// discovery. A failed package fails the step only after every other one is published.
pub fn publish(
    config: &Config,
    catalog: &PackageCatalog,
    plan: &WorkPlan,
    directory: &Path,
    records: &Path,
) -> Result<(), String> {
    let public_key = &config
        .pdr
        .as_ref()
        .ok_or("forge.toml does not name a PDR")?
        .public_key;
    let encoded = std::env::var("PDR_SIGNING_KEY")
        .map_err(|_| "publication needs PDR_SIGNING_KEY from its environment")?;
    let key_der =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded.trim())
            .map_err(|_| "PDR_SIGNING_KEY is not base64")?;
    let published = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_secs();
    let signer = rootbeer_packaging::Signer {
        key_der: &key_der,
        public_key,
        published,
    };
    fs::create_dir_all(records).map_err(|error| error.to_string())?;
    let releases = tempfile::tempdir().map_err(|error| error.to_string())?;

    let mut builds = builds(directory)?;
    builds.sort_by_cached_key(|build| closure_size(build));

    let mut text = String::new();
    let mut failures = Vec::new();
    let mut done = Vec::new();
    let mut released = BTreeMap::new();
    for build in builds {
        let result = release(
            catalog,
            plan,
            &build,
            (releases.path(), &released),
            &signer,
            public_key,
        )
        .and_then(|(package, key, reference)| {
            fs::copy(
                releases.path().join(&key).join("package.json"),
                records.join(format!("{key}.json")),
            )
            .map_err(|error| error.to_string())?;
            Ok((package, key, reference))
        });
        match result {
            Ok((package, key, reference)) => {
                text.push_str(&format!("- Published `{package}`: `{reference}`\n"));
                released.insert(package.clone(), releases.path().join(key));
                done.push(package);
            }
            Err(error) => {
                eprintln!("error: {}: {error}", build.display());
                text.push_str(&format!("- Failed `{}`: {error}\n", build.display()));
                failures.push(error);
            }
        }
    }
    for task in &plan.tasks {
        let is_published = matches!(task.work, Work::Build { .. } | Work::Recover { .. });
        if is_published && !done.contains(&task.package) {
            text.push_str(&format!("- No build of `{}` to publish\n", task.package));
        }
    }
    summary(&text)?;
    if !failures.is_empty() {
        return Err(format!("{} packages failed to publish", failures.len()));
    }
    Ok(())
}

/// Dependencies first: a build's closure strictly contains each of its dependencies' closures.
fn closure_size(build: &Path) -> usize {
    fs::read(build.join("receipt.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<BuildArtifact>(&bytes).ok())
        .map_or(0, |artifact| artifact.dependencies.len())
}

/// Releases and pushes one build under the key it was built with, or, for a recovered build, the
/// key its verified run qualified it under. Runtime dependencies released earlier in this job
/// are referenced from their releases.
fn release(
    catalog: &PackageCatalog,
    plan: &WorkPlan,
    build: &Path,
    (releases, released): (&Path, &BTreeMap<String, PathBuf>),
    signer: &rootbeer_packaging::Signer,
    public_key: &str,
) -> Result<(String, String, String), String> {
    let receipt = build.join("receipt.json");
    let artifact: BuildArtifact = serde_json::from_slice(
        &fs::read(&receipt).map_err(|error| format!("{}: {error}", receipt.display()))?,
    )
    .map_err(|error| error.to_string())?;
    let package = artifact.package.id();
    let task = plan.task(&package)?;
    let key = match fs::read(build.join("build.json")) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|error| error.to_string())?["built"]["key"]
            .as_str()
            .ok_or("build.json names no built key")?
            .to_string(),
        Err(_) => task.key.clone(),
    };
    let destination = releases.join(&key);
    rootbeer_packaging::release_package(
        catalog,
        &receipt,
        &format!("{}/{}", plan.registry, task.name),
        &destination,
        signer,
        Some(&key),
        released,
    )?;
    let reference = rootbeer_packaging::push_package(&destination, public_key)?;
    Ok((package, key, reference))
}

/// Each downloaded build, found by its receipt, in this run's and the recovered run's groups; a
/// group holding one download has it extracted in place.
fn builds(directory: &Path) -> Result<Vec<PathBuf>, String> {
    let mut builds = Vec::new();
    for group in [directory.join("current"), directory.join("recovered")] {
        if group.join("receipt.json").is_file() {
            builds.push(group);
            continue;
        }
        let Ok(entries) = fs::read_dir(&group) else {
            continue;
        };
        for entry in entries {
            let path = entry.map_err(|error| error.to_string())?.path();
            if path.join("receipt.json").is_file() {
                builds.push(path);
            }
        }
    }
    builds.sort();
    Ok(builds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_each_build_in_both_groups_however_they_were_extracted() {
        let root = tempfile::tempdir().unwrap();
        let receipt = |path: &Path| {
            fs::create_dir_all(path).unwrap();
            fs::write(path.join("receipt.json"), "{}").unwrap();
        };
        receipt(&root.path().join("current"));
        receipt(&root.path().join("recovered/package-a-1"));
        receipt(&root.path().join("recovered/package-b-1"));
        fs::create_dir_all(root.path().join("recovered/empty")).unwrap();
        assert_eq!(
            builds(root.path()).unwrap(),
            [
                root.path().join("current"),
                root.path().join("recovered/package-a-1"),
                root.path().join("recovered/package-b-1"),
            ]
        );
        assert!(builds(&root.path().join("missing")).unwrap().is_empty());
    }
}
