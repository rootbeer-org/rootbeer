use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use ring::signature::{Ed25519KeyPair, KeyPair};
use rootbeer_package::distribution::{read_record, verify_record, SignedPackageRecord};
use rootbeer_package::pdr::{
    DocumentPlatform, DocumentVersion, PackageDocument, PackageKind, Root, RootPackage,
    RootPlatform, ROOT_SCHEMA,
};
use rootbeer_package::{CatalogPackage, CatalogRecipe, PackageCatalog};
use rootbeer_store::hash_bytes;

/// Package, version and system.
type Key = (String, String, String);

/// A signed record as the documents describe it.
#[derive(Debug, Clone, PartialEq)]
struct Published {
    recipe: CatalogRecipe,
    revision: u32,
    record: String,
    published: u64,
}

/// Publishes the signed root and package documents for every approved record, adding new
/// records to those the previous root already carried.
///
/// Nothing is downloaded or rebuilt. A previous record stays published while the catalog still
/// declares its version and platform, even once its recipe, revision or dependencies change, so
/// a failed rebuild never takes a working package away; a new record for it replaces it.
/// [`coverage`] reports the ones still waiting for that replacement.
pub fn publish_records(
    catalog: &PackageCatalog,
    references: &[String],
    site: &Path,
    key_der: &[u8],
    public_key: &str,
) -> Result<usize, String> {
    catalog.validate()?;
    let key =
        Ed25519KeyPair::from_pkcs8(key_der).map_err(|_| "invalid Ed25519 PKCS#8 signing key")?;
    if key.public_key().as_ref() != rootbeer_catalog::decode_hex::<32>(public_key)? {
        return Err("signing key does not match the PDR verification key".into());
    }

    let current = site.join("current.json");
    let previous = if current.exists() {
        let bytes = fs::read(&current).map_err(|error| error.to_string())?;
        let previous = Root::from_bytes(&bytes, public_key)?;
        if let Some(name) = previous.unreadable.keys().next() {
            return Err(format!(
                "{name}: the previous root was published by a newer engine than this one"
            ));
        }
        Some(previous)
    } else {
        None
    };
    let mut published = match &previous {
        Some(previous) => retained(catalog, &previous_documents(previous, site)?),
        None => BTreeMap::new(),
    };
    for entry in published.values() {
        let path = site.join("records").join(format!("{}.json", entry.record));
        if !path.is_file() {
            return Err(format!(
                "retained record {} is missing from the site",
                entry.record
            ));
        }
    }

    let mut released: BTreeMap<Key, (Published, Vec<u8>)> = BTreeMap::new();
    for reference in references {
        let bytes = read_record(reference)?;
        let (key, entry) = approve(catalog, &bytes, public_key)?;
        if released
            .get(&key)
            .is_some_and(|(other, _)| other.record != entry.record)
        {
            return Err(format!(
                "{}@{} {}: conflicting records in one publication",
                key.0, key.1, key.2
            ));
        }
        released.insert(key, (entry, bytes));
    }
    for (key, (entry, _)) in &released {
        published.insert(key.clone(), entry.clone());
    }
    if published.is_empty() {
        return Err("a PDR publishes at least one record".into());
    }

    let sequence = match &previous {
        Some(previous) => previous
            .sequence
            .checked_add(1)
            .ok_or("PDR sequence overflow")?,
        None => 1,
    };
    let (mut root, documents) = assemble(catalog, previous.as_ref(), &published, sequence)?;
    root.signature = key
        .sign(&root.signing_message()?)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let bytes = serde_json::to_vec(&root).map_err(|error| error.to_string())?;
    Root::from_bytes(&bytes, public_key)?;

    for directory in ["records", "packages", "roots"] {
        fs::create_dir_all(site.join(directory)).map_err(|error| error.to_string())?;
    }
    for (entry, record) in released.values() {
        fs::write(
            site.join("records").join(format!("{}.json", entry.record)),
            record,
        )
        .map_err(|error| error.to_string())?;
    }
    for (digest, document) in &documents {
        fs::write(
            site.join("packages").join(format!("{digest}.json")),
            document,
        )
        .map_err(|error| error.to_string())?;
    }
    fs::write(
        site.join("roots")
            .join(format!("{}.json", hash_bytes(&bytes))),
        &bytes,
    )
    .map_err(|error| error.to_string())?;
    let temporary = site.join("current.json.tmp");
    fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    fs::rename(temporary, current).map_err(|error| error.to_string())?;
    Ok(root
        .packages
        .values()
        .map(|package| package.platforms.len())
        .sum())
}

