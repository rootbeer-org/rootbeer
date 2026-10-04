use crate::{Error, Request, darwin, derivation, io_at, linux, scan};
use rootbeer_drv::{Allow, Build, Check};
use rootbeer_drv::{
    Dependency, DependencyKind, Derivation, Key, Platform, STORE_ROOT, fetch_path, output_path,
};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};
use std::collections::{BTreeMap, BTreeSet};
use std::env::consts::{ARCH, OS};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use walkdir::WalkDir;

const FIXED: [(&str, &str); 8] = [
    ("HOME", "/nonexistent"),
    ("LC_ALL", "C"),
    ("TZ", "UTC"),
    ("SOURCE_DATE_EPOCH", "1"),
    ("ZERO_AR_DATE", "1"),
    ("CC", "/usr/bin/cc"),
    ("CXX", "/usr/bin/c++"),
    ("CONFIG_SHELL", "/bin/sh"),
];

const HOST_PATH: [&str; 4] = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];

#[derive(Clone, Copy)]
enum Profile {
    Darwin,
    Linux,
}

struct Sandbox {
    profile: Profile,
    allow: BTreeSet<Allow>,
    closure: BTreeSet<Key>,
    variables: BTreeMap<String, String>,
    bins: Vec<String>,
    reads: Vec<String>,
    out: Option<String>,
}

pub(crate) fn build(request: &mut Request, build: &Build) -> Result<BTreeSet<Key>, Error> {
    if build.outputs.len() != 1 {
        return Err(failure(
            request.key,
            "declares outputs besides out, which aren't supported yet",
        ));
    }

    let out = output_path(request.key, &build.name, &build.version, "out");
    remove(&out)?;

    let mut sandbox = Sandbox::new(request, &build.sandbox, build.platform, &build.env)?;
    sandbox.allow.clone_from(&build.allow);
    sandbox.dependencies(request, &build.dependencies)?;
    for (name, key) in &build.inputs {
        let path = sandbox.read(request, key)?;
        sandbox.variables.insert(name.clone(), path);
    }

    let out_path = out.display().to_string();
    sandbox.variables.insert("out".into(), out_path.clone());
    sandbox.out = Some(out_path);

    // Outputs might reference runtime dependencies
    let mut allowed = sandbox.closure.clone();
    allowed.extend(
        build
            .dependencies
            .iter()
            .filter(|dependency| dependency.kind == DependencyKind::Runtime)
            .map(|dependency| dependency.key.clone()),
    );

    let built = sandbox.run(request, &build.script).and_then(|()| {
        if !out.exists() {
            return Err(failure(request.key, "script did not create $out"));
        }

        Ok(())
    });

    if let Err(error) = built {
        let _ = remove(&out);
        return Err(error);
    }

    let references = scan::scan(request.key, &out, &allowed, &mut *request.log)?;
    read_only(&out)?;
    Ok(references)
}

pub(crate) fn check(request: &mut Request, check: &Check) -> Result<(), Error> {
    let mut sandbox = Sandbox::new(request, &check.sandbox, check.platform, &check.env)?;
    sandbox.dependencies(request, &check.dependencies)?;

    let target = sandbox.read(request, &check.target)?;
    sandbox.bins.insert(0, format!("{target}/bin"));
    sandbox.variables.insert("target".into(), target);
    sandbox.run(request, &check.script)
}

impl Sandbox {
    fn new(
        request: &Request,
        profile: &str,
        platform: Platform,
        env: &BTreeMap<String, String>,
    ) -> Result<Sandbox, Error> {
        let is_host = platform.to_string() == format!("{ARCH}-{OS}");
        let runnable = match (profile, platform) {
            _ if !is_host => None,
            (darwin::PROFILE, Platform::Aarch64Macos) => Some(Profile::Darwin),
            (linux::PROFILE, Platform::Aarch64Linux | Platform::X86_64Linux) => {
                Some(Profile::Linux)
            }
            _ => None,
        };

        let Some(runnable) = runnable else {
            return Err(failure(
                request.key,
                &format!("needs {profile} on {platform}, which this machine can't run"),
            ));
        };

        // Keys name the toolchain, so building with another one would publish
        // different output under the same key.
        if let Some(expected) = env.get("RB_SYSTEM") {
            let system = match runnable {
                Profile::Darwin => darwin::system(),
                Profile::Linux => linux::system(),
            };

            let actual = system.map_err(|reason| {
                failure(
                    request.key,
                    &format!("can't identify the toolchain, {reason}"),
                )
            })?;

            if *expected != actual {
                return Err(failure(
                    request.key,
                    &format!(
                        "was evaluated for the toolchain {expected}, but this machine has \
                         {actual}. Pass a --host file naming it to build here."
                    ),
                ));
            }
        }

        let mut variables = env.clone();
        variables.extend(FIXED.map(|(name, value)| (name.to_string(), value.to_string())));
        variables.insert("jobs".into(), request.jobs.to_string());

        Ok(Sandbox {
            profile: runnable,
            allow: BTreeSet::new(),
            closure: BTreeSet::new(),
            variables,
            bins: Vec::new(),
            reads: Vec::new(),
            out: None,
        })
    }

