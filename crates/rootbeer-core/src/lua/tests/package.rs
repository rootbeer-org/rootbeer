use std::path::PathBuf;

use crate::package::{ArchiveFormat, LockedInstall, LockedSource, PackageIntent, PackageRequest};
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
fn rb_package_pushes_realize_package_op() {
    let ops = run(r#"
        rb.package({
          name = "demo",
          version = "1.0.0",
          source = {
            file = "demo.tar.gz",
            sha256 = "abc123",
          },
          install = {
            archive = "tar.gz",
            strip_prefix = "demo",
          },
          apps = { ["Demo.app"] = "Demo.app" },
          bins = {
            demo = "bin/demo",
          },
        })
        "#);

    let [Op::Package { intent }] = ops.as_slice() else {
        panic!("expected one Package op, got {ops:?}");
    };
    let PackageIntent::Locked(package) = intent else {
        panic!("expected locked package intent, got {intent:?}");
    };

    assert_eq!(package.name, "demo");
    assert_eq!(package.version, "1.0.0");
    assert_eq!(
        package.provides.apps.get("Demo.app"),
        Some(&PathBuf::from("Demo.app"))
    );
    assert!(matches!(
        &package.source,
        LockedSource::File { path, sha256 }
            if path.ends_with("demo.tar.gz") && sha256 == "abc123"
    ));
    assert!(matches!(
        &package.install,
        LockedInstall::Archive {
            format: ArchiveFormat::TarGz,
            strip_prefix: Some(prefix),
        } if prefix == &PathBuf::from("demo")
    ));
    assert_eq!(
        package.provides.bins.get("demo"),
        Some(&PathBuf::from("bin/demo"))
    );
}

#[test]
fn rb_package_string_pushes_request_intent_without_resolving() {
    let ops = run(r#"
        rb.package("github:owner/tool@v1.0.0")
        "#);

    let [Op::Package { intent }] = ops.as_slice() else {
        panic!("expected one Package op, got {ops:?}");
    };

    assert_eq!(
        intent,
        &PackageIntent::Request(
            PackageRequest::new("owner/tool")
                .resolver("github")
                .version("v1.0.0")
        )
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
fn rb_which_reads_a_lock_from_before_the_signed_index() {
    let tmp = tempfile::tempdir().unwrap();
    let platform = format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS);
    let lock = serde_json::json!({
        "schema": 2,
        "resolutions": {
            "a": { "package": "ripgrep@15.2.0", "proof": { "system": platform } },
            "b": { "package": "ripgrep@14.0.0", "proof": { "system": "other-os" } },
        },
        "packages": {
            "ripgrep@15.2.0": { "provides": { "bins": { "rg": "bin/rg" } } },
            "ripgrep@14.0.0": { "provides": { "bins": { "ripgrep": "bin/ripgrep" } } },
        },
    });
    std::fs::write(tmp.path().join("rootbeer.lock"), lock.to_string()).unwrap();
    let vm = vm_in(
        r#"
        rb.package("ripgrep")
        rg = rb.which("rg")
        other = rb.which("ripgrep")
        "#,
        tmp.path(),
    );

    let rg: Option<String> = vm.lua.globals().get("rg").unwrap();
    let other: Option<String> = vm.lua.globals().get("other").unwrap();
    assert_eq!(
        rg.map(PathBuf::from),
        Some(crate::env::bin_dir().join("rg"))
    );
    assert_eq!(other, None);
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
        intent: PackageIntent::Request(request),
    }] = ops.as_slice()
    else {
        panic!("expected a package request");
    };
    assert_eq!(request.asset.as_deref(), Some("tool-darwin-arm64.tar.gz"));
    assert_eq!(request.bins["tool"], PathBuf::from("release/bin/tool"));
}

#[test]
fn accepts_raw_and_additional_archive_formats() {
    let ops = run(r#"
        for _, install in ipairs({ { binary = "bin/demo" }, { archive = "zip", directory = false }, { archive = "tar.xz" } }) do
            rb.package({ name = "demo", version = "1", source = { file = "asset", sha256 = "hash" },
                install = install, bins = { demo = "bin/demo" } })
        end
    "#);
    assert_eq!(ops.len(), 3);
}

#[test]
fn batches_and_structured_requests_match_individual_declarations() {
    let single = run(r#"
        rb.package("github:owner/first@v1")
        rb.package("github:owner/second@v2", { asset = "second.zip", bins = { second = "bin/second" } })
        rb.package({ name = "raw", version = "1", source = { file = "/raw.zip", sha256 = "hash" },
            install = { archive = "zip" }, bins = { raw = "bin/raw" } })
    "#);
    let batch = run(r#"
        rb.packages({
            "github:owner/first@v1",
            { request = "github:owner/second@v2", asset = "second.zip", bins = { second = "bin/second" } },
            { name = "raw", version = "1", source = { file = "/raw.zip", sha256 = "hash" },
                install = { archive = "zip" }, bins = { raw = "bin/raw" } },
        })
    "#);
    let intents = |ops: Vec<Op>| {
        ops.into_iter()
            .map(|op| match op {
                Op::Package { intent } => intent,
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
        r#"rb.package({ name = "raw", version = 1, source = { file = "tool", sha256 = "hash" }, install = { binary = "tool" }, bins = { tool = "tool" } })"#,
        r#"rb.package({ name = "raw", version = "1", source = { file = "tool", url = "https://example.com/tool", sha256 = "hash" }, install = { binary = "tool" }, bins = { tool = "tool" } })"#,
        r#"rb.package({ name = "raw", version = "1", source = { path = "tool", file = "tool", sha256 = "hash" }, install = { binary = "tool" }, bins = { tool = "tool" } })"#,
        r#"rb.package({ name = "raw", version = "1", source = { file = "tool", sha256 = "hash" }, install = { directory = true, archive = "zip" }, bins = { tool = "tool" } })"#,
        r#"rb.package({ name = "raw", version = "1", source = { file = "tool", sha256 = "hash" }, install = { binary = "tool", archive = "zip" }, bins = { tool = "tool" } })"#,
        r#"rb.package({ name = "raw", version = "1", source = { file = "tool", sha256 = "hash" }, install = { directory = "true" }, bins = { tool = "tool" } })"#,
        r#"rb.package({ name = "raw", version = "1", source = { file = "tool", sha256 = "hash" }, install = { binary = "tool" }, bins = { tool = 42 } })"#,
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
fn package_specs_preserve_pinned_runtime_dependencies() {
    let ops = run(r#"
        rb.package({ name = "demo", version = "1", source = { file = "package.tar.gz", sha256 = "source" },
            install = { archive = "tar.gz" }, bins = { demo = "bin/demo" },
            runtime_dependencies = { ["library@1"] = {
                name = "library", version = "1", source = { file = "runtime/library.tar.gz", sha256 = "source" },
                install = { archive = "tar.gz" }, bins = {}, output_sha256 = string.rep("a", 64),
            } },
        })
    "#);
    let [Op::Package {
        intent: PackageIntent::Locked(package),
    }] = ops.as_slice()
    else {
        panic!("expected locked package")
    };
    let dependency = &package.runtime_dependencies["library@1"];
    assert_eq!(
        dependency.output_sha256.as_deref(),
        Some("a".repeat(64).as_str())
    );
    assert!(
        matches!(&dependency.source, LockedSource::File { path, .. } if path.is_absolute() && path.ends_with("runtime/library.tar.gz"))
    );
}

#[test]
fn source_selections_are_explicit_planned_inputs() {
    let ops = run(r#"rb.package("jq@1.8.2", { source = true })
        rb.package("zlib", { head = true })
        rb.package({ request = "zlib", tag = "v1.3.2" })"#);
    let selections: Vec<_> = ops
        .iter()
        .map(|op| match op {
            Op::Package {
                intent: PackageIntent::Request(request),
            } => request.source.clone().unwrap(),
            _ => panic!("expected a source request"),
        })
        .collect();
    assert_eq!(
        selections,
        [
            crate::package::SourceSelection::Release,
            crate::package::SourceSelection::Head,
            crate::package::SourceSelection::Tag("v1.3.2".into())
        ]
    );
}

fn write_local_recipe(root: &std::path::Path) {
    std::fs::create_dir(root.join("packages")).unwrap();
    std::fs::write(
        root.join("packages/demo.lua"),
        format!(
            r#"return {{
        name = "demo", aliases = {{ "demo-alias" }},
        description = "Local test tool", homepage = "https://example.invalid/demo",
        default_license = "MIT",
        prebuilt = {{ github = "owner/demo", tag = "v{{version}}", asset = "demo.tar.gz" }},
        outputs = {{ bins = {{ "demo" }}, checks = {{ {{ "demo", "--version" }} }} }},
        platforms = {{ ["{system}"] = {{ default_version = "1" }} }},
        versions = {{ ["1"] = {{ digests = {{ ["{system}"] = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" }} }} }},
    }}"#,
            system = crate::package::ResolveContext::current().system
        ),
    )
    .unwrap();
}

#[test]
fn local_recipes_plan_without_resolution() {
    let root = tempfile::tempdir().unwrap();
    write_local_recipe(root.path());
    let vm = vm_in(
        r#"
        rb.package_catalog("packages")
        rb.packages({ "demo-alias", "registry-tool" })
    "#,
        root.path(),
    );
    assert_eq!(vm.drain_ops().len(), 2);
    assert!(!root.path().join("rootbeer.lock").exists());
}

#[test]
fn local_catalogs_reject_late_duplicate_and_invalid_declarations() {
    let root = tempfile::tempdir().unwrap();
    write_local_recipe(root.path());
    for script in [
        "rb.package('demo'); rb.package_catalog('packages')",
        "rb.package_catalog('packages'); rb.package_catalog('packages')",
        "rb.package_catalog('missing')",
    ] {
        let vm = super::super::test_support::vm(root.path(), None);
        assert!(vm
            .exec(&format!("local rb = require('rootbeer'); {script}"), "test")
            .is_err());
    }
    std::fs::write(
        root.path().join("packages/demo.lua"),
        "return { name = 'demo' }",
    )
    .unwrap();
    let vm = super::super::test_support::vm(root.path(), None);
    assert!(vm
        .exec(
            "local rb = require('rootbeer'); rb.package_catalog('packages')",
            "test"
        )
        .is_err());
}
