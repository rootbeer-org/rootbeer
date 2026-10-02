use crate::{Error, Request, darwin};
use rootbeer_drv::{Allow, Build, Check};
use rootbeer_drv::{
    Dependency, DependencyKind, Derivation, Key, Platform, fetch_path, output_path,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;
use std::process::Stdio;

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
    let out = output_path(request.key, &build.name, &build.version, "out");
    remove(&out)?;

    let mut sandbox = Sandbox::new(request, &build.sandbox, build.platform, &build.env)?;
    sandbox.allow.clone_from(&build.allow);
    sandbox.dependencies(request, &build.dependencies)?;
    for (name, key) in &build.inputs {
        let path = realized(request, key)?;
        sandbox.reads.push(path.clone());
        sandbox.variables.insert(name.clone(), path);
    }

    let out_path = out.display().to_string();
    sandbox.variables.insert("out".into(), out_path.clone());
    sandbox.writes.push(out_path);

    if let Err(error) = sandbox.run(request, &build.script) {
        remove(&out)?;
        return Err(error);
    }

    if !out.exists() {
        return Err(failure(request.key, "script did not create $out"));
    }

    Ok(())
}

pub(crate) fn check(request: &mut Request, check: &Check) -> Result<(), Error> {
    let target = realized(request, &check.target)?;
    let mut sandbox = Sandbox::new(request, &check.sandbox, check.platform, &check.env)?;

    sandbox.dependencies(request, &check.dependencies)?;
    sandbox.bins.insert(0, format!("{target}/bin"));
    sandbox.reads.push(target.clone());
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
        let host = Platform::try_from(format!(
            "{}-{}",
            std::env::consts::ARCH,
            std::env::consts::OS
        ));

        if profile != darwin::PROFILE || host.ok() != Some(platform) {
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
            if dependency.kind == DependencyKind::Runtime {
                continue;
            }

            let path = realized(request, &dependency.key)?;
            self.reads.push(path.clone());
            match dependency.kind {
                DependencyKind::Build => self.bins.push(format!("{path}/bin")),
                DependencyKind::Linked | DependencyKind::Runtime => linked.push(path),
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
        let (mut reader, writer) = io::pipe().map_err(io(&directory))?;
        let stderr = writer.try_clone().map_err(io(&directory))?;
        let mut child = darwin::command(&self.reads, &self.writes, &self.allow)
            .arg("/bin/sh")
            .arg(&file)
            .current_dir(&source)
            .env_clear()
            .envs(&self.variables)
            .stdin(Stdio::null())
            .stdout(writer)
            .stderr(stderr)
            .spawn()
            .map_err(io(&directory))?;

        if let Err(error) = io::copy(&mut reader, &mut *request.log) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io(&directory)(error));
        }

        let status = child.wait().map_err(io(&directory))?;
        if !status.success() {
            let reason = format!("script {status}; kept {}", directory.display());
            return Err(failure(request.key, &reason));
        }

        remove(&directory)
    }
}

/// Store path of a derivation the request depends on, which must be realized.
fn realized(request: &Request, key: &Key) -> Result<String, Error> {
    let path = match request.graph.get(key) {
        Some(Derivation::Fetch(_)) => fetch_path(key),
        Some(Derivation::Build(build)) => output_path(key, &build.name, &build.version, "out"),
        _ => {
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

    Ok(path.display().to_string())
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
