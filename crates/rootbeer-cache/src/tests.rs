use crate::manifest::{self, Descriptor, Manifest};
use crate::{Cache, Error, Output, Verified, encode, reference};
use rootbeer_drv::{Build, Derivation, Key, Platform};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex, OnceLock};

fn build() -> Build {
    Build {
        name: "zlib".into(),
        version: "1.3.2".into(),
        platform: Platform::Aarch64Linux,
        sandbox: "linux-v1".into(),
        allow: BTreeSet::new(),
        inputs: BTreeMap::new(),
        dependencies: Vec::new(),
        env: BTreeMap::new(),
        script: "mkdir \"$out\"\n".into(),
        outputs: BTreeSet::from(["out".into()]),
    }
}

fn descriptor(media_type: &str, bytes: &[u8]) -> Descriptor {
    Descriptor {
        media_type: media_type.into(),
        digest: encode(&Sha256::digest(bytes)),
        size: bytes.len() as u64,
    }
}

#[derive(Debug, Clone)]
struct Request {
    method: String,
    target: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

type Reply = (u16, String, Vec<u8>);

fn serve(
    respond: impl Fn(&Request) -> Reply + Send + 'static,
) -> (String, Arc<Mutex<Vec<Request>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let lines: Vec<String> = (&mut reader)
                .lines()
                .map(Result::unwrap)
                .take_while(|line| !line.is_empty())
                .collect();

            let header = |name: &str| {
                lines.iter().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case(name)
                        .then(|| value.trim().to_string())
                })
            };

            let length = header("content-length").map_or(0, |value| value.parse().unwrap());
            let mut body = Vec::new();
            (&mut reader).take(length).read_to_end(&mut body).unwrap();

            let mut parts = lines[0].split(' ');
            let request = Request {
                method: parts.next().unwrap().into(),
                target: parts.next().unwrap().into(),
                authorization: header("authorization"),
                body,
            };

            let (status, headers, body) = respond(&request);
            write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
                body.len()
            )
            .unwrap();

            if request.method != "HEAD" {
                stream.write_all(&body).unwrap();
            }

            seen.lock().unwrap().push(request);
        }
    });

    (url, log)
}

#[test]
fn manifests_match_the_published_contract() {
    let key: Key = "a".repeat(32).parse().unwrap();
    let references = BTreeMap::from([
        ("b".repeat(32).parse().unwrap(), "lz4".to_string()),
        ("c".repeat(32).parse().unwrap(), "zlib".to_string()),
    ]);

    let digest = |digit: char| format!("sha256:{}", digit.to_string().repeat(64));
    let manifest = Manifest::output(
        &key,
        &build(),
        &references,
        Descriptor {
            media_type: manifest::CONFIG.into(),
            digest: digest('1'),
            size: 10,
        },
        Descriptor {
            media_type: manifest::LAYER.into(),
            digest: digest('2'),
            size: 20,
        },
    );

    let json = serde_json::to_string_pretty(&manifest).unwrap() + "\n";
    assert_eq!(json, include_str!("../tests/fixtures/manifest.json"));
}

#[test]
fn archives_are_verified_before_their_last_bytes() {
    let expected = descriptor(manifest::LAYER, b"archive");
    let verified = |bytes: &'static [u8], size: u64| Verified {
        reader: bytes,
        hasher: Sha256::new(),
        expected: Descriptor {
            size,
            ..expected.clone()
        },
        read: 0,
    };

    let cases: [(&[u8], u64, Option<&str>); 5] = [
        (b"archive", 7, None),
        (b"archive and more", 7, None),
        (b"archivf", 7, Some("is sha256:")),
        (b"archive", 6, Some("is sha256:")),
        (b"archive", 8, Some("smaller than")),
    ];

    for (bytes, size, error) in cases {
        let mut out = Vec::new();
        let result = verified(bytes, size).read_to_end(&mut out).map(|_| out);
        match error {
            None => assert_eq!(result.unwrap(), b"archive"),
            Some(reason) => assert!(result.unwrap_err().to_string().contains(reason)),
        }
    }

    // A mismatch fails the read that would complete the archive.
    assert!(verified(b"archivf", 7).read(&mut [0; 16]).is_err());
}

