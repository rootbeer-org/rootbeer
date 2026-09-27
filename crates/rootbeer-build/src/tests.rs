use rootbeer_package::graph::DependencyGraph;

use std::os::unix::fs::PermissionsExt;

use super::*;

fn source_catalog() -> PackageCatalog {
    let catalog = crate::test_catalog::catalog();
    PackageCatalog {
        extra: Default::default(),
        packages: BTreeMap::from([("xz".into(), catalog.packages["xz"].clone())]),
    }
}

#[test]
fn validates_backend_options_and_patch_inputs() {
    let catalog = source_catalog();
    let mut build = catalog.packages["xz"].versions["5.8.3"].platforms
        [&ResolveContext::current().system]
        .build
        .clone()
        .unwrap();
    build.args = vec!["-Doptimize=ReleaseFast".into()];
    assert!(build.validate().unwrap_err().contains("Autotools"));
    build.backend = BuildBackend::Zig;
    build.configure.clear();
    assert!(build.validate().is_ok());
    for argument in ["--prefix=/tmp", "--global-cache-dir=/tmp", "build", "-D"] {
        build.args = vec![argument.into()];
        assert!(build.validate().is_err());
    }
    build.args.clear();
    build.patches = vec![String::new()];
    assert!(build.validate().unwrap_err().contains("patches"));
}

#[test]
fn build_graph_orders_dependencies_once_and_rejects_cycles() {
    let mut catalog = source_catalog();
    let mut dependency = catalog.packages["xz"].clone();
    dependency.name = "build-tool".into();
    catalog.packages.insert("build-tool".into(), dependency);
    let platform = catalog
        .packages
        .get_mut("xz")
        .unwrap()
        .versions
        .get_mut("5.8.3")
        .unwrap()
        .platforms
        .get_mut(&ResolveContext::current().system)
        .unwrap();
    platform.build.as_mut().unwrap().dependencies = vec!["build-tool@5.8.3".into()];
    catalog.validate().unwrap();
    let order = DependencyGraph::new(&catalog, &["xz".into()], &ResolveContext::current().system)
        .unwrap()
        .order;
    assert_eq!(order, ["build-tool@5.8.3", "xz@5.8.3"]);
    let platform = catalog
        .packages
        .get_mut("build-tool")
        .unwrap()
        .versions
        .get_mut("5.8.3")
        .unwrap()
        .platforms
        .get_mut(&ResolveContext::current().system)
        .unwrap();
    platform.build.as_mut().unwrap().dependencies = vec!["xz@5.8.3".into()];
    assert!(catalog.validate().unwrap_err().contains("cycle"));
}

#[test]
fn invalid_build_inputs_fail_before_creating_output() {
    let mut catalog = source_catalog();
    let platform = catalog
        .packages
        .get_mut("xz")
        .unwrap()
        .versions
        .get_mut("5.8.3")
        .unwrap()
        .platforms
        .get_mut(&ResolveContext::current().system)
        .unwrap();
    let build = platform.build.as_mut().unwrap();
    build.strip_prefix = "../outside".into();
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("output");
    assert!(
        build_package(&catalog, "xz", &output, &BuildOptions::default())
            .unwrap_err()
            .contains("strip_prefix")
    );
    assert!(!output.exists());
    let mut catalog = source_catalog();
    let platform = catalog
        .packages
        .get_mut("xz")
        .unwrap()
        .versions
        .get_mut("5.8.3")
        .unwrap()
        .platforms
        .get_mut(&ResolveContext::current().system)
        .unwrap();
    platform.build.as_mut().unwrap().dependencies = vec!["missing@1".into()];
    assert!(catalog
        .validate()
        .unwrap_err()
        .contains("canonical package name"));
    assert!(build_package(
        &source_catalog(),
        "xz",
        &output,
        &BuildOptions {
            jobs: 0,
            ..BuildOptions::default()
        }
    )
    .is_err());
    assert!(!output.exists());
}

