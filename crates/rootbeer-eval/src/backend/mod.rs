mod autotools;
mod custom;
mod go;
pub(crate) mod prebuilt;
mod rust;
mod zig;

use crate::Host;
use crate::recipe::{ArchiveFormat, Backend, Bins, Resolved, Source, Steps};
use crate::template::{Values, path, quote};
use rootbeer_drv::{Build, Check, Dependency, DependencyKind, Fetch, Key, Platform};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const PATCH_END: &str = "RB_PATCH";
const UNPACKER: &str = "rb-unpack";
const SEVEN_ZIP: &str = "7zip";
const SOURCE: &str = r#""${source}""#;
const TAR: &str = r#""${TMPDIR}/source.tar""#;

/// A package is a recipe's resolved values and the target it builds for.
pub(crate) struct Package<'a> {
    pub name: &'a str,
    pub host: &'a Host,
    pub resolved: Resolved<'a>,
    pub rpaths: Vec<String>,
}

/// What a backend emits to configure and build a package correctly.
#[derive(Default)]
struct Script {
    env: BTreeMap<String, String>,
    phases: Phases,
}

/// A backend's own commands for each phase, in the order the phases run.
#[derive(Default)]
struct Phases {
    prepare: Vec<String>,
    configure: Vec<String>,
    build: Vec<String>,
    check: Vec<String>,
    install: Vec<String>,
}

impl Package<'_> {
    pub(crate) fn source(&self, source: &Source) -> Result<Fetch, String> {
        let urls = std::iter::once(&source.url)
            .chain(&source.mirrors)
            .map(|url| self.resolved.values.literal(url))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(self.fetch(urls))
    }

    /// Go, Rust, and Zig fetch their locked dependencies with network, apart
    /// from the build, so editing a build never refetches them.
    // TODO: Emit a tree-mode Fetch instead once recipes record vendor hashes
    pub(crate) fn vendor(
        &self,
        source: &Source,
        backend: Backend,
        source_key: &Key,
        dependencies: &[Dependency],
    ) -> Result<Option<Build>, String> {
        let (toolchain, mut script) = match backend {
            Backend::Go => ("go", go::vendor()),
            Backend::Rust => ("rust", rust::vendor()),
            Backend::Zig => ("zig", zig::vendor()),
            Backend::Autotools | Backend::Custom => return Ok(None),
        };

        script.phases.prepare = self.prepare(source, dependencies)?;
        let lines = script
            .phases
            .run(&self.resolved.values, &Steps::default())?;

        let is_tool = |dependency: &&Dependency| {
            dependency.name == toolchain
                || ([UNPACKER, SEVEN_ZIP].contains(&dependency.name.as_str())
                    && matches!(source.archive, ArchiveFormat::Zip | ArchiveFormat::TarBz2))
        };

        let tools = dependencies
            .iter()
            .filter(|dependency| dependency.kind == DependencyKind::Build)
            .filter(is_tool)
            .cloned()
            .collect::<Vec<_>>();

        if !tools.iter().any(|tool| tool.name == toolchain) {
            return Err(format!(
                "a {toolchain} build needs {toolchain} as a build dependency"
            ));
        }

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
        let mut script = match build.backend {
            Backend::Autotools => autotools::build(values, build)?,
            Backend::Custom => custom::build(build)?,
            Backend::Go => go::build(values, build, bins)?,
            Backend::Rust => rust::build(values, build, bins)?,
            Backend::Zig => zig::build(values, build)?,
        };

        let mut variables = env(&[("RB_SYSTEM", &self.host.system)]);
        variables.extend(script.env);

        let mut prepare = self.prepare(source, &dependencies)?;
        if matches!(build.backend, Backend::Autotools | Backend::Custom) {
            prepare.push(link_flags(&self.rpaths));
        }

        prepare.append(&mut script.phases.prepare);
        script.phases.prepare = prepare;

        let steps = build.steps.clone().unwrap_or_default();
        let mut lines = script.phases.run(values, &steps)?;
        lines.extend(link(bins));

        let compiled = self.build(
            self.name.to_string(),
            inputs,
            dependencies,
            variables,
            lines,
        );

        Ok(Build {
            allow: build.allow.clone(),
            host: self.host_references()?,
            ..compiled
        })
    }

    /// A reference may name the version, as a library named after it does.
    fn host_references(&self) -> Result<BTreeSet<String>, String> {
        let references = self.resolved.spec.outputs.host.iter().flatten();
        references
            .map(|reference| self.resolved.values.literal(reference))
            .collect()
    }

    pub(crate) fn is_shared(&self) -> Result<bool, String> {
        let libraries = self
            .resolved
            .spec
            .build
            .iter()
            .flat_map(|build| &build.libraries)
            .map(|library| self.resolved.values.literal(library))
            .collect::<Result<Vec<_>, String>>()?;

        Ok(libraries.iter().any(|library| {
            let extension = Path::new(library).extension();
            extension.is_some_and(|extension| extension == "so" || extension == "dylib")
        }))
    }

    /// Checks run the recipe's commands and confirm that outputs exist.
    pub(crate) fn check(
        &self,
        target: &Key,
        dependencies: Vec<Dependency>,
    ) -> Result<Option<Check>, String> {
        let values = &self.resolved.values;
        let commands = self.resolved.spec.outputs.checks.as_deref();
        let libraries = self
            .resolved
            .spec
            .build
            .as_ref()
            .map(|build| build.libraries.as_slice());

        let commands = commands.unwrap_or_default();
        let is_build_path =
            |word: &String| word.contains("{prefix}") || word.contains("{dependencies");
        if commands.iter().flatten().any(is_build_path) {
            return Err(
                "checks have no `{prefix}` or `{dependencies.<name>}`; they run against `${target}`"
                    .into(),
            );
        }

        let mut lines = values.commands(commands)?;
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
            dependencies,
            env: env(&[("RB_SYSTEM", &self.host.system)]),
            script: script(lines),
        }))
    }

    fn fetch(&self, urls: Vec<String>) -> Fetch {
        Fetch {
            sha256: self.resolved.sha256.clone(),
            urls,
        }
    }

    // Unpacks the source and applies patches
    fn prepare(&self, source: &Source, dependencies: &[Dependency]) -> Result<Vec<String>, String> {
        let unpacker = match source.archive {
            ArchiveFormat::Zip => unpacker(dependencies, "zip source", "build.dependencies")?,
            ArchiveFormat::TarBz2 => {
                unpacker(dependencies, "tar.bz2 source", "build.dependencies")?
            }
            ArchiveFormat::TarGz | ArchiveFormat::TarXz => Unpacker::Tar,
        };

        let mut lines = vec![extract(source.archive, ".", unpacker)];
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
            allow: BTreeSet::new(),
            host: BTreeSet::new(),
            inputs,
            dependencies,
            env,
            script: script(lines),
            outputs: BTreeSet::from(["out".to_string()]),
        }
    }
}