#[test]
fn pulls_refuse_what_a_hostile_registry_serves() {
    let derivation = Derivation::Build(build());
    let key = derivation.key().unwrap();
    let other = Derivation::Build(Build {
        version: "1.3.1".into(),
        ..build()
    });

    let layer = b"not really a tar.zst".to_vec();
    let honest = |derivation: &Derivation| {
        let config = serde_json::to_vec(derivation).unwrap();
        let manifest = Manifest::output(
            &key,
            &build(),
            &BTreeMap::new(),
            descriptor(manifest::CONFIG, &config),
            descriptor(manifest::LAYER, &layer),
        );
        (manifest, config, layer.clone())
    };

    let foreign = {
        let (mut manifest, config, layer) = honest(&derivation);
        manifest.artifact_type = "application/vnd.oci.image.config.v1+json".into();
        (manifest, config, layer)
    };

    let tampered_config = {
        let (manifest, _, layer) = honest(&derivation);
        (manifest, serde_json::to_vec(&other).unwrap(), layer)
    };

    let tampered_layer = {
        let (manifest, config, _) = honest(&derivation);
        (manifest, config, b"not really a tar.zsT".to_vec())
    };

    let cases = [
        (honest(&derivation), "zlib", Ok(())),
        (honest(&derivation), "zstd", Err("isn't a build of zstd")),
        (honest(&other), "zlib", Err("the derivation is")),
        (foreign, "zlib", Err("isn't a rootbeer output")),
        (tampered_config, "zlib", Err("doesn't match its digest")),
        (tampered_layer, "zlib", Err("the archive is sha256:")),
    ];

    for ((manifest, served_config, served_layer), name, expected) in cases {
        let body = serde_json::to_vec(&manifest).unwrap();
        let manifest_path = format!("/manifests/{key}");
        let config_path = format!("/blobs/{}", manifest.config.digest);
        let layer_path = format!("/blobs/{}", manifest.layers[0].digest);
        let (registry, _) = serve(move |request| {
            let body = match &request.target {
                target if target.ends_with(&manifest_path) => &body,
                target if target.ends_with(&config_path) => &served_config,
                target if target.ends_with(&layer_path) => &served_layer,
                _ => return (404, String::new(), Vec::new()),
            };
            (200, String::new(), body.clone())
        });

        let cache = Cache::new(&registry, "rootbeer-test/store").allow_http();
        let pulled = cache
            .pull(name, &key)
            .map_err(|error| error.to_string())
            .and_then(|pulled| {
                assert_eq!(pulled.derivation, derivation);
                let mut bytes = Vec::new();
                pulled
                    .archive()
                    .map_err(|error| error.to_string())?
                    .read_to_end(&mut bytes)
                    .map_err(|error| error.to_string())?;
                assert_eq!(bytes, layer);
                Ok(())
            });

        match expected {
            Ok(()) => pulled.unwrap(),
            Err(reason) => assert!(pulled.unwrap_err().contains(reason)),
        }
    }
}

