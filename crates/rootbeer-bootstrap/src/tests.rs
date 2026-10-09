use super::*;
use crate::fetch::unpack;
use crate::release::{Payload, newer};
use data_encoding::{BASE64, HEXLOWER};
use flate2::{Compression, write::GzEncoder};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;

// Fixtures and keys were produced with OpenSSL separately for a reference
const RELEASE: &[u8] = include_bytes!("../tests/fixtures/release.json");
const RELEASE_BACKUP: &[u8] = include_bytes!("../tests/fixtures/release-backup.json");
const ACTIVE: &str = "12db59a8f3afb49808ee51e7c3b732a3d798b6971a539deaabc868f2fa87fe99";
const BACKUP: &str = "9a25e4c32ced6bc101ae39492d3cde03f3f03edd85c6e37bf294952cc705638b";
const PLATFORM: &str = "aarch64-macos";
const SERIAL: u64 = 20261001093000;

fn key(hex: &str) -> [u8; 32] {
    HEXLOWER.decode(hex.as_bytes()).unwrap().try_into().unwrap()
}

fn release() -> Release {
    newer(RELEASE, &[key(ACTIVE)], PLATFORM, 0)
        .unwrap()
        .unwrap()
}

fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    for (path, contents) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        builder.append_data(&mut header, path, *contents).unwrap();
    }

    builder.into_inner().unwrap().finish().unwrap()
}

fn release_for(archive: &[u8]) -> Release {
    Release(Payload {
        sha256: HEXLOWER.encode(ring::digest::digest(&ring::digest::SHA256, archive).as_ref()),
        ..release().0
    })
}

#[test]
fn fixture_verifies_and_ignores_unknown_fields() {
    let release = release();

    assert_eq!(release.serial(), SERIAL);
    assert_eq!(release.version(), "2026.10.1-nightly");
    assert_eq!(
        release.0.url,
        "https://rbpkg.com/bootstrap/v1/archives/f92745f7ad4eb028954b740892f8078fbf2a04084d30e2e1295bd2ac2e1153aa.tar.gz"
    );
    assert_eq!(
        release.0.sha256,
        "f92745f7ad4eb028954b740892f8078fbf2a04084d30e2e1295bd2ac2e1153aa"
    );
}

#[test]
fn backup_key_is_trusted_only_when_embedded() {
    let trusted = newer(RELEASE_BACKUP, &[key(ACTIVE), key(BACKUP)], PLATFORM, 0).unwrap();
    assert_eq!(trusted.map(|release| release.serial()), Some(SERIAL));

    assert!(matches!(
        newer(RELEASE_BACKUP, &[key(ACTIVE)], PLATFORM, 0),
        Err(Error::Untrusted)
    ));
}

#[test]
fn tampered_payload_is_untrusted() {
    let mut envelope: Value = serde_json::from_slice(RELEASE).unwrap();
    let payload = BASE64
        .decode(envelope["payload"].as_str().unwrap().as_bytes())
        .unwrap();

    let tampered = String::from_utf8(payload)
        .unwrap()
        .replace("20261001093000", "20991231000000");

    envelope["payload"] = json!(BASE64.encode(tampered.as_bytes()));
    let document = serde_json::to_vec(&envelope).unwrap();

    assert!(matches!(
        newer(&document, &[key(ACTIVE)], PLATFORM, 0),
        Err(Error::Untrusted)
    ));
}

#[test]
fn newer_only_moves_forward() {
    for (client, expected) in [(SERIAL - 1, Some(SERIAL)), (SERIAL, None)] {
        let release = newer(RELEASE, &[key(ACTIVE)], PLATFORM, client);
        assert_eq!(release.unwrap().map(|release| release.serial()), expected);
    }
}

#[test]
fn payloads_require_this_platform_https_and_a_digest() {
    let invalid = [
        Payload {
            platform: "x86_64-linux".to_string(),
            ..release().0
        },
        Payload {
            url: "http://example.com/rb.tar.gz".to_string(),
            ..release().0
        },
        Payload {
            sha256: release().0.sha256.to_uppercase(),
            ..release().0
        },
    ];

    for payload in invalid {
        assert!(
            matches!(payload.validate(PLATFORM), Err(Error::Invalid(_))),
            "{payload:?}"
        );
    }
}

#[test]
fn unpack_checks_the_digest_and_keeps_the_directory_private() {
    let archive = archive(&[("rb", b"new rb"), ("rb-store", b"new store")]);
    let release = release_for(&archive);

    let tampered = [archive.as_slice(), b"\0"].concat();
    assert!(matches!(
        unpack(&tampered, &release),
        Err(Error::Digest { .. })
    ));

    let rb = unpack(&archive, &release).unwrap();
    let directory = rb.parent().unwrap();
    assert_eq!(fs::read(&rb).unwrap(), b"new rb");
    assert_eq!(
        fs::metadata(directory).unwrap().permissions().mode() & 0o777,
        0o700
    );

    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn unpack_requires_rb() {
    let archive = archive(&[("rb-store", b"new store")]);
    assert!(matches!(
        unpack(&archive, &release_for(&archive)),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound
    ));
}

#[test]
fn location_is_frozen() {
    assert_eq!(
        location("x86_64-linux"),
        "https://rbpkg.com/bootstrap/v1/x86_64-linux.json"
    );
}

#[test]
#[ignore = "needs bash, OpenSSL 3, and jq"]
fn the_release_scripts_sign_what_clients_verify() {
    let scripts = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts");
    let work = tempfile::tempdir().unwrap();
    let keys = work.path().join("keys");
    let output = std::process::Command::new(format!("{scripts}/bootstrap-ceremony.sh"))
        .arg(&keys)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    let printed = String::from_utf8(output.stdout).unwrap();
    let public = |name: &str| {
        let line = printed.lines().find(|line| line.starts_with(name)).unwrap();
        key(line.split(' ').nth(1).unwrap())
    };

    let rb = archive(&[("rb", b"#!/bin/sh\n")]);
    let path = work.path().join("rb.tar.gz");
    fs::write(&path, &rb).unwrap();

    let out = work.path().join("v1");
    let status = std::process::Command::new(format!("{scripts}/bootstrap-sign.sh"))
        .arg(keys.join("active.pem"))
        .arg(&out)
        .args([PLATFORM, "20261009120000", "nightly-0123456789ab", "0123"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());

    let document = fs::read(out.join(format!("{PLATFORM}.json"))).unwrap();
    let release = newer(&document, &[public("active")], PLATFORM, 0)
        .unwrap()
        .unwrap();
    assert_eq!(release.serial(), 20261009120000);
    assert_eq!(release.version(), "nightly-0123456789ab");

    let served = fs::read(out.join(format!("archives/{}.tar.gz", release.0.sha256))).unwrap();
    let unpacked = unpack(&served, &release).unwrap();
    assert_eq!(fs::read(&unpacked).unwrap(), b"#!/bin/sh\n");
    fs::remove_dir_all(unpacked.parent().unwrap()).unwrap();

    let error = newer(&document, &[public("backup")], PLATFORM, 0).unwrap_err();
    assert!(matches!(error, Error::Untrusted));
}
