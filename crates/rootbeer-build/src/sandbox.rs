use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use rootbeer_package::BuildEnvironmentLock;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

/// OS-enforced read-only inputs, private writable scratch, and denied networking.
pub struct Sandbox {
    reads: BTreeSet<PathBuf>,
    workspace: PathBuf,
}

impl Sandbox {
    /// Constructs a policy for declared inputs and realized package roots.
    pub fn new(
        environment: &BuildEnvironmentLock,
        reads: impl IntoIterator<Item = PathBuf>,
        workspace: &Path,
    ) -> Result<Self, String> {
        let workspace = workspace
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let mut paths = BTreeSet::from([PathBuf::from("/usr/bin/env")]);
        for path in environment
            .tools
            .values()
            .chain(environment.inputs.values())
            .map(|input| input.path.clone())
            .chain(reads)
        {
            if !path.is_absolute() {
                return Err(format!(
                    "sandbox input must be absolute: {}",
                    path.display()
                ));
            }
            paths.insert(path.canonicalize().map_err(|error| error.to_string())?);
            paths.insert(path);
        }
        if paths.iter().any(|path| path.starts_with(&workspace)) {
            return Err("sandbox scratch must not contain read-only inputs".into());
        }
        Ok(Self {
            reads: paths,
            workspace,
        })
    }

    /// Identifies the isolation implementation and launcher bytes for build cache keys.
    pub fn identity() -> Result<String, String> {
        let launcher = launcher()?;
        let hash = rootbeer_store_legacy::hash_file(launcher).map_err(|error| {
            format!(
                "cannot use sandbox launcher {}: {error}",
                launcher.display()
            )
        })?;
        let identity = format!("{}-v1:{hash}", std::env::consts::OS);
        #[cfg(target_os = "macos")]
        let identity = {
            let loader = Path::new("/System/Library/Sandbox/Profiles/dyld-support.sb");
            if loader.is_file() {
                format!(
                    "{identity}:{}",
                    rootbeer_store_legacy::hash_file(loader).map_err(|error| error.to_string())?
                )
            } else {
                identity
            }
        };
        Ok(identity)
    }

    pub(crate) fn command(
        &self,
        args: &[String],
        source: &Path,
        environment: &BTreeMap<&str, String>,
    ) -> Result<Command, String> {
        let mut isolated_args = vec!["/usr/bin/env".into(), "-i".into()];
        isolated_args.extend(
            environment
                .iter()
                .map(|(name, value)| format!("{name}={value}")),
        );
        isolated_args.extend_from_slice(args);
        #[cfg(target_os = "macos")]
        return macos::command(self, &isolated_args, source);
        #[cfg(target_os = "linux")]
        return linux::command(self, &isolated_args, source);
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        Err("build isolation is supported on macOS and Linux only".into())
    }
}

fn launcher() -> Result<&'static Path, String> {
    #[cfg(target_os = "macos")]
    return Ok(Path::new("/usr/bin/sandbox-exec"));
    #[cfg(target_os = "linux")]
    return Ok(Path::new("/usr/bin/bwrap"));
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    Err("build isolation is supported on macOS and Linux only".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::time::Duration;

    fn shell_lock() -> BuildEnvironmentLock {
        BuildEnvironmentLock {
            schema: 1,
            system: rootbeer_package::ResolveContext::current().system,
            tools: BTreeMap::from([(
                "sh".into(),
                rootbeer_package::BuildEnvironmentInput {
                    path: "/bin/sh".into(),
                    sha256: rootbeer_store_legacy::hash_file("/bin/sh").unwrap(),
                },
            )]),
            inputs: BTreeMap::new(),
            variables: BTreeMap::new(),
        }
    }

    #[test]
    fn denies_host_reads_and_writes_and_keeps_inputs_read_only() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let scratch = root.join("scratch with \"quotes\"");
        let input = root.join("input");
        let secret = root.join("secret");
        fs::create_dir(&scratch).unwrap();
        fs::write(&input, "input\n").unwrap();
        fs::write(&secret, "secret").unwrap();
        symlink(&secret, scratch.join("escape")).unwrap();
        let ln = ["/bin/ln", "/usr/bin/ln"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
            .unwrap();
        let sandbox = Sandbox::new(&shell_lock(), [input.clone(), ln.clone()], &scratch).unwrap();
        let environment = BTreeMap::from([
            ("LN", ln.to_string_lossy().into_owned()),
            ("INPUT", input.to_string_lossy().into_owned()),
            ("SECRET", secret.to_string_lossy().into_owned()),
        ]);
        let script = r#"set -eu
read value < "$INPUT"
test "$value" = input
printf allowed > output
if (read value < "$SECRET") 2>/dev/null; then exit 10; fi
if (printf bad > "$SECRET") 2>/dev/null; then exit 11; fi
if (printf bad > "$INPUT") 2>/dev/null; then exit 12; fi
if (read value < escape) 2>/dev/null; then exit 13; fi
if (printf bad > escape) 2>/dev/null; then exit 14; fi
if "$LN" "$INPUT" alias 2>/dev/null; then
    if (printf bad > alias) 2>/dev/null; then exit 15; fi
fi
"#;
        crate::run_with_sandbox(
            &["/bin/sh".into(), "-c".into(), script.into()],
            &scratch,
            &environment,
            &root.join("checks.log"),
            Duration::from_secs(10),
            Some(&sandbox),
        )
        .unwrap_or_else(|error| {
            panic!(
                "{error}: {}",
                fs::read_to_string(root.join("checks.log"))
                    .or_else(|_| fs::read_to_string(root.join("network.log")))
                    .unwrap()
            )
        });
        assert_eq!(
            fs::read_to_string(scratch.join("output")).unwrap(),
            "allowed"
        );
        assert_eq!(fs::read_to_string(&input).unwrap(), "input\n");
        assert_eq!(fs::read_to_string(&secret).unwrap(), "secret");
    }

    #[test]
    fn network_connections_are_denied() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source = root.join("network.c");
        let probe = root.join("network");
        fs::write(
            &source,
            r#"#include <sys/socket.h>
#include <arpa/inet.h>
#include <stdlib.h>
int main(int argc, char **argv) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) return 0;
    struct sockaddr_in address = {0};
    address.sin_family = AF_INET;
    address.sin_port = htons(atoi(argv[1]));
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    return connect(fd, (struct sockaddr *)&address, sizeof(address)) == 0 ? 42 : 0;
}
"#,
        )
        .unwrap();
        assert!(Command::new("/usr/bin/cc")
            .args([&source, Path::new("-o"), &probe])
            .status()
            .unwrap()
            .success());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port().to_string();
        assert_eq!(
            Command::new(&probe).arg(&port).status().unwrap().code(),
            Some(42)
        );
        let scratch = root.join("scratch");
        fs::create_dir(&scratch).unwrap();
        let sandbox = Sandbox::new(&shell_lock(), [probe.clone()], &scratch).unwrap();
        crate::run_with_sandbox(
            &[probe.to_string_lossy().into_owned(), port],
            &scratch,
            &BTreeMap::new(),
            &root.join("network.log"),
            Duration::from_secs(10),
            Some(&sandbox),
        )
        .unwrap_or_else(|error| {
            panic!(
                "{error}: {}",
                fs::read_to_string(root.join("checks.log"))
                    .or_else(|_| fs::read_to_string(root.join("network.log")))
                    .unwrap()
            )
        });
    }
}
