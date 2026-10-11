use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::{fs, os::unix::fs as unix_fs, process, process::Command, thread};

use crate::{
    executor::{ExecutionHandler, ExecutionReport, OpResult},
    package::{applications::Applications, profile as package_profile, PackageRealizer},
    plan::WriteSource,
    store::Store,
    Op,
};

/// Resolve a `WriteSource` to its concrete bytes. Inline sources are a
/// straight clone; secret-backed sources shell out to their provider here
/// (apply time only).
fn resolve_source(source: &WriteSource, tools: &crate::tools::ToolRuntime) -> io::Result<Vec<u8>> {
    match source {
        WriteSource::Bytes(bytes) => Ok(bytes.clone()),
        WriteSource::AgeFile { path, identity, .. } => crate::age::decrypt(path, identity, tools),
        WriteSource::OpDocument { reference } => {
            let output = crate::one_password::command(tools)?
                .args(["document", "get", reference])
                .output()
                .map_err(|e| io::Error::other(format!("failed to run `op`: {e}")))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(io::Error::other(format!(
                    "op document get {reference} failed ({}): {stderr}",
                    output.status
                )));
            }

            Ok(output.stdout)
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ApplyOptions {
    pub package_offline: bool,
    /// Leaves the legacy store, its bins, and its apps alone, because the
    /// caller installed packages elsewhere.
    pub is_packages_taken: bool,
}

#[cfg(test)]
pub fn apply(
    ops: &[Op],
    force: bool,
    handler: &mut impl ExecutionHandler,
) -> io::Result<ExecutionReport> {
    apply_with_options(
        &crate::tools::ToolRuntime::default(),
        ops,
        force,
        handler,
        ApplyOptions::default(),
    )
}

pub fn apply_with_options(
    tools: &crate::tools::ToolRuntime,
    ops: &[Op],
    force: bool,
    handler: &mut impl ExecutionHandler,
    options: ApplyOptions,
) -> io::Result<ExecutionReport> {
    #[cfg(test)]
    let application_root = tempfile::tempdir()?;
    #[cfg(test)]
    let store = Store::new(application_root.path().join("store"));
    #[cfg(not(test))]
    let store = Store::default();
    let package_realizer = if options.package_offline {
        PackageRealizer::offline(store)
    } else {
        PackageRealizer::new(store)
    };

    #[cfg(test)]
    let applications = Applications::new(
        application_root.path().join("state"),
        application_root.path().join("Applications"),
    );
    #[cfg(not(test))]
    let applications = Applications::default();
    #[cfg(test)]
    let bin_dir = application_root.path().join("profile/bin");
    #[cfg(not(test))]
    let bin_dir = package_profile::bin_dir();

    apply_ops(
        tools,
        ops,
        force,
        handler,
        &package_realizer,
        &bin_dir,
        &applications,
        !options.is_packages_taken,
    )
}

#[cfg(test)]
fn apply_with_package_realizer(
    tools: &crate::tools::ToolRuntime,
    ops: &[Op],
    force: bool,
    handler: &mut impl ExecutionHandler,
    package_realizer: &PackageRealizer,
    package_bin_dir: &Path,
    applications: &Applications,
) -> io::Result<ExecutionReport> {
    apply_ops(
        tools,
        ops,
        force,
        handler,
        package_realizer,
        package_bin_dir,
        applications,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn apply_ops(
    tools: &crate::tools::ToolRuntime,
    ops: &[Op],
    force: bool,
    handler: &mut impl ExecutionHandler,
    package_realizer: &PackageRealizer,
    package_bin_dir: &Path,
    applications: &Applications,
    should_sync_packages: bool,
) -> io::Result<ExecutionReport> {
    let mut report = ExecutionReport::default();
    let mut desired_apps = BTreeMap::new();
    let mut live = BTreeSet::new();
    let mut desired_bins = BTreeSet::new();

    for op in ops {
        handler.on_start(op);

        match op {
            Op::WriteFile { path, source } => {
                let bytes = resolve_source(source, tools)?;

                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }

                if let WriteSource::AgeFile { mode, .. } = source {
                    let parent = path
                        .parent()
                        .ok_or_else(|| io::Error::other("missing destination parent"))?;
                    let mut file = tempfile::NamedTempFile::new_in(parent)?;
                    file.write_all(&bytes)?;
                    file.as_file()
                        .set_permissions(fs::Permissions::from_mode(*mode))?;
                    file.persist(path).map_err(|error| error.error)?;
                } else {
                    fs::write(path, &bytes)?;
                }
                let result = OpResult::FileWritten {
                    path: path.clone(),
                    bytes: Some(bytes.len()),
                };

                handler.on_result(&result);
                report.results.push(result);
            }

            Op::Symlink { src, dst } => {
                let mut overwritten = false;

                if dst.is_symlink() {
                    if let Ok(target) = fs::read_link(dst) {
                        if target == *src {
                            let result = OpResult::SymlinkUnchanged { dst: dst.clone() };
                            handler.on_result(&result);
                            report.results.push(result);
                            continue;
                        }

                        fs::remove_file(dst)?;
                    }
                } else if dst.exists() {
                    if force {
                        overwritten = true;
                        if dst.is_dir() {
                            fs::remove_dir_all(dst)?;
                        } else {
                            fs::remove_file(dst)?;
                        }
                    } else {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!(
                                "target {} exists and is not a symlink (use --force to overwrite)",
                                dst.display()
                            ),
                        ));
                    }
                }

                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }

                unix_fs::symlink(src, dst)?;

                let result = if overwritten {
                    OpResult::SymlinkOverwritten {
                        src: src.clone(),
                        dst: dst.clone(),
                    }
                } else {
                    OpResult::SymlinkCreated {
                        src: src.clone(),
                        dst: dst.clone(),
                    }
                };
                handler.on_result(&result);
                report.results.push(result);
            }

            Op::CopyFileIfMissing { src, dst } => {
                if dst.exists() || dst.is_symlink() {
                    let result = OpResult::FileCopySkipped { dst: dst.clone() };
                    handler.on_result(&result);
                    report.results.push(result);
                    continue;
                }

                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }

                fs::copy(src, dst)?;

                let result = OpResult::FileCopied {
                    src: src.clone(),
                    dst: dst.clone(),
                };
                handler.on_result(&result);
                report.results.push(result);
            }

            Op::Exec { cmd, args, cwd } => {
                let display = std::iter::once(cmd.as_str())
                    .chain(args.iter().map(|s| s.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ");

                let mut child = process::Command::new(cmd)
                    .args(args)
                    .current_dir(cwd)
                    .stdin(process::Stdio::inherit())
                    .stdout(process::Stdio::piped())
                    .stderr(process::Stdio::piped())
                    .spawn()?;

                let stderr = child.stderr.take().unwrap();
                let stderr_lines = thread::spawn(move || {
                    BufReader::new(stderr)
                        .lines()
                        .collect::<Result<Vec<_>, _>>()
                });

                let stdout = child.stdout.take().unwrap();
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    handler.on_output(&line);
                }

                if let Ok(Ok(lines)) = stderr_lines.join() {
                    for line in lines {
                        handler.on_output(&line);
                    }
                }

                let status = child.wait()?;

                let result = OpResult::CommandRan {
                    cmd: display,
                    status: status.code().unwrap_or(1),
                };
                handler.on_result(&result);
                report.results.push(result);
            }

            Op::Chmod { path, mode } => {
                let perms = fs::Permissions::from_mode(*mode);
                fs::set_permissions(path, perms)?;
                let result = OpResult::Chmodded {
                    path: path.clone(),
                    mode: *mode,
                };
                handler.on_result(&result);
                report.results.push(result);
            }

            Op::SetRemoteUrl { dir, url } => {
                let current = Command::new("git")
                    .args(["-C", &dir.to_string_lossy(), "remote", "get-url", "origin"])
                    .output()
                    .map_err(|e| io::Error::other(format!("git: {e}")))?;

                if !current.status.success() {
                    return Err(io::Error::other(
                        "failed to get origin URL; is the source directory a git repo?",
                    ));
                }

                let current_url = String::from_utf8_lossy(&current.stdout).trim().to_string();

                if *url == current_url {
                    let result = OpResult::RemoteUnchanged { url: current_url };
                    handler.on_result(&result);
                    report.results.push(result);
                } else {
                    let status = Command::new("git")
                        .args([
                            "-C",
                            &dir.to_string_lossy(),
                            "remote",
                            "set-url",
                            "origin",
                            url,
                        ])
                        .status()
                        .map_err(|e| io::Error::other(format!("git: {e}")))?;

                    if !status.success() {
                        return Err(io::Error::other("failed to set origin URL"));
                    }

                    let result = OpResult::RemoteUpdated {
                        from: current_url,
                        to: url.clone(),
                    };
                    handler.on_result(&result);
                    report.results.push(result);
                }
            }

            Op::RealizePackage { package } => {
                let is_cached = package_realizer.is_cached(package)?;
                let realized = package_realizer.realize(package)?;
                live.extend(realized.runtime_paths(package_realizer.store())?);
                desired_bins.extend(realized.bins.keys().cloned());
                for (name, path) in &realized.apps {
                    if desired_apps
                        .insert(name.clone(), path.clone())
                        .is_some_and(|previous| previous != *path)
                    {
                        return Err(io::Error::other(format!(
                            "packages export conflicting application '{name}'"
                        )));
                    }
                }
                let has_bin_changes =
                    activate_package_bins(&realized.bins, force, package_bin_dir)?;
                package_profile::write_env_file_for_bin_dir(package_bin_dir)?;
                let is_unchanged = is_cached
                    && !has_bin_changes
                    && applications.contains_exports("configuration", &realized.apps)?;
                let result = if is_unchanged {
                    OpResult::PackageUnchanged {
                        name: package.name.clone(),
                        version: package.version.clone(),
                    }
                } else {
                    OpResult::PackageRealized {
                        name: package.name.clone(),
                        version: package.version.clone(),
                        store_path: Some(realized.store_entry.path),
                    }
                };
                handler.on_result(&result);
                report.results.push(result);
            }

            Op::Package { intent } => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("package `{intent}` must be locked before apply"),
                ));
            }
        }
    }

    if !should_sync_packages {
        return Ok(report);
    }

    if !report
        .results
        .iter()
        .any(|result| matches!(result, OpResult::CommandRan { status, .. } if *status != 0))
    {
        applications.synchronize("configuration", &desired_apps)?;
    }
    package_realizer
        .store()
        .write_root("configuration", &live)?;
    remove_stale_package_bins(&desired_bins, package_realizer.store(), package_bin_dir)?;
    Ok(report)
}

