use super::*;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::Ed25519KeyPair;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::num::NonZeroU64;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use tough::editor::RepositoryEditor;
use tough::editor::signed::{PathExists, SignedRole};
use tough::key_source::{KeySource, LocalKeySource};
use tough::schema::{KeyHolder, RoleKeys, RoleType, Target};

struct Fixture {
    directory: tempfile::TempDir,
    runtime: tokio::runtime::Runtime,
}

impl Fixture {
    fn new() -> Fixture {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        Fixture {
            directory: tempfile::tempdir().unwrap(),
            runtime,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    fn key(&self, name: &str) -> PathBuf {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let path = self.path(name);
        fs::write(&path, pkcs8.as_ref()).unwrap();
        path
    }

    fn root(&self, version: u64, root_key: &Path, online: &Path) -> Vec<u8> {
        self.root_expiring(version, root_key, online, days(365))
    }

    fn root_expiring(
        &self,
        version: u64,
        root_key: &Path,
        online: &Path,
        expires: jiff::Timestamp,
    ) -> Vec<u8> {
        self.runtime.block_on(async {
            let mut keys = HashMap::new();
            let mut ids = Vec::new();
            for path in [root_key, online] {
                let key = source(path).as_sign().await.unwrap().tuf_key();
                let id = key.key_id().unwrap();
                keys.insert(id.clone(), key);
                ids.push(id);
            }

            let role =
                |id: &tough::schema::decoded::Decoded<tough::schema::decoded::Hex>| RoleKeys {
                    keyids: vec![id.clone()],
                    threshold: NonZeroU64::MIN,
                    _extra: HashMap::new(),
                };

            let (root_id, online_id) = (&ids[0], &ids[1]);
            let roles = [
                (RoleType::Root, role(root_id)),
                (RoleType::Snapshot, role(online_id)),
                (RoleType::Targets, role(online_id)),
                (RoleType::Timestamp, role(online_id)),
            ];

            let root = Root {
                spec_version: "1.0.0".into(),
                consistent_snapshot: true,
                version: NonZeroU64::new(version).unwrap(),
                expires,
                keys,
                roles: HashMap::from(roles),
                _extra: HashMap::new(),
            };

            let holder = KeyHolder::Root(root.clone());
            let rng = SystemRandom::new();
            let signed = SignedRole::new(root, &holder, &[source(root_key)], &rng)
                .await
                .unwrap();
            fs::write(self.path("root.json"), signed.buffer()).unwrap();
            signed.buffer().clone()
        })
    }

    fn publish(
        &self,
        entries: &[(&str, Vec<u8>)],
        version: u64,
        key: &Path,
        timestamp: jiff::Timestamp,
    ) {
        self.runtime.block_on(async {
            let version = NonZeroU64::new(version).unwrap();
            let mut editor = RepositoryEditor::new(self.path("root.json")).await.unwrap();
            editor
                .snapshot_version(version)
                .snapshot_expires(days(30))
                .timestamp_version(version)
                .timestamp_expires(timestamp);
            editor.targets_version(version).unwrap();
            editor.targets_expires(days(30)).unwrap();

            let staged = self.path("staged");
            fs::create_dir_all(&staged).unwrap();

            let mut names = Vec::new();
            for (target, bytes) in entries {
                let file = staged.join(format!("{target}.json"));
                fs::write(&file, bytes).unwrap();
                let name = TargetName::new(format!("{target}.json")).unwrap();
                let target = Target::from_path(&file).await.unwrap();
                editor.add_target(name.clone(), target).unwrap();
                names.push((file, name));
            }

            let repository = self.path("repository");
            let signed = editor.sign(&[source(key)]).await.unwrap();
            signed.write(repository.join("metadata")).await.unwrap();

            let targets = repository.join("targets");
            fs::create_dir_all(&targets).unwrap();
            for (file, name) in &names {
                signed
                    .copy_target(file, &targets, PathExists::Replace, Some(name))
                    .await
                    .unwrap();
            }
        });
    }

    fn url(&self) -> String {
        format!("file://{}", self.path("repository").display())
    }

    fn refresh(&self, root: &[u8]) -> Result<Index, Error> {
        Index::refresh(root, &self.url(), &self.path("datastore"), false)
    }
}

fn source(path: &Path) -> Box<dyn KeySource> {
    Box::new(LocalKeySource {
        path: path.to_path_buf(),
    })
}

fn days(days: i64) -> jiff::Timestamp {
    jiff::Timestamp::now()
        .checked_add(jiff::SignedDuration::from_hours(
            days.checked_mul(24).unwrap(),
        ))
        .unwrap()
}

fn package(name: &str) -> Package {
    let platform = Platform::Aarch64Linux;
    let output = Output {
        key: "a".repeat(32).parse().unwrap(),
        manifest: format!("sha256:{}", "0".repeat(64)),
        bins: BTreeSet::from([name.into()]),
        apps: BTreeMap::new(),
    };

    Package {
        name: name.into(),
        description: format!("{name} compresses"),
        license: "BSD-3-Clause".into(),
        default: BTreeMap::from([(platform, "1.0".into())]),
        versions: BTreeMap::from([("1.0".into(), BTreeMap::from([(platform, output)]))]),
        retired: BTreeMap::new(),
    }
}

fn entry(package: &Package) -> Vec<u8> {
    serde_json::to_vec(package).unwrap()
}

fn is_tuf(result: Result<Index, Error>, expected: fn(&tough::error::Error) -> bool) -> bool {
    matches!(result, Err(Error::Tuf(error)) if expected(&error))
}

/// One key for every role, root version 1, published at version 1.
fn signed(entries: &[(&str, Vec<u8>)]) -> (Fixture, Vec<u8>) {
    let fixture = Fixture::new();
    let key = fixture.key("key");
    let root = fixture.root(1, &key, &key);
    fixture.publish(entries, 1, &key, days(7));
    (fixture, root)
}

#[test]
fn signed_entries_are_read_and_unlisted_ones_are_absent() {
    let zstd = package("zstd");
    let (fixture, root) = signed(&[("zstd", entry(&zstd))]);
    let index = fixture.refresh(&root).unwrap();

    let read = index.package("zstd").unwrap().unwrap();
    assert_eq!(read, zstd);
    assert_eq!(index.package("lz4").unwrap(), None);

    let default = read.output(Platform::Aarch64Linux, None);
    assert!(default.is_some());
    assert_eq!(default, read.output(Platform::Aarch64Linux, Some("1.0")));
    assert_eq!(read.output(Platform::Aarch64Macos, None), None);
    assert_eq!(read.output(Platform::Aarch64Linux, Some("2.0")), None);
}

#[test]
fn entries_are_refused_unless_signed_as_served_and_named_for_themselves() {
    let (fixture, root) = signed(&[
        ("zstd", entry(&package("zstd"))),
        ("lz4", entry(&package("zstd"))),
    ]);

    for item in fs::read_dir(fixture.path("repository/targets")).unwrap() {
        let path = item.unwrap().path();
        if path.to_string_lossy().ends_with(".zstd.json") {
            let forged = String::from_utf8(entry(&package("zstd")))
                .unwrap()
                .replace("1.0", "6.6");
            fs::write(path, forged).unwrap();
        }
    }

    let index = fixture.refresh(&root).unwrap();
    let error = index.package("zstd").unwrap_err();
    assert!(error.to_string().contains("Hash mismatch"), "{error}");

    let error = index.package("lz4").unwrap_err();
    assert!(matches!(&error, Error::Invalid(reason) if reason.ends_with("describes zstd")));
    assert!(matches!(index.package("../zstd"), Err(Error::Name(_))));
}

#[test]
fn unknown_platforms_are_dropped_and_digests_checked() {
    let platform = |name: &str| serde_json::json!({ "key": "a".repeat(32), "manifest": format!("sha256:{}", "0".repeat(64)), "x": name });

    let grown = serde_json::json!({
        "name": "zstd",
        "homepage": "a field this client doesn't know",
        "default": { "aarch64-linux": "1.0", "riscv64-linux": "1.0" },
        "versions": { "1.0": { "aarch64-linux": platform("a"), "riscv64-linux": platform("b") } },
    });

    let read: Package = serde_json::from_value(grown).unwrap();
    assert_eq!(read.default.len(), 1);
    assert_eq!(read.versions["1.0"].len(), 1);

    let mut bad = package("zstd");
    for outputs in bad.versions.values_mut() {
        for output in outputs.values_mut() {
            output.manifest = "sha256:nope".into();
        }
    }

    let error = serde_json::from_slice::<Package>(&entry(&bad)).unwrap_err();
    assert!(
        error.to_string().contains("isn't a sha256 digest"),
        "{error}"
    );

    let escapes = |change: fn(&mut Output)| {
        let mut escaping = package("zstd");
        escaping
            .versions
            .values_mut()
            .flat_map(|outputs| outputs.values_mut())
            .for_each(change);
        serde_json::from_slice::<Package>(&entry(&escaping)).is_err()
    };

    assert!(escapes(|output| {
        output.bins.insert("../zstd".into());
    }));

    assert!(escapes(|output| {
        output.apps.insert("..".into(), "Zstd.app".into());
    }));

    assert!(escapes(|output| {
        output
            .apps
            .insert("Zstd.app".into(), "/Applications/Zstd.app".into());
    }));

    assert!(escapes(|output| {
        output
            .apps
            .insert("Zstd.app".into(), "Contents/../../Zstd.app".into());
    }));

    assert!(escapes(|output| {
        output.apps.insert("Zstd".into(), "Zstd.app".into());
    }));

    assert!(escapes(|output| {
        output.apps.insert(".Zstd.app".into(), "Zstd.app".into());
    }));

    assert!(escapes(|output| {
        output.bins.insert(".".into());
    }));

    assert!(escapes(|output| {
        output.bins.insert(String::new());
    }));

    assert!(escapes(|output| {
        output.bins.insert("zs\0td".into());
    }));

    assert!(!escapes(|output| {
        output
            .apps
            .insert("Zstd.app".into(), "Applications/Zstd.app".into());
    }));
}

#[test]
fn metadata_is_refused_when_expired_older_or_signed_by_an_untrusted_key() {
    let fixture = Fixture::new();
    let key = fixture.key("key");
    let root = fixture.root(1, &key, &key);

    let expired = days(-1);
    fixture.publish(&[], 2, &key, expired);
    let is_expired = |error: &tough::error::Error| {
        matches!(
            error,
            tough::error::Error::ExpiredMetadata {
                role: RoleType::Timestamp,
                ..
            }
        )
    };
    assert!(is_tuf(fixture.refresh(&root), is_expired));

    fixture.publish(&[], 3, &key, days(7));
    fixture.refresh(&root).unwrap();
    fixture.publish(&[], 2, &key, days(7));
    let is_older =
        |error: &tough::error::Error| matches!(error, tough::error::Error::OlderMetadata { .. });
    assert!(is_tuf(fixture.refresh(&root), is_older));

    let other = Fixture::new();
    let stranger = other.key("key");
    let untrusted = other.root(1, &stranger, &stranger);
    assert!(matches!(fixture.refresh(&untrusted), Err(Error::Tuf(_))));
}

#[test]
fn a_revoked_online_key_stays_revoked_after_the_repository_drops_the_rotation() {
    let fixture = Fixture::new();
    let root_key = fixture.key("root");
    let leaked = fixture.key("leaked");
    let replacement = fixture.key("replacement");
    let built_in = fixture.root(1, &root_key, &leaked);
    fixture.publish(&[], 1, &leaked, days(7));
    fixture.refresh(&built_in).unwrap();

    fixture.root(2, &root_key, &replacement);
    fixture.publish(&[], 2, &replacement, days(7));
    fixture.refresh(&built_in).unwrap();

    fs::remove_file(fixture.path("repository/metadata/2.root.json")).unwrap();
    fs::write(fixture.path("root.json"), &built_in).unwrap();
    fixture.publish(&[], 3, &leaked, days(7));
    assert!(matches!(fixture.refresh(&built_in), Err(Error::Tuf(_))));

    let forgetful = Index::refresh(&built_in, &fixture.url(), &fixture.path("fresh"), false);
    assert!(
        forgetful.is_ok(),
        "only the kept root refuses the leaked key"
    );
}

#[test]
fn the_datastore_is_private_and_a_broken_root_in_it_is_refused() {
    let (fixture, root) = signed(&[]);
    let datastore = fixture.path("datastore");
    fixture.refresh(&root).unwrap();

    let mode = |mode| fs::set_permissions(&datastore, fs::Permissions::from_mode(mode)).unwrap();
    mode(0o770);
    assert!(matches!(fixture.refresh(&root), Err(Error::Datastore(_))));
    mode(0o700);

    // A clock that once ran ahead no longer blocks every refresh
    fs::write(
        datastore.join("latest_known_time.json"),
        "\"2999-01-01T00:00:00Z\"",
    )
    .unwrap();
    fixture.refresh(&root).unwrap();

    fs::write(datastore.join("root.json"), "{}").unwrap();
    let error = fixture.refresh(&root).err().unwrap();
    assert!(
        matches!(&error, Error::Datastore(reason) if reason.contains("doesn't verify")),
        "{error}"
    );
}

fn serve(directory: PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();

            let target = line.split(' ').nth(1).unwrap_or_default();
            let path = directory.join(target.trim_start_matches('/'));
            let (status, body) = match fs::read(&path) {
                Ok(body) => (200, body),
                Err(_) => (404, Vec::new()),
            };

            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
        }
    });

    url
}