#[test]
fn credentials_never_leave_the_registry_in_the_clear() {
    let (realm, realm_log) = serve(|_| (200, String::new(), br#"{"token":"t"}"#.to_vec()));
    let challenge = format!(
        "WWW-Authenticate: Bearer realm=\"{realm}/token\",scope=\"repository:a:pull,push\",service=\"fake\"\r\n"
    );

    let (registry, _) = serve(move |request| match request.authorization {
        Some(_) => (200, String::new(), Vec::new()),
        None => (401, challenge.clone(), Vec::new()),
    });

    let key: Key = "a".repeat(32).parse().unwrap();
    assert!(
        Cache::new(&registry, "a")
            .allow_http()
            .exists("zlib", &key)
            .unwrap()
    );
    assert_eq!(realm_log.lock().unwrap()[0].authorization, None);

    let error = Cache::new(&registry, "a")
        .allow_http()
        .with_credentials("user", "secret")
        .exists("zlib", &key)
        .unwrap_err();

    assert!(
        matches!(error, Error::Invalid(reason) if reason.contains("refusing to send credentials"))
    );

    assert_eq!(realm_log.lock().unwrap().len(), 1);
}

#[test]
fn uploads_elsewhere_carry_no_authorization() {
    let (elsewhere, elsewhere_log) = serve(|_| (201, String::new(), Vec::new()));
    let location = format!("Location: {elsewhere}/upload?state=1\r\n");
    let (registry, registry_log) = serve(move |request| match request {
        Request {
            authorization: None,
            ..
        } => (
            401,
            "WWW-Authenticate: Basic realm=\"fake\"\r\n".into(),
            Vec::new(),
        ),
        Request { method, .. } if method == "HEAD" => (404, String::new(), Vec::new()),
        Request { method, .. } if method == "POST" => (202, location.clone(), Vec::new()),
        _ => (201, String::new(), Vec::new()),
    });

    let derivation = Derivation::Build(build());
    let archive = tempfile::NamedTempFile::new().unwrap();
    let output = Output {
        key: &derivation.key().unwrap(),
        derivation: &derivation,
        references: &BTreeMap::new(),
        archive: archive.path(),
    };

    Cache::new(&registry, "a")
        .allow_http()
        .with_credentials("user", "secret")
        .push(&output, &crate::manifest(&output).unwrap())
        .unwrap();

    let uploads = elsewhere_log.lock().unwrap().clone();
    assert_eq!(uploads.len(), 2);
    assert!(uploads.iter().all(|upload| upload.authorization.is_none()));
    assert!(
        uploads[0]
            .target
            .starts_with("/upload?state=1&digest=sha256%3A")
    );

    let manifest = registry_log.lock().unwrap().last().cloned().unwrap();
    assert_eq!(manifest.method, "PUT");
    assert_eq!(
        manifest.authorization.as_deref(),
        Some("Basic dXNlcjpzZWNyZXQ=")
    );
}

#[test]
#[ignore = "needs RB_REGISTRY, such as http://localhost:5050 from registry:2"]
fn outputs_round_trip_through_a_registry() {
    let registry = std::env::var("RB_REGISTRY").unwrap();
    let cache = Cache::new(&registry, "rootbeer-test/store").allow_http();

    let derivation = Derivation::Build(build());
    let key = derivation.key().unwrap();
    let references = BTreeMap::from([("b".repeat(32).parse().unwrap(), "lz4".to_string())]);
    let archive = tempfile::NamedTempFile::new().unwrap();

    std::fs::write(archive.path(), b"not really a tar.zst").unwrap();
    let output = Output {
        key: &key,
        derivation: &derivation,
        references: &references,
        archive: archive.path(),
    };

    let manifest = crate::manifest(&output).unwrap();
    cache.push(&output, &manifest).unwrap();

    assert!(cache.exists("zlib", &key).unwrap());
    assert!(
        !cache
            .exists("zlib", &"d".repeat(32).parse().unwrap())
            .unwrap()
    );

    let pulled = cache.pull("zlib", &key).unwrap();
    assert_eq!(pulled.derivation, derivation);
    assert_eq!(pulled.references, references);
    assert_eq!(pulled.manifest(), manifest);

    let mut bytes = Vec::new();
    pulled.archive().unwrap().read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"not really a tar.zst");

    let promoted = Cache::new(&registry, "rootbeer-test/promoted").allow_http();
    let staged = cache.pull("zlib", &key).unwrap();
    let digest = staged.digest.clone();
    promoted.promote(staged).unwrap();

    let pulled = promoted.pull("zlib", &key).unwrap();
    assert_eq!(pulled.digest, digest);
    assert_eq!(pulled.references, references);
    assert_eq!(pulled.manifest(), manifest);

    let mut bytes = Vec::new();
    pulled.archive().unwrap().read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"not really a tar.zst");
}

