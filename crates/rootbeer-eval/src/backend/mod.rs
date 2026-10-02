mod autotools;
mod custom;
mod go;
mod prebuilt;
mod rust;
mod zig;

use crate::Host;
use crate::recipe::{ArchiveFormat, Backend, Bins, Resolved, Source};
use crate::template::{path, quote};
use rootbeer_drv::{Build, Check, Dependency, DependencyKind, Fetch, Key, Platform};
use std::collections::{BTreeMap, BTreeSet};

const PATCH_END: &str = "RB_PATCH";

/// A package is a recipe's resolved values and the target it builds for.
pub(crate) struct Package<'a> {
    pub name: &'a str,
    pub host: &'a Host,
    pub resolved: Resolved<'a>,
}

/// What a backend emits to configure and build a package correctly.
struct Script {
    env: BTreeMap<String, String>,
    lines: Vec<String>,
}

impl Package<'_> {
    pub(crate) fn source(&self, source: &Source) -> Result<Fetch, String> {
        Ok(self.fetch(self.resolved.values.literal(&source.url)?))
    }

    /// Go and Rust fetch their locked dependencies with network, apart from the
    /// build, so editing a build never refetches them.
    // TODO: Emit a tree-mode Fetch instead once recipes record vendor hashes
    pub(crate) fn vendor(
        &self,
        source: &Source,
        backend: Backend,
        source_key: &Key,
        dependencies: &[Dependency],
    ) -> Result<Option<Build>, String> {
        let script = match backend {
            Backend::Go => go::vendor(),
            Backend::Rust => rust::vendor(self.host),
            Backend::Autotools | Backend::Custom | Backend::Zig => return Ok(None),
        };

        let mut lines = self.prepare(source)?;
        lines.extend(script.lines);

        let tools = dependencies
            .iter()
            .filter(|dependency| dependency.kind == DependencyKind::Build)
            .cloned()
            .collect();

        let build = self.build(
            format!("{}-vendor", self.name),
            BTreeMap::from([("source".into(), source_key.clone())]),
            tools,
            script.env,
            lines,
        );

        Ok(Some(Build {
            sandbox: format!("{}-net", build.sandbox),
            ..build
        }))
    }

    pub(crate) fn compile(
        &self,
        source: &Source,
        build: &crate::recipe::Build,
        inputs: BTreeMap<String, Key>,
        dependencies: Vec<Dependency>,
    ) -> Result<Build, String> {
        let values = &self.resolved.values;
        let bins = self.resolved.spec.outputs.bins.as_ref();

        let script = match build.backend {
            Backend::Autotools => autotools::build(values, build)?,
            Backend::Custom => custom::build(values, build)?,
            Backend::Go => go::build(values, build, bins)?,
            Backend::Rust => rust::build(values, build, bins, self.host)?,
            Backend::Zig => zig::build(values, build)?,
        };

        let mut variables = env(&[("RB_SYSTEM", &self.host.system)]);
        variables.extend(script.env);

        let mut lines = self.prepare(source)?;
        lines.extend(script.lines);
        lines.extend(link(bins));

        Ok(self.build(
            self.name.to_string(),
            inputs,
            dependencies,
            variables,
            lines,
        ))
    }

    /// Checks run the recipe's commands and confirm that outputs exist.
    pub(crate) fn check(&self, target: &Key) -> Result<Option<Check>, String> {
        let values = &self.resolved.values;
        let commands = self.resolved.spec.outputs.checks.as_deref();
        // TODO: Key libraries on the build once $deps stages them; they shape dependents' builds
        let libraries = self
            .resolved
            .spec
            .build
            .as_ref()
            .map(|build| build.libraries.as_slice());

        // TODO: `{prefix}` renders as `${out}`, but it isn't used in checks
        let mut lines = values.commands(commands.unwrap_or_default())?;
        for library in libraries.unwrap_or_default() {
            lines.push(format!(
                "test -e {}",
                path("target", &values.literal(library)?)
            ));
        }

        if lines.is_empty() {
            return Ok(None);
        }

        Ok(Some(Check {
            name: self.name.to_string(),
            target: target.clone(),
            platform: values.platform,
            sandbox: sandbox(values.platform).into(),
            dependencies: Vec::new(),
            env: env(&[("RB_SYSTEM", &self.host.system)]),
            script: script(lines),
        }))
    }

    fn fetch(&self, url: String) -> Fetch {
        Fetch {
            sha256: self.resolved.sha256.clone(),
            urls: vec![url],
        }
    }

    // Unpacks the source and applies patches
    fn prepare(&self, source: &Source) -> Result<Vec<String>, String> {
        let mut lines = vec![extract(source.archive, ".")];
        if let Some(prefix) = &source.strip_prefix {
            lines.push(format!(
                "cd {}",
                quote(&self.resolved.values.literal(prefix)?)
            ));
        }

        for patch in &source.patches {
            if patch.lines().any(|line| line == PATCH_END) {
                return Err(format!("a patch contains the line {PATCH_END}"));
            }

            let newline = if patch.ends_with('\n') { "" } else { "\n" };
            lines.push(format!(
                "patch --batch -p1 <<'{PATCH_END}'\n{patch}{newline}{PATCH_END}"
            ));
        }

        Ok(lines)
    }

    fn build(
        &self,
        name: String,
        inputs: BTreeMap<String, Key>,
        dependencies: Vec<Dependency>,
        env: BTreeMap<String, String>,
        lines: Vec<String>,
    ) -> Build {
        let platform = self.resolved.values.platform;
        Build {
            name,
            version: self.resolved.values.version.to_string(),
            platform,
            sandbox: sandbox(platform).into(),
            inputs,
            dependencies,
            env,
            script: script(lines),
            outputs: BTreeSet::from(["out".to_string()]),
        }
    }
}

fn extract(format: ArchiveFormat, directory: &str) -> String {
    match format {
        ArchiveFormat::TarGz => format!(r#"tar -xzf "${{source}}" -C {directory}"#),
        ArchiveFormat::TarXz => format!(r#"tar -xJf "${{source}}" -C {directory}"#),
        ArchiveFormat::Zip => format!(r#"unzip -q "${{source}}" -d {directory}"#),
    }
}

/// Links each command declared at a path outside `bin/` at `$out/bin/<name>`.
fn link(bins: Option<&Bins>) -> Vec<String> {
    let Some(Bins::Paths(paths)) = bins else {
        return Vec::new();
    };

    let links = paths
        .iter()
        .filter(|(name, located)| **located != format!("bin/{name}"))
        .map(|(name, located)| {
            format!(
                "ln -s {} {}",
                quote(&format!("../{located}")),
                path("out", &format!("bin/{name}"))
            )
        })
        .collect::<Vec<_>>();
    if links.is_empty() {
        return links;
    }

    [vec![r#"mkdir -p "${out}/bin""#.into()], links].concat()
}

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn lines(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|line| line.to_string()).collect()
}

fn sandbox(platform: Platform) -> &'static str {
    match platform {
        Platform::Aarch64Macos => "darwin-v1",
        Platform::Aarch64Linux | Platform::X86_64Linux => "linux-v1",
    }
}

fn script(lines: Vec<String>) -> String {
    ["set -eu".to_string()]
        .into_iter()
        .chain(lines)
        .map(|line| line + "\n")
        .collect()
}