/// Removes links to store entries for commands no package in the plan exports.
fn remove_stale_package_bins(
    desired: &BTreeSet<String>,
    store: &Store,
    bin_dir: &Path,
) -> io::Result<()> {
    let entries = match fs::read_dir(bin_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    // TODO(legacy-store): links made before the move still point at the old store.
    let legacy = crate::state_dir().join("store");

    for entry in entries {
        let entry = entry?;
        let is_desired = entry
            .file_name()
            .to_str()
            .is_some_and(|name| desired.contains(name));
        if is_desired || !entry.file_type()?.is_symlink() {
            continue;
        }
        let target = fs::read_link(entry.path())?;
        if target.starts_with(store.root()) || target.starts_with(&legacy) {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn activate_package_bins(
    bins: &std::collections::BTreeMap<String, PathBuf>,
    force: bool,
    bin_dir: &Path,
) -> io::Result<bool> {
    fs::create_dir_all(bin_dir)?;
    let mut has_changes = false;

    for (name, src) in bins {
        let dst = bin_dir.join(name);
        if dst.is_symlink() {
            if fs::read_link(&dst).ok().as_ref() == Some(src) {
                continue;
            }
            fs::remove_file(&dst)?;
        } else if dst.exists() {
            if force {
                if dst.is_dir() {
                    fs::remove_dir_all(&dst)?;
                } else {
                    fs::remove_file(&dst)?;
                }
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "package bin {} exists and is not a symlink (use --force to overwrite)",
                        dst.display()
                    ),
                ));
            }
        }

        unix_fs::symlink(src, dst)?;
        has_changes = true;
    }

    Ok(has_changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{ArchiveFormat, LockedInstall, LockedPackage, LockedSource, Provides};
    use crate::store::{hash_file, Store};
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[derive(Default)]
    struct Recorder {
        results: Vec<OpResult>,
    }

    impl ExecutionHandler for Recorder {
        fn on_start(&mut self, _: &Op) {}
        fn on_output(&mut self, _: &str) {}
        fn on_result(&mut self, r: &OpResult) {
            self.results.push(r.clone());
        }
    }

    #[test]
    fn age_file_apply_is_private_and_failure_preserves_destination() {
        let dir = tempfile::tempdir().unwrap();
        let plaintext = b"private binary\x00\xff";
        let identity = crate::age::tests::fixture(dir.path(), plaintext, false);
        let path = dir.path().join("nested/output");
        let encrypted = dir.path().join("secret.age");
        let ops = vec![Op::WriteFile {
            path: path.clone(),
            source: WriteSource::AgeFile {
                path: encrypted.clone(),
                identity,
                mode: 0o600,
            },
        }];
        crate::executor::dry_run(&ops, &mut Recorder::default());
        assert!(!path.exists());
        apply(&ops, false, &mut Recorder::default()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), plaintext);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        apply(&ops, false, &mut Recorder::default()).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::write(encrypted, b"corrupted").unwrap();
        assert!(apply(&ops, false, &mut Recorder::default()).is_err());
        assert_eq!(fs::read(&path).unwrap(), plaintext);
    }

    #[test]
    fn age_file_dry_run_needs_no_key_or_ciphertext() {
        let dir = tempfile::tempdir().unwrap();
        let ops = vec![Op::WriteFile {
            path: dir.path().join("output"),
            source: WriteSource::AgeFile {
                path: dir.path().join("missing.age"),
                identity: crate::AgeIdentity::File(dir.path().join("missing.key")),
                mode: 0o600,
            },
        }];
        let report = crate::executor::dry_run(&ops, &mut Recorder::default());
        assert!(matches!(
            report.results[0],
            OpResult::FileWritten { bytes: None, .. }
        ));
        assert!(!dir.path().join("output").exists());
    }

    #[test]
    fn write_file_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a/b/c.txt");
        let ops = vec![Op::WriteFile {
            path: path.clone(),
            source: WriteSource::text("hello\n"),
        }];

        let mut h = Recorder::default();
        apply(&ops, false, &mut h).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n");
        assert!(matches!(
            &h.results[0],
            OpResult::FileWritten { bytes: Some(6), .. }
        ));
    }

    #[test]
    fn symlink_create_idempotent_skips_when_target_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.txt");
        let dst = tmp.path().join("dst.txt");
        fs::write(&src, "x").unwrap();
        unix_fs::symlink(&src, &dst).unwrap();

        let ops = vec![Op::Symlink {
            src: src.clone(),
            dst: dst.clone(),
        }];

        let mut h = Recorder::default();
        apply(&ops, false, &mut h).unwrap();

        assert!(matches!(&h.results[0], OpResult::SymlinkUnchanged { .. }));
    }

    #[test]
    fn symlink_replaces_stale_link() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real.txt");
        let stale = tmp.path().join("stale.txt");
        let dst = tmp.path().join("dst.txt");
        fs::write(&real, "x").unwrap();
        fs::write(&stale, "y").unwrap();
        unix_fs::symlink(&stale, &dst).unwrap();

        apply(
            &[Op::Symlink {
                src: real.clone(),
                dst: dst.clone(),
            }],
            false,
            &mut Recorder::default(),
        )
        .unwrap();

        assert_eq!(fs::read_link(&dst).unwrap(), real);
    }

    #[test]
    fn symlink_refuses_to_overwrite_real_file_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.txt");
        let dst = tmp.path().join("dst.txt");
        fs::write(&src, "x").unwrap();
        fs::write(&dst, "real-file").unwrap();

        let err = apply(
            &[Op::Symlink {
                src,
                dst: dst.clone(),
            }],
            false,
            &mut Recorder::default(),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&dst).unwrap(), "real-file");
    }

    #[test]
    fn symlink_overwrites_real_file_with_force() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.txt");
        let dst = tmp.path().join("dst.txt");
        fs::write(&src, "x").unwrap();
        fs::write(&dst, "real-file").unwrap();

        let mut h = Recorder::default();
        apply(
            &[Op::Symlink {
                src: src.clone(),
                dst: dst.clone(),
            }],
            true,
            &mut h,
        )
        .unwrap();

        assert!(matches!(&h.results[0], OpResult::SymlinkOverwritten { .. }));
        assert_eq!(fs::read_link(&dst).unwrap(), src);
    }

    #[test]
    fn copy_file_creates_dest_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("seed.txt");
        let dst = tmp.path().join("nested/seed.txt");
        fs::write(&src, "hello").unwrap();

        let mut h = Recorder::default();
        apply(
            &[Op::CopyFileIfMissing {
                src: src.clone(),
                dst: dst.clone(),
            }],
            false,
            &mut h,
        )
        .unwrap();

        assert_eq!(fs::read_to_string(&dst).unwrap(), "hello");
        assert!(matches!(&h.results[0], OpResult::FileCopied { .. }));
    }

    #[test]
    fn copy_file_skips_when_dest_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("seed.txt");
        let dst = tmp.path().join("seed.txt.dst");
        fs::write(&src, "fresh").unwrap();
        fs::write(&dst, "user-modified").unwrap();

        let mut h = Recorder::default();
        apply(
            &[Op::CopyFileIfMissing {
                src,
                dst: dst.clone(),
            }],
            false,
            &mut h,
        )
        .unwrap();

        assert_eq!(fs::read_to_string(&dst).unwrap(), "user-modified");
        assert!(matches!(&h.results[0], OpResult::FileCopySkipped { .. }));
    }

    #[test]
    fn exec_captures_stdout_lines() {
        let tmp = tempfile::tempdir().unwrap();
        #[derive(Default)]
        struct OutputSink {
            lines: Vec<String>,
            results: Vec<OpResult>,
        }
        impl ExecutionHandler for OutputSink {
            fn on_start(&mut self, _: &Op) {}
            fn on_output(&mut self, line: &str) {
                self.lines.push(line.into());
            }
            fn on_result(&mut self, r: &OpResult) {
                self.results.push(r.clone());
            }
        }

        let ops = vec![Op::Exec {
            cmd: "sh".into(),
            args: vec!["-c".into(), "echo hello-stdout".into()],
            cwd: tmp.path().to_path_buf(),
        }];

        let mut h = OutputSink::default();
        apply(&ops, false, &mut h).unwrap();

        assert!(
            h.lines.iter().any(|l| l == "hello-stdout"),
            "got lines: {:?}",
            h.lines
        );
        assert!(matches!(
            &h.results[0],
            OpResult::CommandRan { status: 0, .. }
        ));
    }

    #[test]
    fn write_file_op_paths_use_pathbuf() {
        // Sanity check that PathBuf round-trips through the Op variant
        // — guards against future changes that might tempt String paths.
        let op = Op::WriteFile {
            path: PathBuf::from("/tmp/x"),
            source: WriteSource::text("y"),
        };
        assert!(matches!(op, Op::WriteFile { .. }));
    }

    fn archive_source() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let archive_path = root.path().join("demo.tar.gz");
        let file = fs::File::create(&archive_path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = tar::Builder::new(encoder);

        let mut header = tar::Header::new_gnu();
        let body = b"#!/bin/sh\n";
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "demo/bin/demo", &body[..])
            .unwrap();
        for (path, bytes) in [
            ("demo/Demo.app/Contents/Info.plist", b"<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleExecutable</key><string>demo</string><key>CFBundleIdentifier</key><string>me.tale.rootbeer.fixture</string><key>CFBundlePackageType</key><string>APPL</string></dict></plist>".as_slice()),
            ("demo/Demo.app/Contents/MacOS/demo", body.as_slice()),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, path, bytes).unwrap();
        }
        builder.finish().unwrap();
        builder.into_inner().unwrap().finish().unwrap();

        (root, archive_path)
    }

    fn locked_package(archive: &PathBuf) -> LockedPackage {
        LockedPackage {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            source: LockedSource::File {
                path: archive.clone(),
                sha256: hash_file(archive).unwrap(),
            },
            install: LockedInstall::Archive {
                format: ArchiveFormat::TarGz,
                strip_prefix: Some(PathBuf::from("demo")),
            },
            provides: Provides {
                apps: BTreeMap::new(),
                bins: BTreeMap::from([("demo".to_string(), PathBuf::from("bin/demo"))]),
            },
            runtime_dependencies: Default::default(),
            output_sha256: None,
        }
    }

    #[test]
    fn roots_survive_failed_commands_and_dropped_packages_lose_their_links() {
        let root = tempfile::tempdir().unwrap();
        let (_archive_root, archive) = archive_source();
        let realizer = PackageRealizer::with_dirs(
            Store::new(root.path().join("store")),
            root.path().join("downloads"),
            root.path().join("tmp"),
        );
        let bins = root.path().join("profile/bin");
        let apps = Applications::new(
            root.path().join("app-state"),
            root.path().join("Applications"),
        );
        let apply = |ops: &[Op]| {
            apply_with_package_realizer(
                &crate::tools::ToolRuntime::default(),
                ops,
                false,
                &mut Recorder::default(),
                &realizer,
                &bins,
                &apps,
            )
            .unwrap()
        };
        let uid = unsafe { libc::getuid() }.to_string();
        let root_file = root
            .path()
            .join("var/roots")
            .join(uid)
            .join("configuration");

        apply(&[
            Op::RealizePackage {
                package: locked_package(&archive),
            },
            Op::Exec {
                cmd: "/bin/sh".into(),
                args: vec!["-c".into(), "exit 7".into()],
                cwd: root.path().into(),
            },
        ]);
        assert!(fs::read_to_string(&root_file)
            .unwrap()
            .contains("-demo-1.0.0"));

        unix_fs::symlink(root.path(), bins.join("mine")).unwrap();
        apply(&[]);
        assert!(!bins.join("demo").is_symlink());
        assert!(bins.join("mine").is_symlink());
        assert!(fs::read_to_string(&root_file).unwrap().is_empty());
    }

    #[test]
    fn realize_package_op_installs_locked_package_into_store() {
        let root = tempfile::tempdir().unwrap();
        let (_archive_root, archive) = archive_source();
        let package = locked_package(&archive);
        let ops = vec![Op::RealizePackage {
            package: package.clone(),
        }];
        let package_realizer = PackageRealizer::with_dirs(
            Store::new(root.path().join("store")),
            root.path().join("downloads"),
            root.path().join("tmp"),
        );

        let mut h = Recorder::default();
        apply_with_package_realizer(
            &crate::tools::ToolRuntime::default(),
            &ops,
            false,
            &mut h,
            &package_realizer,
            &root.path().join("profile/bin"),
            &Applications::new(
                root.path().join("app-state"),
                root.path().join("Applications"),
            ),
        )
        .unwrap();

        let OpResult::PackageRealized {
            name,
            version,
            store_path: Some(store_path),
        } = &h.results[0]
        else {
            panic!("expected package realized result");
        };
        assert_eq!(name, "demo");
        assert_eq!(version, "1.0.0");
        assert!(store_path.join("bin/demo").is_file());
        assert_eq!(
            fs::read_link(root.path().join("profile/bin/demo")).unwrap(),
            store_path.join("bin/demo")
        );
        assert!(root.path().join("profile/env.sh").is_file());
        assert!(fs::read_to_string(root.path().join("profile/env.sh"))
            .unwrap()
            .contains("profile/bin"));
    }

    #[test]
    fn cached_package_skips_unchanged_links_but_repairs_missing_links() {
        use std::os::unix::fs::MetadataExt;

        let root = tempfile::tempdir().unwrap();
        let (_archive_root, archive) = archive_source();
        let mut package = locked_package(&archive);
        let realizer = PackageRealizer::with_dirs(
            Store::new(root.path().join("store")),
            root.path().join("downloads"),
            root.path().join("tmp"),
        );
        package.output_sha256 = Some(
            realizer
                .realize(&package)
                .unwrap()
                .store_entry
                .output_sha256,
        );
        let bins = root.path().join("profile/bin");
        let apps = Applications::new(
            root.path().join("app-state"),
            root.path().join("Applications"),
        );
        let ops = [Op::RealizePackage { package }];
        let apply = || {
            apply_with_package_realizer(
                &crate::tools::ToolRuntime::default(),
                &ops,
                false,
                &mut Recorder::default(),
                &realizer,
                &bins,
                &apps,
            )
            .unwrap()
        };
        assert!(matches!(
            apply().results[0],
            OpResult::PackageRealized { .. }
        ));
        let link_before = fs::symlink_metadata(bins.join("demo")).unwrap();
        let env_path = root.path().join("profile/env.sh");
        let env_before = fs::metadata(&env_path).unwrap();
        assert!(matches!(
            apply().results[0],
            OpResult::PackageUnchanged { .. }
        ));
        assert_eq!(
            fs::symlink_metadata(bins.join("demo")).unwrap().ino(),
            link_before.ino()
        );
        let env_after = fs::metadata(&env_path).unwrap();
        assert_eq!(
            (env_after.mtime(), env_after.mtime_nsec()),
            (env_before.mtime(), env_before.mtime_nsec())
        );
        fs::remove_file(bins.join("demo")).unwrap();
        assert!(matches!(
            apply().results[0],
            OpResult::PackageRealized { .. }
        ));
        assert!(bins.join("demo").exists());
        let target = fs::read_link(bins.join("demo")).unwrap();
        fs::write(target, b"tampered").unwrap();
        assert!(apply_with_package_realizer(
            &crate::tools::ToolRuntime::default(),
            &ops,
            false,
            &mut Recorder::default(),
            &realizer,
            &bins,
            &apps
        )
        .is_err());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn cached_app_package_skips_only_when_owned_app_link_is_current() {
        let root = tempfile::tempdir().unwrap();
        let (_archive_root, archive) = archive_source();
        let mut package = locked_package(&archive);
        package
            .provides
            .apps
            .insert("Demo.app".into(), "Demo.app".into());
        let realizer = PackageRealizer::with_dirs(
            Store::new(root.path().join("store")),
            root.path().join("downloads"),
            root.path().join("tmp"),
        );
        package.output_sha256 = Some(
            realizer
                .realize(&package)
                .unwrap()
                .store_entry
                .output_sha256,
        );
        let bins = root.path().join("profile/bin");
        let apps = Applications::new(
            root.path().join("app-state"),
            root.path().join("Applications"),
        );
        let ops = [Op::RealizePackage { package }];
        let apply = || {
            apply_with_package_realizer(
                &crate::tools::ToolRuntime::default(),
                &ops,
                false,
                &mut Recorder::default(),
                &realizer,
                &bins,
                &apps,
            )
            .unwrap()
        };
        assert!(matches!(
            apply().results[0],
            OpResult::PackageRealized { .. }
        ));
        assert!(matches!(
            apply().results[0],
            OpResult::PackageUnchanged { .. }
        ));
        fs::remove_file(root.path().join("Applications/Demo.app")).unwrap();
        assert!(matches!(
            apply().results[0],
            OpResult::PackageRealized { .. }
        ));
        assert!(root.path().join("Applications/Demo.app").is_symlink());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn application_links_reconcile_only_after_success_and_preserve_user_owner() {
        let root = tempfile::tempdir().unwrap();
        let (_archive_root, archive) = archive_source();
        let mut package = locked_package(&archive);
        package.provides.apps = BTreeMap::from([("Demo.app".into(), "Demo.app".into())]);
        let realizer = PackageRealizer::with_dirs(
            Store::new(root.path().join("store")),
            root.path().join("downloads"),
            root.path().join("tmp"),
        );
        let applications = Applications::new(
            root.path().join("app-state"),
            root.path().join("Applications"),
        );
        let apply = |ops: &[Op]| {
            apply_with_package_realizer(
                &crate::tools::ToolRuntime::default(),
                ops,
                false,
                &mut Recorder::default(),
                &realizer,
                &root.path().join("profile/bin"),
                &applications,
            )
        };
        apply(&[Op::RealizePackage {
            package: package.clone(),
        }])
        .unwrap();
        let link = root.path().join("Applications/Demo.app");
        let original = fs::read_link(&link).unwrap();
        let desired = BTreeMap::from([("Demo.app".into(), original.clone())]);
        applications.synchronize("user", &desired).unwrap();
        apply(&[]).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), original);
        apply(&[Op::RealizePackage {
            package: package.clone(),
        }])
        .unwrap();
        applications.synchronize("user", &BTreeMap::new()).unwrap();
        let failed = Op::Chmod {
            path: root.path().join("missing"),
            mode: 0o755,
        };
        assert!(apply(&[failed]).is_err());
        assert_eq!(fs::read_link(&link).unwrap(), original);
        apply(&[Op::Exec {
            cmd: "/bin/sh".into(),
            args: vec!["-c".into(), "exit 7".into()],
            cwd: root.path().into(),
        }])
        .unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), original);
        let mut update = package.clone();
        update.version = "2.0.0".into();
        apply(&[Op::RealizePackage {
            package: update.clone(),
        }])
        .unwrap();
        let updated = fs::read_link(&link).unwrap();
        assert_ne!(updated, original);
        let error = apply(&[
            Op::RealizePackage { package },
            Op::RealizePackage { package: update },
        ])
        .unwrap_err();
        assert!(error.to_string().contains("conflicting application"));
        assert_eq!(fs::read_link(&link).unwrap(), updated);
        apply(&[]).unwrap();
        assert!(!link.is_symlink());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn application_links_do_not_overwrite_unowned_paths_even_with_force() {
        let root = tempfile::tempdir().unwrap();
        let (_archive_root, archive) = archive_source();
        let mut package = locked_package(&archive);
        package.provides.apps = BTreeMap::from([("Demo.app".into(), "Demo.app".into())]);
        let realizer = PackageRealizer::with_dirs(
            Store::new(root.path().join("store")),
            root.path().join("downloads"),
            root.path().join("tmp"),
        );
        let directory = root.path().join("Applications");
        fs::create_dir_all(directory.join("Demo.app")).unwrap();
        fs::write(directory.join("Demo.app/user-file"), "preserve").unwrap();
        let applications = Applications::new(root.path().join("app-state"), directory.clone());
        assert!(apply_with_package_realizer(
            &crate::tools::ToolRuntime::default(),
            &[Op::RealizePackage { package }],
            true,
            &mut Recorder::default(),
            &realizer,
            &root.path().join("profile/bin"),
            &applications
        )
        .is_err());
        assert_eq!(
            fs::read_to_string(directory.join("Demo.app/user-file")).unwrap(),
            "preserve"
        );
    }
}