#[test]
fn plain_http_needs_an_explicit_opt_in() {
    let key: Key = "a".repeat(32).parse().unwrap();
    let error = Cache::new("http://localhost:1", "a")
        .exists("zlib", &key)
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "refusing plain HTTP to http://localhost:1 unless allowed"
    );
}

#[test]
fn only_an_anonymous_denial_means_not_cached() {
    let registry = Arc::new(OnceLock::<String>::new());
    let realm = registry.clone();
    let (url, _) = serve(move |request| match request.target.starts_with("/token") {
        true => (403, String::new(), Vec::new()),
        false => {
            let realm = realm.get().unwrap();
            let challenge = format!("WWW-Authenticate: Bearer realm=\"{realm}/token\"\r\n");
            (401, challenge, Vec::new())
        }
    });
    registry.set(url.clone()).unwrap();

    let key: Key = "a".repeat(32).parse().unwrap();
    let cache = Cache::new(&url, "a").allow_http();
    assert!(!cache.exists("zlib", &key).unwrap());

    let error = cache
        .with_credentials("user", "secret")
        .exists("zlib", &key)
        .unwrap_err();
    assert!(
        matches!(error, Error::Status { status: 403, .. }),
        "{error}"
    );
}

#[test]
fn references_name_a_plain_package() {
    let key = "b".repeat(32);
    let cases = [
        (format!("lz4:{key}"), true),
        (format!("xz-utils2:{key}"), true),
        (key.clone(), false),
        (format!(":{key}"), false),
        (format!("-x:{key}"), false),
        (format!("Zlib:{key}"), false),
        (format!("../x:{key}"), false),
        ("lz4:nope".into(), false),
    ];

    for (text, is_valid) in cases {
        assert_eq!(reference(&text).is_ok(), is_valid, "{text}");
    }

    let derivation = Derivation::Build(build());
    let references = BTreeMap::from([(key.parse().unwrap(), "Zlib".to_string())]);
    let error = crate::manifest(&Output {
        key: &derivation.key().unwrap(),
        derivation: &derivation,
        references: &references,
        archive: "missing".as_ref(),
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "reference name \"Zlib\" is invalid");
}

#[test]
fn pushes_refuse_a_manifest_of_anything_else() {
    let (registry, log) = serve(|_| (201, String::new(), Vec::new()));
    let derivation = Derivation::Build(build());
    let key = derivation.key().unwrap();
    let archive = tempfile::NamedTempFile::new().unwrap();
    let references = BTreeMap::new();
    let output = Output {
        key: &key,
        derivation: &derivation,
        references: &references,
        archive: archive.path(),
    };

    let honest = crate::manifest(&output).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&honest).unwrap();
    let pretty = serde_json::to_vec_pretty(&value).unwrap();
    let cache = Cache::new(&registry, "a").allow_http();
    let refuse = |manifest: &[u8]| {
        let error = cache.push(&output, manifest).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("the manifest doesn't describe {key}")
        );
    };

    refuse(b"{}");
    refuse(&pretty);
    std::fs::write(archive.path(), b"rebuilt since").unwrap();
    refuse(&honest);
    assert!(log.lock().unwrap().is_empty());
}

