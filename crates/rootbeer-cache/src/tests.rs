use crate::manifest::{self, Descriptor, Manifest};
use crate::{Cache, Error, Verified, encode};
use rootbeer_drv::{Build, Derivation, Key, Platform};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

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
            io::copy(&mut (&mut reader).take(length), &mut io::sink()).unwrap();

            let mut parts = lines[0].split(' ');
            let request = Request {
                method: parts.next().unwrap().into(),
                target: parts.next().unwrap().into(),
                authorization: header("authorization"),
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
    let references = BTreeSet::from([
        "b".repeat(32).parse().unwrap(),
        "c".repeat(32).parse().unwrap(),
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
            &BTreeSet::new(),
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
        (honest(&derivation), Ok(())),
        (honest(&other), Err("the derivation is")),
        (foreign, Err("isn't a rootbeer output")),
        (tampered_config, Err("doesn't match its digest")),
        (tampered_layer, Err("the archive is sha256:")),
    ];

    for ((manifest, served_config, served_layer), expected) in cases {
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

        let pulled = Cache::new(&registry, "rootbeer-test/store")
            .pull("zlib", &key)
            .map_err(|error| error.to_string())
            .and_then(|mut pulled| {
                let mut bytes = Vec::new();
                pulled
                    .archive
                    .read_to_end(&mut bytes)
                    .map_err(|error| error.to_string())?;
                assert_eq!(pulled.derivation, derivation);
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
    assert!(Cache::new(&registry, "a").exists("zlib", &key).unwrap());
    assert_eq!(realm_log.lock().unwrap()[0].authorization, None);

    let error = Cache::new(&registry, "a")
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
    Cache::new(&registry, "a")
        .with_credentials("user", "secret")
        .push(
            &derivation.key().unwrap(),
            &derivation,
            &BTreeSet::new(),
            archive.path(),
        )
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
    let cache = Cache::new(&registry, "rootbeer-test/store");

    let derivation = Derivation::Build(build());
    let key = derivation.key().unwrap();
    let references = BTreeSet::from(["b".repeat(32).parse().unwrap()]);
    let archive = tempfile::NamedTempFile::new().unwrap();

    std::fs::write(archive.path(), b"not really a tar.zst").unwrap();
    cache
        .push(&key, &derivation, &references, archive.path())
        .unwrap();

    assert!(cache.exists("zlib", &key).unwrap());
    assert!(
        !cache
            .exists("zlib", &"d".repeat(32).parse().unwrap())
            .unwrap()
    );

    let mut pulled = cache.pull("zlib", &key).unwrap();
    assert_eq!(pulled.derivation, derivation);
    assert_eq!(pulled.references, references);

    let mut bytes = Vec::new();
    pulled.archive.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"not really a tar.zst");
}