#[test]
fn pinned_build_rejects_an_unpinned_catalog_before_io() {
    let catalog = source_catalog();
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("output");
    let inputs = PackageResolverInputs::default();
    assert!(BuildPlan::resolve(&catalog, "xz", &inputs)
        .and_then(|plan| plan.execute(&output, &BuildOptions::default()))
        .unwrap_err()
        .contains("pin the current catalog"));
    assert!(!output.exists());
}

#[test]
fn source_packages_do_not_implicitly_compile_during_resolution() {
    let inputs = PackageResolverInputs::default();
    let resolver = rootbeer_package::catalog::CatalogResolver::new(
        &source_catalog(),
        &inputs,
        rootbeer_package::backend_stack(),
    );
    let error = rootbeer_package::PackageResolver::resolve(
        &resolver,
        &PackageRequest::parse("xz"),
        &ResolveContext::current(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("rootbeer-forge build"));
}

#[test]
fn archive_roundtrip_preserves_contents_and_symlinks() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("prefix");
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(root.join("bin/tool"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(root.join("bin/tool"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("tool", root.join("bin/alias")).unwrap();
    let first = directory.path().join("one.tar.gz");
    let second = directory.path().join("two.tar.gz");
    pack(&root, &first).unwrap();
    pack(&root, &second).unwrap();
    assert_eq!(hash_file(&first).unwrap(), hash_file(&second).unwrap());
    let extracted = directory.path().join("extracted");
    fs::create_dir(&extracted).unwrap();
    rootbeer_package::realize::extract_archive(&first, ArchiveFormat::TarGz, &extracted).unwrap();
    assert_eq!(hash_tree(&root).unwrap(), hash_tree(&extracted).unwrap());
}

#[test]
fn steps_use_only_the_declared_environment_and_fail_on_errors_or_timeouts() {
    let directory = tempfile::tempdir().unwrap();
    let log = directory.path().join("env.log");
    let environment = BTreeMap::from([("ROOTBEER_BUILD_TEST", "set".into())]);
    run(
        &["/usr/bin/env".into()],
        directory.path(),
        &environment,
        &log,
        Duration::from_secs(1),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(&log).unwrap().trim(),
        "ROOTBEER_BUILD_TEST=set"
    );
    assert!(run(
        &["/bin/sh".into(), "-c".into(), "exit 7".into()],
        directory.path(),
        &environment,
        &log,
        Duration::from_secs(1)
    )
    .unwrap_err()
    .contains("failed"));
    assert!(run(
        &["/bin/sleep".into(), "10".into()],
        directory.path(),
        &environment,
        &log,
        Duration::from_millis(10)
    )
    .unwrap_err()
    .contains("time limit"));
}

#[test]
fn compiles_cached_sources_and_produces_an_installable_artifact() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    fs::create_dir_all(source.join("fixture")).unwrap();
    fs::write(
        source.join("fixture/configure"),
        r#"#!/bin/sh
set -eu
fixture-tool
cat > Makefile <<'EOF'
all:
	printf '#!/bin/sh\nexit 0\n' > fixture
	chmod +x fixture
check: all
	./fixture
install:
	mkdir -p "$(DESTDIR)/bin"
	cp fixture "$(DESTDIR)/bin/fixture"
EOF
"#,
    )
    .unwrap();
    let archive = directory.path().join("source.tar.gz");
    pack(&source, &archive).unwrap();
    let downloads = DownloadCache::new(directory.path().join("downloads"));
    let cached = downloads
        .materialize(&format!("file://{}", archive.display()), None)
        .unwrap();
    let mut catalog = source_catalog();
    let package = catalog.packages.remove("xz").unwrap();
    let mut package = CatalogPackage {
        name: "fixture".into(),
        ..package
    };
    let platform = package
        .versions
        .get_mut("5.8.3")
        .unwrap()
        .platforms
        .get_mut(&ResolveContext::current().system)
        .unwrap();
    platform.bins = rootbeer_package::Bins::Names(vec!["fixture".into()]);
    platform.checks = vec![vec!["fixture".into()]];
    let build = platform.build.as_mut().unwrap();
    build.url = "https://source.invalid/fixture.tar.gz".into();
    build.sha256 = cached.sha256.clone();
    build.strip_prefix = "fixture".into();
    build.configure.clear();
    build.patches = vec![r#"--- a/configure
+++ b/configure
@@ -1,4 +1,5 @@
 #!/bin/sh
 set -eu
 fixture-tool
+printf 'source-patch-applied\n'
 cat > Makefile <<'EOF'
"#
    .into()];
    catalog.packages.insert("fixture".into(), package);
    catalog.validate().unwrap();
    let tools = directory.path().join("tools");
    fs::create_dir(&tools).unwrap();
    fs::write(tools.join("fixture-tool"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(
        tools.join("fixture-tool"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let output = directory.path().join("output");
    fs::create_dir(&output).unwrap();
    let inputs = PackageResolverInputs {
        resolvers: BTreeMap::from([(
            "rootbeer".into(),
            ResolverInput::Catalog {
                sha256: catalog.sha256(),
            },
        )]),
    };
    let plan = BuildPlan::resolve(&catalog, "fixture", &inputs).unwrap();
    let opts = BuildOptions {
        downloads: directory.path().join("downloads"),
        jobs: 1,
        ..BuildOptions::default()
    };
    let environment = environment::Environment::resolve(None).unwrap();
    let host_tools = directory.path().join("host-tools");
    environment.stage(&host_tools).unwrap();
    let artifact = compile(
        &plan,
        "fixture@5.8.3",
        BTreeMap::new(),
        &tools,
        &output,
        &opts,
        (&environment, &host_tools, &BTreeMap::new()),
    )
    .unwrap();
    write_install_files(&artifact, &output).unwrap();
    let realizer = PackageRealizer::with_dirs(
        Store::new(directory.path().join("store")),
        directory.path().join("downloads"),
        directory.path().join("tmp"),
    );
    let realized = realizer.realize(&artifact.package).unwrap();
    assert!(Command::new(&realized.bins["fixture"])
        .status()
        .unwrap()
        .success());
    assert!(output.join("install.lua").is_file());
    assert!(artifact.toolchain.contains_key("/usr/bin/cc"));
    assert_eq!(artifact.build.patches.len(), 1);
    assert!(fs::read_to_string(output.join("build.log"))
        .unwrap()
        .contains("source-patch-applied\n"));

    fs::create_dir(source.join("tool")).unwrap();
    let configure = fs::read_to_string(source.join("fixture/configure"))
        .unwrap()
        .replace("fixture-tool\n", "")
        .replace("fixture", "fixture-tool");
    fs::write(source.join("tool/configure"), configure).unwrap();
    pack(&source, &archive).unwrap();
    let cached = downloads
        .materialize(&format!("file://{}", archive.display()), None)
        .unwrap();
    let mut dependency = catalog.packages["fixture"].clone();
    dependency.name = "tool".into();
    let platform = dependency
        .versions
        .get_mut("5.8.3")
        .unwrap()
        .platforms
        .get_mut(&ResolveContext::current().system)
        .unwrap();
    platform.bins = rootbeer_package::Bins::Names(vec!["fixture-tool".into()]);
    platform.checks = vec![vec!["fixture-tool".into()]];
    let build = platform.build.as_mut().unwrap();
    build.sha256 = cached.sha256.clone();
    build.strip_prefix = "tool".into();
    build.patches.clear();
    catalog.packages.insert("tool".into(), dependency);
    let platform = catalog
        .packages
        .get_mut("fixture")
        .unwrap()
        .versions
        .get_mut("5.8.3")
        .unwrap()
        .platforms
        .get_mut(&ResolveContext::current().system)
        .unwrap();
    platform.build.as_mut().unwrap().dependencies = vec!["tool@5.8.3".into()];
    let inputs = PackageResolverInputs {
        resolvers: BTreeMap::from([(
            "rootbeer".into(),
            rootbeer_package::ResolverInput::Catalog {
                sha256: catalog.sha256(),
            },
        )]),
    };
    let output = directory.path().join("build-with-inputs");
    let artifact = BuildPlan::resolve(&catalog, "fixture", &inputs)
        .unwrap()
        .execute(&output, &opts)
        .unwrap();
    assert_eq!(artifact.resolver_inputs, inputs);
    let dependency: BuildArtifact = serde_json::from_slice(
        &fs::read(output.join("dependency-tool@5.8.3/receipt.json")).unwrap(),
    )
    .unwrap();
    let receipt: BuildArtifact =
        serde_json::from_slice(&fs::read(output.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(receipt.resolver_inputs, inputs);
    assert_eq!(dependency.resolver_inputs, inputs);
    assert_eq!(receipt.dependencies["tool@5.8.3"], dependency.package);

    let cache_directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let cache = BuildCache {
        directory: PathBuf::from(cache_directory.path().file_name().unwrap()),
        context: "test-host-v1".into(),
        recheck: false,
    };
    let first = directory.path().join("cached-first");
    let second = directory.path().join("cached-second");
    let first_artifact = BuildPlan::resolve(&catalog, "fixture", &inputs)
        .unwrap()
        .execute(
            &first,
            &BuildOptions {
                cache: Some(cache.clone()),
                ..opts.clone()
            },
        )
        .unwrap();
    assert!(first.join("build.log").is_file());
    assert!(first.join("dependency-tool@5.8.3/build.log").is_file());
    let second_artifact = BuildPlan::resolve(&catalog, "fixture", &inputs)
        .unwrap()
        .execute(
            &second,
            &BuildOptions {
                jobs: 3,
                cache: Some(cache.clone()),
                ..opts.clone()
            },
        )
        .unwrap();
    assert_eq!(
        first_artifact.package.output_sha256,
        second_artifact.package.output_sha256
    );
    assert!(!second.join("build.log").exists());
    assert!(!second.join("dependency-tool@5.8.3/build.log").exists());
    assert!(second.join("install.lua").is_file());
    assert!(second.join("receipt.json").is_file());
    let tool = directory.path().join("cached-tool");
    BuildPlan::resolve(&catalog, "tool", &inputs)
        .unwrap()
        .execute(
            &tool,
            &BuildOptions {
                cache: Some(cache.clone()),
                ..opts.clone()
            },
        )
        .unwrap();
    assert!(!tool.join("build.log").exists());

    let changed = BuildCache {
        context: "test-host-v2".into(),
        ..cache.clone()
    };
    let rebuilt = directory.path().join("new-environment");
    BuildPlan::resolve(&catalog, "tool", &inputs)
        .unwrap()
        .execute(
            &rebuilt,
            &BuildOptions {
                cache: Some(changed),
                ..opts.clone()
            },
        )
        .unwrap();
    assert!(rebuilt.join("build.log").exists());
    let refreshed = directory.path().join("recheck");
    BuildPlan::resolve(&catalog, "tool", &inputs)
        .unwrap()
        .execute(
            &refreshed,
            &BuildOptions {
                cache: Some(BuildCache {
                    recheck: true,
                    ..cache.clone()
                }),
                ..opts.clone()
            },
        )
        .unwrap();
    assert!(refreshed.join("build.log").exists());

    let session = BuildSession::default();
    for (name, request, should_build, should_build_dependency) in [
        ("session-first", "tool", true, false),
        ("session-consumer", "fixture", true, false),
        ("session-repeat", "fixture", false, false),
    ] {
        let output = directory.path().join(name);
        BuildPlan::resolve(&catalog, request, &inputs)
            .unwrap()
            .execute(
                &output,
                &BuildOptions {
                    cache: Some(BuildCache {
                        recheck: true,
                        ..cache.clone()
                    }),
                    session: Some(session.clone()),
                    ..opts.clone()
                },
            )
            .unwrap();
        assert_eq!(output.join("build.log").exists(), should_build);
        assert_eq!(
            output.join("dependency-tool@5.8.3/build.log").exists(),
            should_build_dependency
        );
    }
    let output = directory.path().join("new-session");
    BuildPlan::resolve(&catalog, "tool", &inputs)
        .unwrap()
        .execute(
            &output,
            &BuildOptions {
                cache: Some(BuildCache {
                    recheck: true,
                    ..cache.clone()
                }),
                session: Some(BuildSession::default()),
                ..opts.clone()
            },
        )
        .unwrap();
    assert!(output.join("build.log").is_file());

    let mut failing_catalog = catalog.clone();
    let platform = failing_catalog
        .packages
        .get_mut("fixture")
        .unwrap()
        .versions
        .get_mut("5.8.3")
        .unwrap()
        .platforms
        .get_mut(&ResolveContext::current().system)
        .unwrap();
    platform.build.as_mut().unwrap().patches = vec!["invalid patch".into()];
    let failing_inputs = PackageResolverInputs {
        resolvers: BTreeMap::from([(
            "rootbeer".into(),
            ResolverInput::Catalog {
                sha256: failing_catalog.sha256(),
            },
        )]),
    };
    let entries = fs::read_dir(cache.directory.join("results"))
        .unwrap()
        .count();
    let failed = directory.path().join("failed-build");
    assert!(
        BuildPlan::resolve(&failing_catalog, "fixture", &failing_inputs)
            .unwrap()
            .execute(
                &failed,
                &BuildOptions {
                    cache: Some(cache.clone()),
                    ..opts.clone()
                }
            )
            .is_err()
    );
    assert_eq!(
        fs::read_dir(cache.directory.join("results"))
            .unwrap()
            .count(),
        entries
    );
    assert!(!failed.join("dependency-tool@5.8.3/build.log").exists());

    let plan = BuildPlan::resolve(&catalog, "tool", &inputs).unwrap();
    let frozen = serde_json::to_value(&plan).unwrap();
    catalog
        .packages
        .get_mut("tool")
        .unwrap()
        .description
        .push_str(" changed after planning");
    assert_eq!(serde_json::to_value(&plan).unwrap(), frozen);
    let snapshot = directory.path().join("frozen-plan");
    let snapshot_artifact = plan
        .execute(
            &snapshot,
            &BuildOptions {
                cache: Some(cache.clone()),
                ..opts.clone()
            },
        )
        .unwrap();
    assert_eq!(
        snapshot_artifact.catalog_sha256,
        inputs.catalog_sha256().unwrap()
    );
    assert_eq!(
        snapshot_artifact.build_environment.as_deref(),
        Some("test-host-v1")
    );
    assert!(snapshot_artifact.build_key.is_some());
    assert!(!snapshot.join("build.log").exists());
    let description = &mut catalog.packages.get_mut("tool").unwrap().description;
    description.truncate(description.len() - " changed after planning".len());
    let mut specification = BuildEnvironment {
        tools: environment
            .lock
            .tools
            .iter()
            .map(|(name, input)| (name.clone(), input.path.clone()))
            .collect(),
        ..Default::default()
    };
    for tool in ["cat", "chmod", "mkdir", "cp"] {
        let path = ["/usr/bin", "/bin"]
            .iter()
            .map(|root| PathBuf::from(root).join(tool))
            .find(|path| path.is_file())
            .unwrap();
        specification.tools.insert(tool.into(), path);
    }
    let sdk = directory.path().join("sdk");
    fs::create_dir(&sdk).unwrap();
    fs::write(sdk.join("header.h"), "#define VERSION 1\n").unwrap();
    specification.inputs.insert("sdk".into(), sdk.clone());
    let mut pinned_opts = BuildOptions {
        environment: Some(specification.pin().unwrap()),
        cache: Some(cache.clone()),
        ..opts.clone()
    };
    let pinned_plan = BuildPlan::resolve(&catalog, "fixture", &inputs).unwrap();
    let pinned_first = pinned_plan
        .execute(&directory.path().join("pinned-first"), &pinned_opts)
        .unwrap();
    let reused = directory.path().join("pinned-reused");
    let pinned_second = pinned_plan.execute(&reused, &pinned_opts).unwrap();
    assert_eq!(pinned_first.build_key, pinned_second.build_key);
    assert_eq!(pinned_first.environment, pinned_opts.environment);
    assert!(!reused.join("build.log").exists());
    fs::write(sdk.join("header.h"), "#define VERSION 2\n").unwrap();
    let stale_output = directory.path().join("pinned-stale");
    assert!(pinned_plan
        .execute(&stale_output, &pinned_opts)
        .unwrap_err()
        .contains("changed"));
    assert!(!stale_output.exists());
    pinned_opts.environment = Some(specification.pin().unwrap());
    let changed = directory.path().join("pinned-sdk-changed");
    let changed_artifact = pinned_plan.execute(&changed, &pinned_opts).unwrap();
    assert_ne!(pinned_first.build_key, changed_artifact.build_key);
    assert!(changed.join("build.log").exists());
    specification
        .variables
        .insert("CFLAGS".into(), "-O2".into());
    pinned_opts.environment = Some(specification.pin().unwrap());
    let changed_flags = pinned_plan
        .execute(&directory.path().join("pinned-flags-changed"), &pinned_opts)
        .unwrap();
    assert_ne!(changed_artifact.build_key, changed_flags.build_key);
    let mutating_make = directory.path().join("mutating-make");
    fs::write(&mutating_make, "#!/bin/sh\nif [ \"$1\" != --version ]; then printf changed > \"$MUTATION_TARGET\"; fi\nexec /usr/bin/make \"$@\"\n").unwrap();
    fs::set_permissions(&mutating_make, fs::Permissions::from_mode(0o755)).unwrap();
    specification.tools.insert("make".into(), mutating_make);
    specification.variables.insert(
        "MUTATION_TARGET".into(),
        sdk.join("header.h").to_string_lossy().into_owned(),
    );
    pinned_opts.environment = Some(specification.pin().unwrap());
    let cache_entries = || {
        fs::read_dir(cache.directory.join("results"))
            .unwrap()
            .count()
    };
    let before = cache_entries();
    assert!(pinned_plan
        .execute(
            &directory.path().join("pinned-mutated-during-build"),
            &pinned_opts
        )
        .unwrap_err()
        .contains("changed"));
    assert_eq!(cache_entries(), before);
    let key = cache::key(
        "tool@5.8.3",
        catalog.packages["tool"].versions["5.8.3"].revision,
        &catalog.packages["tool"].versions["5.8.3"].platforms[&ResolveContext::current().system],
        &ResolveContext::current().system,
        &BTreeMap::new(),
        &environment
            .identity(&format!("{}\0host", cache.context))
            .unwrap(),
        &BTreeMap::new(),
    )
    .unwrap();
    fs::write(
        cache
            .directory
            .join("results")
            .join(key)
            .join("package.tar.gz"),
        "corrupted",
    )
    .unwrap();
    let error = BuildPlan::resolve(&catalog, "tool", &inputs)
        .unwrap()
        .execute(
            &directory.path().join("corrupted-cache"),
            &BuildOptions {
                cache: Some(cache),
                ..opts
            },
        )
        .unwrap_err();
    assert!(error.contains("hash mismatch"));
}

#[test]
fn isolated_build_requires_a_lock_before_creating_output() {
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("output");
    let error = build_package(
        &source_catalog(),
        "xz",
        &output,
        &BuildOptions {
            is_isolated: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(error.contains("pinned environment"));
    assert!(!output.exists());
}

#[test]
fn isolated_builds_and_cache_hits_use_distinct_policy_keys() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let secret = root.join("secret");
    fs::write(&secret, "secret\n").unwrap();
    let source = root.join("sources");
    fs::create_dir_all(source.join("fixture")).unwrap();
    fs::write(source.join("fixture/input"), "source\n").unwrap();
    fs::write(
        source.join("fixture/main.c"),
        "int answer(void) { return 42; }\n",
    )
    .unwrap();
    let archive = root.join("source.tar.gz");
    pack(&source, &archive).unwrap();
    let downloads = root.join("downloads");
    let downloaded = DownloadCache::new(&downloads)
        .materialize(&format!("file://{}", archive.display()), None)
        .unwrap();
    let mut catalog = source_catalog();
    let package = catalog.packages.get_mut("xz").unwrap();
    let recipe = package.versions.get_mut("5.8.3").unwrap();
    let isolation_build: rootbeer_package::SourceBuild = serde_json::from_value(serde_json::json!({
        "backend": "custom", "url": "https://source.invalid/isolation.tar.gz",
        "sha256": downloaded.sha256, "archive": "tar.gz", "strip_prefix": "fixture",
        "steps": {
            "configure": [], "build": [["cc", "-c", "main.c", "-o", "main.o"], ["sh", "-c", "read value < input; test \"$value\" = source"]],
            "check": [["sh", "-c", "exit 0"]], "install": [["sh", "-c", "mkdir -p \"$1/bin\"; printf '#!/bin/sh\\nif (read value < \"$SECRET\") 2>/dev/null; then printf host-visible; else printf isolated; fi\\n' > \"$1/bin/xz\"; chmod +x \"$1/bin/xz\"", "install", "{prefix}"]]
        }
    })).unwrap();
    let platform = recipe
        .platforms
        .get_mut(&ResolveContext::current().system)
        .unwrap();
    platform.bins = rootbeer_package::Bins::Names(vec!["xz".into()]);
    platform.checks = vec![vec!["xz".into()]];
    platform.build = Some(isolation_build.clone());
    let tools: BTreeMap<String, PathBuf> = ["sh", "cc", "as", "make", "patch", "mkdir", "chmod"]
        .into_iter()
        .map(|name| {
            let path = ["/bin", "/usr/bin"]
                .iter()
                .map(|root| Path::new(root).join(name))
                .find(|path| path.is_file())
                .unwrap();
            (name.into(), path)
        })
        .collect();
    #[cfg(target_os = "macos")]
    let tools = {
        let mut tools = tools;
        for (name, command) in [("cc", "clang"), ("make", "make")] {
            let found = Command::new("/usr/bin/xcrun")
                .args(["--find", command])
                .output()
                .unwrap();
            assert!(found.status.success());
            tools.insert(
                name.into(),
                String::from_utf8(found.stdout).unwrap().trim().into(),
            );
        }
        tools
    };
    #[cfg(target_os = "linux")]
    let tools = {
        let mut tools = tools;
        let found = Command::new(&tools["cc"])
            .arg("-print-prog-name=cc1")
            .output()
            .unwrap();
        assert!(found.status.success());
        let helper = PathBuf::from(String::from_utf8(found.stdout).unwrap().trim());
        if helper.is_absolute() {
            tools.insert("cc1".into(), helper);
        }
        tools
    };
    let environment = BuildEnvironment {
        tools,
        variables: BTreeMap::from([("SECRET".into(), secret.to_string_lossy().into_owned())]),
        ..Default::default()
    }
    .pin()
    .unwrap();
    let mut opts = BuildOptions {
        environment: Some(environment),
        downloads,
        jobs: 1,
        cache: Some(BuildCache {
            directory: root.join("cache"),
            context: "sandbox-test".into(),
            recheck: false,
        }),
        ..Default::default()
    };
    let host = build_package(&catalog, "xz", &root.join("host"), &opts).unwrap();
    opts.is_isolated = true;
    let isolated =
        build_package(&catalog, "xz", &root.join("isolated"), &opts).unwrap_or_else(|error| {
            panic!(
                "{error}: {}",
                ["configure.log", "build.log", "checks.log"]
                    .iter()
                    .filter_map(|name| fs::read_to_string(root.join("isolated").join(name)).ok())
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        });
    assert_ne!(host.build_key, isolated.build_key);
    assert_ne!(host.isolation, isolated.isolation);
    let cached = root.join("cached");
    let reused = build_package(&catalog, "xz", &cached, &opts).unwrap();
    assert_eq!(isolated.build_key, reused.build_key);
    assert!(!cached.join("build.log").exists());
    assert!(fs::read_to_string(root.join("host/checks.log"))
        .unwrap()
        .contains("host-visible"));
    assert_eq!(
        fs::read_to_string(root.join("isolated/checks.log")).unwrap(),
        "isolated"
    );
    assert_eq!(
        fs::read_to_string(cached.join("checks.log")).unwrap(),
        "isolated"
    );
}

#[test]
fn failed_configure_retains_diagnostics_without_publishing_a_result() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    fs::create_dir_all(source.join("project")).unwrap();
    let archive = directory.path().join("source.tar.gz");
    pack(&source, &archive).unwrap();
    let downloads = directory.path().join("downloads");
    let cached = DownloadCache::new(&downloads)
        .materialize(&format!("file://{}", archive.display()), None)
        .unwrap();
    let mut catalog = source_catalog();
    let diagnostics_build: rootbeer_package::SourceBuild = serde_json::from_value(serde_json::json!({
        "backend": "custom", "url": "https://source.invalid/diagnostics.tar.gz", "sha256": cached.sha256,
        "archive": "tar.gz", "strip_prefix": "project", "steps": {
            "configure": [["sh", "-c", "printf compiler-diagnostics > config.log; exit 7"]],
            "build": [["sh", "-c", "exit 0"]], "check": [["sh", "-c", "exit 0"]], "install": [["sh", "-c", "exit 0"]]
        }
    })).unwrap();
    let platform = catalog
        .packages
        .get_mut("xz")
        .unwrap()
        .versions
        .get_mut("5.8.3")
        .unwrap()
        .platforms
        .get_mut(&ResolveContext::current().system)
        .unwrap();
    platform.build = Some(diagnostics_build.clone());
    let output = directory.path().join("output");
    let cache = directory.path().join("cache");
    let error = build_package(
        &catalog,
        "xz",
        &output,
        &BuildOptions {
            downloads,
            cache: Some(BuildCache {
                directory: cache.clone(),
                context: "retained-diagnostics-test".into(),
                recheck: false,
            }),
            ..Default::default()
        },
    )
    .unwrap_err();
    let retained = Path::new(error.split("build workspace retained at ").nth(1).unwrap());
    assert_eq!(
        fs::read_to_string(retained.join("project/config.log")).unwrap(),
        "compiler-diagnostics"
    );
    assert!(!output.join("receipt.json").exists());
    assert!(!cache.join("results").exists());
}

#[test]
fn execution_deadlines_stop_descendants_before_returning() {
    let root = tempfile::tempdir().unwrap();
    let writes = root.path().join("writes");
    for should_cancel in [false, true] {
        let execution = Execution::with_timeout(Duration::from_millis(300)).unwrap();
        let cancel = execution.clone();
        let writer = writes.clone();
        let trigger = std::thread::spawn(move || {
            if should_cancel {
                let started = std::time::Instant::now();
                while !writer.exists() && started.elapsed() < Duration::from_secs(5) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                cancel.cancel();
            }
        });
        let error = run_with_execution(
            &[
                "/bin/sh".into(),
                "-c".into(),
                "(trap '' INT TERM; while :; do printf x >> writes; /bin/sleep 0.01; done) & wait"
                    .into(),
            ],
            root.path(),
            &BTreeMap::new(),
            &root.path().join("log"),
            Duration::from_secs(30),
            None,
            &execution,
        )
        .unwrap_err();
        trigger.join().unwrap();
        assert!(
            error.contains(if should_cancel {
                "cancelled"
            } else {
                "deadline"
            }),
            "{error}"
        );
        let completed = fs::read(&writes).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(completed, fs::read(&writes).unwrap());
        fs::remove_file(&writes).unwrap();
    }
}

#[test]
fn predecessor_generations_reproduce_identities_per_backend() {
    let backends = [
        None,
        Some(BuildBackend::Autotools),
        Some(BuildBackend::Custom),
        Some(BuildBackend::Go),
        Some(BuildBackend::Rust),
        Some(BuildBackend::Zig),
    ];
    let current = Generation::current();
    for backend in &backends {
        assert_eq!(
            current.engine_identity(backend.as_ref()),
            engine_identity(backend.as_ref())
        );
    }
    assert_eq!(current.record().split(' ').count(), 6);
    let older = Generation {
        go: "0".repeat(64),
        ..current.clone()
    };
    for backend in &backends {
        let is_go = backend == &Some(BuildBackend::Go);
        assert_eq!(
            older.engine_identity(backend.as_ref()) != current.engine_identity(backend.as_ref()),
            is_go
        );
    }
    assert!(compatible_generations()
        .iter()
        .all(|generation| generation != &current));
}