    // Build dependencies are commands; linked ones are found by search paths.
    fn dependencies(
        &mut self,
        request: &Request,
        dependencies: &[Dependency],
    ) -> Result<(), Error> {
        let mut linked = Vec::new();
        for dependency in dependencies {
            match dependency.kind {
                DependencyKind::Build => {
                    let path = self.read(request, &dependency.key)?;
                    self.bins.push(format!("{path}/bin"));
                }
                DependencyKind::Linked => linked.push(self.read(request, &dependency.key)?),
                DependencyKind::Runtime => {}
            }
        }

        if linked.is_empty() {
            return Ok(());
        }

        let join = |suffixes: &[&str]| {
            linked
                .iter()
                .flat_map(|path| suffixes.iter().map(move |suffix| format!("{path}{suffix}")))
                .collect::<Vec<_>>()
                .join(":")
        };

        self.variables.extend([
            ("CPATH".to_string(), join(&["/include"])),
            ("LIBRARY_PATH".to_string(), join(&["/lib"])),
            (
                "PKG_CONFIG_PATH".to_string(),
                join(&["/lib/pkgconfig", "/share/pkgconfig"]),
            ),
            ("CMAKE_PREFIX_PATH".to_string(), join(&[""])),
        ]);

        Ok(())
    }

    /// Store path of a derivation the sandbox may read, along with everything
    /// it references at runtime. All of it must be realized.
    fn read(&mut self, request: &Request, key: &Key) -> Result<String, Error> {
        let path = self.include(request, key)?;
        let mut pending = request
            .references
            .get(key)
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        while let Some(reference) = pending.pop() {
            if self.closure.contains(reference) {
                continue;
            }

            self.include(request, reference)?;
            pending.extend(request.references.get(reference).into_iter().flatten());
        }

        Ok(path)
    }

    fn include(&mut self, request: &Request, key: &Key) -> Result<String, Error> {
        let path = match derivation(request, key)? {
            Derivation::Fetch(_) => fetch_path(key),
            Derivation::Build(build) => output_path(key, &build.name, &build.version, "out"),
            Derivation::Check(_) => {
                return Err(failure(
                    request.key,
                    &format!("names {key}, which has no output"),
                ));
            }
        };

        if !path.exists() {
            return Err(failure(
                request.key,
                &format!("needs {key}, which is not realized"),
            ));
        }

        let path = path.display().to_string();
        if self.closure.insert(key.clone()) {
            self.reads.push(path.clone());
        }

        Ok(path)
    }

    // Runs in a fresh directory that is kept when the script fails.
    fn run(&mut self, request: &mut Request, script: &str) -> Result<(), Error> {
        let directory = directory(request.key);
        let source = directory.join("src");
        let tmp = directory.join("tmp");
        let file = directory.join("script");

        // The parent is shared by EVERY user so we need to create the build
        // directory from scratch every time.
        remove(&directory)?;
        if let Some(parent) = directory.parent() {
            fs::create_dir_all(parent).map_err(io_at(parent))?;
        }

        fs::create_dir(&directory).map_err(io_at(&directory))?;
        fs::create_dir(&source).map_err(io_at(&source))?;
        fs::create_dir(&tmp).map_err(io_at(&tmp))?;
        fs::write(&file, script).map_err(io_at(&file))?;

        // The workspace is what the script sees as the build directory
        let (workspace, mut command) = match self.profile {
            Profile::Darwin => {
                let writes = self
                    .out
                    .iter()
                    .cloned()
                    .chain([directory.display().to_string()]);
                let mut command =
                    darwin::command(&self.reads, &writes.collect::<Vec<_>>(), &self.allow);

                command.current_dir(&source);
                (directory.clone(), command)
            }
            Profile::Linux => {
                let staging = linux::staging(&directory);
                fs::create_dir(&staging).map_err(io_at(&staging))?;
                let command = linux::command(&self.reads, &directory);
                (PathBuf::from(linux::WORKSPACE), command)
            }
        };

        let path = self.bins.iter().map(String::as_str).chain(HOST_PATH);
        self.variables
            .insert("PATH".into(), path.collect::<Vec<_>>().join(":"));

        self.variables
            .insert("TMPDIR".into(), workspace.join("tmp").display().to_string());

        command
            .arg("/bin/sh")
            .arg(workspace.join("script"))
            .env_clear()
            .envs(&self.variables);

        let status = logged(command, &mut *request.log).map_err(io_at(&directory))?;
        if !status.success() {
            let reason = format!("script {status}; kept {}", directory.display());
            return Err(failure(request.key, &reason));
        }

        if let (Profile::Linux, Some(out)) = (self.profile, &self.out) {
            let out = Path::new(out);
            let staged = out
                .strip_prefix(STORE_ROOT)
                .map(|name| linux::staging(&directory).join(name))
                .map_err(|_| failure(request.key, "has an output outside the store"))?;

            if staged.symlink_metadata().is_ok() {
                fs::rename(&staged, out).map_err(io_at(out))?;
            }
        }

        remove(&directory)
    }
}

