//! The caller's /opt/rb profiles on PATH, which `rb env` prints and
//! `rb.env_export` writes.

use std::fs;
use std::path::{Path, PathBuf};

const PROFILES: &str = "/opt/rb/profiles";
const LEGACY_ROOT: &str = "/opt/rootbeer";
const LEGACY_PROFILES: [&str; 2] = ["default", "user"];

/// The profile `rb apply` installs into.
pub const DEFAULT: &str = "default";

/// The profile `rb use` installs into.
pub const USER: &str = "user";

/// Later ones take precedence on PATH.
pub const ON_PATH: [&str; 2] = [DEFAULT, USER];

/// `/opt/rb/profiles/<uid>`.
pub fn profiles() -> PathBuf {
    Path::new(PROFILES).join(unsafe { libc::getuid() }.to_string())
}

/// The `default` profile's commands, which stay at this path across
/// generations.
pub fn bin_dir() -> PathBuf {
    rootbeer_profile::bin(&profiles(), DEFAULT)
}

/// Where `rb.env_export` writes `contents`.
pub fn path() -> PathBuf {
    crate::state_dir().join("env.sh")
}

/// What the old `/opt/rootbeer` store holds for this user. `rb migrate` empties
/// it, and `ROOTBEER_ROOT` moves the old store.
pub fn legacy_roots() -> PathBuf {
    std::env::var_os("ROOTBEER_ROOT")
        .map_or(PathBuf::from(LEGACY_ROOT), PathBuf::from)
        .join("var/roots")
        .join(unsafe { libc::getuid() }.to_string())
}

/// Whether the old store may still hold something this user runs. Clients
/// from before roots were recorded have only the old profiles to go by, so
/// `rb migrate` leaves a marker once it moves everything.
pub fn is_legacy_live() -> bool {
    if migrated_path().exists() {
        return false;
    }

    let has_roots = fs::read_dir(legacy_roots()).is_ok_and(|mut entries| entries.next().is_some());
    has_roots
        || LEGACY_PROFILES
            .iter()
            .any(|name| legacy_profile(name).exists())
}

/// Written by `rb migrate` once the old store holds nothing for this user.
pub fn migrated_path() -> PathBuf {
    crate::state_dir().join("migrated")
}

/// The `env.sh` the old store's `default` profile kept, which configurations
/// from before /opt/rb source.
pub fn legacy_env_path() -> PathBuf {
    legacy_profile("default").join("env.sh")
}

/// Until `rb migrate` moves everything, the old profiles stay on PATH behind
/// the new ones, so nothing installed there disappears first.
pub fn contents() -> String {
    render(is_legacy_live())
}

/// What `contents` becomes once the old store holds nothing for this user.
pub fn migrated_contents() -> String {
    render(false)
}

fn render(has_legacy: bool) -> String {
    let legacy = has_legacy.then(|| LEGACY_PROFILES.map(|name| legacy_profile(name).join("bin")));
    let profiles = profiles();
    let current = ON_PATH.map(|name| rootbeer_profile::bin(&profiles, name));
    let lines = legacy.into_iter().flatten().chain(current);
    lines.map(|bin| prepend(&bin)).collect()
}

fn legacy_profile(name: &str) -> PathBuf {
    crate::state_dir()
        .join("profiles")
        .join(name)
        .join("current")
}

fn prepend(bin: &Path) -> String {
    let quoted = format!("'{}'", bin.to_string_lossy().replace('\'', "'\\''"));
    format!("case \":$PATH:\" in *:{quoted}:*) ;; *) export PATH={quoted}:\"$PATH\" ;; esac\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quoted_bin_is_added_once() {
        let line = prepend(Path::new("/it's/bin"));
        assert_eq!(
            line,
            "case \":$PATH:\" in *:'/it'\\''s/bin':*) ;; *) export PATH='/it'\\''s/bin':\"$PATH\" ;; esac\n"
        );
    }
}
