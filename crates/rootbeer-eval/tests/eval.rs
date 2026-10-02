#![cfg(test)]

use rootbeer_drv::Platform;
use rootbeer_eval::{Catalog, Error, Host, Target};
use std::collections::BTreeMap;

const FIXTURES: [(&str, &str); 9] = [
    ("auto.lua", include_str!("fixtures/auto.lua")),
    ("crab.lua", include_str!("fixtures/crab.lua")),
    ("finder.lua", include_str!("fixtures/finder.lua")),
    ("go.lua", include_str!("fixtures/go.lua")),
    ("gopher.lua", include_str!("fixtures/gopher.lua")),
    ("libz.lua", include_str!("fixtures/libz.lua")),
    ("tool.lua", include_str!("fixtures/tool.lua")),
    ("viewer.lua", include_str!("fixtures/viewer.lua")),
    ("ziggy.lua", include_str!("fixtures/ziggy.lua")),
];

fn hosts(system: &str) -> BTreeMap<Platform, Host> {
    let host = Host {
        system: system.to_string(),
        rust: "1.98.1".to_string(),
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

    assert_eq!(unchanged, ["finder", "go", "tool", "tool", "viewer"]);
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
    let unknown = FIXTURES[5].1.replace("steps = {", "stepz = {");
    assert!(parse_error(&unknown).contains("unknown field `stepz`"));

    assert!(parse_error("while true do end").contains("evaluation budget"));
    assert!(parse_error("local s = 'x' for _ = 1, 40 do s = s .. s end").contains("memory error"));
    assert!(parse_error("return os.getenv('HOME')").contains("attempt to index nil with 'getenv'"));
}
