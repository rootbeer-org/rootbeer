use crate::{Error, Origin, ROOT, Store, pack, unpack};
use rootbeer_drv::Key;
use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use tar::{EntryType, Header};

const TREE: &[u8] = include_bytes!("../tests/fixtures/tree.tar.zst");

fn tree(root: &Path) {
    let long = format!("a/b/{}", "l".repeat(120));
    fs::create_dir_all(root.join("a/b")).unwrap();

    for (path, contents, mode) in [
        ("a/file", "hi", 0o644),
        ("a/tool", "#!/bin/sh\n", 0o755),
        (long.as_str(), "long", 0o644),
    ] {
        fs::write(root.join(path), contents).unwrap();
        fs::set_permissions(root.join(path), fs::Permissions::from_mode(mode)).unwrap();
    }

    symlink("file", root.join("a/link")).unwrap();
}

fn packed(root: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    pack(root, &mut bytes).unwrap();
    bytes
}

fn writable(root: &Path) {
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry.unwrap();
        if entry.file_type().is_dir() {
            fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}

fn raw(entries: &[(&str, EntryType, u32, &str)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, kind, mode, contents) in entries {
        let mut header = Header::new_gnu();
        let name = &mut header.as_gnu_mut().unwrap().name;
        name[..path.len()].copy_from_slice(path.as_bytes());
        header.set_entry_type(*kind);
        header.set_mode(*mode);

        let mut data = contents.as_bytes();
        if kind.is_symlink() || kind.is_hard_link() {
            header.set_link_name(contents).unwrap();
            data = b"";
        }

        header.set_size(data.len() as u64);
        header.set_cksum();
        builder.append(&header, data).unwrap();
    }

    zstd::encode_all(builder.into_inner().unwrap().as_slice(), 0).unwrap()
}

#[test]
fn packing_is_deterministic_and_round_trips() {
    let source = tempfile::tempdir().unwrap();
    tree(source.path());
    let bytes = packed(source.path());
    assert_eq!(bytes, TREE);

    let target = tempfile::tempdir().unwrap();
    unpack(bytes.as_slice(), target.path()).unwrap();
    assert_eq!(packed(target.path()), bytes);

    let mode = fs::metadata(target.path()).unwrap().permissions().mode();
    assert_eq!(mode & 0o7777, 0o555);
    writable(target.path());
}

#[test]
fn unpacking_refuses_what_packing_never_writes() {
    let outside = tempfile::tempdir().unwrap();
    let escape = outside.path().to_str().unwrap();
    let long_name = EntryType::GNULongName;
    let cases = [
        ("leaves", raw(&[("../evil", EntryType::Regular, 0o444, "")])),
        ("leaves", raw(&[("/evil", EntryType::Regular, 0o444, "")])),
        (
            "leaves",
            raw(&[
                ("././@LongLink", long_name, 0o444, "../evil"),
                ("benign", EntryType::Regular, 0o444, ""),
            ]),
        ),
        (
            "inside a symlink",
            raw(&[
                ("a", EntryType::Symlink, 0o555, escape),
                ("a/evil", EntryType::Regular, 0o444, ""),
            ]),
        ),
        ("Link entry", raw(&[("hard", EntryType::Link, 0o444, "x")])),
        (
            "mode 4555",
            raw(&[("suid", EntryType::Regular, 0o4555, "")]),
        ),
        (
            "File exists",
            raw(&[
                ("twice", EntryType::Regular, 0o444, "first"),
                ("twice", EntryType::Regular, 0o444, "second"),
            ]),
        ),
    ];

    for (reason, bytes) in cases {
        let target = tempfile::tempdir().unwrap();
        let error = unpack(bytes.as_slice(), target.path()).unwrap_err();
        assert!(error.to_string().contains(reason), "{error}");
    }

    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn ingest_moves_in_a_whole_output_or_nothing() {
    let source = tempfile::tempdir().unwrap();
    tree(source.path());
    let bytes = packed(source.path());
    let refused = raw(&[("../evil", EntryType::Regular, 0o444, "")]);

    let root = tempfile::tempdir().unwrap();
    let store_directory = root.path().join("store");
    fs::create_dir(&store_directory).unwrap();
    let store = Store::open(root.path()).unwrap();

    let path = store.ingest("entry", bytes.as_slice()).unwrap();
    assert_eq!(path, store_directory.join("entry"));
    assert_eq!(packed(&path), bytes);

    let error = store.ingest("entry", bytes.as_slice()).unwrap_err();
    let is_taken =
        matches!(&error, Error::Io { source, .. } if source.kind() == ErrorKind::AlreadyExists);

    assert!(is_taken, "{error}");
    let failures = [
        ("other", refused.as_slice()),
        ("../escape", bytes.as_slice()),
        (".tmp-entry", bytes.as_slice()),
    ];

    for (entry, archive) in failures {
        assert!(store.ingest(entry, archive).is_err(), "{entry}");
    }

    let names: Vec<_> = fs::read_dir(&store_directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();

    assert_eq!(names, ["entry"]);
    writable(&path);
}

#[test]
fn registered_outputs_persist_and_cannot_be_replaced() {
    let root = tempfile::tempdir().unwrap();
    let key: Key = "a".repeat(32).parse().unwrap();
    let references = BTreeSet::from(["b".repeat(32).parse().unwrap()]);

    let mut store = Store::open(root.path()).unwrap();
    assert_eq!(store.path(&key).unwrap(), None);
    store
        .register(
            &key,
            "first",
            Origin::Pulled,
            Some("sha256:00"),
            &references,
        )
        .unwrap();

    let none = BTreeSet::new();
    let error = store
        .register(&key, "second", Origin::Local, None, &none)
        .unwrap_err();

    assert!(matches!(error, Error::Database(_)), "{error}");
    let reopened = Store::open(root.path()).unwrap();
    let first = root.path().join("store/first");
    assert_eq!(reopened.path(&key).unwrap(), Some(first));
    assert_eq!(reopened.references(&key).unwrap(), references);
}

#[test]
fn root_holds_the_store_output_paths_embed() {
    let store = Path::new(ROOT).join("store");
    assert_eq!(store, Path::new(rootbeer_drv::STORE_ROOT));
}
