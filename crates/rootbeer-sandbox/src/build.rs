use crate::{Error, Request, darwin, derivation};
use rootbeer_drv::{Allow, Build, Check};
use rootbeer_drv::{
    Dependency, DependencyKind, Derivation, Key, Platform, fetch_path, output_path,
};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;

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

struct Sandbox {
    allow: BTreeSet<Allow>,
    variables: BTreeMap<String, String>,
    bins: Vec<String>,
    reads: Vec<String>,
    writes: Vec<String>,
}

pub(crate) fn build(request: &mut Request, build: &Build) -> Result<(), Error> {
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
    sandbox.writes.push(out_path);

    let result = sandbox.run(request, &build.script).and_then(|()| {
        if !out.exists() {
            return Err(failure(request.key, "script did not create $out"));
        }

        Ok(())
    });

    if result.is_err() {
        // Best effort: the next attempt removes it before building anyway.
        let _ = remove(&out);
    }

    result
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
        let is_runnable = profile == darwin::PROFILE
            && platform == Platform::Aarch64Macos
            && cfg!(all(target_os = "macos", target_arch = "aarch64"));

        if !is_runnable {
            return Err(failure(
                request.key,
                &format!("needs {profile} on {platform}, which this machine can't run"),
            ));
        }

        let mut variables = env.clone();
        variables.extend(FIXED.map(|(name, value)| (name.to_string(), value.to_string())));
        variables.insert("jobs".into(), request.jobs.to_string());

        Ok(Sandbox {
            allow: BTreeSet::new(),
            variables,
            bins: Vec::new(),
            reads: Vec::new(),
            writes: Vec::new(),
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

    /// Store path of a derivation the sandbox may read, which must be realized.
    fn read(&mut self, request: &Request, key: &Key) -> Result<String, Error> {
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
        self.reads.push(path.clone());
        Ok(path)
    }

    // Runs in a fresh directory that is kept when the script fails, for debugging.
    fn run(mut self, request: &mut Request, script: &str) -> Result<(), Error> {
        let directory = darwin::directory(request.key.as_str());
        let source = directory.join("src");
        let tmp = directory.join("tmp");
        let file = directory.join("script");

        remove(&directory)?;
        fs::create_dir_all(&source).map_err(io(&source))?;
        fs::create_dir(&tmp).map_err(io(&tmp))?;
        fs::write(&file, script).map_err(io(&file))?;

        let path = self.bins.iter().map(String::as_str).chain(HOST_PATH);
        self.variables
            .insert("PATH".into(), path.collect::<Vec<_>>().join(":"));

        self.variables
            .insert("TMPDIR".into(), tmp.display().to_string());

        self.writes.push(directory.display().to_string());
        let mut command = darwin::command(&self.reads, &self.writes, &self.allow);
        command
            .arg("/bin/sh")
            .arg(&file)
            .current_dir(&source)
            .env_clear()
            .envs(&self.variables);

        let status = logged(command, &mut *request.log).map_err(io(&directory))?;
        if !status.success() {
            let reason = format!("script {status}; kept {}", directory.display());
            return Err(failure(request.key, &reason));
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

fn remove(path: &Path) -> Result<(), Error> {
    let result = match path.symlink_metadata() {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    };

    result.map_err(io(path))
}

fn failure(key: &Key, reason: &str) -> Error {
    Error::Build {
        key: key.clone(),
        reason: reason.to_string(),
    }
}

fn io(path: &Path) -> impl Fn(io::Error) -> Error {
    let path = path.to_path_buf();
    move |source| Error::Io {
        path: path.clone(),
        source,
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
}
