use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::{fs, os::unix::fs as unix_fs, process, process::Command, thread};

use crate::{
    executor::{ExecutionHandler, ExecutionReport, OpResult},
    plan::WriteSource,
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

#[cfg(test)]
pub fn apply(
    ops: &[Op],
    force: bool,
    handler: &mut impl ExecutionHandler,
) -> io::Result<ExecutionReport> {
    apply_with_tools(&crate::tools::ToolRuntime::default(), ops, force, handler)
}

pub fn apply_with_tools(
    tools: &crate::tools::ToolRuntime,
    ops: &[Op],
    force: bool,
    handler: &mut impl ExecutionHandler,
) -> io::Result<ExecutionReport> {
    let mut report = ExecutionReport::default();

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

            Op::Package { request } => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("package `{request}` must be installed before apply"),
                ));
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
