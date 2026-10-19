use crate::{
    executor::{ExecutionHandler, ExecutionReport, OpResult},
    Op,
};

pub fn dry_run(ops: &[Op], handler: &mut impl ExecutionHandler) -> ExecutionReport {
    let mut report = ExecutionReport::default();

    for op in ops {
        handler.on_start(op);

        match op {
            Op::WriteFile { path, source } => {
                // Inline sources know their size up front; secret-backed
                // sources can't be measured until apply actually fetches.
                let result = OpResult::FileWritten {
                    path: path.clone(),
                    bytes: source.known_size(),
                };
                handler.on_result(&result);
                report.results.push(result);
            }

            Op::Symlink { src, dst } => {
                let result = OpResult::SymlinkCreated {
                    src: src.clone(),
                    dst: dst.clone(),
                };
                handler.on_result(&result);
                report.results.push(result);
            }

            Op::CopyFileIfMissing { src, dst } => {
                let result = if dst.exists() || dst.is_symlink() {
                    OpResult::FileCopySkipped { dst: dst.clone() }
                } else {
                    OpResult::FileCopied {
                        src: src.clone(),
                        dst: dst.clone(),
                    }
                };
                handler.on_result(&result);
                report.results.push(result);
            }

            Op::Exec { cmd, args, .. } => {
                let display = std::iter::once(cmd.as_str())
                    .chain(args.iter().map(|s| s.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ");

                let result = OpResult::CommandRan {
                    cmd: display,
                    status: 0,
                };
                handler.on_result(&result);
                report.results.push(result);
            }

            Op::Chmod { path, mode } => {
                let result = OpResult::Chmodded {
                    path: path.clone(),
                    mode: *mode,
                };
                handler.on_result(&result);
                report.results.push(result);
            }

            Op::SetRemoteUrl { dir: _, url } => {
                let result = OpResult::RemoteUpdated {
                    from: String::from("(current origin)"),
                    to: url.clone(),
                };
                handler.on_result(&result);
                report.results.push(result);
            }

            Op::Package { request } => {
                let result = OpResult::PackagePlanned {
                    spec: request.to_string(),
                };
                handler.on_result(&result);
                report.results.push(result);
            }
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::WriteSource;
    use std::path::PathBuf;

    #[derive(Default)]
    struct Recorder {
        starts: Vec<String>,
        results: Vec<OpResult>,
    }

    impl ExecutionHandler for Recorder {
        fn on_start(&mut self, op: &Op) {
            self.starts.push(format!("{op:?}"));
        }
        fn on_output(&mut self, _: &str) {}
        fn on_result(&mut self, r: &OpResult) {
            self.results.push(r.clone());
        }
    }

    #[test]
    fn dry_run_does_not_touch_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing/file.txt");
        let ops = vec![Op::WriteFile {
            path: path.clone(),
            source: WriteSource::text("hello"),
        }];

        dry_run(&ops, &mut Recorder::default());

        assert!(!path.exists(), "dry run must not create files");
    }

    #[test]
    fn dry_run_emits_one_result_per_op_in_order() {
        let ops = vec![
            Op::WriteFile {
                path: PathBuf::from("/tmp/rb-test/a"),
                source: WriteSource::text("a"),
            },
            Op::Symlink {
                src: PathBuf::from("/tmp/rb-test/src"),
                dst: PathBuf::from("/tmp/rb-test/dst"),
            },
            Op::Exec {
                cmd: "echo".into(),
                args: vec!["hi".into()],
                cwd: PathBuf::from("/tmp"),
            },
            Op::SetRemoteUrl {
                dir: PathBuf::from("/tmp"),
                url: "git@example.com:repo.git".into(),
            },
        ];

        let mut h = Recorder::default();
        let report = dry_run(&ops, &mut h);

        assert_eq!(report.results.len(), 4);
        assert!(matches!(report.results[0], OpResult::FileWritten { .. }));
        assert!(matches!(report.results[1], OpResult::SymlinkCreated { .. }));
        assert!(matches!(
            report.results[2],
            OpResult::CommandRan { status: 0, .. }
        ));
        assert!(matches!(report.results[3], OpResult::RemoteUpdated { .. }));
    }
}
