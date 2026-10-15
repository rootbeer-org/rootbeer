use super::{Phases, Script, lines};
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

    // Zig finds each package in the global cache by its hash, so a build
    // with the vendored packages there never fetches. The compilers it spawns
    // read the cache from the environment, not the flags.
    Ok(Script {
        env: BTreeMap::new(),
        phases: Phases {
            configure: lines(&[
                r#"export ZIG_GLOBAL_CACHE_DIR="${PWD}/zig-global-cache" ZIG_LOCAL_CACHE_DIR="${PWD}/zig-cache""#,
                "mkdir zig-global-cache",
                r#"cp -R "${vendor}" zig-global-cache/p"#,
                "chmod -R u+w zig-global-cache/p",
            ]),
            build: vec![values.command(&[zig.as_slice(), &build.args].concat())?],
            ..Phases::default()
        },
    })
}

/// Zig checks each fetched package against the hash in `build.zig.zon`.
pub(super) fn vendor() -> Script {
    Script {
        env: BTreeMap::new(),
        phases: Phases {
            // Zig stages a zip download in the cache's tmp/ without creating it.
            build: lines(&[
                r#"mkdir -p "${TMPDIR}/zig/tmp""#,
                r#"zig build --fetch=all --global-cache-dir "${TMPDIR}/zig""#,
                r#"mkdir -p "${TMPDIR}/zig/p""#,
                r#"cp -R "${TMPDIR}/zig/p" "${out}""#,
            ]),
            ..Phases::default()
        },
    }
}
