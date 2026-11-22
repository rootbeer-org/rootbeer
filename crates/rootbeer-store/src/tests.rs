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

#[cfg(target_os = "macos")]
#[test]
fn code_signatures_in_extended_attributes_round_trip_alone() {
    use rustix::fs::{XattrFlags, getxattr, setxattr};

    // A real signature is binary, newlines and all.
    let signature = (0..=255).collect::<Vec<u8>>();
    let source = tempfile::tempdir().unwrap();
    let jar = source.path().join("app.jar");
    fs::write(&jar, "PK").unwrap();
    for (name, value) in [
        ("com.apple.cs.CodeSignature", signature.as_slice()),
        ("com.apple.cs.CodeDirectory", b"hashes"),
        ("com.apple.quarantine", b"downloaded"),
    ] {
        setxattr(&jar, name, value, XattrFlags::empty()).unwrap();
    }

    fs::set_permissions(&jar, fs::Permissions::from_mode(0o444)).unwrap();
    let bytes = packed(source.path());

    let target = tempfile::tempdir().unwrap();
    unpack(bytes.as_slice(), target.path()).unwrap();
    seal(target.path()).unwrap();

    let unpacked = target.path().join("app.jar");
    let mut value = [0u8; 512];
    let size = getxattr(&unpacked, "com.apple.cs.CodeSignature", &mut value[..]).unwrap();
    assert_eq!(value.get(..size), Some(signature.as_slice()));
    assert!(getxattr(&unpacked, "com.apple.quarantine", &mut value[..]).is_err());

    let mode = fs::metadata(&unpacked).unwrap().permissions().mode();
    assert_eq!(mode & 0o7777, 0o444);
    assert_eq!(packed(target.path()), bytes);
    writable(target.path());
}

#[test]
fn long_link_targets_round_trip() {
    let source = tempfile::tempdir().unwrap();
    let target = "t".repeat(150);
    symlink(&target, source.path().join("link")).unwrap();
    let bytes = packed(source.path());

    let unpacked = tempfile::tempdir().unwrap();
    unpack(bytes.as_slice(), unpacked.path()).unwrap();
    let read = fs::read_link(unpacked.path().join("link")).unwrap();
    assert_eq!(read, Path::new(&target));
}

#[test]
fn unpacking_refuses_extended_attributes_it_wouldnt_keep() {
    let archive = |key: &str, value: &[u8], kind: EntryType| {
        let mut builder = tar::Builder::new(Vec::new());
        builder.append_pax_extensions([(key, value)]).unwrap();
        let mut header = Header::new_gnu();
        header.set_entry_type(kind);
        header.set_mode(0o555);
        header.set_size(0);
        builder
            .append_data(&mut header, "entry", std::io::empty())
            .unwrap();
        zstd::encode_all(builder.into_inner().unwrap().as_slice(), 0).unwrap()
    };

    // The newline in the second value doesn't end its record, so the size
    // record it seems to hold is never read.
    let cases: [(&str, &[u8], _, _); 3] = [
        (
            "SCHILY.xattr.com.apple.quarantine",
            b"x",
            EntryType::Regular,
            "com.apple.quarantine",
        ),
        (
            "SCHILY.xattr.com.apple.cs.CodeSignature",
            b"\n9 size=1\n",
            EntryType::Directory,
            "isn't a file",
        ),
        ("size", b"1", EntryType::Regular, "PAX record size"),
    ];
    for (key, value, kind, reason) in cases {
        let target = tempfile::tempdir().unwrap();
        let bytes = archive(key, value, kind);
        let error = unpack(bytes.as_slice(), target.path()).unwrap_err();
        assert!(error.to_string().contains(reason), "{error}");
    }
}

