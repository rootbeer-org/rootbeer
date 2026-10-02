use rootbeer_drv::STORE_ROOT;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) const PROFILE: &str = "linux-v1";

pub(crate) const WORKSPACE: &str = "/build";
const LINKS: [&str; 4] = ["/bin", "/sbin", "/lib", "/lib64"];

// Toolchains resolve alternatives and the loader cache. Test suites resolve
// hosts, users, and time zones.
const ETC: [&str; 9] = [
    "/etc/alternatives",
    "/etc/ld.so.cache",
    "/etc/localtime",
    "/etc/nsswitch.conf",
    "/etc/hosts",
    "/etc/services",
    "/etc/protocols",
    "/etc/passwd",
    "/etc/group",
];

pub(crate) fn staging(directory: &Path) -> PathBuf {
    directory.join("store")
}

/// `bwrap` with fresh namespaces for everything, so the network is loopback
/// only. It sees the host toolchain and `reads` read-only and writes only the
/// build directory and staged outputs.
pub(crate) fn command(reads: &[String], directory: &Path) -> Command {
    let mut command = Command::new("/usr/bin/bwrap");
    command.args([
        "--unshare-all",
        "--die-with-parent",
        "--new-session",
        "--hostname",
        "rootbeer",
        "--uid",
        "1000",
        "--gid",
        "1000",
        "--ro-bind",
        "/usr",
        "/usr",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--tmpfs",
        "/tmp",
    ]);

    for link in LINKS {
        if let Ok(target) = fs::read_link(link) {
            command.arg("--symlink").arg(target).arg(link);
        }
    }

    for path in ETC {
        command.args(["--ro-bind-try", path, path]);
    }

    command.arg("--bind").arg(directory).arg(WORKSPACE);
    command
        .arg("--bind")
        .arg(staging(directory))
        .arg(STORE_ROOT);
    for path in reads {
        command.args(["--ro-bind", path, path]);
    }

    command.args(["--chdir", &format!("{WORKSPACE}/src"), "--"]);
    command
}
