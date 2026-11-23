use super::*;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

fn signed() -> Vec<(String, Vec<u8>)> {
    // A real signature is binary, newlines and NULs and all.
    vec![
        ("com.apple.cs.CodeDirectory".into(), b"hashes".to_vec()),
        ("com.apple.cs.CodeSignature".into(), (0..=255).collect()),
    ]
}

fn long() -> Vec<u8> {
    [b"a/b/".as_slice(), &[b'l'; 200]].concat()
}

/// A tree with every kind of entry, a long name, and `a-c`, which sorts after
/// everything inside `a` though `-` is a lower byte than `/`.
fn sample(attributes: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut writer = Writer::new(Vec::new()).unwrap();
    writer.directory(b"a").unwrap();
    writer
        .file(b"a/app.jar", false, attributes, 2, &mut &b"PK"[..])
        .unwrap();
    writer.directory(b"a/b").unwrap();
    writer
        .file(&long(), false, &[], 4, &mut &b"long"[..])
        .unwrap();
    writer.symlink(b"a/link", b"tool").unwrap();
    writer
        .file(b"a/tool", true, &[], 10, &mut &b"#!/bin/sh\n"[..])
        .unwrap();
    writer.directory(b"a-c").unwrap();
    writer.directory(b"empty").unwrap();
    writer.finish().unwrap()
}

fn bytes(bytes: &[u8]) -> Vec<u8> {
    let length = u32::try_from(bytes.len()).unwrap();
    [length.to_le_bytes().as_slice(), bytes].concat()
}

fn stream(records: &[Vec<u8>]) -> Vec<u8> {
    [MAGIC.to_vec(), records.concat(), b"e".to_vec()].concat()
}

fn directory(path: &[u8]) -> Vec<u8> {
    [b"d".to_vec(), bytes(path)].concat()
}

fn link(path: &[u8], target: &[u8]) -> Vec<u8> {
    [b"l".to_vec(), bytes(path), bytes(target)].concat()
}

fn file(path: &[u8], attributes: &[(&str, &[u8])], contents: &[u8], size: u64) -> Vec<u8> {
    let count = u32::try_from(attributes.len()).unwrap().to_le_bytes();
    let attributes = attributes
        .iter()
        .flat_map(|(name, value)| [bytes(name.as_bytes()), bytes(value)].concat())
        .collect::<Vec<_>>();
    [
        b"f".to_vec(),
        bytes(path),
        count.to_vec(),
        attributes,
        size.to_le_bytes().to_vec(),
        contents.to_vec(),
    ]
    .concat()
}

// Frozen with rootbeer-tree-v1. A change here moves every recorded tree, so it
// needs a new format version, never an edit.
#[test]
fn a_tree_has_one_frozen_encoding() {
    let mut writer = Writer::new(Vec::new()).unwrap();
    writer.directory(b"d").unwrap();
    writer.file(b"d/f", true, &[], 2, &mut &b"hi"[..]).unwrap();
    let expected = [
        b"rootbeer-tree-v1\n".as_slice(),
        b"d\x01\x00\x00\x00d",
        b"x\x03\x00\x00\x00d/f\x00\x00\x00\x00\x02\x00\x00\x00\x00\x00\x00\x00hi",
        b"e",
    ]
    .concat();
    assert_eq!(writer.finish().unwrap(), expected);

    assert_eq!(
        digest(sample(&signed()).as_slice()).unwrap().as_str(),
        "ea523051dcb6807c64f3bfa4b69a45b70baf3861af57ca93aa537f37fb342fd5"
    );
}

#[test]
fn a_stream_reads_back_as_written() {
    let written = sample(&signed());
    let mut reader = Reader::new(written.as_slice()).unwrap();
    let mut entries = Vec::new();
    while let Some(entry) = reader.entry().unwrap() {
        let mut contents = Vec::new();
        std::io::Read::read_to_end(&mut reader.contents(), &mut contents).unwrap();
        entries.push((entry, contents));
    }

    let file = |path: &[u8], is_executable, attributes: Vec<_>, contents: &[u8]| {
        let size = u64::try_from(contents.len()).unwrap();
        let kind = Kind::File {
            is_executable,
            attributes,
            size,
        };
        (
            Entry {
                path: path.to_vec(),
                kind,
            },
            contents.to_vec(),
        )
    };
    let entry = |path: &[u8], kind| {
        (
            Entry {
                path: path.to_vec(),
                kind,
            },
            Vec::new(),
        )
    };
    let link = Kind::Symlink {
        target: b"tool".to_vec(),
    };
    assert_eq!(
        entries,
        [
            entry(b"a", Kind::Directory),
            file(b"a/app.jar", false, signed(), b"PK"),
            entry(b"a/b", Kind::Directory),
            file(&long(), false, Vec::new(), b"long"),
            entry(b"a/link", link),
            file(b"a/tool", true, Vec::new(), b"#!/bin/sh\n"),
            entry(b"a-c", Kind::Directory),
            entry(b"empty", Kind::Directory),
        ]
    );
    assert_eq!(
        reader.finish().unwrap(),
        digest(written.as_slice()).unwrap()
    );
}

