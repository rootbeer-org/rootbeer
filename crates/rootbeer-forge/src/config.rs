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

/// Identity of the machine a build runs on: GitHub-hosted runners name their image, elsewhere
/// the host's platform stands in.
pub fn detect_context() -> String {
    let image = ["ImageOS", "ImageVersion", "RUNNER_ARCH"].map(|name| std::env::var(name).ok());
    match image {
        [Some(os), Some(version), Some(arch)] => format!("{os}-{version}-{arch}"),
        _ => format!("local-{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
