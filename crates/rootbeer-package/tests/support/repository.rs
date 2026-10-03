use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use ring::signature::{Ed25519KeyPair, KeyPair};
use rootbeer_package::pdr::{
    DocumentPlatform, DocumentVersion, PackageDocument, PackageKind, Root, RootPackage,
    RootPlatform, ROOT_SCHEMA,
};
use rootbeer_package::{PackageCatalog, RepositoryPin};
use rootbeer_store_legacy::hash_bytes;

/// Publishes every recipe in `catalog` as a signed repository site in `directory`.
///
/// Record digests are named but no records are written, so this serves what resolves through
/// package documents alone (source builds and their dependency closures), not installs of
/// published packages.
pub fn publish(catalog: &PackageCatalog, directory: &Path) -> RepositoryPin {
    for subdirectory in ["packages", "roots"] {
        fs::create_dir_all(directory.join(subdirectory)).unwrap();
    }
    let mut packages = BTreeMap::new();
    for (name, package) in &catalog.packages {
        let document = PackageDocument {
            name: name.clone(),
            versions: package
                .versions
                .iter()
                .map(|(version, entry)| {
                    let platforms = entry
                        .platforms
                        .iter()
                        .map(|(system, recipe)| {
                            let record =
                                hash_bytes(format!("{name}@{version} {system}").as_bytes());
                            let platform = DocumentPlatform {
                                recipe: recipe.clone(),
                                record,
                                published: 1,
                            };
                            (system.clone(), platform)
                        })
                        .collect();
                    let version_entry = DocumentVersion {
                        license: entry.license.clone(),
                        revision: entry.revision,
                        platforms,
                    };
                    (version.clone(), version_entry)
                })
                .collect(),
            unreadable: Default::default(),
        };
        let platforms: BTreeMap<_, _> = package
            .default_versions
            .iter()
            .filter_map(|(system, version)| {
                let recipe = package.versions.get(version)?.platforms.get(system)?;
                let commands: Vec<String> = recipe.bins.names().into_iter().cloned().collect();
                let kind = match (commands.is_empty(), recipe.apps.is_empty()) {
                    (false, _) => PackageKind::Command,
                    (true, false) => PackageKind::App,
                    (true, true) => PackageKind::Library,
                };
                let platform = RootPlatform {
                    version: version.clone(),
                    kind,
                    commands,
                };
                Some((system.clone(), platform))
            })
            .collect();
        let Some(first) = platforms.values().next() else {
            continue;
        };
        let license = package.versions[&first.version].license.clone();
        let bytes = rootbeer_catalog::canonical_json(&document).unwrap();
        let digest = hash_bytes(&bytes);
        fs::write(
            directory.join("packages").join(format!("{digest}.json")),
            bytes,
        )
        .unwrap();
        packages.insert(
            name.clone(),
            RootPackage {
                aliases: package.aliases.clone(),
                description: package.description.clone(),
                homepage: package.homepage.clone(),
                license,
                maintainers: Vec::new(),
                min_engine_level: None,
                added: 1,
                updated: 1,
                platforms,
                document: digest,
            },
        );
    }

    let key = Ed25519KeyPair::from_seed_unchecked(&[9; 32]).unwrap();
    let hex = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    let mut root = Root {
        schema: ROOT_SCHEMA,
        sequence: 1,
        packages,
        unreadable: BTreeMap::new(),
        signature: String::new(),
    };
    root.signature = hex(key.sign(&root.signing_message().unwrap()).as_ref());
    let bytes = serde_json::to_vec(&root).unwrap();
    let digest = hash_bytes(&bytes);
    fs::write(
        directory.join("roots").join(format!("{digest}.json")),
        &bytes,
    )
    .unwrap();
    fs::write(directory.join("current.json"), &bytes).unwrap();
    RepositoryPin {
        url: format!("file://{}/current.json", directory.display()),
        public_key: hex(key.public_key().as_ref()),
        root: digest,
    }
}
