use std::path::PathBuf;

mod apply;
mod dry_run;

use crate::plan::Op;

#[derive(Debug, Clone)]
pub enum OpResult {
    /// `bytes` is `None` for dry-runs of secret-backed writes whose payload
    /// size isn't known until the apply phase fetches it. Apply always sets
    /// `Some(real_count)`.
    FileWritten {
        path: PathBuf,
        bytes: Option<usize>,
    },
    SymlinkCreated {
        src: PathBuf,
        dst: PathBuf,
    },
    SymlinkUnchanged {
        dst: PathBuf,
    },
    SymlinkOverwritten {
        src: PathBuf,
        dst: PathBuf,
    },
    FileCopied {
        src: PathBuf,
        dst: PathBuf,
    },
    FileCopySkipped {
        dst: PathBuf,
    },
    CommandRan {
        cmd: String,
        status: i32,
    },
    Chmodded {
        path: PathBuf,
        mode: u32,
    },
    RemoteUpdated {
        from: String,
        to: String,
    },
    RemoteUnchanged {
        url: String,
    },
    PackagePlanned {
        spec: String,
    },
}

/// Receives lifecycle events during pipeline execution.
pub trait ExecutionHandler {
    /// An operation is about to be executed.
    fn on_start(&mut self, op: &Op);
    /// A line of stdout/stderr output from an exec command.
    fn on_output(&mut self, line: &str);
    /// An operation completed.
    fn on_result(&mut self, result: &OpResult);
}

#[derive(Debug, Default)]
pub struct ExecutionReport {
    pub results: Vec<OpResult>,
}

#[cfg(test)]
pub use apply::apply;
pub use apply::apply_with_tools;
pub use dry_run::dry_run;
