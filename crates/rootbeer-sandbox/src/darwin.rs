use rootbeer_drv::Allow;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

pub(crate) const PROFILE: &str = "darwin-v1";

// The host toolchain stand-in (RB_SYSTEM) until toolchains are catalog packages.
const HOST: [&str; 9] = [
    "/bin",
    "/sbin",
    "/usr/bin",
    "/usr/sbin",
    "/usr/lib",
    "/usr/libexec",
    "/usr/share",
    "/Library/Developer/CommandLineTools",
    "/Applications/Xcode.app",
];

const EXECUTABLES: [&str; 7] = [
    "/bin",
    "/sbin",
    "/usr/bin",
    "/usr/sbin",
    "/usr/libexec",
    "/Library/Developer/CommandLineTools",
    "/Applications/Xcode.app",
];

// Test suites resolve hosts, users, and time zones.
const SYSTEM: [&str; 11] = [
    "/System",
    "/private/var/db/dyld",
    "/private/var/db/xcode_select_link",
    "/private/var/select",
    "/private/var/db/timezone",
    "/private/etc/localtime",
    "/private/etc/hosts",
    "/private/etc/services",
    "/private/etc/protocols",
    "/private/etc/passwd",
    "/private/etc/group",
];

const DEVICES: [&str; 4] = ["/dev/null", "/dev/zero", "/dev/random", "/dev/urandom"];
const ANCESTORS: [&str; 5] = ["/", "/opt", "/opt/rb", "/opt/rb/var", "/opt/rb/var/build"];
const LINKS: [&str; 3] = ["/etc", "/tmp", "/var"];

// This is basically a convoluted way of saying HTTP(S) and DNS are allowed. No
// Unix sockets except for mDNS.
const NETWORK: &str = "(allow network-outbound (remote ip \"*:443\") (remote ip \"*:80\"))
(allow network-outbound (remote unix-socket (path-literal \"/private/var/run/mDNSResponder\")))
(allow mach-lookup (global-name \"com.apple.dnssd.service\" \"com.apple.SystemConfiguration.configd\" \"com.apple.trustd\" \"com.apple.trustd.agent\"))
(allow file-read* (subpath \"/private/etc/ssl\") (literal \"/private/etc/resolv.conf\") (literal \"/private/var/run/resolv.conf\"))
";

/// `sandbox-exec` with a profile that reads only the host toolchain and
/// `reads`, writes only `writes`, and has no network unless `has_network` or
/// allowed loopback.
pub(crate) fn command(
    reads: &[String],
    writes: &[String],
    allow: &BTreeSet<Allow>,
    has_network: bool,
) -> Command {
    let parents = HOST
        .into_iter()
        .chain(SYSTEM)
        .chain(DEVICES)
        .chain(reads.iter().map(String::as_str))
        .chain(writes.iter().map(String::as_str))
        .flat_map(|path| Path::new(path).ancestors().skip(1))
        .map(|path| path.display().to_string())
        .chain(LINKS.map(String::from))
        .collect::<BTreeSet<_>>()
        .iter()
        .map(|path| format!("(literal {path:?})"))
        .collect::<Vec<_>>()
        .join(" ");

    let host = rules(HOST);
    let executables = rules(EXECUTABLES);
    let system = rules(SYSTEM);
    let devices = rules(DEVICES);
    let reads = rules(reads.iter().map(String::as_str));
    let sockets = writes
        .iter()
        .map(|path| {
            format!(
                "(local unix-socket (subpath {path:?})) (remote unix-socket (subpath {path:?}))"
            )
        })
        .collect::<Vec<_>>()
        .join(" ");

    let writes = rules(writes.iter().map(String::as_str));
    let ancestors = ANCESTORS
        .map(|path| format!("(literal {path:?})"))
        .join(" ");

    let mut profile = format!(
        "(version 1)
(deny default)
(import \"dyld-support.sb\")
(allow process-fork)
(allow signal (target same-sandbox))
(allow sysctl-read)
(allow file-read-metadata {parents})
(allow file-read* {host} {system} {devices} {reads} {writes} {ancestors} (subpath \"/dev/fd\"))
(allow file-ioctl {host} {devices} {reads} {writes})
(allow process-exec {executables} {reads} {writes})
(allow file-write* {writes} (literal \"/dev/null\") (literal \"/dev/zero\") (subpath \"/dev/fd\"))
"
    );

    for allow in allow {
        let rule = match allow {
            Allow::LocalNetwork => format!(
                "(allow network* (local ip \"localhost:*\") (remote ip \"localhost:*\") {sockets})\n"
            ),
            Allow::Ipc => "(allow ipc-sysv* ipc-posix*)\n".to_string(),
            Allow::Tmp => {
                "(allow file-read* file-write* file-ioctl (subpath \"/private/tmp\"))\n".to_string()
            }
        };

        profile.push_str(&rule);
    }

    if has_network {
        profile.push_str(NETWORK);
    }

    let mut command = Command::new("/usr/bin/sandbox-exec");
    command.arg("-p").arg(profile);
    command
}

fn rules<'a>(paths: impl IntoIterator<Item = &'a str>) -> String {
    paths
        .into_iter()
        .map(|path| format!("(subpath {path:?})"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn system() -> Result<String, String> {
    let sdk = output("/usr/bin/xcrun", &["--show-sdk-version"])?;
    let clang = output("/usr/bin/cc", &["--version"])?;

    // The first line reads "Apple clang version 21.0.0 (clang-2100.3.34.2)".
    let build = clang
        .split_once("(clang-")
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(build, _)| build)
        .ok_or_else(|| format!("cc --version printed {clang:?}"))?;

    Ok(format!("macos-sdk-{}-clang-{build}", sdk.trim()))
}

fn output(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .env_clear()
        .output()
        .map_err(|error| format!("{program}: {error}"))?;

    if !output.status.success() {
        return Err(format!("{program} {}", output.status));
    }

    String::from_utf8(output.stdout).map_err(|error| format!("{program}: {error}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_a_fetching_build_reaches_the_network() {
        for has_network in [false, true] {
            let command = super::command(&[], &[], &Default::default(), has_network);
            let profile = command.get_args().nth(1).unwrap().to_str().unwrap();
            assert_eq!(profile.contains("network-outbound"), has_network);
            assert_eq!(profile.contains(super::NETWORK), has_network);
        }
    }
}
