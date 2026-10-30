use crate::Download;
use crate::backend::prebuilt;
use crate::template::Values;
use mlua::{Lua, LuaOptions, LuaSerdeExt, StdLib, VmState};
use rootbeer_drv::{Allow, Platform, Sha256};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde::de::IgnoredAny;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU32, Ordering};

const MEMORY_LIMIT: usize = 64 << 20;
const INTERRUPT_BUDGET: u32 = 100_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Recipe {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub description: String,
    pub homepage: String,
    #[serde(default)]
    pub recipe_maintainers: Vec<String>,
    pub default_license: Option<String>,
    pub upstream: Option<Upstream>,
    #[serde(flatten)]
    pub shared: Spec,
    pub platforms: BTreeMap<Platform, PlatformSpec>,
    pub versions: BTreeMap<String, Version>,
    #[serde(skip)]
    pub has_commit_placeholder: bool,
}

/// Where a recipe finds new versions, and how a version names its tag there.
/// Only `tag` and `separator` reach a derivation, through `{tag}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Upstream {
    /// A GitHub repository as `owner/repo`.
    pub github: Option<String>,
    /// A git repository's HTTPS URL.
    pub git: Option<String>,
    /// The GitHub repository's numeric ID, which a rename keeps and a new
    /// repository under the same name doesn't.
    pub repository_id: Option<u64>,
    /// The tag template around `{version}`. Just the version when unset.
    pub tag: Option<String>,
    /// What stands for each `.` of the version in its tag.
    pub separator: Option<String>,
    /// Tags that fit the template but aren't releases. Each is a glob, so
    /// `v5.?[13579].*` leaves out every odd minor.
    #[serde(default)]
    pub exclude_tags: Vec<String>,
    /// A tag upstream moves to each new build, followed instead of version
    /// tags.
    pub rolling: Option<Rolling>,
    /// Whether an update's PR merges itself once its checks pass.
    #[serde(default, rename = "automerge")]
    pub should_automerge: bool,
}