#[test]
fn http_needs_an_explicit_opt_in() {
    let (fixture, root) = signed(&[("zstd", entry(&package("zstd")))]);
    let url = serve(fixture.path("repository"));
    let datastore = fixture.path("datastore");

    let refused = Index::refresh(&root, &url, &datastore, false);
    assert!(matches!(refused, Err(Error::Tuf(_))));

    let index = Index::refresh(&root, &url, &datastore, true).unwrap();
    assert_eq!(index.package("zstd").unwrap(), Some(package("zstd")));
}

fn ceremony() -> (Fixture, Vec<u8>, PathBuf, PathBuf) {
    ceremony_expiring(days(365))
}

fn ceremony_expiring(expires: jiff::Timestamp) -> (Fixture, Vec<u8>, PathBuf, PathBuf) {
    let fixture = Fixture::new();
    let root_key = fixture.key("root");
    let online = fixture.key("online");
    let root = fixture.root_expiring(1, &root_key, &online, expires);
    let metadata = fixture.path("repository/metadata");
    fs::create_dir_all(&metadata).unwrap();
    fs::write(metadata.join("1.root.json"), &root).unwrap();
    (fixture, root, root_key, online)
}

fn timestamp_version(fixture: &Fixture) -> u64 {
    let path = fixture.path("repository/metadata/timestamp.json");
    let timestamp: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    timestamp["signed"]["version"].as_u64().unwrap()
}

