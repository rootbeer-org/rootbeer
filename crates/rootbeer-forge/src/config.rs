use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rootbeer_packaging::repository::Repository;
use rootbeer_packaging::work::Distribution;
use serde::Deserialize;

const FILE: &str = "forge.toml";

/// `forge.toml` in the working directory: a PDR checkout's recipes, where its packages are
/// published, and how its CI runs.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    pub catalog: Option<PathBuf>,
    pub pdr: Option<Pdr>,
    /// Builds of `name` are pushed to `ghcr.io/<registry>/<name>`.
    pub registry: Option<String>,
    #[serde(default)]
    pub ci: Ci,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Pdr {
    pub url: String,
    pub public_key: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Ci {
    /// The GitHub runner each system builds on.
    #[serde(default)]
    pub runners: BTreeMap<String, String>,
    /// The workflow whose runs build packages, as `.github/workflows/<file>`.
    pub workflow: Option<String>,
    /// File pinning the engine CI installs.
    pub engine_pin: Option<PathBuf>,
    /// Paths a promoted run must have verified with exactly the approved content.
    #[serde(default)]
    pub trusted: Vec<String>,
}

impl Config {
    pub fn load() -> Result<Self, String> {
        let path = Path::new(FILE);
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path).map_err(|error| format!("{FILE}: {error}"))?;
        toml::from_str(&text).map_err(|error| format!("{FILE}: {error}"))
    }

    pub fn distribution(&self) -> Result<Distribution, String> {
        let pdr = self
            .pdr
            .as_ref()
            .ok_or(format!("{FILE} does not name a PDR"))?;
        let registry = self
            .registry
            .clone()
            .ok_or(format!("{FILE} does not name a registry"))?;
        let pdr = Repository {
            url: pdr.url.clone(),
            public_key: pdr.public_key.clone(),
        };
        pdr.validate()?;
        rootbeer_packaging::ghcr::validate_repository(&registry)?;
        Ok(Distribution { pdr, registry })
    }

    pub fn runner(&self, system: &str) -> Result<&str, String> {
        self.ci
            .runners
            .get(system)
            .map(String::as_str)
            .ok_or_else(|| format!("{FILE} names no CI runner for {system}"))
    }

    pub fn workflow(&self) -> Result<&str, String> {
        self.ci
            .workflow
            .as_deref()
            .ok_or(format!("{FILE} does not name the package workflow"))
    }
}

/// Identity of the machine a build runs on: its OS and architecture, and the compiler, linker,
/// and system headers builds use. The pinned host tools can't see those behind compiler shims,
/// and a runner image's own version changes weekly without changing any of them.
pub fn detect_context() -> String {
    context(
        std::env::var("ImageOS").ok().as_deref(),
        std::env::var("RUNNER_ARCH").ok().as_deref(),
        toolchain().as_deref(),
    )
}

fn context(image: Option<&str>, arch: Option<&str>, toolchain: Option<&str>) -> String {
    let platform = match (image, arch) {
        (Some(image), Some(arch)) => format!("{image}-{arch}"),
        _ => format!("local-{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    };
    match toolchain {
        Some(toolchain) => format!("{platform}-{toolchain}"),
        None => platform,
    }
}

fn toolchain() -> Option<String> {
    let facts = if cfg!(target_os = "macos") {
        xcode()?
    } else {
        system_packages()?
    };
    Some(rootbeer_packaging::store::hash_bytes(facts.as_bytes())[..16].to_string())
}

/// The real clang and linker behind the xcrun shims, and the SDK they build against.
fn xcode() -> Option<String> {
    let mut facts = String::new();
    for tool in ["clang", "ld"] {
        let path = command("xcrun", &["--find", tool])?;
        let digest = rootbeer_packaging::store::hash_file(Path::new(&path)).ok()?;
        facts.push_str(&format!("{tool} {digest}\n"));
    }
    for query in ["--show-sdk-version", "--show-sdk-build-version"] {
        facts.push_str(&format!("{query} {}\n", command("xcrun", &[query])?));
    }
    Some(facts)
}

/// Installed versions of the compiler, binutils, and C library headers packages.
fn system_packages() -> Option<String> {
    let output = std::process::Command::new("dpkg-query")
        .args([
            "-W",
            "-f=${db:Status-Abbrev} ${Package} ${Version} ${Architecture}\n",
            "gcc*",
            "g++*",
            "cpp*",
            "binutils*",
            "libc6*",
            "linux-libc-dev*",
            "libgcc*",
            "libstdc++*",
        ])
        .output()
        .ok()?;
    let mut installed: Vec<_> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("ii "))
        .map(str::to_string)
        .collect();
    installed.sort();
    (!installed.is_empty()).then(|| installed.join("\n"))
}

fn command(program: &str, arguments: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(arguments)
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (output.status.success() && !text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts_name_the_platform_and_toolchain_but_not_the_image_version() {
        assert_eq!(
            context(Some("ubuntu24"), Some("X64"), Some("0123")),
            "ubuntu24-X64-0123"
        );
        assert_eq!(context(Some("ubuntu24"), Some("X64"), None), "ubuntu24-X64");
        assert!(context(None, Some("X64"), Some("0123")).starts_with("local-"));
    }

    #[test]
    fn the_toolchain_digest_is_stable() {
        let digest = toolchain();
        if cfg!(target_os = "macos") {
            assert!(digest.is_some(), "xcrun resolves the real toolchain");
        }
        assert_eq!(digest, toolchain());
    }

    #[test]
    fn reads_the_pdr_checkout_layout() {
        let config: Config = toml::from_str(
            r#"
            catalog = "packages"
            registry = "rootbeer-org/pdr"

            [pdr]
            url = "https://pdr.rbpkg.com/v3/current.json"
            public-key = "028c5b185fb63ea61128a0bf6fb0decc8b700020561db08d82a998c7d0493bc0"

            [ci]
            workflow = "package-builds.yml"
            engine-pin = "package-engine-revision"
            trusted = ["forge.toml", "package-engine-revision"]

            [ci.runners]
            x86_64-linux = "ubuntu-24.04"
            "#,
        )
        .unwrap();
        assert_eq!(config.catalog.as_deref(), Some(Path::new("packages")));
        assert_eq!(config.distribution().unwrap().registry, "rootbeer-org/pdr");
        assert_eq!(config.runner("x86_64-linux").unwrap(), "ubuntu-24.04");
        assert!(config.runner("aarch64-macos").is_err());
        assert!(toml::from_str::<Config>("unknown = 1").is_err());
    }
}
