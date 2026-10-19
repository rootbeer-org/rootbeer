use std::path::PathBuf;

use std::collections::BTreeMap;

use crate::package::PackageRequest;
use crate::plan::Op;

use super::super::test_support::{run, vm_in};

#[test]
fn planning_without_a_lock_does_not_guess_catalog_commands() {
    let root = tempfile::tempdir().unwrap();
    let vm = vm_in(
        r#"
        local rb = require("rootbeer")
        rb.package("uninstalled_catalog_test_command")
        result = rb.which("uninstalled_catalog_test_command")
    "#,
        root.path(),
    );
    let result: Option<String> = vm.lua.globals().get("result").unwrap();
    assert!(result.is_none());
    assert!(!root.path().join("rootbeer.lock").exists());
}

#[test]
fn rb_package_string_pushes_request_intent_without_resolving() {
    let ops = run(r#"
        rb.package("github:owner/tool@v1.0.0")
        "#);

    let [Op::Package { request }] = ops.as_slice() else {
        panic!("expected one Package op, got {ops:?}");
    };

    assert_eq!(
        request,
        &PackageRequest::Release {
            repository: "owner/tool".into(),
            tag: Some("v1.0.0".into()),
            asset: None,
            bins: BTreeMap::new(),
        }
    );
}

#[test]
fn rb_which_returns_a_locked_package_bin_path() {
    let tmp = tempfile::tempdir().unwrap();
    let platform = format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS);
    let lock = serde_json::json!({
        "schema": 4,
        "packages": { "demo": { platform: {
            "version": "1.0.0",
            "key": "a".repeat(32),
            "manifest": format!("sha256:{}", "0".repeat(64)),
            "bins": ["demo"],
        } } },
    });
    std::fs::write(tmp.path().join("rootbeer.lock"), lock.to_string()).unwrap();
    let vm = vm_in(
        r#"
        rb.package("demo")
        result = rb.which("demo")
        missing = rb.which("missing")
        "#,
        tmp.path(),
    );

    let result: Option<String> = vm.lua.globals().get("result").unwrap();
    let missing: Option<String> = vm.lua.globals().get("missing").unwrap();

    assert_eq!(
        result.map(PathBuf::from),
        Some(crate::env::bin_dir().join("demo"))
    );
    assert_eq!(missing, None);
}

#[test]
fn rb_which_finds_a_locked_release_under_its_repo_name() {
    let tmp = tempfile::tempdir().unwrap();
    let platform = format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS);
    let lock = serde_json::json!({
        "schema": 5,
        "packages": { "ripgrep": { platform: {
            "version": "14.1.1",
            "key": "a".repeat(32),
            "release": {
                "repository": "BurntSushi/ripgrep",
                "asset": "ripgrep.tar.gz",
                "url": "https://github.com/BurntSushi/ripgrep/releases/download/14.1.1/ripgrep.tar.gz",
                "sha256": "0".repeat(64),
                "format": "tar.gz",
                "paths": { "rg": "ripgrep-14.1.1/rg" },
            },
            "bins": ["rg"],
        } } },
    });
    std::fs::write(tmp.path().join("rootbeer.lock"), lock.to_string()).unwrap();
    let vm = vm_in(
        r#"
        rb.package("github:BurntSushi/ripgrep")
        result = rb.which("rg")
        "#,
        tmp.path(),
    );

    let result: Option<String> = vm.lua.globals().get("result").unwrap();
    assert_eq!(
        result.map(PathBuf::from),
        Some(crate::env::bin_dir().join("rg"))
    );
}

