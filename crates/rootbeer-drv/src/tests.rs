use super::*;
use serde_json::json;

const DEPENDENCY_KEY: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SOURCE_KEY: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SHA256: &str = "0000000000000000000000000000000000000000000000000000000000000000";

type Mutation = (&'static str, fn(&mut Build));

fn build() -> Build {
    Build {
        name: "zlib".to_string(),
        version: "1.3.2".to_string(),
        platform: Platform::X86_64Linux,
        sandbox: "linux-v1".to_string(),
        allow: BTreeSet::new(),
        inputs: BTreeMap::from([("source".to_string(), SOURCE_KEY.parse().unwrap())]),
        dependencies: vec![Dependency {
            key: DEPENDENCY_KEY.parse().unwrap(),
            name: "cmake".to_string(),
            kind: DependencyKind::Build,
        }],
        env: BTreeMap::from([("CFLAGS".to_string(), "-O2".to_string())]),
        script: "make install".to_string(),
        outputs: BTreeSet::from(["out".to_string()]),
    }
}

fn fetch() -> Fetch {
    Fetch {
        sha256: SHA256.to_string().try_into().unwrap(),
        urls: vec!["https://example.com/zlib.tar.gz".to_string()],
    }
}

fn key(build: Build) -> Key {
    Derivation::Build(build).key().unwrap()
}

#[test]
fn canonical_bytes_omit_empty_fields_and_escape_like_jcs() {
    let minimal = Build {
        inputs: BTreeMap::new(),
        dependencies: Vec::new(),
        env: BTreeMap::new(),
        script: "q\"b\\\u{08}\t\n\u{0c}\r\u{01}\u{1f}\u{7f}é😀".to_string(),
        ..build()
    };

    let bytes = Derivation::Build(minimal).canonical_bytes().unwrap();
    let expected = concat!(
        r#"{"kind":"build","name":"zlib","outputs":["out"],"platform":"x86_64-linux","#,
        r#""sandbox":"linux-v1","script":"q\"b\\\b\t\n\f\r\u0001\u001f"#,
        "\u{7f}é😀",
        r#"","version":"1.3.2"}"#,
    );
    assert_eq!(String::from_utf8(bytes).unwrap(), expected);
}

#[test]
fn allow_encodes_in_declaration_order() {
    let build = Build {
        allow: BTreeSet::from(Allow::ALL),
        ..build()
    };

    let bytes = Derivation::Build(build).canonical_bytes().unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(
        text.contains(r#""allow":["local-network","ipc","tmp"]"#),
        "{text}"
    );
}

#[test]
fn every_build_field_changes_the_key() {
    let base = key(build());
    let mutations: Vec<Mutation> = vec![
        ("name", |b| b.name = "zlib-ng".to_string()),
        ("version", |b| b.version = "1.3.1".to_string()),
        ("platform", |b| b.platform = Platform::Aarch64Linux),
        ("sandbox", |b| b.sandbox = "linux-v2".to_string()),
        ("allow", |b| {
            b.allow.insert(Allow::Ipc);
        }),
        ("inputs", |b| {
            b.inputs
                .insert("data".to_string(), DEPENDENCY_KEY.parse().unwrap());
        }),
        ("dependencies key", |b| {
            b.dependencies[0].key = SOURCE_KEY.parse().unwrap()
        }),
        ("dependencies name", |b| {
            b.dependencies[0].name = "cmake3".to_string()
        }),
        ("dependencies kind", |b| {
            b.dependencies[0].kind = DependencyKind::Linked
        }),
        ("env", |b| {
            b.env.insert("CFLAGS".to_string(), "-O3".to_string());
        }),
        ("script", |b| b.script.push_str("\nmake check")),
        ("outputs", |b| {
            b.outputs.insert("dev".to_string());
        }),
    ];

    for (field, mutate) in mutations {
        let mut changed = build();
        mutate(&mut changed);
        assert_ne!(key(changed), base, "changing {field} must change the key");
    }
}

#[test]
fn dependency_order_changes_the_key() {
    let mut first = build();
    first.dependencies.push(Dependency {
        key: SOURCE_KEY.parse().unwrap(),
        name: "pkgconf".to_string(),
        kind: DependencyKind::Build,
    });

    let mut second = first.clone();
    second.dependencies.reverse();
    assert_ne!(key(first), key(second));
}

#[test]
fn fetch_key_covers_hash_not_urls() {
    let base = Derivation::Fetch(fetch()).key().unwrap();

    let mut mirrored = fetch();
    mirrored.urls = vec!["https://mirror.example.org/zlib.tar.gz".to_string()];
    assert_eq!(Derivation::Fetch(mirrored).key().unwrap(), base);

    let mut changed = fetch();
    changed.sha256 = SHA256.replace('0', "1").try_into().unwrap();
    assert_ne!(Derivation::Fetch(changed).key().unwrap(), base);
}

#[test]
fn unknown_fields_are_rejected() {
    let top_level = serde_json::from_value::<Derivation>(json!({
        "kind": "fetch",
        "sha256": SHA256,
        "urls": ["https://example.com/a"],
        "mode": "tree",
    }));

    let mut nested = serde_json::to_value(Derivation::Build(build())).unwrap();
    nested["dependencies"][0]["optional"] = json!(true);
    let nested = serde_json::from_value::<Derivation>(nested);

    assert!(
        top_level
            .unwrap_err()
            .to_string()
            .contains("unknown field `mode`")
    );
    assert!(
        nested
            .unwrap_err()
            .to_string()
            .contains("unknown field `optional`")
    );
}

#[test]
fn invalid_derivations_have_no_key() {
    let cases: Vec<Mutation> = vec![
        ("name", |b| b.name = "Zlib".to_string()),
        ("version", |b| b.version = "1.3/2".to_string()),
        ("sandbox", |b| b.sandbox = String::new()),
        ("script", |b| b.script = "  ".to_string()),
        ("outputs", |b| {
            b.outputs = BTreeSet::from(["dev".to_string()])
        }),
        ("inputs", |b| {
            b.inputs
                .insert("Data".to_string(), DEPENDENCY_KEY.parse().unwrap());
        }),
        ("env", |b| {
            b.env.insert("source".to_string(), "x".to_string());
        }),
        ("env", |b| {
            b.env.insert("1X".to_string(), "x".to_string());
        }),
        ("env", |b| {
            b.env.insert("PATH".to_string(), "/bin".to_string());
        }),
        ("inputs", |b| {
            b.inputs
                .insert("out".to_string(), DEPENDENCY_KEY.parse().unwrap());
        }),
        ("dependencies[1]", |b| {
            b.dependencies.push(b.dependencies[0].clone())
        }),
    ];

    for (field, mutate) in cases {
        let mut invalid = build();
        mutate(&mut invalid);
        let error = Derivation::Build(invalid).key().unwrap_err();
        assert_eq!(error.field, field, "{error}");
    }

    let mut unfetchable = fetch();
    unfetchable.urls.clear();
    let error = Derivation::Fetch(unfetchable).key().unwrap_err();
    assert_eq!(error.field, "urls");
}

#[test]
fn same_dependency_may_be_build_and_linked() {
    let mut both = build();
    both.dependencies.push(Dependency {
        kind: DependencyKind::Linked,
        ..both.dependencies[0].clone()
    });

    Derivation::Build(both).key().unwrap();
}

#[test]
fn keys_and_hashes_reject_malformed_strings() {
    let keys = [
        "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567",
        "abcdefghijklmnopqrstuvwxyz23456",
        "abcdefghijklmnopqrstuvwxyz234561",
    ];
    let hashes = [SHA256.replace('0', "A"), SHA256[1..].to_string()];

    for key in keys {
        assert_eq!(key.parse::<Key>().unwrap_err().field, "key");
    }

    for hash in hashes {
        assert_eq!(Sha256::try_from(hash).unwrap_err().field, "sha256");
    }
}

#[test]
fn store_paths_follow_the_schema_layout() {
    let key: Key = DEPENDENCY_KEY.parse().unwrap();

    assert_eq!(
        output_path(&key, "zlib", "1.3.2", "out"),
        PathBuf::from(format!("/opt/rb/store/{DEPENDENCY_KEY}-zlib-1.3.2"))
    );
    assert_eq!(
        output_path(&key, "zlib", "1.3.2", "dev"),
        PathBuf::from(format!("/opt/rb/store/{DEPENDENCY_KEY}-zlib-1.3.2-dev"))
    );
    assert_eq!(
        fetch_path(&key),
        PathBuf::from(format!("/opt/rb/store/{DEPENDENCY_KEY}-fetch"))
    );
}

#[test]
fn view_keeps_script_lines_and_quotes_what_would_hide() {
    let view = Derivation::Build(Build {
        env: BTreeMap::from([("CFLAGS".to_string(), " -O2".to_string())]),
        script: "./configure\n\nmake".to_string(),
        ..build()
    })
    .to_string();

    let expected = r#"kind: build
name: zlib
version: 1.3.2
platform: x86_64-linux
sandbox: linux-v1
inputs:
  source: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
dependencies:
  - cmake (build) aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
env:
  CFLAGS: " -O2"
outputs:
  - out
script: |-
  ./configure

  make
"#;

    assert_eq!(view, expected);
    let scripts = [
        ("make\n", "script: |\n  make\n"),
        ("make\n\n", "script: \"make\\n\\n\"\n"),
        ("make\r\n", "script: \"make\\r\\n\"\n"),
    ];

    for (script, expected) in scripts {
        let view = Derivation::Build(Build {
            script: script.to_string(),
            ..build()
        })
        .to_string();

        assert!(view.ends_with(expected), "{view}");
    }
}

#[test]
fn names_and_paths_never_leave_the_directory_they_are_joined_to() {
    for path in ["bin", "Contents/Slack.app", "a/b/c", "a/./b"] {
        assert!(is_inside(path), "{path:?}");
    }

    for path in ["", "/abs", "..", "a/../..", "./a", "a\nb"] {
        assert!(!is_inside(path), "{path:?}");
    }

    for name in ["", ".", "..", "a/b", "a\0b"] {
        assert!(!is_file_name(name), "{name:?}");
    }

    assert!(is_app_name("Slack.app"));
    for name in ["Slack", ".app", ".Slack.app", "a/Slack.app"] {
        assert!(!is_app_name(name), "{name:?}");
    }
}