/// Runs `command` with its output in `log`. It runs in its own process group,
/// killed once it exits to prevent lingering processes/hanging in the pipe.
fn logged(mut command: Command, log: &mut dyn Write) -> io::Result<ExitStatus> {
    let (mut reader, writer) = io::pipe()?;
    command
        .stdin(Stdio::null())
        .stderr(writer.try_clone()?)
        .stdout(writer)
        .process_group(0);

    let mut child = command.spawn()?;
    drop(command);

    let group = Pid::from_child(&child);
    thread::scope(|scope| {
        let waiter = scope.spawn(|| wait(&mut child, group));
        let copied = io::copy(&mut reader, log);
        if copied.is_err() {
            kill(group);
        }

        let status = waiter
            .join()
            .map_err(|_| io::Error::other("waiting for the build panicked"))?;

        copied?;
        status
    })
}

fn wait(child: &mut Child, group: Pid) -> io::Result<ExitStatus> {
    let options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
    rustix::io::retry_on_intr(|| rustix::process::waitid(WaitId::Pid(group), options))?;

    kill(group);
    child.wait()
}

fn kill(group: Pid) {
    // This can't fail unless nothing is left to kill, we don't care.
    let _ = rustix::process::kill_process_group(group, Signal::KILL);
}

/// Where a build runs on the host.
fn directory(key: &Key) -> PathBuf {
    format!("/opt/rb/var/build/{key}").into()
}

/// Outputs are immutable once scanned.
fn read_only(out: &Path) -> Result<(), Error> {
    chmod(out, |mode| mode & !0o222)
}

fn remove(path: &Path) -> Result<(), Error> {
    let result = match path.symlink_metadata() {
        Ok(metadata) if metadata.is_dir() => {
            // A read-only output's directories must be writable to empty them.
            chmod(path, |mode| mode | 0o700)?;
            fs::remove_dir_all(path)
        }
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    };

    result.map_err(io_at(path))
}

fn chmod(root: &Path, change: impl Fn(u32) -> u32) -> Result<(), Error> {
    for entry in WalkDir::new(root).follow_root_links(false) {
        let entry = entry.map_err(|error| io_at(root)(error.into()))?;
        if entry.path_is_symlink() {
            continue;
        }

        let path = entry.path();
        let mut permissions = entry
            .metadata()
            .map_err(|error| io_at(path)(error.into()))?
            .permissions();
        permissions.set_mode(change(permissions.mode()));
        fs::set_permissions(path, permissions).map_err(io_at(path))?;
    }

    Ok(())
}

fn failure(key: &Key, reason: &str) -> Error {
    Error::Build {
        key: key.clone(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn logged_returns_when_a_background_process_holds_the_pipe() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 60 & echo ok"]);

        let started = Instant::now();
        let mut log = Vec::new();
        let status = logged(command, &mut log).unwrap();

        assert!(status.success());
        assert_eq!(log, b"ok\n");
        assert!(started.elapsed() < Duration::from_secs(30));
    }

    #[test]
    fn read_only_outputs_stay_removable_and_symlinks_are_not_followed() {
        let root = tempfile::tempdir().unwrap();
        let host = root.path().join("host");
        let out = root.path().join("out");
        fs::create_dir_all(out.join("lib")).unwrap();
        fs::write(out.join("lib/libx.a"), "").unwrap();
        fs::create_dir(&host).unwrap();
        fs::write(host.join("file"), "").unwrap();
        std::os::unix::fs::symlink(&host, out.join("lib/host")).unwrap();

        read_only(&out).unwrap();
        assert!(fs::write(out.join("lib/new"), "").is_err());
        remove(&out).unwrap();
        assert!(!out.exists());

        std::os::unix::fs::symlink(&host, &out).unwrap();
        read_only(&out).unwrap();
        assert!(fs::write(host.join("file"), "").is_ok());
    }
}