/// Verifies one released record against the catalog that is about to publish it.
fn approve(
    catalog: &PackageCatalog,
    bytes: &[u8],
    public_key: &str,
) -> Result<(Key, Published), String> {
    let signed: SignedPackageRecord =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    let claimed: rootbeer_package::distribution::PackageRecord =
        serde_json::from_str(signed.record.get()).map_err(|error| error.to_string())?;
    let id = claimed.artifact.package.id();
    let record = verify_record(bytes, public_key, &id, &claimed.system)?;
    let package = &record.artifact.package;
    let version = catalog
        .packages
        .get(&package.name)
        .and_then(|entry| entry.versions.get(&package.version))
        .ok_or_else(|| format!("{id}: not in the catalog"))?;
    if version.platforms.get(&record.system) != Some(&record.recipe)
        || version.revision != record.revision
    {
        return Err(format!(
            "{id} {}: signed record differs from the approved recipe",
            record.system
        ));
    }
    Ok((
        (
            package.name.clone(),
            package.version.clone(),
            record.system.clone(),
        ),
        Published {
            recipe: record.recipe.clone(),
            revision: record.revision,
            record: hash_bytes(bytes),
            published: record.published,
        },
    ))
}

/// Reads the documents the previous root pins, checking each against its digest.
fn previous_documents(
    previous: &Root,
    site: &Path,
) -> Result<BTreeMap<String, PackageDocument>, String> {
    let mut documents = BTreeMap::new();
    for (name, package) in &previous.packages {
        let path = site
            .join("packages")
            .join(format!("{}.json", package.document));
        let bytes = fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        if hash_bytes(&bytes) != package.document {
            return Err(format!(
                "{name}: package document does not match its digest"
            ));
        }
        let document = PackageDocument::from_bytes(&bytes)?;
        if !document.unreadable.is_empty() {
            return Err(format!(
                "{name}: the previous document was published by a newer engine than this one"
            ));
        }
        package.check_document(name, &document)?;
        documents.insert(name.clone(), document);
    }
    Ok(documents)
}

/// Revision and recipe digest of each package a build was compiled with.
type Closure = BTreeMap<String, (u32, String)>;

/// Previous records whose version and platform the catalog still declares. Each record pins its
/// exact runtime dependencies, so it stays installable after the catalog moves on.
fn retained(
    catalog: &PackageCatalog,
    documents: &BTreeMap<String, PackageDocument>,
) -> BTreeMap<Key, Published> {
    let mut retained = BTreeMap::new();
    for (name, document) in documents {
        for (version, entry) in &document.versions {
            let Some(approved) = catalog
                .packages
                .get(name)
                .and_then(|package| package.versions.get(version))
            else {
                continue;
            };
            for (system, platform) in &entry.platforms {
                if !approved.platforms.contains_key(system) {
                    continue;
                }
                retained.insert(
                    (name.clone(), version.clone(), system.clone()),
                    Published {
                        recipe: platform.recipe.clone(),
                        revision: entry.revision,
                        record: platform.record.clone(),
                        published: platform.published,
                    },
                );
            }
        }
    }
    retained
}

