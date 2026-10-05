use crate::{Error, Origin, ROOT, Store, pack, seal, unpack};
use rootbeer_drv::Key;
use sha2::Digest;
use std::collections::BTreeSet;
use std::fs;
use std::io::{ErrorKind, Read};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;
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
    seal(target.path()).unwrap();
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

    let path = store
        .ingest("entry", &mut bytes.as_slice(), |_| Ok(()))
        .unwrap();
    assert_eq!(path, store_directory.join("entry"));
    assert_eq!(packed(&path), bytes);

    let error = store
        .ingest("entry", &mut bytes.as_slice(), |_| Ok(()))
        .unwrap_err();
    let is_taken =
        matches!(&error, Error::Io { source, .. } if source.kind() == ErrorKind::AlreadyExists);

    assert!(is_taken, "{error}");
    let failures = [
        ("other", refused.as_slice()),
        ("../escape", bytes.as_slice()),
        (".tmp-entry", bytes.as_slice()),
    ];

    for (entry, archive) in failures {
        assert!(
            store.ingest(entry, &mut &archive[..], |_| Ok(())).is_err(),
            "{entry}"
        );
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

fn store_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("store")).unwrap();
    root
}

#[test]
fn sealing_registers_only_a_callers_own_plain_build() {
    let root = store_root();
    let store_directory = root.path().join("store");
    let key: Key = "a".repeat(32).parse().unwrap();
    let entry = format!("{key}-zlib-1.3.2");
    let path = store_directory.join(&entry);
    fs::create_dir(&path).unwrap();
    tree(&path);

    let link = format!("{key}-link");
    symlink(&path, store_directory.join(&link)).unwrap();

    let builder = rustix::process::getuid().as_raw();
    let mut store = Store::open(root.path()).unwrap();
    let references = BTreeSet::new();

    let other = format!("{}-zlib-1.3.2", "b".repeat(32));
    let slashed = format!("{link}/");
    let refused = [
        (entry.as_str(), builder + 1),
        (other.as_str(), builder),
        (link.as_str(), builder),
        (slashed.as_str(), builder),
    ];
    for (name, caller) in refused {
        let result = store.seal(&key, name, caller, &references);
        assert!(
            matches!(result, Err(Error::Refused(_))),
            "{name} {result:?}"
        );
    }

    let fifo = path.join("a/fifo");
    let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
    assert!(status.success());

    let error = store.seal(&key, &entry, builder, &references).unwrap_err();
    let reason = "isn't a file, directory, or symlink";
    assert!(error.to_string().ends_with(reason), "{error}");

    writable(&path);
    fs::remove_file(&fifo).unwrap();

    fs::set_permissions(path.join("a/tool"), fs::Permissions::from_mode(0o4755)).unwrap();
    let error = store.seal(&key, &entry, builder, &references).unwrap_err();
    let reason = "is setuid, setgid, or sticky";
    assert!(error.to_string().ends_with(reason), "{error}");

    assert_eq!(store.path(&key).unwrap(), None);
    fs::set_permissions(path.join("a/tool"), fs::Permissions::from_mode(0o755)).unwrap();
    #[cfg(target_os = "macos")]
    {
        let file = path.join("a/file");
        let acl = ["+a", "everyone allow write,append"];
        let status = Command::new("chmod").args(acl).arg(&file).status().unwrap();
        assert!(status.success());
    }

    assert_eq!(
        store.seal(&key, &entry, builder, &references).unwrap(),
        path
    );

    assert_eq!(store.path(&key).unwrap(), Some(path.clone()));
    let mode = |relative: &str| {
        let metadata = fs::metadata(path.join(relative)).unwrap();
        metadata.permissions().mode() & 0o7777
    };

    assert_eq!(
        (mode("a"), mode("a/file"), mode("a/tool")),
        (0o555, 0o444, 0o555)
    );

    #[cfg(target_os = "macos")]
    {
        let opened = fs::OpenOptions::new()
            .append(true)
            .open(path.join("a/file"));
        assert_eq!(opened.unwrap_err().kind(), ErrorKind::PermissionDenied);
    }

    writable(&path);
}

#[test]
fn pulling_replaces_leftovers_and_registers_only_the_expected_archive() {
    let source = tempfile::tempdir().unwrap();
    tree(source.path());
    let bytes = packed(source.path());
    let digest = format!(
        "sha256:{}",
        data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(&bytes))
    );

    let root = store_root();
    let key: Key = "a".repeat(32).parse().unwrap();
    let entry = format!("{key}-zlib-1.3.2");
    fs::create_dir(root.path().join("store").join(&entry)).unwrap();

    let mut store = Store::open(root.path()).unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("kept"), "").unwrap();
    let link = format!("{key}-link");
    symlink(outside.path(), root.path().join("store").join(&link)).unwrap();
    let slashed = format!("{link}/");

    let error = store
        .pull(&key, &slashed, &digest, &BTreeSet::new(), bytes.as_slice())
        .unwrap_err();

    assert!(matches!(error, Error::Refused(_)), "{error}");
    assert!(outside.path().join("kept").exists());

    // A separate read for the trailing byte, so only draining to EOF sees it.
    let wrong = format!("sha256:{}", "0".repeat(64));
    for (expected, trailing) in [(wrong.as_str(), &[][..]), (digest.as_str(), &[0][..])] {
        let archive = bytes.as_slice().chain(trailing);
        let error = store
            .pull(&key, &entry, expected, &BTreeSet::new(), archive)
            .unwrap_err();

        assert!(
            error.to_string().starts_with("the archive is sha256:"),
            "{error}"
        );
        let names: Vec<_> = fs::read_dir(root.path().join("store"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();

        assert_eq!(names, [link.as_str()]);
        assert_eq!(store.path(&key).unwrap(), None);
    }

    let path = store
        .pull(&key, &entry, &digest, &BTreeSet::new(), bytes.as_slice())
        .unwrap();

    assert!(path.join("a/file").exists());
    let recorded: String = store
        .connection
        .query_row(
            "SELECT digest FROM outputs WHERE key = ?1",
            [key.as_str()],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(recorded, digest);

    let reader = Store::open_read_only(root.path()).unwrap();
    assert_eq!(reader.path(&key).unwrap(), Some(path.clone()));
    writable(&path);
}
