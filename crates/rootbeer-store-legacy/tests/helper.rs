use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use rootbeer_store_legacy::{hash_tree, stream, Store};

fn sample(root: &Path) {
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(root.join("bin/tool"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(root.join("bin/tool"), fs::Permissions::from_mode(0o755)).unwrap();
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn helper_stores_a_streamed_tree_under_its_own_hash() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    sample(&source);
    let mut bytes = Vec::new();
    stream::write_tree(&source, &mut bytes).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_rb-store"))
        .args(["add", "tool", "1"])
        .env("ROOTBEER_ROOT", root.path().join("opt"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&bytes).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());

    let path = String::from_utf8(output.stdout).unwrap();
    let store = Store::new(root.path().join("opt/store"));
    let manifest = store.verify_entry(path.trim()).unwrap();
    assert_eq!(manifest.output_sha256, hash_tree(&source).unwrap());
}

#[test]
fn helper_rejects_garbage_without_leaving_temporary_trees() {
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_rb-store"))
        .args(["add", "tool", "1"])
        .env("ROOTBEER_ROOT", root.path())
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"rootbeer-tree-1\nf\0\0\0\x09../escape")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(!output.status.success());
    assert_eq!(fs::read_dir(root.path().join("store")).unwrap().count(), 0);
    assert!(!root.path().join("escape").exists());
}

#[test]
fn unwritable_store_inserts_through_the_helper() {
    let root = tempfile::tempdir().unwrap();
    let opt = root.path().join("opt");
    let store = opt.join("store");
    fs::create_dir_all(&store).unwrap();
    fs::create_dir_all(opt.join("bin")).unwrap();

    // Stands in for the setuid bit: the store is writable only while the helper runs.
    let helper = opt.join("bin/rb-store");
    fs::write(
        &helper,
        format!(
            "#!/bin/sh\nchmod u+w '{store}'\n'{real}' \"$@\"\nstatus=$?\nchmod u-w '{store}'\nexit $status\n",
            store = store.display(),
            real = env!("CARGO_BIN_EXE_rb-store"),
        ),
    )
    .unwrap();
    set_mode(&helper, 0o755);
    set_mode(&store, 0o555);

    let source = root.path().join("source");
    sample(&source);
    let entry = Store::new(&store).add_tree("tool", "1", &source);

    set_mode(&store, 0o755);
    let entry = entry.unwrap();
    assert_eq!(entry.output_sha256, hash_tree(&source).unwrap());
    Store::new(&store).verify_entry(&entry.path).unwrap();
}

#[test]
fn helper_reports_its_release_and_protocols() {
    let output = Command::new(env!("CARGO_BIN_EXE_rb-store"))
        .arg("version")
        .output()
        .unwrap();

    assert!(output.status.success());
    let reported: rootbeer_store_legacy::layout::Helper =
        serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reported, rootbeer_store_legacy::helper::current());
}

#[test]
fn helper_creates_roots_and_collects_only_old_unrooted_entries() {
    let root = tempfile::tempdir().unwrap();
    let opt = root.path().join("opt");
    let source = root.path().join("source");
    sample(&source);
    let store = Store::new(opt.join("store"));
    let entry = store.add_tree("tool", "1", &source).unwrap();
    let helper = |command: &str| {
        Command::new(env!("CARGO_BIN_EXE_rb-store"))
            .arg(command)
            .env("ROOTBEER_ROOT", &opt)
            .output()
            .unwrap()
    };

    let output = helper("roots");
    assert!(output.status.success());
    let uid = unsafe { libc::getuid() }.to_string();
    assert!(opt.join("var/roots").join(uid).is_dir());

    let output = helper("gc");
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "young entries survive the grace period"
    );
    assert!(entry.path.is_dir());
}