// Each of these says something a canonical stream already says another way,
// or something no tree can hold, so accepting it would let one tree have two
// digests.
#[test]
fn only_the_canonical_encoding_is_accepted() {
    let cases: [(&str, Vec<u8>); 18] = [
        ("sibling order", stream(&[directory(b"b"), directory(b"a")])),
        ("duplicate", stream(&[directory(b"a"), directory(b"a")])),
        (
            "component order",
            stream(&[directory(b"a-c"), directory(b"a")]),
        ),
        ("no parent", stream(&[file(b"a/f", &[], b"", 0)])),
        (
            "under a symlink",
            stream(&[link(b"a", b"x"), file(b"a/f", &[], b"", 0)]),
        ),
        (
            "under a file",
            stream(&[file(b"a", &[], b"", 0), file(b"a/f", &[], b"", 0)]),
        ),
        ("dot dot", stream(&[directory(b"..")])),
        ("dot", stream(&[directory(b"a"), directory(b"a/.")])),
        (
            "empty component",
            stream(&[directory(b"a"), directory(b"a//b")]),
        ),
        ("absolute", stream(&[directory(b"/a")])),
        ("nul", stream(&[directory(b"a\0")])),
        (
            "other attribute",
            stream(&[file(b"f", &[("com.apple.quarantine", b"q")], b"", 0)]),
        ),
        (
            "unsorted attributes",
            stream(&[file(
                b"f",
                &[("com.apple.cs.b", b""), ("com.apple.cs.a", b"")],
                b"",
                0,
            )]),
        ),
        ("empty target", stream(&[link(b"l", b"")])),
        ("unknown record", stream(&[b"z".to_vec()])),
        ("short contents", stream(&[file(b"f", &[], b"ab", 5)])),
        ("no end", [MAGIC, b"d\x01\x00\x00\x00a"].concat()),
        ("trailing bytes", [stream(&[]), b"e".to_vec()].concat()),
    ];

    for (case, bytes) in cases {
        let result = digest(bytes.as_slice());
        assert!(
            matches!(result, Err(Error::Malformed(_))),
            "{case}: {result:?}"
        );
    }

    let other = [b"rootbeer-tree-v2\n".as_slice(), b"e"].concat();
    assert!(matches!(digest(other.as_slice()), Err(Error::Malformed(_))));
    assert!(digest(stream(&[]).as_slice()).is_ok());
}

#[test]
fn a_writer_refuses_what_a_reader_would() {
    let mut writer = Writer::new(Vec::new()).unwrap();
    writer.directory(b"b").unwrap();
    assert!(matches!(writer.directory(b"a"), Err(Error::Malformed(_))));

    let quarantined = [("com.apple.quarantine".to_string(), b"q".to_vec())];
    let refused = writer.file(b"c", false, &quarantined, 0, &mut &b""[..]);
    assert!(matches!(refused, Err(Error::Malformed(_))));

    let short = writer.file(b"d", false, &[], 5, &mut &b"ab"[..]);
    assert!(matches!(short, Err(Error::Malformed(_))));
}

// Linux can't hold code signature attributes, so there they're refused.
#[test]
fn a_materialized_tree_holds_exactly_what_the_stream_says() {
    let attributes = match cfg!(target_os = "macos") {
        true => signed(),
        false => Vec::new(),
    };
    let written = sample(&attributes);
    let root = tempfile::tempdir().unwrap();
    let out = root.path().join("out");
    assert_eq!(
        materialize(written.as_slice(), &out).unwrap(),
        digest(written.as_slice()).unwrap()
    );

    assert_eq!(fs::read(out.join("a/tool")).unwrap(), b"#!/bin/sh\n");
    assert_eq!(
        fs::read(out.join(String::from_utf8(long()).unwrap())).unwrap(),
        b"long"
    );
    assert_eq!(
        fs::read_link(out.join("a/link")).unwrap().to_str(),
        Some("tool")
    );
    let mode = |path: &str| fs::metadata(out.join(path)).unwrap().permissions().mode() & 0o111;
    assert_ne!(mode("a/tool"), 0);
    assert_eq!(mode("a/app.jar"), 0);

    for path in ["", "a", "a/link", "a/tool", "empty"] {
        let metadata = fs::symlink_metadata(out.join(path)).unwrap();
        assert_eq!(metadata.mtime(), 1, "{path}");
    }

    for (name, value) in &attributes {
        let mut read = vec![0; 512];
        let size = rustix::fs::getxattr(out.join("a/app.jar"), name.as_str(), &mut read).unwrap();
        assert_eq!(read.get(..size), Some(value.as_slice()));
    }

    let again = materialize(written.as_slice(), &out).unwrap_err();
    assert!(matches!(again, Error::Io { .. }), "{again}");

    let empty = root.path().join("empty");
    fs::create_dir(&empty).unwrap();
    materialize(written.as_slice(), &empty).unwrap();
}