#[test]
fn signing_merges_into_the_index_and_keeps_dropped_versions() {
    let (fixture, root, _, online) = ceremony();
    let repository = fixture.path("repository");
    sign(&repository, &online, &[package("zstd"), package("lz4")]).unwrap();
    let index = fixture.refresh(&root).unwrap();
    assert_eq!(index.package("zstd").unwrap(), Some(package("zstd")));
    drop(index);

    // The catalog moved to 2.0, and 1.0 stays listed for locks that pin it
    let mut newer = package("zstd");
    let outputs = newer.versions.remove("1.0").unwrap();
    newer.versions.insert("2.0".into(), outputs);
    newer
        .default
        .values_mut()
        .for_each(|version| *version = "2.0".into());
    sign(&repository, &online, &[newer.clone()]).unwrap();

    let index = fixture.refresh(&root).unwrap();
    let read = index.package("zstd").unwrap().unwrap();
    assert_eq!(read.default, newer.default);
    assert_eq!(read.versions.keys().collect::<Vec<_>>(), ["1.0", "2.0"]);
    assert_eq!(index.package("lz4").unwrap(), Some(package("lz4")));
    assert_eq!(timestamp_version(&fixture), 2);
}

#[test]
fn re_signing_alone_renews_an_expired_index() {
    let (fixture, root, root_key, online) = ceremony();
    fixture.publish(&[("zstd", entry(&package("zstd")))], 1, &online, days(-1));
    assert!(fixture.refresh(&root).is_err());

    let repository = fixture.path("repository");
    assert!(is_unsigned(sign(&repository, &root_key, &[])));
    sign(&repository, &online, &[]).unwrap();

    let index = fixture.refresh(&root).unwrap();
    assert_eq!(index.package("zstd").unwrap(), Some(package("zstd")));
    assert_eq!(timestamp_version(&fixture), 2);
}

