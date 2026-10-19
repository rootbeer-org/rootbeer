use std::io;
use std::process::Command;

use crate::tools::ToolRuntime;

pub(crate) fn command(tools: &ToolRuntime) -> io::Result<Command> {
    tools.command("op", "2.39.0", "op")
}

pub(crate) fn read(tools: &ToolRuntime, reference: &str) -> io::Result<String> {
    let output = command(tools)?
        .args(["read", "--no-newline", reference])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "op read failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    String::from_utf8(output.stdout).map_err(|_| io::Error::other("op returned invalid UTF-8"))
}