impl Phases {
    /// Runs each phase's recipe steps after the backend's own commands.
    fn run(self, values: &Values, steps: &Steps) -> Result<Vec<String>, String> {
        let phases = [
            (self.prepare, &steps.prepare),
            (self.configure, &steps.configure),
            (self.build, &steps.build),
            (self.check, &steps.check),
            (self.install, &steps.install),
        ];

        let mut lines = Vec::new();
        for (commands, steps) in phases {
            lines.extend(commands);
            lines.extend(values.commands(steps)?);
        }

        Ok(lines)
    }
}

/// What extracts an archive. tar opens gzip and xz itself.
#[derive(Clone, Copy, PartialEq)]
enum Unpacker {
    Tar,
    RbUnpack,
    // Recipes that still declare 7zip extract with it until they move.
    SevenZip,
}

fn extract(format: ArchiveFormat, directory: &str, unpacker: Unpacker) -> String {
    match (format, unpacker) {
        (ArchiveFormat::TarGz, _) => format!(r#"tar -xzf "${{source}}" -C {directory}"#),
        (ArchiveFormat::TarXz, _) => format!(r#"tar -xJf "${{source}}" -C {directory}"#),
        (ArchiveFormat::Zip, Unpacker::SevenZip) => seven_zip(SOURCE, directory),
        // 7-Zip opens one layer per run, so the tar goes through a file in between.
        (ArchiveFormat::TarBz2, Unpacker::SevenZip) => format!(
            "7zz x -so -bsp0 {SOURCE} > {TAR}\n{}\nrm {TAR}",
            seven_zip(TAR, directory)
        ),
        (ArchiveFormat::Zip, _) => rb_unpack("zip", directory),
        (ArchiveFormat::TarBz2, _) => rb_unpack("tar.bz2", directory),
    }
}

fn rb_unpack(format: &str, directory: &str) -> String {
    format!("rb-unpack {format} {SOURCE} {directory}")
}

// -spod takes the output path literally, -sns- skips alternate streams, and
// -snld20 extracts every symlink as archived.
fn seven_zip(archive: &str, directory: &str) -> String {
    format!("7zz x -y -bso0 -bsp0 -spod -sns- -snld20 -o{directory} {archive}")
}

/// Whether `name` is a package a build extracts archives with. rb extracts a
/// version that records its tree itself, so the package never reaches that
/// version's key.
pub(crate) fn is_unpacker(name: &str) -> bool {
    [UNPACKER, SEVEN_ZIP].contains(&name)
}

/// Zips, tar.bz2s, disk images, and packages extract with a catalog
/// rb-unpack on `PATH`.
fn unpacker(dependencies: &[Dependency], archive: &str, field: &str) -> Result<Unpacker, String> {
    let declares = |name: &str| {
        dependencies
            .iter()
            .any(|dependency| dependency.kind == DependencyKind::Build && dependency.name == name)
    };

    if declares(UNPACKER) {
        return Ok(Unpacker::RbUnpack);
    }

    if declares(SEVEN_ZIP) {
        return Ok(Unpacker::SevenZip);
    }

    Err(format!(
        "a {archive} extracts with {UNPACKER}, so declare it in {field} with kind = \"build\""
    ))
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

// Store paths are fixed, so outputs find shared libraries through rpaths rather
// than relocation. -S removes debug information which can persist as false
// references since they show paths to deleted build directories.
fn link_flags(rpaths: &[String]) -> String {
    let flags = ["-Wl,-S".to_string()]
        .into_iter()
        .chain(
            rpaths
                .iter()
                .map(|directory| format!("-Wl,-rpath,{directory}")),
        )
        .collect::<Vec<_>>();

    format!(r#"export LDFLAGS="{}""#, flags.join(" "))
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