fn is_unsigned(result: Result<(), Error>) -> bool {
    use tough::error::Error::{KeysNotFoundInRoot, SigningKeysNotFound};
    matches!(result, Err(Error::Tuf(error)) if matches!(*error, SigningKeysNotFound { .. } | KeysNotFoundInRoot { .. }))
}

#[test]
fn a_rotated_online_key_signs_and_the_old_one_no_longer_can() {
    let (fixture, root, root_key, online) = ceremony();
    let repository = fixture.path("repository");
    sign(&repository, &online, &[package("zstd")]).unwrap();

    let replacement = fixture.key("replacement");
    let rotated = fixture.root(2, &root_key, &replacement);
    fs::write(repository.join("metadata/2.root.json"), rotated).unwrap();

    assert!(is_unsigned(sign(&repository, &online, &[])));
    sign(&repository, &replacement, &[]).unwrap();
    let index = fixture.refresh(&root).unwrap();
    assert_eq!(index.package("zstd").unwrap(), Some(package("zstd")));
}

#[test]
fn a_changed_output_is_retired_and_defaults_outlive_a_missing_one() {
    let (fixture, root, _, online) = ceremony();
    let repository = fixture.path("repository");
    let first = package("zstd");
    sign(&repository, &online, std::slice::from_ref(&first)).unwrap();

    let mut rekeyed = first.clone();
    let output = rekeyed
        .output(Platform::Aarch64Linux, None)
        .unwrap()
        .clone();
    let replaced = Output {
        key: "b".repeat(32).parse().unwrap(),
        manifest: format!("sha256:{}", "1".repeat(64)),
        ..output.clone()
    };
    rekeyed.versions.insert(
        "1.0".into(),
        BTreeMap::from([(Platform::Aarch64Linux, replaced.clone())]),
    );
    rekeyed.default.clear();
    sign(&repository, &online, &[rekeyed]).unwrap();

    let read = fixture
        .refresh(&root)
        .unwrap()
        .package("zstd")
        .unwrap()
        .unwrap();

    assert_eq!(read.output(Platform::Aarch64Linux, None), Some(&replaced));
    assert_eq!(
        read.retired,
        BTreeMap::from([(output.key, output.manifest)])
    );
}