/// Whether a published record is what the catalog approves now: the same recipe and revision,
/// built with the same dependencies. `built_with` reads a record's closure by its digest.
///
/// Builder identity is deliberately not compared: an engine change alone keeps a package's own
/// record current, so it keeps its dependents' too.
fn is_current(
    catalog: &PackageCatalog,
    key: &Key,
    entry: &Published,
    built_with: impl Fn(&str) -> Result<Closure, String>,
) -> Result<bool, String> {
    let (name, version, system) = key;
    let Some(approved) = catalog
        .packages
        .get(name)
        .and_then(|package| package.versions.get(version))
    else {
        return Ok(false);
    };
    if approved.revision != entry.revision || approved.platforms.get(system) != Some(&entry.recipe)
    {
        return Ok(false);
    }
    let has_dependencies = entry
        .recipe
        .build
        .as_ref()
        .is_some_and(|build| !build.dependencies.is_empty());
    if !has_dependencies {
        return Ok(true);
    }

    let current: Closure = crate::package_plan::dependency_inputs(
        catalog,
        &format!("{name}@{version}"),
        system,
        &Default::default(),
    )?
    .into_iter()
    .map(|(id, inputs)| (id, (inputs.revision, inputs.recipe_sha256)))
    .collect();
    Ok(built_with(&entry.record)? == current)
}

/// Platforms the catalog declares that the published PDR lacks or serves from an outdated record.
#[derive(Debug, Default, PartialEq)]
pub struct Coverage {
    /// `name@version system` with no published record.
    pub missing: Vec<String>,
    /// `name@version system` still served from a record built before a recipe, revision or
    /// dependency change.
    pub stale: Vec<String>,
}

impl Coverage {
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty() && self.stale.is_empty()
    }
}

/// Compares the PDR at `site` against every version and platform the catalog declares.
pub fn coverage(
    catalog: &PackageCatalog,
    site: &Path,
    public_key: &str,
) -> Result<Coverage, String> {
    catalog.validate()?;
    let bytes = fs::read(site.join("current.json")).map_err(|error| error.to_string())?;
    let root = Root::from_bytes(&bytes, public_key)?;
    if let Some(name) = root.unreadable.keys().next() {
        return Err(format!(
            "{name}: the root was published by a newer engine than this one"
        ));
    }
    let published = retained(catalog, &previous_documents(&root, site)?);

    let mut coverage = Coverage::default();
    for (name, package) in &catalog.packages {
        for (version, approved) in &package.versions {
            for system in approved.platforms.keys() {
                let key = (name.clone(), version.clone(), system.clone());
                let label = format!("{name}@{version} {system}");
                match published.get(&key) {
                    None => coverage.missing.push(label),
                    Some(entry) => {
                        if !is_current(catalog, &key, entry, |digest| record_closure(site, digest))?
                        {
                            coverage.stale.push(label);
                        }
                    }
                }
            }
        }
    }
    Ok(coverage)
}

/// The closure a published record was built with, read from the site that already holds it.
fn record_closure(site: &Path, digest: &str) -> Result<Closure, String> {
    let bytes = fs::read(site.join("records").join(format!("{digest}.json")))
        .map_err(|error| format!("record {digest}: {error}"))?;
    let signed: SignedPackageRecord =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    let record: rootbeer_package::distribution::PackageRecord =
        serde_json::from_str(signed.record.get()).map_err(|error| error.to_string())?;
    let rootbeer_package::distribution::PackageProvenance::Source(provenance) = record.provenance
    else {
        return Ok(Closure::new());
    };
    Ok(provenance
        .dependencies
        .into_iter()
        .map(|(id, built)| (id, (built.inputs.revision, built.inputs.recipe_sha256)))
        .collect())
}

