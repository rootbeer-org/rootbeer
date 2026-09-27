use super::*;
use std::fs;
use std::path::PathBuf;

fn catalog(system: &str) -> PackageCatalog {
    let mut catalog = PackageCatalog {
        extra: Default::default(),
        packages: BTreeMap::new(),
    };
    for name in ["root", "tool", "compiler", "runtime"] {
        let mut package = crate::test_catalog::catalog().packages["xz"].clone();
        package.name = name.into();
        package.aliases.clear();
        package.default_versions = BTreeMap::from([(system.to_string(), "1".to_string())]);
        let mut recipe = package.versions.remove("5.8.3").unwrap();
        recipe.platforms.retain(|platform, _| platform == system);
        let dependencies: Vec<BuildDependency> = match name {
            "root" => vec!["tool@1".into()],
            "tool" => vec![
                "compiler@1".into(),
                BuildDependency::Scoped {
                    package: "runtime@1".into(),
                    kind: DependencyKind::Runtime,
                },
            ],
            _ => vec![],
        };
        let platform = recipe.platforms.get_mut(system).unwrap();
        platform.bins = rootbeer_package::Bins::Names(vec![name.into()]);
        platform.checks = vec![vec![name.into()]];
        if name == "runtime" {
            platform.source = Some(format!("github:fixture/{name}@1"));
            platform.asset = Some(name.into());
            platform.build = None;
        } else {
            platform.build.as_mut().unwrap().dependencies = dependencies;
        }
        package.versions = BTreeMap::from([("1".into(), recipe)]);
        catalog.packages.insert(name.into(), package);
    }
    catalog
}

struct LocalBinaries(PathBuf);

impl PackageResolver for LocalBinaries {
    fn name(&self) -> &str {
        "github"
    }

    fn resolve(
        &self,
        request: &PackageRequest,
        _: &ResolveContext,
    ) -> Result<Option<PackageResolution>, String> {
        let name = request.name.strip_prefix("fixture/").unwrap();
        assert_eq!(
            name, "runtime",
            "source-capable dependencies must not resolve upstream binaries"
        );
        Ok(Some(PackageResolution::new(
            LockedPackage {
                name: name.into(),
                version: "1".into(),
                source: LockedSource::File {
                    path: self.0.clone(),
                    sha256: rootbeer_store::hash_file(&self.0).unwrap(),
                },
                install: LockedInstall::Binary { path: name.into() },
                provides: Provides {
                    bins: BTreeMap::from([(name.into(), name.into())]),
                    apps: BTreeMap::new(),
                },
                runtime_dependencies: BTreeMap::new(),
                output_sha256: None,
            },
            ResolutionProof::GitRelease(GitReleaseProof {
                host: "github.com".into(),
                owner: "fixture".into(),
                repo: name.into(),
                tag: Some("1".into()),
                target_commit: None,
                release_id: None,
                documents: vec![],
            }),
        )))
    }
}

