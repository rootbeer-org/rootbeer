use super::*;
use std::process::Command;

fn compile(args: &[String]) {
    let output = Command::new("/usr/bin/cc").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture(root: &Path, absolute: bool) -> PathBuf {
    let package = root.join("package");
    let lib = package.join("lib");
    fs::create_dir_all(&lib).unwrap();
    fs::create_dir(package.join("bin")).unwrap();
    let source = root.join("answer.c");
    fs::write(&source, "int answer(void) { return 42; }\n").unwrap();
    let middle = root.join("middle.c");
    fs::write(
        &middle,
        "int answer(void); int middle(void) { return answer(); }\n",
    )
    .unwrap();
    let main = root.join("main.c");
    fs::write(
        &main,
        "int middle(void); int main(void) { return middle() != 42; }\n",
    )
    .unwrap();
    let extension = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    for (name, file) in [("answer", &source), ("middle", &middle)] {
        let output = lib.join(format!("lib{name}.{extension}"));
        let mut args = vec![
            file.display().to_string(),
            "-o".into(),
            output.display().to_string(),
        ];
        if cfg!(target_os = "macos") {
            let identity = if absolute {
                output.display().to_string()
            } else {
                format!("@rpath/lib{name}.dylib")
            };
            args.extend([
                "-dynamiclib".into(),
                format!("-Wl,-install_name,{identity}"),
            ]);
        } else {
            args.extend([
                "-shared".into(),
                "-fPIC".into(),
                format!("-Wl,-soname,lib{name}.so"),
                "-Wl,-rpath,$ORIGIN".into(),
            ]);
        }
        if name == "middle" {
            args.extend([format!("-L{}", lib.display()), "-lanswer".into()]);
        }
        compile(&args);
    }
    let rpath = if absolute {
        lib.display().to_string()
    } else if cfg!(target_os = "macos") {
        "@executable_path/../lib".into()
    } else {
        "$ORIGIN/../lib".into()
    };
    compile(&[
        main.display().to_string(),
        "-o".into(),
        package.join("bin/main").display().to_string(),
        format!("-L{}", lib.display()),
        "-lmiddle".into(),
        "-lanswer".into(),
        format!("-Wl,-rpath,{rpath}"),
    ]);
    package
}

#[test]
fn bundled_chain_remains_valid_after_relocation_without_build_files() {
    let source = tempfile::tempdir().unwrap();
    let package = fixture(source.path(), false);
    let extension = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    let library = package.join(format!("lib/libanswer.{extension}"));
    let versioned = package.join(format!("lib/libanswer.1.{extension}"));
    fs::rename(&library, &versioned).unwrap();
    std::os::unix::fs::symlink(versioned.file_name().unwrap(), library).unwrap();
    let report = audit(&package).unwrap();
    report.validate().unwrap();
    assert_eq!(report.binaries.len(), 3);
    let destination = tempfile::tempdir().unwrap();
    let installed = destination.path().join("installed");
    fs::rename(&package, &installed).unwrap();
    drop(source);
    assert_eq!(
        serde_json::to_value(&report).unwrap(),
        serde_json::to_value(audit(&installed).unwrap()).unwrap()
    );
    assert!(Command::new(installed.join("bin/main"))
        .env_clear()
        .current_dir("/")
        .status()
        .unwrap()
        .success());
}

#[test]
fn rejects_absolute_build_paths_and_missing_bundled_libraries() {
    let source = tempfile::tempdir().unwrap();
    let package = fixture(source.path(), true);
    let report = audit(&package).unwrap();
    assert!(report.validate().unwrap_err().contains("host"));
    let source = tempfile::tempdir().unwrap();
    let package = fixture(source.path(), false);
    let extension = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    fs::remove_file(package.join(format!("lib/libanswer.{extension}"))).unwrap();
    let report = audit(&package).unwrap();
    assert!(report.violations.iter().any(
        |issue| issue.reference.contains("libanswer") && issue.reason.contains("no compatible")
    ));
}

#[test]
fn rejects_symlink_escapes_and_malformed_native_files() {
    let source = tempfile::tempdir().unwrap();
    let package = fixture(source.path(), false);
    let extension = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    let library = package.join(format!("lib/libanswer.{extension}"));
    let outside = source.path().join("external-library");
    fs::rename(&library, &outside).unwrap();
    std::os::unix::fs::symlink(&outside, &library).unwrap();
    assert!(audit(&package).unwrap().validate().is_err());
    fs::write(package.join("broken"), b"\x7fELFbroken").unwrap();
    assert!(audit(&package)
        .unwrap_err()
        .contains("malformed native binary"));
}

#[cfg(target_os = "macos")]
#[test]
fn universal_macho_checks_every_architecture() {
    let source = tempfile::tempdir().unwrap();
    let package = source.path().join("package");
    fs::create_dir(&package).unwrap();
    let main = source.path().join("main.c");
    fs::write(&main, "int main(void) { return 0; }\n").unwrap();
    compile(&[
        main.display().to_string(),
        "-arch".into(),
        "arm64".into(),
        "-arch".into(),
        "x86_64".into(),
        "-o".into(),
        package.join("main").display().to_string(),
    ]);
    let report = audit(&package).unwrap();
    report.validate().unwrap();
    assert_eq!(report.binaries.len(), 2);
    assert_ne!(
        report.binaries[0].architecture,
        report.binaries[1].architecture
    );
}

#[test]
fn source_build_rejects_unsafe_binary_before_checks_or_cache_publication() {
    use crate::{build_package, BuildCache, BuildOptions};
    use rootbeer_package::{CatalogPackage, ResolveContext};
    let directory = tempfile::tempdir().unwrap();
    let package = fixture(directory.path(), true);
    let source = directory.path().join("source.tar.gz");
    let staging = directory.path().join("staging");
    std::fs::create_dir(&staging).unwrap();
    std::fs::rename(&package, staging.join("fixture")).unwrap();
    crate::pack(&staging, &source).unwrap();
    let downloads = directory.path().join("downloads");
    let downloaded = rootbeer_package::download::DownloadCache::new(&downloads)
        .materialize(&format!("file://{}", source.display()), None)
        .unwrap();
    let mut catalog = crate::test_catalog::catalog().clone();
    let template = catalog.packages["xz"].clone();
    let system = ResolveContext::current().system;
    let mut recipe = template.versions[template.default_version_for(&system).unwrap()].clone();
    recipe.platforms.retain(|platform, _| platform == &system);
    let build = Some(serde_json::from_value(serde_json::json!({
        "backend": "custom", "url": "https://source.invalid/audit.tar.gz", "sha256": downloaded.sha256,
        "archive": "tar.gz", "strip_prefix": "fixture",
        "steps": {"configure": [], "build": [["sh", "-c", "exit 0"]], "check": [["sh", "-c", "exit 0"]],
            "install": [["sh", "-c", "cp -R bin lib \"$1/\"", "install", "{prefix}"]]}
    })).unwrap());
    let platform = recipe
        .platforms
        .get_mut(&ResolveContext::current().system)
        .unwrap();
    platform.bins = rootbeer_package::Bins::Names(vec!["main".into()]);
    platform.checks = vec![vec!["main".into()]];
    platform.build = build.clone();
    catalog.packages.clear();
    catalog.packages.insert(
        "fixture".into(),
        CatalogPackage {
            name: "fixture".into(),
            default_versions: std::collections::BTreeMap::from([(system.clone(), "1".into())]),
            versions: std::collections::BTreeMap::from([("1".into(), recipe)]),
            ..template
        },
    );
    let output = directory.path().join("output");
    let cache = directory.path().join("cache");
    let error = build_package(
        &catalog,
        "fixture",
        &output,
        &BuildOptions {
            downloads,
            cache: Some(BuildCache {
                directory: cache.clone(),
                context: "audit-test".into(),
                recheck: false,
            }),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(error.contains("runtime audit failed"), "{error}");
    assert!(output.join("runtime-audit.json").is_file());
    assert!(!output.join("checks.log").exists());
    assert!(!output.join("package.tar.gz").exists());
    assert!(!cache.join("results").exists());
}