#[test]
fn rb_env_export_writes_package_env_file_and_returns_path() {
    let tmp = tempfile::tempdir().unwrap();
    let vm = vm_in(
        r#"
        result = rb.env_export("zsh")
        "#,
        tmp.path(),
    );
    let result: String = vm.lua.globals().get("result").unwrap();
    let ops = super::super::test_support::drain(vm);

    let [Op::WriteFile { path, source }] = ops.as_slice() else {
        panic!("expected one WriteFile op, got {ops:?}");
    };

    assert_eq!(result, path.to_string_lossy());
    assert!(path.ends_with("rootbeer/env.sh"));
    let content = source.as_str().expect("inline package environment");
    let bin = crate::env::bin_dir();
    assert!(content.contains(&format!("export PATH='{}'", bin.display())));
}

#[test]
fn github_options_remain_declarative() {
    let ops = run(r#"
        rb.package("github:owner/tool@v1", {
            asset = "tool-darwin-arm64.tar.gz",
            bins = { tool = "release/bin/tool" },
        })
    "#);
    let [Op::Package {
        request: PackageRequest::Release { asset, bins, .. },
    }] = ops.as_slice()
    else {
        panic!("expected a release request");
    };
    assert_eq!(asset.as_deref(), Some("tool-darwin-arm64.tar.gz"));
    assert_eq!(bins["tool"], PathBuf::from("release/bin/tool"));
}

#[test]
fn batches_and_structured_requests_match_individual_declarations() {
    let single = run(r#"
        rb.package("github:owner/first@v1")
        rb.package("github:owner/second@v2", { asset = "second.zip", bins = { second = "bin/second" } })
        rb.package("jq@1.8.1")
    "#);
    let batch = run(r#"
        rb.packages({
            "github:owner/first@v1",
            { request = "github:owner/second@v2", asset = "second.zip", bins = { second = "bin/second" } },
            { request = "jq@1.8.1" },
        })
    "#);
    let intents = |ops: Vec<Op>| {
        ops.into_iter()
            .map(|op| match op {
                Op::Package { request } => request,
                _ => panic!("unexpected op"),
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(intents(single), intents(batch));
    let structured = intents(run(
        r#"rb.package({ request = "github:owner/second@v2", asset = "second.zip", bins = { second = "bin/second" } })"#,
    ));
    assert_eq!(
        structured,
        intents(run(
            r#"rb.package("github:owner/second@v2", { asset = "second.zip", bins = { second = "bin/second" } })"#
        ))
    );
}

#[test]
fn invalid_batches_leave_operations_and_planned_bins_untouched() {
    let root = tempfile::tempdir().unwrap();
    let vm = vm_in(
        r#"
        local first = { request = "github:owner/tool@v1", bins = { batch_atomic_test_command = "bin/tool" } }
        ok, failure = pcall(function() rb.packages({ first, { request = "github:owner/tool@v1", asset = 42 } }) end)
        failure = tostring(failure)
        leaked = rb.which("batch_atomic_test_command")
    "#,
        root.path(),
    );
    assert!(!vm.lua.globals().get::<bool>("ok").unwrap());
    assert!(vm
        .lua
        .globals()
        .get::<String>("failure")
        .unwrap()
        .contains("entry 2"));
    assert!(vm
        .lua
        .globals()
        .get::<Option<String>>("leaked")
        .unwrap()
        .is_none());
    assert!(super::super::test_support::drain(vm).is_empty());
}

#[test]
fn rejects_sparse_mixed_lists_and_ambiguous_or_mistyped_specs() {
    for invalid in [
        r#"rb.packages({ [1] = "rg", [3] = "jq" })"#,
        r#"rb.packages({ "rg", name = "jq" })"#,
        r#"rb.packages({ [0] = "rg" })"#,
        r#"rb.package({ request = "rg", version = "1" })"#,
        r#"rb.package({ request = "rg", name = "ambiguous" })"#,
        r#"rb.package("github:owner/tool@v1", { assset = "typo" })"#,
        r#"rb.package("github:owner/tool@v1", { request = "ignored" })"#,
        r#"rb.package({ request = "rg", bins = {} })"#,
        r#"rb.package("rg", { asset = "rg.zip" })"#,
    ] {
        let root = tempfile::tempdir().unwrap();
        let vm = vm_in(
            &format!("ok = pcall(function() {invalid} end)"),
            root.path(),
        );
        assert!(!vm.lua.globals().get::<bool>("ok").unwrap(), "{invalid}");
        assert!(super::super::test_support::drain(vm).is_empty());
    }
}

#[test]
fn profile_path_helpers_work_before_any_package_is_declared() {
    let root = tempfile::tempdir().unwrap();
    let vm = vm_in(
        r#"
        directory = rb.bin_dir()
        path = rb.bin_path("fresh_bootstrap_test_command")
        installed = rb.which("fresh_bootstrap_test_command")
        for _, name in ipairs({ "", ".", "..", "/bin/sh", "../sh", "foo/bar", "foo\\bar" }) do
            assert(not pcall(rb.which, name))
            assert(not pcall(rb.bin_path, name))
        end
        assert(not pcall(rb.which, 42))
        assert(not pcall(rb.bin_path, 42))
    "#,
        root.path(),
    );
    assert_eq!(
        PathBuf::from(vm.lua.globals().get::<String>("directory").unwrap()),
        crate::env::bin_dir()
    );
    assert_eq!(
        PathBuf::from(vm.lua.globals().get::<String>("path").unwrap()),
        crate::env::bin_dir().join("fresh_bootstrap_test_command")
    );
    assert!(vm
        .lua
        .globals()
        .get::<Option<String>>("installed")
        .unwrap()
        .is_none());
    assert!(super::super::test_support::drain(vm).is_empty());
}

#[test]
fn exact_specs_source_selectors_and_other_resolvers_are_gone() {
    for (script, reason) in [
        (
            r#"rb.package({ name = "demo", version = "1", source = { file = "demo.tar.gz", sha256 = "abc" }, install = { archive = "tar.gz" }, bins = { demo = "bin/demo" } })"#,
            "package field `request` is required",
        ),
        (
            r#"rb.package("jq@1.8.2", { source = true })"#,
            "unknown package field `source`",
        ),
        (
            r#"rb.package({ request = "zlib", tag = "v1.3.2" })"#,
            "unknown package field `tag`",
        ),
        (
            r#"rb.package("brew:jq")"#,
            "the only places rb installs from",
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let vm = super::super::test_support::vm(root.path(), None);
        let error = vm
            .exec(&format!("local rb = require('rootbeer'); {script}"), "test")
            .unwrap_err();
        assert!(error.to_string().contains(reason), "{error}");
    }
}

#[test]
fn a_catalog_is_a_checkout_with_recipes_and_repositories_are_gone() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("packages")).unwrap();
    std::fs::write(root.path().join("host.lua"), "return {}").unwrap();
    let vm = vm_in(
        r#"rb.package_catalog("."); rb.package("demo")"#,
        root.path(),
    );
    assert_eq!(
        vm.lua
            .app_data_ref::<crate::lua::Checkout>()
            .map(|checkout| checkout.0.clone()),
        Some(root.path().join("."))
    );

    let elsewhere = vm_in(r#"rb.package_catalog("missing")"#, root.path());
    assert!(elsewhere
        .lua
        .app_data_ref::<crate::lua::Checkout>()
        .is_some());

    for (script, reason) in [
        (
            "rb.package_catalog('.'); rb.package_catalog('.')",
            "declare package_catalog once",
        ),
        (
            "rb.package_catalog('packages')",
            "isn't a checkout of recipes",
        ),
        (
            "rb.package_repository({ url = 'https://example.org', public_key = 'a' })",
            "signed repositories are gone",
        ),
    ] {
        let vm = super::super::test_support::vm(root.path(), None);
        let error = vm
            .exec(&format!("local rb = require('rootbeer'); {script}"), "test")
            .unwrap_err();
        assert!(error.to_string().contains(reason), "{error}");
    }
}