#[test]
fn source_dependencies_build_and_reuse_our_outputs_despite_upstream_binaries() {
    let root = tempfile::tempdir().unwrap();
    let system = ResolveContext::current().system;
    let mut catalog = catalog(&system);
    let source = root.path().join("source");
    fs::create_dir_all(source.join("fixture")).unwrap();
    fs::write(source.join("fixture/input"), "source").unwrap();
    let archive = root.path().join("source.tar.gz");
    crate::pack(&source, &archive).unwrap();
    let downloads = root.path().join("downloads");
    let cached = download::DownloadCache::new(&downloads)
        .materialize(&format!("file://{}", archive.display()), None)
        .unwrap();
    for name in ["root", "tool", "compiler"] {
        let platform = catalog
            .packages
            .get_mut(name)
            .unwrap()
            .versions
            .get_mut("1")
            .unwrap()
            .platforms
            .get_mut(&system)
            .unwrap();
        let build = platform.build.as_mut().unwrap();
        build.backend = BuildBackend::Custom;
        build.configure.clear();
        build.url = "https://source.invalid/archive.tar.gz".into();
        build.sha256 = cached.sha256.clone();
        build.strip_prefix = "fixture".into();
        build.steps = Some(BuildSteps {
            configure: vec![],
            build: if name == "root" { vec![vec!["tool".into()]] } else { vec![vec!["sh".into(), "-c".into(), "exit 0".into()]] },
            check: vec![vec!["sh".into(), "-c".into(), "exit 0".into()]],
            install: vec![vec!["sh".into(), "-c".into(), format!("mkdir -p \"$1/bin\"; printf '#!/bin/sh\\nexit 0\\n' > \"$1/bin/{name}\"; chmod +x \"$1/bin/{name}\""), "install".into(), "{prefix}".into()]],
        });
    }
    catalog.validate().unwrap();
    let graph = DependencyGraph::new(&catalog, &["root".into()], &system).unwrap();
    assert_eq!(graph.order, ["compiler@1", "runtime@1", "tool@1", "root@1"]);
    assert_eq!(graph.nodes["tool@1"].runtime_closure, ["runtime@1"]);
    assert!(graph.nodes["root@1"].exports.contains_key("compiler@1"));
    let binary = root.path().join("binary");
    fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
    let mut backends = ResolverStack::new();
    backends.push(LocalBinaries(binary));
    let inputs = PackageResolverInputs {
        resolvers: BTreeMap::from([(
            "rootbeer".into(),
            ResolverInput::Catalog {
                sha256: catalog.sha256(),
            },
        )]),
    };
    let plan = BuildPlan::from_graph(&catalog, graph, &inputs, backends).unwrap();
    assert_eq!(
        plan.binaries.keys().map(String::as_str).collect::<Vec<_>>(),
        ["runtime@1"]
    );
    let options = BuildOptions {
        downloads,
        cache: Some(crate::BuildCache {
            directory: root.path().join("cache"),
            context: "source-dependency-test".into(),
            recheck: false,
        }),
        ..Default::default()
    };
    let artifact = plan.execute(&root.path().join("output"), &options).unwrap();
    assert!(artifact.dependencies["tool@1"]
        .runtime_dependencies
        .contains_key("runtime@1"));
    assert!(artifact.dependencies.contains_key("compiler@1"));
    for name in ["tool", "compiler"] {
        assert!(root
            .path()
            .join(format!("output/dependency-{name}@1/receipt.json"))
            .exists());
        assert!(matches!(
            artifact.dependencies[&format!("{name}@1")].source,
            LockedSource::File { .. }
        ));
    }
    let repeated = plan
        .execute(&root.path().join("repeated"), &options)
        .unwrap();
    assert_eq!(artifact.build_key, repeated.build_key);
    for name in ["tool", "compiler"] {
        assert!(!root
            .path()
            .join(format!("repeated/dependency-{name}@1/build.log"))
            .exists());
        assert_eq!(
            artifact.dependencies[&format!("{name}@1")].output_sha256,
            repeated.dependencies[&format!("{name}@1")].output_sha256
        );
    }
}

#[test]
fn source_roots_and_dependencies_keep_source_edges() {
    let system = ResolveContext::current().system;
    let catalog = catalog(&system);
    let graph = DependencyGraph::new(&catalog, &["tool".into()], &system).unwrap();
    assert_eq!(graph.order, ["compiler@1", "runtime@1", "tool@1"]);
    let graph = DependencyGraph::new(&catalog, &["root".into()], &system).unwrap();
    assert_eq!(graph.order, ["compiler@1", "runtime@1", "tool@1", "root@1"]);
    let inputs = PackageResolverInputs {
        resolvers: BTreeMap::from([(
            "rootbeer".into(),
            ResolverInput::Catalog {
                sha256: catalog.sha256(),
            },
        )]),
    };
    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("binary");
    fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
    let mut backends = ResolverStack::new();
    backends.push(LocalBinaries(binary));
    let plan = BuildPlan::from_graph(&catalog, graph, &inputs, backends).unwrap();
    assert_eq!(
        plan.binaries.keys().map(String::as_str).collect::<Vec<_>>(),
        ["runtime@1"]
    );
}

#[test]
fn source_libraries_preserve_transitive_link_exports() {
    let system = ResolveContext::current().system;
    let mut catalog = catalog(&system);
    let platform = catalog
        .packages
        .get_mut("tool")
        .unwrap()
        .versions
        .get_mut("1")
        .unwrap()
        .platforms
        .get_mut(&system)
        .unwrap();
    let build = platform.build.as_mut().unwrap();
    build.libraries = vec!["lib/libtool.a".into()];
    build.dependencies[0] = BuildDependency::Scoped {
        package: "compiler@1".into(),
        kind: DependencyKind::Link,
    };
    let platform = catalog
        .packages
        .get_mut("compiler")
        .unwrap()
        .versions
        .get_mut("1")
        .unwrap()
        .platforms
        .get_mut(&system)
        .unwrap();
    platform.build.as_mut().unwrap().libraries = vec!["lib/libbase.a".into()];
    let graph = DependencyGraph::new(&catalog, &["root".into()], &system).unwrap();
    assert_eq!(graph.order, ["compiler@1", "runtime@1", "tool@1", "root@1"]);
    assert_eq!(
        graph.nodes["root@1"].exports["compiler@1"].libraries,
        vec![PathBuf::from("lib/libbase.a")]
    );
}