#[test]
fn fields_this_signer_does_not_know_survive_a_merge() {
    let (fixture, _, _, online) = ceremony();
    let mut grown = serde_json::to_value(package("zstd")).unwrap();
    grown["homepage"] = "https://facebook.github.io/zstd".into();
    fixture.publish(
        &[("zstd", serde_json::to_vec(&grown).unwrap())],
        1,
        &online,
        days(7),
    );

    let repository = fixture.path("repository");
    let mut described = package("zstd");
    described.description = "zstd compresses better".into();
    sign(&repository, &online, &[described]).unwrap();
    let kept = fs::read_dir(repository.join("targets"))
        .unwrap()
        .map(|item| fs::read_to_string(item.unwrap().path()).unwrap())
        .filter(|entry| entry.contains("homepage"))
        .count();

    assert_eq!(kept, 2, "the published entry and its merge both have it");
}

#[test]
fn signing_refuses_duplicates_a_damaged_repository_and_an_expiring_root() {
    let (fixture, _, _, online) = ceremony();
    let repository = fixture.path("repository");
    let twice = sign(&repository, &online, &[package("zstd"), package("zstd")]);
    assert!(matches!(twice, Err(Error::Invalid(reason)) if reason.ends_with("given twice")));

    fs::write(repository.join("metadata/1.snapshot.json"), "{}").unwrap();
    assert!(sign(&repository, &online, &[]).is_err());
    assert!(sign(&fixture.path("elsewhere"), &online, &[]).is_err());

    let (fixture, _, _, online) = ceremony_expiring(days(5));
    let expiring = sign(&fixture.path("repository"), &online, &[]);
    assert!(matches!(expiring, Err(Error::Invalid(reason)) if reason.contains("offline keys")));
}
