#![cfg(test)]

use rootbeer_drv::{Derivation, Platform};
use rootbeer_eval::{Catalog, Error, Host, Target};
use std::collections::BTreeMap;

const FIXTURES: [(&str, &str); 12] = [
    ("7zip.lua", include_str!("fixtures/7zip.lua")),
    ("auto.lua", include_str!("fixtures/auto.lua")),
    ("crab.lua", include_str!("fixtures/crab.lua")),
    ("finder.lua", include_str!("fixtures/finder.lua")),
    ("go.lua", include_str!("fixtures/go.lua")),
    ("gopher.lua", include_str!("fixtures/gopher.lua")),
    ("libz.lua", include_str!("fixtures/libz.lua")),
    ("lister.lua", include_str!("fixtures/lister.lua")),
    ("rust.lua", include_str!("fixtures/rust.lua")),
    ("tool.lua", include_str!("fixtures/tool.lua")),
    ("viewer.lua", include_str!("fixtures/viewer.lua")),
    ("ziggy.lua", include_str!("fixtures/ziggy.lua")),
];

fn hosts(system: &str) -> BTreeMap<Platform, Host> {
    let host = Host {
        system: system.to_string(),
    };

    [
        Platform::Aarch64Macos,
        Platform::Aarch64Linux,
        Platform::X86_64Linux,
    ]
    .map(|platform| (platform, host.clone()))
    .into()
}

fn build_keys(fixtures: &[(&str, &str)], system: &str) -> BTreeMap<String, String> {
    let catalog = Catalog::parse(fixtures.iter().copied()).unwrap();
    let graph = catalog
        .evaluate(&hosts(system), &catalog.targets())
        .unwrap();

    graph
        .packages
        .iter()
        .map(|(target, package)| (label(target), package.build.to_string()))
        .collect()
}

fn label(target: &Target) -> String {
    format!("{}@{}-{}", target.name, target.version, target.platform)
}

fn parse_error(source: &str) -> String {
    match Catalog::parse([("bad.lua", source)]) {
        Err(Error::Parse { message, .. }) => message,
        Err(error) => panic!("unexpected error: {error}"),
        Ok(_) => panic!("{source} parsed"),
    }
}

#[test]
fn fixtures_evaluate_to_the_pinned_derivations() {
    let catalog = Catalog::parse(FIXTURES).unwrap();
    let graph = catalog
        .evaluate(&hosts("builder-sha256:0000"), &catalog.targets())
        .unwrap();

    for (target, package) in &graph.packages {
        let snapshot = graph
            .derivations_of(package)
            .into_iter()
            .map(|(key, derivation)| format!("\n# {key}\n{derivation}"))
            .collect::<String>();

        let name = label(target);
        insta::with_settings!({
            omit_expression => true,
            prepend_module_to_snapshot => false
        }, {
            insta::assert_snapshot!(name, snapshot);
        });
    }
}

#[test]
fn metadata_never_reaches_a_key() {
    let edited = FIXTURES.map(|(file, source)| {
        let source = source
            .replace("Find entries", "Find files")
            .replace("\"MIT\"", "\"0BSD\"")
            .replace("{ \"fnd\" }", "{}");

        (file, source)
    });

    let edited = edited
        .iter()
        .map(|(file, source)| (*file, source.as_str()))
        .collect::<Vec<_>>();

    assert_eq!(build_keys(&FIXTURES, "a"), build_keys(&edited, "a"));
}

#[test]
fn host_system_rekeys_source_builds_but_not_prebuilts() {
    let before = build_keys(&FIXTURES, "builder-a");
    let after = build_keys(&FIXTURES, "builder-b");

    let unchanged = before
        .iter()
        .filter(|(target, key)| after.get(*target) == Some(key))
        .map(|(target, _)| target.split('@').next().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        unchanged,
        [
            "7zip", "7zip", "finder", "go", "lister", "rust", "tool", "tool", "viewer"
        ]
    );
}

#[test]
fn rust_and_go_builds_need_their_toolchain_to_vendor_with() {
    let gopher = FIXTURES[5].1.replace(
        r#"package = "go", version = "1.27.1""#,
        r#"package = "libz", version = "1.3.2""#,
    );
    let mut fixtures = FIXTURES;
    fixtures[5].1 = &gopher;
    let catalog = Catalog::parse(fixtures).unwrap();
    let target = Target {
        name: "gopher".to_string(),
        version: "0.4.0".to_string(),
        platform: Platform::X86_64Linux,
    };

    let error = catalog.evaluate(&hosts("a"), &[target]).unwrap_err();
    assert!(
        error
            .to_string()
            .ends_with("a go build needs go as a build dependency"),
        "{error}"
    );
}

