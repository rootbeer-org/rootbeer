//! Tools rb runs on untrusted input, like unpacking an archive. A tool is a
//! function of rb itself, run in a fresh rb process with no network, its
//! input on stdin, and its output on stdout. The sandbox lets it read only
//! the host's libraries and rb, and write only a private scratch directory.
//! What a tool makes is pinned by a digest, so the rb that ran it never
//! reaches a key.

use crate::{Error, darwin, io_at, linux};
use rootbeer_drv::Format;
use std::collections::BTreeSet;
use std::fmt::Display;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::os::fd::AsFd;
use std::process::{ChildStdout, Stdio};
use std::thread;

/// Marks a process as a tool rather than a command, and names the tool.
const MARKER: &str = "RB_TOOL";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tool {
    /// Writes the tree an archive holds as a rootbeer-tree stream.
    Unpack(Format),
}

impl Tool {
    fn name(self) -> String {
        match self {
            Tool::Unpack(format) => format!("unpack {format}"),
        }
    }

    fn parse(name: &str) -> Option<Tool> {
        let format = name.strip_prefix("unpack ")?;
        Format::try_from(format.to_string()).ok().map(Tool::Unpack)
    }

    fn serve(self, input: File, output: impl Write) -> Result<(), String> {
        match self {
            Tool::Unpack(format) => rootbeer_unpack::unpack(format, input, output),
        }
    }
}

/// Serves the tool this process was started as, returning the code to exit
/// with. A process that isn't a tool gets none, so `main` calls this first
/// and carries on.
pub fn dispatch() -> Option<i32> {
    let name = std::env::var_os(MARKER)?;
    let served = name
        .to_str()
        .and_then(Tool::parse)
        .ok_or_else(|| format!("{MARKER} names no tool: {name:?}"))
        .and_then(|tool| {
            let input = io::stdin()
                .as_fd()
                .try_clone_to_owned()
                .map(File::from)
                .map_err(|error| format!("stdin: {error}"))?;
            let mut output = BufWriter::new(io::stdout().lock());
            tool.serve(input, &mut output)?;
            output.flush().map_err(|error| format!("stdout: {error}"))
        });

    match served {
        Ok(()) => Some(0),
        Err(error) => {
            eprintln!("rb {}: {error}", name.to_string_lossy());
            Some(1)
        }
    }
}

/// Runs `tool` on `input` in its sandbox and hands its output to `read`. A
/// tool that fails says why, alongside what `read` made of its output.
pub(crate) fn run<T, E: Display>(
    tool: Tool,
    input: File,
    read: impl FnOnce(&mut ChildStdout) -> Result<T, E>,
) -> Result<T, Error> {
    let failed = |reason: String| Error::Tool(format!("rb's {} tool {reason}", tool.name()));

    // The sandbox grants real paths, and a temporary directory or the rb on
    // PATH is often reached through a symlink.
    let program = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| failed(format!("can't find rb: {error}")))?;
    let program = program
        .to_str()
        .ok_or_else(|| failed(format!("needs a UTF-8 path, not {}", program.display())))?
        .to_string();
    let held = tempfile::tempdir().map_err(|error| failed(error.to_string()))?;
    let scratch = fs::canonicalize(held.path()).map_err(io_at(held.path()))?;

    let mut command = match cfg!(target_os = "macos") {
        true => {
            let writes = [scratch.display().to_string()];
            let reads = [program.clone()];
            darwin::command(&reads, &writes, &BTreeSet::new(), false)
        }
        false => linux::tool(&program, &scratch),
    };

    let mut child = command
        .arg(&program)
        .env_clear()
        .env(MARKER, tool.name())
        .env("TMPDIR", &scratch)
        .stdin(input)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| failed(format!("didn't start: {error}")))?;

    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(failed("has no output".into()));
    };

    thread::scope(|scope| {
        // What a tool says only explains a failure, so losing it mustn't
        // cause one.
        let said = scope.spawn(move || {
            let mut said = Vec::new();
            stderr
                .read_to_end(&mut said)
                .map(|_| said)
                .unwrap_or_default()
        });

        // A reader that stops early closes the pipe, so the tool can't block
        // on writing what nobody reads.
        let result = read(&mut stdout).map_err(|error| error.to_string());
        drop(stdout);

        let status = child.wait().map_err(|error| failed(error.to_string()))?;
        let said = said.join().unwrap_or_default();
        let said = String::from_utf8_lossy(&said);
        let said = said.trim();
        match (result, status.success()) {
            (Ok(value), true) => Ok(value),
            (Err(error), true) => Err(Error::Tool(error)),
            (Ok(_), false) => Err(failed(format!("{status}: {said}"))),
            (Err(error), false) => Err(failed(format!("{status}: {said} ({error})"))),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_is_named_the_way_it_parses() {
        for format in [Format::Dmg, Format::Pkg, Format::TarBz2, Format::Zip] {
            let tool = Tool::Unpack(format);
            assert_eq!(Tool::parse(&tool.name()), Some(tool));
        }

        assert_eq!(Tool::parse("unpack rar"), None);
        assert_eq!(Tool::parse("build zip"), None);
    }
}
