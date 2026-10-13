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

// DNS and the CA bundle, for steps that fetch locked dependencies.
const NETWORK: [&str; 4] = [
    "/etc/resolv.conf",
    "/etc/ssl",
    "/etc/pki",
    "/etc/ca-certificates",
];

pub(crate) fn staging(directory: &Path) -> PathBuf {
    directory.join("store")
}

/// `bwrap` with fresh namespaces allow us to isolate network access and FS
/// access. Also locks down write access to the build directory and outputs.
pub(crate) fn command(reads: &[String], directory: &Path, has_network: bool) -> Command {
    let mut command = Command::new("/usr/bin/bwrap");
    command.arg("--unshare-all");
    if has_network {
        command.arg("--share-net");
        for path in NETWORK {
            command.args(["--ro-bind-try", path, path]);
        }
    }

    command.args([
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

    command.args(["--remount-ro", "/"]);
    command.args(["--chdir", &format!("{WORKSPACE}/src"), "--"]);
    command
}

pub(crate) fn system() -> Result<String, String> {
    std::env::var("RB_HOST_SYSTEM")
        .map_err(|_| "RB_HOST_SYSTEM is unset outside the builder image".to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_a_fetching_build_shares_the_network() {
        let directory = std::path::Path::new("/opt/rb/var/build/k");
        for has_network in [false, true] {
            let command = super::command(&[], directory, has_network);
            let args = command.get_args().collect::<Vec<_>>();
            assert_eq!(args.first().copied(), Some("--unshare-all".as_ref()));
            assert_eq!(args.contains(&"--share-net".as_ref()), has_network);
            assert_eq!(args.contains(&"/etc/resolv.conf".as_ref()), has_network);
        }
    }

    #[test]
    fn root_is_remounted_read_only_last() {
        let directory = std::path::Path::new("/opt/rb/var/build/k");
        let reads = ["/opt/rb/store/dep".to_string()];
        let command = super::command(&reads, directory, false);
        let args = command.get_args().collect::<Vec<_>>();
        let tail = ["--remount-ro", "/", "--chdir", "/build/src", "--"];
        let tail = tail.map(std::ffi::OsStr::new);
        assert!(args.ends_with(&tail));
    }
}