/// Builds the unsigned root and every package document, keyed by digest.
///
/// A platform lists its authored default when that version is published. Otherwise the
/// version the previous root listed stands in while it is still published, so a pending
/// release never takes a working package away; with neither, the platform is absent.
fn assemble(
    catalog: &PackageCatalog,
    previous: Option<&Root>,
    published: &BTreeMap<Key, Published>,
    sequence: u64,
) -> Result<(Root, BTreeMap<String, Vec<u8>>), String> {
    let mut root = Root {
        schema: ROOT_SCHEMA,
        sequence,
        packages: BTreeMap::new(),
        unreadable: BTreeMap::new(),
        signature: String::new(),
    };
    let mut documents = BTreeMap::new();
    for (name, package) in &catalog.packages {
        let document = document(package, published);
        if document.versions.is_empty() {
            continue;
        }
        let is_published = |system: &str, version: &str| {
            document
                .versions
                .get(version)
                .is_some_and(|entry| entry.platforms.contains_key(system))
        };
        let mut platforms = BTreeMap::new();
        for (system, authored) in &package.default_versions {
            let listed = previous
                .and_then(|previous| previous.packages.get(name))
                .and_then(|entry| entry.platforms.get(system))
                .map(|platform| platform.version.as_str());
            let Some(version) = [Some(authored.as_str()), listed]
                .into_iter()
                .flatten()
                .find(|version| is_published(system, version))
            else {
                continue;
            };
            let recipe = &document.versions[version].platforms[system].recipe;
            platforms.insert(system.clone(), platform(version, recipe));
        }
        if platforms.is_empty() {
            continue;
        }

        let mut licenses = platforms
            .values()
            .map(|platform| &document.versions[&platform.version].license);
        let license = licenses.next().expect("platforms is not empty").clone();
        if licenses.any(|other| *other != license) {
            return Err(format!(
                "{name}: default versions disagree on their license"
            ));
        }
        let times = document
            .versions
            .values()
            .flat_map(|entry| entry.platforms.values())
            .map(|platform| platform.published);
        let added = times.clone().min().expect("document is not empty");
        let updated = times.max().expect("document is not empty");

        let min_engine_level = min_engine_level(name, package, &document)?;

        let bytes =
            rootbeer_catalog::canonical_json(&document).map_err(|error| error.to_string())?;
        let digest = hash_bytes(&bytes);
        documents.insert(digest.clone(), bytes);
        root.packages.insert(
            name.clone(),
            RootPackage {
                aliases: package.aliases.clone(),
                description: package.description.clone(),
                homepage: package.homepage.clone(),
                license,
                maintainers: package.recipe_maintainers.clone(),
                min_engine_level,
                added,
                updated,
                platforms,
                document: digest,
            },
        );
    }
    root.validate()?;
    Ok((root, documents))
}

/// The authored level, raised to what any published record needs. Rootbeer itself is never
/// raised: `rb self-update` is how an older client reaches a newer level.
fn min_engine_level(
    name: &str,
    package: &CatalogPackage,
    document: &PackageDocument,
) -> Result<Option<u32>, String> {
    let has_runtime = document
        .versions
        .values()
        .flat_map(|entry| entry.platforms.values())
        .flat_map(|platform| platform.recipe.build.iter())
        .flat_map(|build| &build.dependencies)
        .any(|dependency| dependency.kind().is_runtime());
    let level = if has_runtime {
        let authored = package.min_engine_level.unwrap_or_default();
        Some(authored.max(rootbeer_package::repository::RUNTIME_DEPENDENCIES_LEVEL))
    } else {
        package.min_engine_level
    };

    if name == rootbeer_package::self_update::PACKAGE && level.is_some_and(|level| level > 1) {
        return Err(format!(
            "{name}: must stay installable by every client, so it cannot require a newer rb"
        ));
    }
    Ok(level)
}

/// A version carries one revision, and clients refuse records of any other. Once a record of
/// the approved revision exists, platforms still on an older one drop out; until then the
/// version keeps the revision its retained records share.
fn document(package: &CatalogPackage, published: &BTreeMap<Key, Published>) -> PackageDocument {
    let mut versions = BTreeMap::new();
    for (version, approved) in &package.versions {
        let entries: Vec<(&String, &Published)> = approved
            .platforms
            .keys()
            .filter_map(|system| {
                let key = (package.name.clone(), version.clone(), system.clone());
                published.get(&key).map(|entry| (system, entry))
            })
            .collect();
        let revision = if entries
            .iter()
            .any(|(_, entry)| entry.revision == approved.revision)
        {
            approved.revision
        } else {
            let Some(newest) = entries.iter().map(|(_, entry)| entry.revision).max() else {
                continue;
            };
            newest
        };
        let platforms: BTreeMap<_, _> = entries
            .into_iter()
            .filter(|(_, entry)| entry.revision == revision)
            .map(|(system, entry)| {
                (
                    system.clone(),
                    DocumentPlatform {
                        recipe: entry.recipe.clone(),
                        record: entry.record.clone(),
                        published: entry.published,
                    },
                )
            })
            .collect();
        versions.insert(
            version.clone(),
            DocumentVersion {
                license: approved.license.clone(),
                revision,
                platforms,
            },
        );
    }
    PackageDocument {
        name: package.name.clone(),
        versions,
        unreadable: Default::default(),
    }
}