#[test]
fn promotions_mount_with_pull_on_the_source_and_refuse_a_copy() {
    let derivation = Derivation::Build(build());
    let key = derivation.key().unwrap();
    let config = serde_json::to_vec(&derivation).unwrap();
    let manifest = Manifest::output(
        &key,
        &build(),
        &BTreeMap::new(),
        descriptor(manifest::CONFIG, &config),
        descriptor(manifest::LAYER, b"not really a tar.zst"),
    );

    let manifest = serde_json::to_vec(&manifest).unwrap();
    let digest = encode(&Sha256::digest(&config)).replace(':', "%3A");
    for (mounted, expected) in [(201, None), (202, Some(202))] {
        let registry = Arc::new(OnceLock::<String>::new());
        let realm = registry.clone();
        let (served, config) = (manifest.clone(), config.clone());
        let (url, log) = serve(move |request| {
            let challenge = format!(
                "WWW-Authenticate: Bearer realm=\"{}/token\"\r\n",
                realm.get().unwrap()
            );

            match (request.method.as_str(), &request.authorization) {
                _ if request.target.starts_with("/token") => {
                    (200, String::new(), br#"{"token":"t"}"#.to_vec())
                }
                (_, None) => (401, challenge, Vec::new()),
                ("GET", _) if request.target.contains("/manifests/") => {
                    (200, String::new(), served.clone())
                }
                ("GET", _) => (200, String::new(), config.clone()),
                ("POST", _) => (mounted, String::new(), Vec::new()),
                _ => (201, String::new(), Vec::new()),
            }
        });

        registry.set(url.clone()).unwrap();
        let staging = Cache::new(&url, "staging").allow_http();
        let store = Cache::new(&url, "store").allow_http();
        let result = store.promote(staging.pull("zlib", &key).unwrap());
        match expected {
            None => result.unwrap(),
            Some(status) => assert!(
                matches!(result, Err(Error::Status { status: actual, .. }) if actual == status)
            ),
        }

        let log = log.lock().unwrap();
        let token = log
            .iter()
            .rfind(|request| request.target.starts_with("/token"))
            .unwrap();

        assert_eq!(
            token.target,
            "/token?scope=repository%3Astore%2Fzlib%3Apull%2Cpush\
             &scope=repository%3Astaging%2Fzlib%3Apull"
        );

        let mount = log.iter().find(|request| request.method == "POST").unwrap();
        assert_eq!(
            mount.target,
            format!("/v2/store/zlib/blobs/uploads/?mount={digest}&from=staging%2Fzlib")
        );

        let put = log.iter().rfind(|request| request.method == "PUT");
        match expected {
            None => assert_eq!(put.unwrap().body, manifest),
            Some(_) => assert!(put.is_none()),
        }
    }
}

#[test]
fn a_token_that_expires_before_the_archive_is_renewed() {
    let derivation = Derivation::Build(build());
    let key = derivation.key().unwrap();
    let config = serde_json::to_vec(&derivation).unwrap();
    let layer = b"not really a tar.zst".to_vec();
    let manifest = Manifest::output(
        &key,
        &build(),
        &BTreeMap::new(),
        descriptor(manifest::CONFIG, &config),
        descriptor(manifest::LAYER, &layer),
    );
    let manifest = serde_json::to_vec(&manifest).unwrap();

    let issued = Mutex::new(0);
    let (realm, _) = serve(move |_| {
        let mut issued = issued.lock().unwrap();
        *issued += 1;
        (
            200,
            String::new(),
            format!(r#"{{"token":"t{issued}"}}"#).into_bytes(),
        )
    });

    let challenge = format!("WWW-Authenticate: Bearer realm=\"{realm}/token\"\r\n");
    let (registry, _) = serve(move |request| {
        let is_blob = request.target.contains(&encode(&Sha256::digest(&layer)));
        let body = match (request.authorization.as_deref(), is_blob) {
            (None, _) | (Some("Bearer t1"), true) => return (401, challenge.clone(), Vec::new()),
            (_, true) => &layer,
            _ if request.target.contains("/manifests/") => &manifest,
            _ => &config,
        };
        (200, String::new(), body.clone())
    });

    let cache = Cache::new(&registry, "a").allow_http();
    let pulled = cache.pull("zlib", &key).unwrap();

    let mut bytes = Vec::new();
    pulled.archive().unwrap().read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"not really a tar.zst");
}
