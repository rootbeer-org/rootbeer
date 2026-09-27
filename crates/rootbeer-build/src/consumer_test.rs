use std::collections::BTreeMap;
use std::fs;

use rootbeer_package::*;

use crate::consumer::SourceResolver;

#[test]
fn builds_repository_recipes_and_local_recipes_that_depend_on_them() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let source = root.path().join("source");
    fs::create_dir_all(source.join("fixture")).unwrap();
    fs::write(
        source.join("fixture/configure"),
        r#"#!/bin/sh
set -eu
cat > Makefile <<'EOF'
all:
	printf '#!/bin/sh\nexit 0\n' > xz
	chmod +x xz
check: all
	./xz
install:
	mkdir -p "$(DESTDIR)/bin"
	cp xz "$(DESTDIR)/bin/xz"
EOF
"#,
    )
    .unwrap();
    let archive = root.path().join("source.tar.gz");
    crate::pack(&source, &archive).unwrap();
    let cached = download::DownloadCache::new(state.join("downloads"))
        .materialize(&format!("file://{}", archive.display()), None)
        .unwrap();
    let mut catalog = crate::test_catalog::catalog().clone();
    catalog.packages.retain(|name, _| name == "xz");
    let recipe = catalog
        .packages
        .get_mut("xz")
        .unwrap()
        .versions
        .get_mut("5.8.3")
        .unwrap();
    let platform = recipe
        .platforms
        .get_mut(&ResolveContext::current().system)
        .unwrap();
    platform.bins = rootbeer_package::Bins::Names(vec!["xz".into()]);
    platform.checks = vec![vec!["xz".into()]];
    let build = platform.build.as_mut().unwrap();
    build.url = "https://source.invalid/archive.tar.gz".into();
    build.sha256 = cached.sha256.clone();
    build.strip_prefix = "fixture".into();
    build.configure.clear();
    let repository = |catalog: &PackageCatalog, name: &str| {
        let mut inputs = PackageResolverInputs::default();
        let pin = crate::test_repository::publish(catalog, &root.path().join(name));
        inputs
            .resolvers
            .insert("rootbeer".into(), ResolverInput::Repository(pin));
        inputs
    };
    let context = ResolveContext::current();
    let inputs = repository(&catalog, "site");
    let pin = inputs.repository().cloned();
    let resolver = SourceResolver::with_inputs(&inputs, &state);
    let request = PackageRequest::parse("xz@source:5.8.3");
    let built = resolver.resolve(&request, &context).unwrap().unwrap();
    let ResolutionProof::SourceBuild(proof) = &built.proof else {
        panic!("expected a source build from the repository recipe");
    };
    assert_eq!(proof.source_sha256, cached.sha256);
    assert_eq!(proof.repository, pin);
    let repeated = resolver.resolve(&request, &context).unwrap().unwrap();
    let ResolutionProof::SourceBuild(repeated_proof) = repeated.proof else {
        panic!("expected source build");
    };
    assert_eq!(proof.build_key, repeated_proof.build_key);
    assert_eq!(built.package.output_sha256, repeated.package.output_sha256);

    let local_sha256 = catalog.sha256();
    let mut local_inputs = inputs.clone();
    local_inputs.resolvers.insert(
        "local".into(),
        ResolverInput::LocalCatalog(Box::new(catalog.clone())),
    );
    let local_resolution = SourceResolver::with_inputs(&local_inputs, &state)
        .resolve(&PackageRequest::parse("xz@5.8.3"), &context)
        .unwrap()
        .unwrap();
    let ResolutionProof::SourceBuild(local_proof) = &local_resolution.proof else {
        panic!("a local source recipe takes precedence over the repository");
    };
    assert_eq!(local_proof.repository, None);
    assert_eq!(
        local_proof.local_catalog_sha256.as_deref(),
        Some(local_sha256.as_str())
    );
    assert_eq!(
        local_resolution.package.output_sha256,
        built.package.output_sha256
    );

    let mut local_tool = catalog.packages["xz"].clone();
    local_tool.name = "local-tool".into();
    local_tool.aliases.clear();
    let platform = local_tool
        .versions
        .get_mut("5.8.3")
        .unwrap()
        .platforms
        .get_mut(&ResolveContext::current().system)
        .unwrap();
    platform.build.as_mut().unwrap().dependencies = vec![BuildDependency::from("xz@5.8.3")];
    let local = PackageCatalog {
        extra: Default::default(),
        packages: BTreeMap::from([("local-tool".into(), local_tool)]),
    };
    assert!(local.requires_pdr());
    let mut dependent_inputs = inputs.clone();
    dependent_inputs
        .resolvers
        .insert("local".into(), ResolverInput::LocalCatalog(Box::new(local)));
    let dependent = SourceResolver::with_inputs(&dependent_inputs, &state)
        .resolve(&PackageRequest::parse("local-tool"), &context)
        .unwrap()
        .unwrap();
    let ResolutionProof::SourceBuild(dependent_proof) = dependent.proof else {
        panic!("expected a local source build with a repository dependency");
    };
    assert_eq!(dependent_proof.repository, pin);
    assert!(dependent_proof.local_catalog_sha256.is_some());
    assert_eq!(dependent.package.name, "local-tool");

    let mut prebuilt = catalog.clone();
    let platform = prebuilt
        .packages
        .get_mut("xz")
        .unwrap()
        .versions
        .get_mut("5.8.3")
        .unwrap()
        .platforms
        .get_mut(&ResolveContext::current().system)
        .unwrap();
    platform.build = None;
    platform.source = Some("github:owner/xz@5.8.3".into());
    platform.asset = Some("xz-5.8.3.tar.gz".into());
    platform.sha256 = Some("d".repeat(64));
    let resolver = SourceResolver::with_inputs(&repository(&prebuilt, "prebuilt"), &state);
    assert!(resolver
        .resolve(&PackageRequest::parse("xz@HEAD"), &context)
        .unwrap_err()
        .contains("prebuilt-only"));
}