fn platform(version: &str, recipe: &CatalogRecipe) -> RootPlatform {
    let commands: Vec<String> = recipe.bins.names().into_iter().cloned().collect();
    let kind = if !commands.is_empty() {
        PackageKind::Command
    } else if !recipe.apps.is_empty() {
        PackageKind::App
    } else {
        PackageKind::Library
    };
    RootPlatform {
        version: version.to_string(),
        kind,
        commands,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rootbeer_package::PackageDefinition;

    const MAC: &str = "aarch64-macos";
    const LINUX: &str = "x86_64-linux";

    /// `tool` builds versions 1 and 2 on macOS and Linux, defaulting to 2.
    fn catalog() -> PackageCatalog {
        let digest = "a".repeat(64);
        let definition = PackageDefinition::from_lua(&format!(
            r#"return {{
                name = "tool", description = "A tool", homepage = "https://example.com",
                default_license = "MIT",
                prebuilt = {{ github = "owner/tool", tag = "v{{version}}", asset = "tool-{{target}}.tar.gz" }},
                outputs = {{ bins = {{ "tool" }}, checks = {{ {{ "tool", "--version" }} }} }},
                platforms = {{
                    ["{MAC}"] = {{ target = "mac", default_version = "2" }},
                    ["{LINUX}"] = {{ target = "linux", default_version = "2" }},
                }},
                versions = {{
                    ["1"] = {{ digests = {{ ["{MAC}"] = "{digest}", ["{LINUX}"] = "{digest}" }} }},
                    ["2"] = {{ digests = {{ ["{MAC}"] = "{digest}", ["{LINUX}"] = "{digest}" }} }},
                }},
            }}"#
        ))
        .unwrap();
        PackageCatalog::from_definitions(&BTreeMap::from([("tool".into(), definition)])).unwrap()
    }

    fn no_closure(digest: &str) -> Result<Closure, String> {
        panic!("{digest}: a package without build dependencies never reads its record")
    }

    fn key(version: &str, system: &str) -> Key {
        ("tool".into(), version.into(), system.into())
    }

    /// Records for `version` on each system, published at `time`.
    fn release(
        catalog: &PackageCatalog,
        version: &str,
        systems: &[&str],
        time: u64,
    ) -> BTreeMap<Key, Published> {
        let approved = &catalog.packages["tool"].versions[version];
        systems
            .iter()
            .map(|system| {
                (
                    key(version, system),
                    Published {
                        recipe: approved.platforms[*system].clone(),
                        revision: approved.revision,
                        record: hash_bytes(format!("{version} {system}").as_bytes()),
                        published: time,
                    },
                )
            })
            .collect()
    }

    fn documents_of(
        root: &Root,
        documents: &BTreeMap<String, Vec<u8>>,
    ) -> BTreeMap<String, PackageDocument> {
        root.packages
            .iter()
            .map(|(name, package)| {
                let document = PackageDocument::from_bytes(&documents[&package.document]).unwrap();
                package.check_document(name, &document).unwrap();
                (name.clone(), document)
            })
            .collect()
    }

    #[test]
    fn a_platform_lists_its_default_once_published_and_its_previous_version_until_then() {
        let catalog = catalog();
        let mut published = release(&catalog, "1", &[MAC, LINUX], 100);
        let (first, _) = assemble(&catalog, None, &published, 1).unwrap();
        assert!(
            first.packages.is_empty(),
            "version 1 was never the default and nothing listed it"
        );

        published.extend(release(&catalog, "2", &[MAC], 200));
        let (second, _) = assemble(&catalog, None, &published, 2).unwrap();
        let platforms = &second.packages["tool"].platforms;
        assert_eq!(platforms[MAC].version, "2");
        assert!(!platforms.contains_key(LINUX));

        let mut listed = second.clone();
        listed.packages.get_mut("tool").unwrap().platforms.insert(
            LINUX.into(),
            platform(
                "1",
                &catalog.packages["tool"].versions["1"].platforms[LINUX],
            ),
        );
        let (third, _) = assemble(&catalog, Some(&listed), &published, 3).unwrap();
        let platforms = &third.packages["tool"].platforms;
        assert_eq!(platforms[MAC].version, "2");
        assert_eq!(
            platforms[LINUX].version, "1",
            "Linux keeps what it had until its version 2 record arrives"
        );
    }

    #[test]
    fn documents_carry_every_published_version_and_its_times() {
        let catalog = catalog();
        let mut published = release(&catalog, "1", &[MAC, LINUX], 100);
        published.extend(release(&catalog, "2", &[MAC, LINUX], 200));
        let (root, documents) = assemble(&catalog, None, &published, 1).unwrap();

        let package = &root.packages["tool"];
        assert_eq!((package.added, package.updated), (100, 200));
        assert_eq!(package.platforms[MAC].kind, PackageKind::Command);
        assert_eq!(package.platforms[MAC].commands, ["tool"]);
        let document = &documents_of(&root, &documents)["tool"];
        assert_eq!(document.versions.len(), 2);
        assert_eq!(document.versions["1"].platforms[LINUX].published, 100);

        let (again, rebuilt) = assemble(&catalog, None, &published, 1).unwrap();
        assert_eq!(again, root);
        assert_eq!(
            rebuilt, documents,
            "unchanged inputs produce identical documents"
        );
    }

    #[test]
    fn a_changed_recipe_keeps_its_previous_record_until_a_new_one_replaces_it() {
        let catalog = catalog();
        let published = release(&catalog, "2", &[MAC, LINUX], 100);
        let (root, documents) = assemble(&catalog, None, &published, 1).unwrap();
        let documents = documents_of(&root, &documents);
        assert_eq!(retained(&catalog, &documents), published);

        let mut changed = catalog.clone();
        changed
            .packages
            .get_mut("tool")
            .unwrap()
            .versions
            .get_mut("2")
            .unwrap()
            .platforms
            .get_mut(MAC)
            .unwrap()
            .checks
            .push(vec!["tool".into(), "--help".into()]);
        let kept = retained(&changed, &documents);
        assert_eq!(kept, published, "a pending rebuild takes nothing away");
        let mac = key("2", MAC);
        let linux = key("2", LINUX);
        assert!(!is_current(&changed, &mac, &kept[&mac], no_closure).unwrap());
        assert!(is_current(&changed, &linux, &kept[&linux], no_closure).unwrap());

        let (root, documents) = assemble(&changed, None, &kept, 2).unwrap();
        let document = &documents_of(&root, &documents)["tool"];
        assert_eq!(
            document.versions["2"].platforms[MAC].recipe,
            catalog.packages["tool"].versions["2"].platforms[MAC],
            "the previous record keeps the recipe it was built from"
        );
    }

    #[test]
    fn a_revision_bump_keeps_the_previous_revision_until_any_platform_reaches_the_new_one() {
        let catalog = catalog();
        let published = release(&catalog, "2", &[MAC, LINUX], 100);
        let (root, documents) = assemble(&catalog, None, &published, 1).unwrap();
        let documents = documents_of(&root, &documents);

        let mut bumped = catalog.clone();
        bumped
            .packages
            .get_mut("tool")
            .unwrap()
            .versions
            .get_mut("2")
            .unwrap()
            .revision = 2;
        let mut kept = retained(&bumped, &documents);
        let (root, documents) = assemble(&bumped, None, &kept, 2).unwrap();
        let version = &documents_of(&root, &documents)["tool"].versions["2"];
        assert_eq!(version.revision, 1);
        assert_eq!(version.platforms.len(), 2);
        assert!(!is_current(&bumped, &key("2", MAC), &kept[&key("2", MAC)], no_closure).unwrap());

        kept.extend(release(&bumped, "2", &[MAC], 200));
        let (root, documents) = assemble(&bumped, None, &kept, 3).unwrap();
        let version = &documents_of(&root, &documents)["tool"].versions["2"];
        assert_eq!(version.revision, 2);
        assert_eq!(
            version.platforms.keys().collect::<Vec<_>>(),
            [MAC],
            "clients refuse a record of another revision than its version's"
        );
    }

    #[test]
    fn coverage_reports_missing_and_stale_platforms() {
        use ring::signature::{Ed25519KeyPair, KeyPair};

        let site = tempfile::tempdir().unwrap();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let public_key: String = key_pair
            .public_key()
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let catalog = catalog();
        let (mut root, documents) = assemble(
            &catalog,
            None,
            &release(&catalog, "2", &[MAC, LINUX], 100),
            1,
        )
        .unwrap();
        root.signature = key_pair
            .sign(&root.signing_message().unwrap())
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        fs::create_dir_all(site.path().join("packages")).unwrap();
        for (digest, document) in &documents {
            fs::write(
                site.path().join("packages").join(format!("{digest}.json")),
                document,
            )
            .unwrap();
        }
        fs::write(
            site.path().join("current.json"),
            serde_json::to_vec(&root).unwrap(),
        )
        .unwrap();

        let mut changed = catalog.clone();
        changed
            .packages
            .get_mut("tool")
            .unwrap()
            .versions
            .get_mut("2")
            .unwrap()
            .platforms
            .get_mut(LINUX)
            .unwrap()
            .checks
            .push(vec!["tool".into(), "--help".into()]);
        let coverage = coverage(&changed, site.path(), &public_key).unwrap();
        assert_eq!(
            coverage,
            Coverage {
                missing: vec![format!("tool@1 {MAC}"), format!("tool@1 {LINUX}")],
                stale: vec![format!("tool@2 {LINUX}")],
            }
        );
        assert!(!coverage.is_complete());
    }

    #[test]
    fn default_versions_must_agree_on_a_license() {
        let mut catalog = catalog();
        catalog
            .packages
            .get_mut("tool")
            .unwrap()
            .default_versions
            .insert(LINUX.into(), "1".into());
        catalog
            .packages
            .get_mut("tool")
            .unwrap()
            .versions
            .get_mut("1")
            .unwrap()
            .license = "Apache-2.0".into();
        let mut published = release(&catalog, "1", &[LINUX], 100);
        published.extend(release(&catalog, "2", &[MAC], 100));
        let error = assemble(&catalog, None, &published, 1).unwrap_err();
        assert!(error.contains("license"), "{error}");
    }

    #[test]
    fn refuses_to_build_on_a_root_it_cannot_fully_read() {
        use ring::signature::{Ed25519KeyPair, KeyPair};

        let site = tempfile::tempdir().unwrap();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let hex = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        let packages = serde_json::json!({ "newer": { "description": "From a newer engine" } });
        let message = rootbeer_package::pdr::signing_message(1, &packages).unwrap();
        let root = serde_json::json!({
            "schema": ROOT_SCHEMA,
            "sequence": 1,
            "packages": packages,
            "signature": hex(key.sign(&message).as_ref()),
        });
        fs::write(
            site.path().join("current.json"),
            serde_json::to_vec(&root).unwrap(),
        )
        .unwrap();

        let error = publish_records(
            &catalog(),
            &[],
            site.path(),
            pkcs8.as_ref(),
            &hex(key.public_key().as_ref()),
        )
        .unwrap_err();
        assert!(error.contains("newer engine"), "{error}");
    }

    /// `app` is built with `lib`; both are source builds published on Linux.
    fn dependent_catalog(lib_configure: &str) -> PackageCatalog {
        linked_catalog(
            lib_configure,
            r#"{ package = "lib", version = "1", kind = "all" }"#,
        )
    }

    /// `app` depends on `lib` through `dependency`, a Lua dependency entry.
    fn linked_catalog(lib_configure: &str, dependency: &str) -> PackageCatalog {
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
            &format!(r#"{{ backend = "autotools", dependencies = {{ {dependency} }} }}"#),
        );
        PackageCatalog::from_definitions(&BTreeMap::from([
            ("lib".into(), lib),
            ("app".into(), app),
        ]))
        .unwrap()
    }

    #[test]
    fn a_dependency_change_makes_what_was_built_with_it_stale_but_keeps_it_published() {
        let catalog = dependent_catalog("--static");
        let published: BTreeMap<Key, Published> = ["lib", "app"]
            .into_iter()
            .map(|name| {
                let approved = &catalog.packages[name].versions["1"];
                let key = (name.to_string(), "1".to_string(), LINUX.to_string());
                let entry = Published {
                    recipe: approved.platforms[LINUX].clone(),
                    revision: approved.revision,
                    record: hash_bytes(name.as_bytes()),
                    published: 100,
                };
                (key, entry)
            })
            .collect();
        let (root, documents) = assemble(&catalog, None, &published, 1).unwrap();
        let documents = documents_of(&root, &documents);

        let built_with = |catalog: &PackageCatalog| {
            let closure: Closure = crate::package_plan::dependency_inputs(
                catalog,
                "app@1",
                LINUX,
                &Default::default(),
            )
            .unwrap()
            .into_iter()
            .map(|(id, inputs)| (id, (inputs.revision, inputs.recipe_sha256)))
            .collect();
            move |_: &str| Ok(closure.clone())
        };
        let app = ("app".to_string(), "1".to_string(), LINUX.to_string());
        assert_eq!(retained(&catalog, &documents), published);
        assert!(is_current(&catalog, &app, &published[&app], built_with(&catalog)).unwrap());

        let changed = dependent_catalog("--shared");
        let kept = retained(&changed, &documents);
        assert_eq!(
            kept, published,
            "app's record pins the lib it was built with"
        );
        assert!(
            !is_current(&changed, &app, &kept[&app], built_with(&catalog)).unwrap(),
            "lib changed, and app was built with the old lib"
        );
        assert!(is_current(&changed, &app, &kept[&app], built_with(&changed)).unwrap());
    }

    fn published_all(catalog: &PackageCatalog) -> BTreeMap<Key, Published> {
        catalog
            .packages
            .keys()
            .map(|name| {
                let approved = &catalog.packages[name].versions["1"];
                let key = (name.to_string(), "1".to_string(), LINUX.to_string());
                let entry = Published {
                    recipe: approved.platforms[LINUX].clone(),
                    revision: approved.revision,
                    record: hash_bytes(name.as_bytes()),
                    published: 100,
                };
                (key, entry)
            })
            .collect()
    }

    #[test]
    fn only_packages_with_runtime_dependencies_need_a_newer_rb() {
        let runtime = r#"{ package = "lib", version = "1", kind = "link_runtime" }"#;
        for (dependency, expected) in [
            (r#"{ package = "lib", version = "1", kind = "all" }"#, None),
            (runtime, Some(2)),
        ] {
            let catalog = linked_catalog("--shared", dependency);
            let (root, _) = assemble(&catalog, None, &published_all(&catalog), 1).unwrap();
            assert_eq!(root.packages["app"].min_engine_level, expected);
            assert_eq!(root.packages["lib"].min_engine_level, None);
        }

        let mut catalog = linked_catalog("--shared", runtime);
        let mut rootbeer = catalog.packages.remove("app").unwrap();
        rootbeer.name = rootbeer_package::self_update::PACKAGE.into();
        catalog.packages.insert(rootbeer.name.clone(), rootbeer);
        let error = assemble(&catalog, None, &published_all(&catalog), 1).unwrap_err();
        assert!(error.contains("cannot require a newer rb"), "{error}");
    }
}