#[test]
fn cycles_name_every_package_in_them() {
    let recipe = |name: &str, dependency: &str| {
        format!(
            r#"return {{
                name = "{name}", description = "", homepage = "", default_license = "MIT",
                source = {{ url = "https://example.com/{name}.tar.gz" }},
                build = {{ backend = "custom", steps = {{}},
                    dependencies = {{ {{ package = "{dependency}", version = "1", kind = "build" }} }} }},
                platforms = {{ ["x86_64-linux"] = {{ default_version = "1" }} }},
                versions = {{ ["1"] = {{ digests = {{ ["x86_64-linux"] = "{}" }} }} }},
            }}"#,
            "0".repeat(64)
        )
    };

    let (a, b) = (recipe("a", "b"), recipe("b", "a"));
    let catalog = Catalog::parse([("a.lua", a.as_str()), ("b.lua", b.as_str())]).unwrap();

    let target = Target {
        name: "a".to_string(),
        version: "1".to_string(),
        platform: Platform::X86_64Linux,
    };

    let error = catalog.evaluate(&hosts("a"), &[target]).unwrap_err();
    assert_eq!(error.to_string(), "dependency cycle: a@1 -> b@1 -> a@1");
}

#[test]
fn recipes_are_sandboxed_declarative_tables() {
    let unknown = FIXTURES[6].1.replace("steps = {", "stepz = {");
    assert!(parse_error(&unknown).contains("unknown field `stepz`"));

    assert!(parse_error("while true do end").contains("evaluation budget"));
    assert!(parse_error("local s = 'x' for _ = 1, 40 do s = s .. s end").contains("memory error"));
    assert!(parse_error("return os.getenv('HOME')").contains("attempt to index nil with 'getenv'"));
}

#[test]
fn zip_and_dmg_archives_need_7zip_to_extract() {
    let (crab, lister, viewer) = (2, 7, 10);
    let dependency = r#"{ package = "7zip", version = "26.04", kind = "build" },"#;
    let runtime = r#"{ package = "7zip", version = "26.04", kind = "runtime" },"#;
    let other = r#"{ package = "libz", version = "1.3.2", kind = "build" },"#;
    let cases = [
        (viewer, "a DMG prebuilt extracts with 7zip", other),
        (viewer, "a DMG prebuilt extracts with 7zip", runtime),
        (lister, "a zip prebuilt extracts with 7zip", other),
        (
            crab,
            "a zip source extracts with 7zip, so declare it in build.dependencies",
            "",
        ),
    ];

    for (index, expected, replacement) in cases {
        let edited = FIXTURES[index].1.replacen(dependency, replacement, 1);
        let mut fixtures = FIXTURES;
        fixtures[index].1 = &edited;

        let catalog = Catalog::parse(fixtures).unwrap();
        let error = catalog
            .evaluate(&hosts("a"), &catalog.targets())
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{replacement}: {error}");
    }

    let allowed = FIXTURES[viewer]
        .1
        .replace("prebuilt = {", r#"prebuilt = { allow = { "tmp" },"#);
    assert!(parse_error(&allowed).contains("unknown field `allow`"));
}

#[test]
fn host_references_override_per_platform_and_key_only_the_build() {
    let (gopher, tool) = (5, 9);
    let shared = r#"outputs = { host = { "libX11.so.6", "/bin/bash" },"#;
    let edited_tool = FIXTURES[tool]
        .1
        .replacen("outputs = {", shared, 1)
        .replace(
            r#"target = "darwin-arm64","#,
            r#"target = "darwin-arm64", outputs = { host = { "@rpath/libjvm.dylib" } },"#,
        )
        .replace(
            r#"target = "linux-amd64","#,
            r#"target = "linux-amd64", outputs = { host = { "libxcb.so.1" } },"#,
        );
    let edited_gopher = FIXTURES[gopher].1.replacen("outputs = {", shared, 1);

    let mut fixtures = FIXTURES;
    fixtures[tool].1 = &edited_tool;
    fixtures[gopher].1 = &edited_gopher;
    let catalog = Catalog::parse(fixtures).unwrap();
    let graph = catalog.evaluate(&hosts("a"), &catalog.targets()).unwrap();

    let host = |name: &str, platform: Platform| {
        let (_, package) = graph
            .packages
            .iter()
            .find(|(target, _)| target.name == name && target.platform == platform)
            .unwrap();
        match graph.derivations.get(&package.build) {
            Some(Derivation::Build(build)) => build.host.iter().cloned().collect::<Vec<_>>(),
            _ => panic!("{name} has no build"),
        }
    };

    assert_eq!(
        host("tool", Platform::Aarch64Macos),
        ["@rpath/libjvm.dylib"]
    );
    assert_eq!(host("tool", Platform::X86_64Linux), ["libxcb.so.1"]);
    assert_eq!(
        host("gopher", Platform::X86_64Linux),
        ["/bin/bash", "libX11.so.6"]
    );

    let acknowledging = graph
        .derivations
        .values()
        .filter(
            |derivation| matches!(derivation, Derivation::Build(build) if !build.host.is_empty()),
        )
        .count();
    assert_eq!(acknowledging, 3, "never the vendor build");

    for invalid in [r#""""#, r#""libX11.so.6\n/bin/sh""#] {
        let edited = FIXTURES[tool].1.replacen(
            "outputs = {",
            &format!("outputs = {{ host = {{ {invalid} }},"),
            1,
        );
        let mut fixtures = FIXTURES;
        fixtures[tool].1 = &edited;

        let catalog = Catalog::parse(fixtures).unwrap();
        let error = catalog
            .evaluate(&hosts("a"), &catalog.targets())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("invalid host") && error.contains("without control characters"),
            "{error}"
        );
    }
}
