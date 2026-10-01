use super::Script;
use crate::recipe::Build;
use crate::template::Values;
use std::collections::BTreeMap;

pub(super) fn build(values: &Values, build: &Build) -> Result<Script, String> {
    let zig = [
        "zig",
        "build",
        "--prefix",
        "{prefix}",
        "--cache-dir",
        "zig-cache",
        "--global-cache-dir",
        "zig-global-cache",
        "-j{jobs}",
    ]
    .map(String::from);

    Ok(Script {
        env: BTreeMap::new(),
        lines: vec![values.command(&[zig.as_slice(), &build.args].concat())?],
    })
}
