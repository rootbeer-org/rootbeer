use super::*;
use rootbeer_package::lockfile::RootbeerLock;

#[test]
fn runtime_chain_survives_cache_reuse_and_installation_without_build_trees() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let sources = root.join("sources");
    for (name, code) in [
        ("base", "int base(void) { return 40; }"),
        (
            "middle",
            "int base(void); int middle(void) { return base() + 2; }",
        ),
        (
            "consumer",
            "int middle(void); int main(void) { return middle() != 42; }",
        ),
    ] {
        fs::create_dir_all(sources.join(name)).unwrap();
        fs::write(sources.join(name).join("main.c"), code).unwrap();
    }
    let archive = root.join("sources.tar.gz");
    pack(&sources, &archive).unwrap();
    let downloads = root.join("downloads");
    let cached = DownloadCache::new(&downloads)
        .materialize(&format!("file://{}", archive.display()), None)
        .unwrap();
    let mut catalog = crate::test_catalog::catalog().clone();
    let template = catalog.packages["xz"].clone();
    catalog.packages.clear();
    let extension = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    for (name, dependency) in [
        ("base", None),
        ("middle", Some("base")),
        ("consumer", Some("middle")),
    ] {
        let is_library = name != "consumer";
        let mut package = template.clone();
        package.name = name.into();
        package.aliases.clear();
        let mut recipe = package.versions.values().next().unwrap().clone();
        package.default_versions = recipe
            .platforms
            .keys()
            .map(|system| (system.clone(), "1".to_string()))
            .collect();
        let bins = if is_library {
            rootbeer_package::Bins::Names(vec![])
        } else {
            rootbeer_package::Bins::Names(vec![name.into()])
        };
        let checks = if is_library {
            vec![]
        } else {
            vec![vec![name.into()]]
        };
        let platform = recipe
            .platforms
            .get_mut(&ResolveContext::current().system)
            .unwrap();
        platform.bins = bins.clone();
        platform.checks = checks.clone();
        let filename = if is_library {
            format!("lib{name}.{extension}")
        } else {
            name.into()
        };
        let mut command = vec!["cc".into(), "main.c".into(), "-o".into(), filename.clone()];
        if is_library {
            if cfg!(target_os = "macos") {
                // base names itself as a DESTDIR install into `/` would; middle by its prefix.
                let directory = if name == "base" { "" } else { "{prefix}" };
                command.extend([
                    "-dynamiclib".into(),
                    format!("-Wl,-install_name,{directory}/lib/{filename}"),
                ]);
            } else {
                command.extend([
                    "-shared".into(),
                    "-fPIC".into(),
                    format!("-Wl,-soname,{filename}"),
                ]);
            }
        }
        if let Some(dependency) = dependency {
            command.extend(["-L{dependencies}/lib".into(), format!("-l{dependency}")]);
            if name == "consumer" {
                command.push("-lbase".into());
            }
        }
        // Links the way make, cmake, and meson do: with the engine's LDFLAGS.
        let command = vec![
            "sh".to_string(),
            "-c".into(),
            format!("{} $LDFLAGS", command.join(" ")),
        ];
        let output_dir = if is_library { "lib" } else { "bin" };
        let runtime_build: rootbeer_package::SourceBuild = serde_json::from_value(serde_json::json!({
            "backend": "custom", "url": "https://source.invalid/runtime.tar.gz", "sha256": cached.sha256,
            "archive": "tar.gz", "strip_prefix": name,
            "dependencies": dependency.map(|name| serde_json::json!({"package": format!("{name}@1"), "kind": "link_runtime"})).into_iter().collect::<Vec<_>>(),
            "libraries": if is_library { vec![format!("lib/{filename}")] } else { vec![] },
            "steps": {"configure": [], "build": [command], "check": [["sh", "-c", "exit 0"]],
                "install": [["mkdir", "-p", format!("{{prefix}}/{output_dir}")], ["cp", filename, format!("{{prefix}}/{output_dir}/")]]}
        })).unwrap();
        let platform = recipe
            .platforms
            .get_mut(&ResolveContext::current().system)
            .unwrap();
        platform.build = Some(runtime_build.clone());
        package.versions = BTreeMap::from([("1".into(), recipe)]);
        catalog.packages.insert(name.into(), package);
    }
    let plan = BuildPlan::current(&catalog, "consumer").unwrap();
    assert_eq!(
        plan.graph().nodes["consumer@1"].runtime_closure,
        ["base@1", "middle@1"]
    );
    fs::create_dir(root.join("cache")).unwrap();
    symlink(root.join("cache"), root.join("cache-alias")).unwrap();
    let opts = BuildOptions {
        downloads,
        cache: Some(BuildCache {
            directory: root.join("cache-alias"),
            context: "native-runtime-test".into(),
            recheck: false,
        }),
        ..Default::default()
    };
    let output = root.join("output");
    let artifact = plan.execute(&output, &opts).unwrap();
    let build_key = artifact.build_key.clone();
    assert_eq!(
        artifact.package.runtime_dependencies["middle@1"].source,
        artifact.dependencies["middle@1"].source,
        "runtime dependencies reference their own build's archive"
    );
    let first_report = fs::read(output.join("runtime-audit.json")).unwrap();
    fs::remove_dir_all(&output).unwrap();
    fs::remove_dir_all(&sources).unwrap();
    fs::remove_file(archive).unwrap();
    let second = root.join("second");
    let artifact = plan.execute(&second, &opts).unwrap();
    assert_eq!(artifact.build_key, build_key);
    assert!(!second.join("build.log").exists());
    assert_eq!(
        first_report,
        fs::read(second.join("runtime-audit.json")).unwrap()
    );
    let lock = RootbeerLock::from_packages([artifact.package.clone()]).unwrap();
    assert_eq!(lock.schema, 3);
    let lock_path = root.join("rootbeer.lock");
    lock.write(&lock_path).unwrap();
    let lock = RootbeerLock::read(&lock_path).unwrap();
    let store = Store::new(root.join("installed"));
    let realizer = PackageRealizer::with_dirs(
        store.clone(),
        root.join("install-downloads"),
        root.join("tmp"),
    );
    let installed = realizer.realize(&lock.packages["consumer@1"]).unwrap();
    assert_eq!(lock.store_paths(&store).unwrap().len(), 3);
    let runtime = rootbeer_package::runtime::closure(&artifact.package).unwrap();
    assert_eq!(runtime.len(), 2);
    let roots = runtime
        .iter()
        .map(|dependency| {
            let directory = rootbeer_package::runtime::store_directory(dependency).unwrap();
            (directory.clone(), store.root().join(directory))
        })
        .collect();
    audit::audit_with_runtime(&installed.store_entry.path, &roots)
        .unwrap()
        .validate()
        .unwrap();
    assert!(audit::audit(&installed.store_entry.path)
        .unwrap()
        .validate()
        .is_err());
    fs::remove_dir_all(second).unwrap();
    fs::remove_dir_all(root.join("cache")).unwrap();
    let relocated = root.join("relocated");
    fs::rename(store.root(), &relocated).unwrap();
    let executable = relocated
        .join(installed.store_entry.path.file_name().unwrap())
        .join("bin/consumer");
    assert!(Command::new(executable)
        .env_clear()
        .current_dir("/")
        .status()
        .unwrap()
        .success());
    let offline = PackageRealizer::with_dirs_and_offline(
        Store::new(&relocated),
        root.join("empty-downloads"),
        root.join("tmp"),
        true,
    );
    offline.realize(&artifact.package).unwrap();
    let base = &runtime[0];
    fs::remove_dir_all(relocated.join(rootbeer_package::runtime::store_directory(base).unwrap()))
        .unwrap();
    assert!(offline.realize(&artifact.package).is_err());
}
