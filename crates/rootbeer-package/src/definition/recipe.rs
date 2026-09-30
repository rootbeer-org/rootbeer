//! Platform-first recipe authoring.
//!
//! A recipe is an identity plus one build definition per platform. Map keys are the
//! systems, so nothing declares `systems`; a version is a digest per platform, so
//! nothing declares which platforms a version covers.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::PackageUpstream;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Recipe {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    pub description: String,
    pub homepage: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recipe_maintainers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_engine_level: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_license: Option<String>,
    /// Shared by every platform that does not declare its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<PackageUpstream>,

    #[serde(flatten)]
    pub shared: Spec,

    pub platforms: BTreeMap<String, Platform>,
    pub versions: BTreeMap<String, Version>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Spec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prebuilt: Option<Prebuilt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<Build>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Outputs>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Platform {
    /// Substituted for `{target}` in this platform's templates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub default_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<PackageUpstream>,
    #[serde(flatten)]
    pub overrides: Spec,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Provider {
    Github(String),
    Url(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Prebuilt {
    #[serde(flatten)]
    pub provider: Provider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<crate::LockedInstall>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mirror: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Source {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<crate::GitSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patches: Vec<String>,
}

/// How a source platform compiles. The archive it compiles is `source`, and its digest is
/// the version's, so neither can be restated here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Build {
    pub backend: crate::BuildBackend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rust: Option<crate::RustBuild>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub go: Option<crate::GoBuild>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub configure: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<crate::BuildDependency>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub libraries: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<crate::BuildSteps>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Outputs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bins: Option<crate::Bins>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<BTreeMap<String, PathBuf>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<Vec<Vec<String>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Version {
    pub digests: BTreeMap<String, String>,
    /// The commit this version's tag points at, for templates that embed `{commit}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub revision: u32,
    #[serde(flatten)]
    pub overrides: Spec,
}

impl Default for Version {
    fn default() -> Self {
        Self {
            digests: BTreeMap::new(),
            commit: None,
            license: None,
            revision: one(),
            overrides: Spec::default(),
        }
    }
}

fn one() -> u32 {
    1
}

fn is_one(value: &u32) -> bool {
    *value == 1
}

/// What a recipe's templates may name for one version on one platform.
#[derive(Clone, Copy)]
struct Placeholders<'a> {
    version: &'a str,
    tag: &'a str,
    target: Option<&'a str>,
    commit: Option<&'a str>,
    /// `dylib` or `so`, naming shared libraries the way this platform's loader does.
    shared_extension: &'a str,
}

impl Placeholders<'_> {
    /// Fills every placeholder this version knows and leaves any other brace as written.
    fn fill(&self, text: &str) -> Result<String, String> {
        let mut value = text
            .replace("{version}", self.version)
            .replace("{tag}", self.tag)
            .replace("{shared_extension}", self.shared_extension);
        if let Some(target) = self.target {
            value = value.replace("{target}", target);
        }
        if value.contains("{commit}") {
            let commit = self
                .commit
                .ok_or("`{commit}` needs the version to record its `commit`")?;
            value = value.replace("{commit}", commit);
        }
        let release = self.version.split(['-', '+']).next().unwrap_or_default();
        let parts: Vec<&str> = release.split('.').collect();
        for (index, name) in ["{major}", "{minor}", "{patch}"].into_iter().enumerate() {
            if !value.contains(name) {
                continue;
            }
            let part = parts.get(index).ok_or_else(|| {
                format!(
                    "`{name}` needs a version with {} parts, not `{}`",
                    index + 1,
                    self.version
                )
            })?;
            value = value.replace(name, part);
        }
        Ok(value)
    }

    /// Fills a URL, asset, or path, where no brace may remain.
    fn substitute(&self, pattern: &str) -> Result<String, String> {
        let value = self.fill(pattern)?;
        if value.contains(['{', '}']) {
            return Err(format!("unsupported placeholder in `{pattern}`"));
        }
        Ok(value)
    }

    /// Fills a command and keeps every other brace: the build fills `{prefix}` and `{jobs}`
    /// itself, and checks carry shell or Python of their own.
    fn command(&self, command: &[String]) -> Result<Vec<String>, String> {
        command.iter().map(|argument| self.fill(argument)).collect()
    }

    fn commands(&self, commands: &[Vec<String>]) -> Result<Vec<Vec<String>>, String> {
        commands
            .iter()
            .map(|command| self.command(command))
            .collect()
    }
}

impl Spec {
    /// Platform and version overrides replace a shared field outright.
    fn overlay(&self, over: &Self) -> Self {
        Self {
            prebuilt: over.prebuilt.clone().or_else(|| self.prebuilt.clone()),
            source: over.source.clone().or_else(|| self.source.clone()),
            build: over.build.clone().or_else(|| self.build.clone()),
            outputs: match (&self.outputs, &over.outputs) {
                (Some(base), Some(over)) => Some(base.overlay(over)),
                (base, over) => over.clone().or_else(|| base.clone()),
            },
        }
    }
}

impl Outputs {
    fn overlay(&self, over: &Self) -> Self {
        Self {
            bins: over.bins.clone().or_else(|| self.bins.clone()),
            apps: over.apps.clone().or_else(|| self.apps.clone()),
            checks: over.checks.clone().or_else(|| self.checks.clone()),
        }
    }
}

impl Recipe {
    pub(super) fn insert_version(
        &mut self,
        version: &str,
        digests: BTreeMap<String, String>,
        license: Option<String>,
        commit: Option<String>,
    ) {
        let entry = self.versions.entry(version.to_string()).or_default();
        entry.digests.extend(digests);
        if commit.is_some() {
            entry.commit = commit;
        }
        if license.is_some() {
            entry.license = license;
        }
    }

    pub(super) fn set_download_url(&mut self, version: &str, url: &str) -> Result<(), String> {
        if url.contains(['{', '}']) {
            return Err(format!(
                "{version}: download URL `{url}` looks like a template"
            ));
        }
        let entry = self
            .versions
            .get(version)
            .ok_or_else(|| format!("{}@{version} is not recorded", self.name))?;
        // Prebuilts compare by their rendering, since install contracts are not `Eq`.
        let rendered = |prebuilt: &Option<Prebuilt>| serde_json::to_string(prebuilt).ok();
        let prebuilts: Vec<Option<Prebuilt>> = entry
            .digests
            .keys()
            .map(|system| {
                let platform = &self.platforms[system];
                self.shared.overlay(&platform.overrides).prebuilt
            })
            .collect();
        if prebuilts
            .iter()
            .any(|other| rendered(other) != rendered(&prebuilts[0]))
        {
            return Err(format!(
                "{}@{version}: platforms with different prebuilts cannot share one URL",
                self.name
            ));
        }
        let Some(Some(mut prebuilt)) = prebuilts.into_iter().next() else {
            return Err(format!(
                "{}@{version}: only a prebuilt can download a URL",
                self.name
            ));
        };
        prebuilt.provider = Provider::Url(url.to_string());
        prebuilt.tag = None;
        prebuilt.asset = None;
        let prebuilt = Some(prebuilt);

        let entry = self.versions.get_mut(version).expect("checked above");
        if entry.overrides.prebuilt.is_some()
            && rendered(&entry.overrides.prebuilt) != rendered(&prebuilt)
        {
            return Err(format!(
                "{}@{version} already downloads from another URL",
                self.name
            ));
        }
        entry.overrides.prebuilt = prebuilt;
        Ok(())
    }

    pub(super) fn set_default_version(
        &mut self,
        system: &str,
        version: &str,
    ) -> Result<(), String> {
        let platform = self
            .platforms
            .get_mut(system)
            .ok_or_else(|| format!("{}: {system} is not a declared platform", self.name))?;
        platform.default_version = version.to_string();
        Ok(())
    }

    /// The same recipe with an upstream every platform repeats declared once, at the top.
    pub(super) fn with_shared_upstream(&self) -> Self {
        let mut recipe = self.clone();
        if recipe.upstream.is_none() {
            let mut declared = recipe.platforms.values().map(|platform| &platform.upstream);
            if let Some(Some(first)) = declared.next() {
                if declared.all(|upstream| upstream.as_ref() == Some(first)) {
                    recipe.upstream = Some(first.clone());
                }
            }
        }
        if let Some(shared) = &recipe.upstream {
            for platform in recipe.platforms.values_mut() {
                if platform.upstream.as_ref() == Some(shared) {
                    platform.upstream = None;
                }
            }
        }
        recipe
    }

    pub(super) fn platform_names(&self) -> Vec<String> {
        self.platforms.keys().cloned().collect()
    }

    pub(super) fn pin_repository_id(&mut self, repository: &str, id: u64) {
        let declared = self.upstream.iter_mut().chain(
            self.platforms
                .values_mut()
                .filter_map(|platform| platform.upstream.as_mut()),
        );
        for upstream in declared {
            if upstream.repository_id.is_none()
                && upstream
                    .github()
                    .is_some_and(|declared| declared.eq_ignore_ascii_case(repository))
            {
                upstream.repository_id = Some(id);
            }
        }
    }

    pub(super) fn candidate(
        &self,
        system: &str,
        version: &str,
        commit: Option<&str>,
    ) -> Result<crate::CatalogRecipe, String> {
        let platform = self
            .platforms
            .get(system)
            .ok_or_else(|| format!("{}: {system} is not a declared platform", self.name))?;
        let spec = self.shared.overlay(&platform.overrides);
        self.resolve(system, version, commit, "", platform, &spec)
            .map_err(|error| format!("{}@{version} {system}: {error}", self.name))
    }

    /// Whether any template embeds `{commit}`, so a new version must record its tag's commit.
    pub(super) fn uses_commit(&self) -> bool {
        serde_json::to_string(self).is_ok_and(|json| json.contains("{commit}"))
    }

    fn platform_upstream<'a>(&'a self, platform: &'a Platform) -> Option<&'a PackageUpstream> {
        platform.upstream.as_ref().or(self.upstream.as_ref())
    }

    pub(super) fn expand(&self) -> Result<super::PackageDefinition, String> {
        if self.platforms.is_empty() {
            return Err("a recipe declares at least one platform".into());
        }
        for upstream in self.upstream().values() {
            upstream
                .validate()
                .map_err(|error| format!("{}: {error}", self.name))?;
        }
        let mut versions: BTreeMap<String, crate::CatalogVersion> = BTreeMap::new();
        for (version, entry) in &self.versions {
            if entry.commit.as_deref().is_some_and(|commit| {
                commit.len() != 40
                    || !commit
                        .bytes()
                        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            }) {
                return Err(format!(
                    "{}@{version}: commit must be a full lowercase SHA-1",
                    self.name
                ));
            }
            let mut platforms = BTreeMap::new();
            for (system, digest) in &entry.digests {
                let platform = self.platforms.get(system).ok_or_else(|| {
                    format!(
                        "{}@{version}: {system} has a digest but no platform",
                        self.name
                    )
                })?;
                let spec = self
                    .shared
                    .overlay(&platform.overrides)
                    .overlay(&entry.overrides);
                platforms.insert(
                    system.clone(),
                    self.resolve(
                        system,
                        version,
                        entry.commit.as_deref(),
                        digest,
                        platform,
                        &spec,
                    )
                    .map_err(|error| format!("{}@{version} {system}: {error}", self.name))?,
                );
            }
            versions.insert(
                version.clone(),
                crate::CatalogVersion {
                    license: entry
                        .license
                        .clone()
                        .or_else(|| self.default_license.clone())
                        .ok_or_else(|| format!("{}@{version}: no license", self.name))?,
                    revision: entry.revision,
                    platforms,
                    extra: Default::default(),
                },
            );
        }

        let default_versions = self
            .platforms
            .iter()
            .map(|(system, platform)| (system.clone(), platform.default_version.clone()))
            .collect();
        let package = crate::CatalogPackage {
            name: self.name.clone(),
            aliases: self.aliases.clone(),
            description: self.description.clone(),
            homepage: self.homepage.clone(),
            recipe_maintainers: self.recipe_maintainers.clone(),
            min_engine_level: self.min_engine_level,
            default_versions,
            versions,
            extra: Default::default(),
        };
        package.validate()?;
        Ok(super::PackageDefinition {
            package,
            upstream: self.upstream(),
            authoring: Some(self.clone()),
            comments: Default::default(),
        })
    }

    fn resolve(
        &self,
        system: &str,
        version: &str,
        commit: Option<&str>,
        digest: &str,
        platform: &Platform,
        spec: &Spec,
    ) -> Result<crate::CatalogRecipe, String> {
        if spec.prebuilt.is_some() && spec.source.is_some() {
            return Err("a platform is prebuilt or built from source, not both".into());
        }
        let tag = self
            .platform_upstream(platform)
            .map(|upstream| upstream.tag_for(version))
            .unwrap_or_else(|| version.to_string());
        let values = Placeholders {
            version,
            tag: &tag,
            target: platform.target.as_deref(),
            commit,
            shared_extension: if system.ends_with("-macos") {
                "dylib"
            } else {
                "so"
            },
        };
        let outputs = spec.outputs.clone().unwrap_or_default();

        let mut recipe = crate::CatalogRecipe {
            source: None,
            install: None,
            build: None,
            asset: None,
            sha256: None,
            bins: outputs.bins.clone().unwrap_or_default(),
            apps: outputs.apps.clone().unwrap_or_default(),
            checks: values.commands(outputs.checks.as_deref().unwrap_or_default())?,
            mirror: false,
            extra: Default::default(),
        };

        if let Some(prebuilt) = &spec.prebuilt {
            // The digest pins the artifact a prebuilt downloads; a source build pins its
            // archive on the build instead, and its output hashes to something else.
            recipe.sha256 = Some(digest.to_string());
            recipe.mirror = prebuilt.mirror;
            recipe.install = prebuilt.install.clone();
            match &prebuilt.provider {
                Provider::Github(repository) => {
                    crate::github::repository(repository)?;
                    let tag = match &prebuilt.tag {
                        Some(pattern) => values.substitute(pattern)?,
                        None => tag.clone(),
                    };
                    recipe.source = Some(format!("github:{repository}@{tag}"));
                    let asset = prebuilt
                        .asset
                        .as_deref()
                        .ok_or("a github prebuilt needs a release asset")?;
                    let values = Placeholders {
                        tag: &tag,
                        ..values
                    };
                    recipe.asset = Some(values.substitute(asset)?);
                }
                Provider::Url(url) => {
                    recipe.source = Some(values.substitute(url)?);
                }
            }
        } else if let Some(source) = &spec.source {
            let build = spec
                .build
                .as_ref()
                .ok_or("a source platform needs a build")?;
            let mut go = build.go.clone();
            if let Some(go) = go.as_mut() {
                for value in go.variables.values_mut() {
                    *value = values.substitute(value)?;
                }
            }
            let mut rust = build.rust.clone();
            if let Some(rust) = rust.as_mut() {
                for value in rust.environment.values_mut() {
                    *value = values.substitute(value)?;
                }
            }
            let archive = match source.archive.as_deref() {
                None => crate::ArchiveFormat::default(),
                Some("tar.gz") => crate::ArchiveFormat::TarGz,
                Some("tar.xz") => crate::ArchiveFormat::TarXz,
                Some("zip") => crate::ArchiveFormat::Zip,
                Some(other) => return Err(format!("unsupported source archive `{other}`")),
            };
            let url = source
                .url
                .as_deref()
                .ok_or("a source platform needs a URL")?;
            let strip_prefix = source
                .strip_prefix
                .as_deref()
                .map(|prefix| values.substitute(prefix))
                .transpose()?
                .map(PathBuf::from)
                .unwrap_or_default();
            recipe.build = Some(crate::SourceBuild {
                git: source.git.clone(),
                backend: build.backend.clone(),
                rust,
                go,
                url: values.substitute(url)?,
                sha256: digest.to_string(),
                archive,
                strip_prefix,
                configure: values.command(&build.configure)?,
                args: values.command(&build.args)?,
                patches: source.patches.clone(),
                dependencies: build.dependencies.clone(),
                libraries: build
                    .libraries
                    .iter()
                    .map(|library| {
                        let library = library.to_str().ok_or("library paths must be UTF-8")?;
                        values.substitute(library).map(PathBuf::from)
                    })
                    .collect::<Result<_, String>>()?,
                steps: build
                    .steps
                    .as_ref()
                    .map(|steps| {
                        Ok::<_, String>(crate::BuildSteps {
                            configure: values.commands(&steps.configure)?,
                            build: values.commands(&steps.build)?,
                            check: values.commands(&steps.check)?,
                            install: values.commands(&steps.install)?,
                        })
                    })
                    .transpose()?,
            });
        } else {
            return Err("a platform needs a prebuilt or a source".into());
        }
        Ok(recipe)
    }

    fn upstream(&self) -> BTreeMap<String, PackageUpstream> {
        self.platforms
            .iter()
            .filter_map(|(system, platform)| {
                Some((system.clone(), self.platform_upstream(platform)?.clone()))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UpstreamProvider;

    fn parse(json: &str) -> Recipe {
        serde_json::from_str(json).unwrap_or_else(|error| panic!("{error}\n{json}"))
    }

    #[test]
    fn a_shared_upstream_varies_only_by_target() {
        let recipe = parse(
            r#"{
              "name": "fd", "description": "Find entries", "homepage": "https://x",
              "recipe_maintainers": ["tale"],
              "prebuilt": { "github": "sharkdp/fd", "tag": "v{version}",
                            "asset": "fd-v{version}-{target}.tar.gz" },
              "outputs": { "bins": ["fd"], "checks": [["fd", "--version"]] },
              "platforms": {
                "aarch64-macos": { "target": "aarch64-apple-darwin", "default_version": "10.5.0" },
                "x86_64-linux": { "target": "x86_64-unknown-linux-musl", "default_version": "10.5.0" }
              },
              "versions": {
                "10.5.0": { "license": "MIT OR Apache-2.0",
                            "digests": { "aarch64-macos": "aa", "x86_64-linux": "bb" } }
              }
            }"#,
        );
        assert_eq!(recipe.platforms.len(), 2);
        assert!(matches!(
            recipe.shared.prebuilt.as_ref().unwrap().provider,
            Provider::Github(ref repo) if repo == "sharkdp/fd"
        ));
        assert_eq!(recipe.versions["10.5.0"].digests.len(), 2);
    }

    #[test]
    fn platforms_may_carry_their_own_upstream_and_outputs() {
        let recipe = parse(
            r#"{
              "name": "helium", "description": "Browse", "homepage": "https://x",
              "platforms": {
                "aarch64-macos": {
                  "default_version": "0.17.2.1",
                  "upstream": { "github": "imputnet/helium-macos" },
                  "prebuilt": { "github": "imputnet/helium-macos",
                                "asset": "helium_{version}_arm64-macos.dmg", "install": "Dmg" },
                  "outputs": { "apps": { "Helium.app": "Helium.app" } }
                },
                "x86_64-linux": {
                  "default_version": "0.17.2.1",
                  "upstream": { "github": "imputnet/helium-linux" },
                  "prebuilt": { "github": "imputnet/helium-linux",
                                "asset": "helium-{tag}-x86_64.AppImage" },
                  "outputs": { "bins": ["helium"] }
                }
              },
              "versions": {
                "0.17.2.1": { "license": "GPL-3.0-only",
                              "digests": { "aarch64-macos": "aa", "x86_64-linux": "bb" } }
              }
            }"#,
        );
        let mac = &recipe.platforms["aarch64-macos"];
        assert_eq!(
            mac.upstream.as_ref().unwrap().provider,
            UpstreamProvider::Github("imputnet/helium-macos".into())
        );
        assert!(mac.overrides.outputs.as_ref().unwrap().apps.is_some());
    }

    #[test]
    fn a_version_covering_one_platform_needs_no_systems_field() {
        let recipe = parse(
            r#"{
              "name": "ghostty", "description": "Terminal", "homepage": "https://x",
              "prebuilt": { "url": "https://r/{version}/Ghostty.dmg", "install": "Dmg" },
              "outputs": { "apps": { "Ghostty.app": "Ghostty.app" } },
              "platforms": { "aarch64-macos": { "default_version": "1.3.1" } },
              "versions": { "1.3.1": { "license": "MIT", "digests": { "aarch64-macos": "aa" } } }
            }"#,
        );
        assert_eq!(
            recipe.versions["1.3.1"].digests.keys().collect::<Vec<_>>(),
            vec!["aarch64-macos"]
        );
    }

    #[test]
    fn shared_libraries_are_named_by_each_platform_loader() {
        let digest = "a".repeat(64);
        let recipe = parse(&format!(
            r#"{{
              "name": "zlib", "description": "Compress", "homepage": "https://x",
              "source": {{ "url": "https://x/zlib-{{version}}.tar.gz", "archive": "tar.gz",
                          "strip_prefix": "zlib-{{version}}" }},
              "build": {{ "backend": "autotools", "libraries": ["lib/libz.{{shared_extension}}"] }},
              "outputs": {{ "bins": [] }},
              "platforms": {{
                "aarch64-macos": {{ "default_version": "1" }},
                "x86_64-linux": {{ "default_version": "1" }}
              }},
              "versions": {{ "1": {{ "license": "Zlib",
                "digests": {{ "aarch64-macos": "{digest}", "x86_64-linux": "{digest}" }} }} }}
            }}"#
        ));
        let package = recipe.expand().unwrap().package;
        let libraries = |system: &str| {
            package.versions["1"].platforms[system]
                .build
                .as_ref()
                .unwrap()
                .libraries
                .clone()
        };
        assert_eq!(
            libraries("aarch64-macos"),
            [PathBuf::from("lib/libz.dylib")]
        );
        assert_eq!(libraries("x86_64-linux"), [PathBuf::from("lib/libz.so")]);
    }

    #[test]
    fn bins_accept_a_list_or_a_map_of_paths() {
        let named: Outputs = serde_json::from_str(r#"{"bins":["fd"]}"#).unwrap();
        assert!(matches!(named.bins, Some(crate::Bins::Names(ref v)) if v == &["fd"]));

        let located: Outputs =
            serde_json::from_str(r#"{"bins":{"kitty":"kitty.app/Contents/MacOS/kitty"}}"#).unwrap();
        assert!(matches!(located.bins, Some(crate::Bins::Paths(ref m)) if m.contains_key("kitty")));
    }

    #[test]
    fn a_provider_cannot_be_both_github_and_url() {
        let both = serde_json::from_str::<Prebuilt>(r#"{"github":"a/b","url":"https://x"}"#);
        assert!(both.is_err(), "two providers must not deserialize");
    }
}