#[test]
fn sealing_makes_an_unpacked_archive_readable_by_all() {
    let archive = raw(&[
        ("dir", EntryType::Directory, 0o550, ""),
        ("dir/file", EntryType::Regular, 0o440, "hi"),
        ("dir/tool", EntryType::Regular, 0o500, "#!/bin/sh\n"),
    ]);

    let target = tempfile::tempdir().unwrap();
    unpack(archive.as_slice(), target.path()).unwrap();
    seal(target.path()).unwrap();

    let mode = |relative: &str| {
        let metadata = fs::metadata(target.path().join(relative)).unwrap();
        metadata.permissions().mode() & 0o7777
    };

    assert_eq!(
        (mode("dir"), mode("dir/file"), mode("dir/tool")),
        (0o555, 0o444, 0o555)
    );
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
            "malformed PAX record",
            raw(&[
                (
                    "pax",
                    EntryType::XHeader,
                    0o444,
                    "99 SCHILY.xattr.com.apple.cs.A=y\n",
                ),
                ("file", EntryType::Regular, 0o444, ""),
            ]),
        ),
        (
            "header for no entry",
            raw(&[("@LongLink", long_name, 0o444, "file")]),
        ),
        (
            "link target but isn't a symlink",
            raw(&[
                ("@LongLink", EntryType::GNULongLink, 0o444, "elsewhere"),
                ("file", EntryType::Regular, 0o444, ""),
            ]),
        ),
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
            &"b".repeat(32).parse().unwrap(),
            "referenced",
            Origin::Pulled,
            None,
            &BTreeSet::new(),
        )
        .unwrap();

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
    fs::set_permissions(path.join("a/tool"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(path.join("a/file"), fs::Permissions::from_mode(0o640)).unwrap();
    fs::set_permissions(path.join("a"), fs::Permissions::from_mode(0o750)).unwrap();
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

#[test]
fn collecting_keeps_what_roots_reach_and_waits_for_holders() {
    let source = tempfile::tempdir().unwrap();
    tree(source.path());
    let bytes = packed(source.path());
    let digest = format!(
        "sha256:{}",
        data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(&bytes))
    );

    let root = store_root();
    let mut store = Store::open(root.path()).unwrap();
    let key = |digit: &str| -> Key { digit.repeat(32).parse().unwrap() };
    let pull = |store: &mut Store, digit: &str, references: &[Key]| {
        let entry = format!("{}-tool-1.0", key(digit));
        let references = references.iter().cloned().collect();
        store.pull(&key(digit), &entry, &digest, &references, bytes.as_slice())
    };

    let error = pull(&mut store, "a", &[key("b")]).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "{} references {}, which isn't in the store",
            key("a"),
            key("b")
        )
    );

    for (digit, references) in [("b", vec![]), ("a", vec![key("b")]), ("c", vec![])] {
        pull(&mut store, digit, &references).unwrap();
    }

    let store_directory = root.path().join("store");
    let orphan = format!("{}-tool-1.0", key("d"));
    fs::create_dir(store_directory.join(&orphan)).unwrap();
    fs::create_dir(store_directory.join(".hidden")).unwrap();
    writable(&store_directory);

    let held = store.hold().unwrap();
    let (waited, waiting) = std::sync::mpsc::channel();
    let collecting = std::thread::spawn(move || {
        let roots = || Ok(BTreeSet::from([key("a")]));
        let removed = store.collect(roots, || waited.send(()).unwrap());
        (store, removed)
    });

    waiting.recv().unwrap();
    drop(held);
    let (mut store, removed) = collecting.join().unwrap();
    let expected = [format!("{}-tool-1.0", key("c")), orphan];

    assert_eq!(
        BTreeSet::from_iter(removed.unwrap()),
        BTreeSet::from(expected)
    );
    assert!(store.path(&key("a")).unwrap().is_some());
    assert!(store.path(&key("b")).unwrap().is_some());
    assert_eq!(store.path(&key("c")).unwrap(), None);
    assert!(store_directory.join(".hidden").exists());

    let error = store
        .collect(|| Err("unreadable".into()), || {})
        .unwrap_err();

    assert_eq!(error.to_string(), "unreadable");
    assert!(store.path(&key("a")).unwrap().is_some());

    fs::remove_dir_all(store_directory.join(format!("{}-tool-1.0", key("b")))).unwrap();
    let removed = store.collect(|| Ok(BTreeSet::new()), || {}).unwrap();
    assert_eq!(removed, [format!("{}-tool-1.0", key("a"))]);
    assert_eq!(store.path(&key("b")).unwrap(), None);
}