/// A rolling tag, whose builds are versioned by the commit it names.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rolling {
    pub tag: String,
    /// What each build's version starts with, before `+` and its commit.
    pub version: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlatformSpec {
    pub target: Option<String>,
    pub default_version: String,
    pub upstream: Option<Upstream>,
    #[serde(flatten)]
    pub spec: Spec,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Version {
    pub digests: BTreeMap<Platform, Sha256>,
    pub commit: Option<String>,
    pub serial: Option<u64>,
    pub license: Option<String>,
    // Content changes rekey on their own, so revisions mean nothing here.
    #[serde(rename = "revision")]
    _revision: Option<IgnoredAny>,
    #[serde(flatten)]
    pub spec: Spec,
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Spec {
    pub prebuilt: Option<Prebuilt>,
    pub source: Option<Source>,
    pub build: Option<Build>,
    #[serde(default)]
    pub outputs: Outputs,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Prebuilt {
    pub github: Option<String>,
    pub url: Option<String>,
    pub tag: Option<String>,
    pub asset: Option<String>,
    pub install: Option<Install>,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    #[serde(default)]
    pub steps: Steps,
    // We don't care about mirrors because we have a pinned digest
    #[serde(rename = "mirror")]
    _mirror: Option<IgnoredAny>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum Install {
    Dmg,
    Archive {
        format: ArchiveFormat,
        strip_prefix: Option<String>,
    },
}

#[derive(Clone, Copy, Default, Deserialize)]
pub(crate) enum ArchiveFormat {
    #[default]
    #[serde(alias = "tar.gz")]
    TarGz,
    #[serde(alias = "tar.xz")]
    TarXz,
    #[serde(alias = "zip")]
    Zip,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Source {
    pub url: String,
    // Selects HEAD builds from a repository, release builds ignore this
    #[serde(rename = "git")]
    _git: Option<IgnoredAny>,
    #[serde(default)]
    pub archive: ArchiveFormat,
    pub strip_prefix: Option<String>,
    #[serde(default)]
    pub patches: Vec<String>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Build {
    pub backend: Backend,
    pub rust: Option<Rust>,
    pub go: Option<Go>,
    #[serde(default)]
    pub configure: Vec<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    #[serde(default)]
    pub libraries: Vec<String>,
    #[serde(default)]
    pub allow: BTreeSet<Allow>,
    pub steps: Option<Steps>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Backend {
    Autotools,
    Custom,
    Go,
    Rust,
    Zig,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Dependency {
    pub package: String,
    /// Unset follows the package's default on the platform, so moving a
    /// toolchain's default rebuilds what depends on it.
    pub version: Option<String>,
    pub kind: DependencyKind,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DependencyKind {
    All,
    Build,
    Link,
    LinkRuntime,
    Runtime,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Go {
    pub binaries: BTreeMap<String, String>,
    #[serde(default)]
    pub generate: Vec<String>,
    #[serde(default)]
    pub experiments: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub variables: BTreeMap<String, String>,
    #[serde(default, rename = "cgo")]
    pub is_cgo_enabled: bool,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rust {
    pub packages: Vec<String>,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub no_default_features: bool,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Steps {
    #[serde(default)]
    pub prepare: Vec<Vec<String>>,
    #[serde(default)]
    pub configure: Vec<Vec<String>>,
    #[serde(default)]
    pub build: Vec<Vec<String>>,
    #[serde(default)]
    pub check: Vec<Vec<String>>,
    #[serde(default)]
    pub install: Vec<Vec<String>>,
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Outputs {
    pub bins: Option<Bins>,
    pub apps: Option<BTreeMap<String, String>>,
    pub checks: Option<Vec<Vec<String>>>,
    pub host: Option<BTreeSet<String>>,
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum Bins {
    Names(Vec<String>),
    Paths(BTreeMap<String, String>),
}

/// One version of a recipe on one platform, with every override applied.
pub(crate) struct Resolved<'a> {
    pub spec: Spec,
    pub values: Values<'a>,
    pub sha256: &'a Sha256,
    pub license: &'a str,
}

pub(crate) fn load<T: DeserializeOwned>(file: &str, source: &str) -> mlua::Result<T> {
    let lua = Lua::new_with(StdLib::NONE, LuaOptions::default())?;
    lua.sandbox(true)?;
    lua.set_memory_limit(MEMORY_LIMIT)?;

    let interrupts = AtomicU32::new(0);
    lua.set_interrupt(move |_| {
        if interrupts.fetch_add(1, Ordering::Relaxed) >= INTERRUPT_BUDGET {
            return Err(mlua::Error::runtime("evaluation budget exceeded"));
        }

        Ok(VmState::Continue)
    });

    let value = lua.load(source).set_name(file).eval::<mlua::Value>()?;
    lua.from_value(value)
}

impl Recipe {
    pub(crate) fn resolve(
        &self,
        version: &str,
        platform: Platform,
    ) -> Result<Resolved<'_>, String> {
        let (version, entry) = self
            .versions
            .get_key_value(version)
            .ok_or_else(|| format!("{} has no version {version}", self.name))?;

        let (spec, values) = self.layer(version, entry, platform)?;
        let sha256 = entry
            .digests
            .get(&platform)
            .ok_or("the version has no digest for this platform")?;

        let license = entry
            .license
            .as_deref()
            .or(self.default_license.as_deref())
            .ok_or("the version has no license")?;

        Ok(Resolved {
            spec,
            values,
            sha256,
            license,
        })
    }

    /// Where `version` downloads from on `platform` before the recipe
    /// declares it, as its own templates render it.
    pub(crate) fn download(
        &self,
        version: &str,
        commit: Option<String>,
        serial: Option<u64>,
        platform: Platform,
    ) -> Result<Download, String> {
        let entry = Version {
            digests: BTreeMap::new(),
            commit,
            serial,
            license: None,
            _revision: None,
            spec: Spec::default(),
        };

        let (spec, values) = self.layer(version, &entry, platform)?;
        match (&spec.prebuilt, &spec.source) {
            (Some(prebuilt), None) => prebuilt::locate(prebuilt, &values),
            (None, Some(source)) => Ok(Download {
                url: values.literal(&source.url)?,
                release: None,
            }),
            (Some(_), Some(_)) => {
                Err("a platform is prebuilt or built from source, not both".into())
            }
            (None, None) => Err("a platform needs a prebuilt or a source".into()),
        }
    }

    fn layer<'a>(
        &'a self,
        version: &'a str,
        entry: &'a Version,
        platform: Platform,
    ) -> Result<(Spec, Values<'a>), String> {
        let platform_spec = self
            .platforms
            .get(&platform)
            .ok_or("the version's platform is not declared")?;

        let tag = match platform_spec.upstream.as_ref().or(self.upstream.as_ref()) {
            Some(upstream) => upstream.tag_for(version),
            None => version.to_string(),
        };

        let spec = self
            .shared
            .overlay(&platform_spec.spec)
            .overlay(&entry.spec);

        let values = Values {
            version,
            tag,
            target: platform_spec.target.as_deref(),
            commit: entry.commit.as_deref(),
            serial: entry.serial,
            platform,
            dependencies: BTreeMap::new(),
        };

        Ok((spec, values))
    }
}

impl Upstream {
    fn tag_for(&self, version: &str) -> String {
        let version = match &self.separator {
            Some(separator) => version.replace('.', separator),
            None => version.to_string(),
        };

        self.tag
            .as_deref()
            .unwrap_or("{version}")
            .replace("{version}", &version)
    }

    /// The version a tag names, when the tag is exactly what `tag_for` makes
    /// of that version.
    pub fn version_of(&self, tag: &str) -> Option<String> {
        let (prefix, suffix) = self
            .tag
            .as_deref()
            .unwrap_or("{version}")
            .split_once("{version}")?;
        let version = tag.strip_prefix(prefix)?.strip_suffix(suffix)?;
        let version = match &self.separator {
            Some(separator) if !separator.is_empty() => version.replace(separator, "."),
            _ => version.to_string(),
        };

        let is_exact = !version.is_empty() && self.tag_for(&version) == tag;
        is_exact.then_some(version)
    }
}

impl Spec {
    /// Platform and version overrides replace a shared field outright.
    fn overlay(&self, over: &Spec) -> Spec {
        let (base, top) = (&self.outputs, &over.outputs);
        Spec {
            prebuilt: over.prebuilt.clone().or_else(|| self.prebuilt.clone()),
            source: over.source.clone().or_else(|| self.source.clone()),
            build: over.build.clone().or_else(|| self.build.clone()),
            outputs: Outputs {
                bins: top.bins.clone().or_else(|| base.bins.clone()),
                apps: top.apps.clone().or_else(|| base.apps.clone()),
                checks: top.checks.clone().or_else(|| base.checks.clone()),
                host: top.host.clone().or_else(|| base.host.clone()),
            },
        }
    }
}

impl Steps {
    pub(crate) fn has_steps_before_install(&self) -> bool {
        [&self.prepare, &self.configure, &self.build, &self.check]
            .iter()
            .any(|phase| !phase.is_empty())
    }
}

impl Bins {
    pub(crate) fn names(&self) -> Vec<&str> {
        match self {
            Bins::Names(names) => names.iter().map(String::as_str).collect(),
            Bins::Paths(paths) => paths.keys().map(String::as_str).collect(),
        }
    }
}
