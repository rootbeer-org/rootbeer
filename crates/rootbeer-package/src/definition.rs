use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use mlua::LuaSerdeExt;
use serde::{Deserialize, Serialize};

mod comments;
pub mod lua;
use super::CatalogPackage;

mod recipe;

/// A package's approved versions and optional update rules, stored in one Lua file.
#[derive(Debug, Clone)]
pub struct PackageDefinition {
    pub package: CatalogPackage,
    pub upstream: BTreeMap<String, PackageUpstream>,
    authoring: Option<recipe::Recipe>,
    comments: comments::Comments,
}

/// Where discovery finds new versions for one or more platforms.
///
/// Authoring metadata only: it never reaches a published catalog, so a new provider is an
/// engine change rather than a client break.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageUpstream {
    #[serde(flatten)]
    pub provider: UpstreamProvider,
    /// Pins the GitHub repository's identity so a rename or takeover is noticed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_id: Option<u64>,
    /// Release tag for a version, such as `v{version}`; `{version}` alone when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Replaces the dots of a version inside its tag, for tags such as `curl-8_22_0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub separator: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamProvider {
    Github(String),
}

impl PackageUpstream {
    pub fn repository(&self) -> &str {
        let UpstreamProvider::Github(repository) = &self.provider;
        repository
    }

    fn tag_template(&self) -> &str {
        self.tag.as_deref().unwrap_or("{version}")
    }

    /// The release tag that publishes `version`.
    pub fn tag_for(&self, version: &str) -> String {
        let version = match &self.separator {
            Some(separator) => version.replace('.', separator),
            None => version.to_string(),
        };
        self.tag_template().replace("{version}", &version)
    }

    /// The version a release tag publishes, when the tag belongs to this upstream.
    pub fn version_of(&self, tag: &str) -> Option<String> {
        let (prefix, suffix) = self.tag_template().split_once("{version}")?;
        let version = tag.strip_prefix(prefix)?.strip_suffix(suffix)?;
        if version.is_empty() {
            return None;
        }
        Some(match &self.separator {
            Some(separator) => version.replace(separator.as_str(), "."),
            None => version.to_string(),
        })
    }

    pub fn validate(&self) -> Result<(), String> {
        super::github::repository(self.repository())?;
        if self.repository_id == Some(0) {
            return Err("invalid repository ID".into());
        }
        let template = self.tag_template();
        if template.matches("{version}").count() != 1
            || template.replace("{version}", "").contains(['{', '}'])
        {
            return Err(format!(
                "upstream tag `{template}` must contain `{{version}}` exactly once"
            ));
        }
        if self
            .separator
            .as_deref()
            .is_some_and(|separator| separator.is_empty() || separator == ".")
        {
            return Err("an upstream separator replaces `.` with something else".into());
        }
        Ok(())
    }
}

impl PackageDefinition {
    /// Wraps exact catalog recipes without update rules or shared authoring defaults.
    pub fn new(package: CatalogPackage) -> Self {
        Self {
            package,
            upstream: BTreeMap::new(),
            authoring: None,
            comments: Default::default(),
        }
    }

    /// Evaluates a package in the same bounded, I/O-free sandbox as catalog recipes.
    pub fn from_lua(source: &str) -> Result<Self, String> {
        let (lua, value) = lua::evaluate(source)?;
        let recipe: recipe::Recipe = lua.from_value(value).map_err(|error| error.to_string())?;
        let mut definition = recipe.expand()?;
        definition.comments = comments::Comments::scan(source)?;
        Ok(definition)
    }

    /// Reads canonical package files in deterministic name order.
    pub fn from_directory(directory: &Path) -> Result<BTreeMap<String, Self>, String> {
        let mut definitions = BTreeMap::new();
        for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.extension().is_none_or(|extension| extension != "lua") {
                continue;
            }
            let source = fs::read_to_string(&path).map_err(|e| e.to_string())?;
            let definition =
                Self::from_lua(&source).map_err(|e| format!("{}: {e}", path.display()))?;
            if path.file_stem().and_then(|name| name.to_str()) != Some(&definition.package.name) {
                return Err(format!(
                    "{}: canonical name must match the filename",
                    path.display()
                ));
            }
            definitions.insert(definition.package.name.clone(), definition);
        }
        Ok(definitions)
    }

    /// Records a discovered version, merging in the digest each platform published for it.
    pub fn add_version(
        &mut self,
        version: &str,
        digests: BTreeMap<String, String>,
        license: Option<String>,
        commit: Option<String>,
    ) -> Result<(), String> {
        let mut recipe = self
            .authoring
            .clone()
            .ok_or("adding a version requires an authored recipe")?;
        if digests.is_empty() {
            return Err(format!("{version}: a version builds at least one platform"));
        }
        recipe.insert_version(version, digests, license, commit);
        *self = recipe.expand()?;
        Ok(())
    }

    /// Points one platform at a version it already builds.
    pub fn set_default_version(&mut self, system: &str, version: &str) -> Result<(), String> {
        let mut recipe = self
            .authoring
            .clone()
            .ok_or("setting a default requires an authored recipe")?;
        recipe.set_default_version(system, version)?;
        *self = recipe.expand()?;
        Ok(())
    }

    /// Each distinct upstream and the platforms it discovers versions for.
    pub fn upstreams(&self) -> Vec<(PackageUpstream, Vec<String>)> {
        let mut groups: Vec<(PackageUpstream, Vec<String>)> = Vec::new();
        for (system, upstream) in &self.upstream {
            match groups.iter_mut().find(|(known, _)| known == upstream) {
                Some((_, systems)) => systems.push(system.clone()),
                None => groups.push((upstream.clone(), vec![system.clone()])),
            }
        }
        groups
    }

    /// What `system` would download for a version not yet recorded, from its templates.
    ///
    /// Discovery pins the digest of exactly this asset or archive, so the digest and the
    /// download cannot come from different templates.
    pub fn candidate(
        &self,
        system: &str,
        version: &str,
        commit: Option<&str>,
    ) -> Result<super::CatalogRecipe, String> {
        self.authoring
            .as_ref()
            .ok_or("resolving a candidate requires an authored recipe")?
            .candidate(system, version, commit)
    }

    /// Whether a new version must record the commit its tag points at.
    pub fn uses_commit(&self) -> bool {
        self.authoring
            .as_ref()
            .is_some_and(recipe::Recipe::uses_commit)
    }

    /// Pins a repository ID wherever `repository` is declared without one.
    pub fn pin_repository_id(&mut self, repository: &str, id: u64) -> Result<(), String> {
        let mut recipe = self
            .authoring
            .clone()
            .ok_or("pinning an upstream requires an authored recipe")?;
        recipe.pin_repository_id(repository, id);
        *self = recipe.expand()?;
        Ok(())
    }

    /// Every platform this recipe declares, whether or not a version covers it.
    pub fn platforms(&self) -> Vec<String> {
        self.authoring
            .as_ref()
            .map(|recipe| recipe.platform_names())
            .unwrap_or_default()
    }

    /// Renders one complete package file, including its update rules when configured.
    pub fn to_lua(&self) -> Result<String, String> {
        let recipe = self
            .authoring
            .as_ref()
            .ok_or("rendering a package file requires its authored recipe")?;
        lua::write_commented(&recipe.with_shared_upstream(), &self.comments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELIUM: &str = r#"return {
        name = "helium",
        description = "Browse the web",
        homepage = "https://helium.computer",
        default_license = "GPL-3.0-only",
        platforms = {
            ["aarch64-macos"] = {
                default_version = "0.17.2.1",
                upstream = { github = "imputnet/helium-macos" },
                prebuilt = { github = "imputnet/helium-macos", asset = "helium-{version}-arm64.dmg" },
                outputs = { apps = { ["Helium.app"] = "Helium.app" } },
            },
            ["x86_64-linux"] = {
                default_version = "0.17.2.1",
                upstream = { github = "imputnet/helium-linux" },
                prebuilt = { github = "imputnet/helium-linux", asset = "helium-{version}-x86_64.AppImage" },
                outputs = { bins = { "helium" }, checks = { { "helium", "--version" } } },
            },
        },
        versions = {
            ["0.17.2.1"] = { digests = { ["aarch64-macos"] = "aa", ["x86_64-linux"] = "bb" } },
        },
    }"#;

    #[test]
    fn a_package_keeps_one_upstream_per_platform() {
        let definition = PackageDefinition::from_lua(HELIUM).unwrap();
        let repositories: Vec<_> = definition
            .upstream
            .values()
            .map(PackageUpstream::repository)
            .collect();
        assert_eq!(
            repositories,
            ["imputnet/helium-macos", "imputnet/helium-linux"]
        );
    }

    #[test]
    fn each_platform_resolves_its_own_asset_and_outputs() {
        let definition = PackageDefinition::from_lua(HELIUM).unwrap();
        let entry = &definition.package.versions["0.17.2.1"];

        let macos = entry.for_system("aarch64-macos").unwrap();
        assert_eq!(macos.asset.as_deref(), Some("helium-0.17.2.1-arm64.dmg"));
        assert!(macos.apps.contains_key("Helium.app") && macos.bins.is_empty());

        let linux = entry.for_system("x86_64-linux").unwrap();
        assert_eq!(
            linux.asset.as_deref(),
            Some("helium-0.17.2.1-x86_64.AppImage")
        );
        assert!(linux.bins.names().contains(&"helium".to_string()) && linux.apps.is_empty());
    }

    #[test]
    fn a_version_only_builds_the_platforms_it_carries_a_digest_for() {
        let source = HELIUM.replace(
            r#"digests = { ["aarch64-macos"] = "aa", ["x86_64-linux"] = "bb" }"#,
            r#"digests = { ["aarch64-macos"] = "aa" }"#,
        );
        let definition = PackageDefinition::from_lua(&source).unwrap_err();
        assert!(definition.contains("x86_64-linux"), "{definition}");
    }

    fn upstream(fields: &str) -> Result<PackageUpstream, String> {
        let (lua, value) =
            lua::evaluate(&format!("return {{ github = \"owner/tool\", {fields} }}"))?;
        let upstream: PackageUpstream = lua.from_value(value).map_err(|error| error.to_string())?;
        upstream.validate().map(|()| upstream)
    }

    #[test]
    fn a_tag_template_round_trips_versions_through_tags() {
        let curl = upstream(r#"tag = "curl-{version}", separator = "_""#).unwrap();
        assert_eq!(curl.tag_for("8.22.0"), "curl-8_22_0");
        assert_eq!(curl.version_of("curl-8_22_0").as_deref(), Some("8.22.0"));
        assert_eq!(curl.version_of("tiny-curl-8_4_0"), None);
        assert_eq!(curl.version_of("curl-"), None);

        let bare = upstream("").unwrap();
        assert_eq!(bare.tag_for("1.2"), "1.2");
        assert_eq!(
            bare.version_of("v1.2").as_deref(),
            Some("v1.2"),
            "no implicit v"
        );
    }

    #[test]
    fn upstream_authoring_is_strict() {
        for invalid in [
            r#"tag_prefix = "v""#,
            r#"tag = "v""#,
            r#"tag = "{version}-{version}""#,
            r#"tag = "{tag}{version}""#,
            r#"separator = ".""#,
            "repository_id = 0",
        ] {
            assert!(upstream(invalid).is_err(), "accepted `{invalid}`");
        }
    }

    #[test]
    fn a_platform_upstream_overrides_the_shared_one() {
        let source = HELIUM
            .replace(r#"upstream = { github = "imputnet/helium-linux" },"#, "")
            .replace(
                r#"default_license = "GPL-3.0-only","#,
                r#"default_license = "GPL-3.0-only", upstream = { github = "imputnet/helium-linux" },"#,
            );
        let definition = PackageDefinition::from_lua(&source).unwrap();
        let groups: Vec<_> = definition
            .upstreams()
            .into_iter()
            .map(|(upstream, systems)| (upstream.repository().to_string(), systems))
            .collect();
        assert_eq!(
            groups,
            [
                (
                    "imputnet/helium-macos".to_string(),
                    vec!["aarch64-macos".to_string()]
                ),
                (
                    "imputnet/helium-linux".to_string(),
                    vec!["x86_64-linux".to_string()]
                ),
            ]
        );
    }

    #[test]
    fn rendering_keeps_comments_beside_their_fields() {
        let source = r#"-- Maintainer note above the recipe.
return {
            name = "tool", description = "A tool", homepage = "https://example.com",
            default_license = "MIT",
            source = { url = "https://example.com/{version}.tar.gz", archive = "tar.gz", strip_prefix = "tool-{version}" },
            build = {
                backend = "autotools",
                -- Upstream #1 breaks the default; drop once it ships.
                configure = { "--disable-thing", "--enable-other" }, -- both needed
            },
            outputs = { bins = { "tool" }, checks = { { "tool", "--version" } } },
            platforms = { ["x86_64-linux"] = { default_version = "1" } },
            versions = { ["1"] = { digests = { ["x86_64-linux"] = "DIGEST" } } },
        }"#
        .replace("DIGEST", &"a".repeat(64));
        let rendered = PackageDefinition::from_lua(&source)
            .unwrap()
            .to_lua()
            .unwrap();
        assert!(rendered.starts_with("-- Maintainer note above the recipe.\nreturn {"));
        assert!(rendered.contains(
            "        -- Upstream #1 breaks the default; drop once it ships.\n        configure = { \"--disable-thing\", \"--enable-other\" }, -- both needed\n"
        ));
        let again = PackageDefinition::from_lua(&rendered)
            .unwrap()
            .to_lua()
            .unwrap();
        assert_eq!(again, rendered);
    }

    #[test]
    fn a_rendered_source_build_reads_back() {
        let source = r#"return {
            name = "tool", description = "A tool", homepage = "https://example.com",
            default_license = "MIT",
            upstream = { github = "owner/tool", tag = "v{version}" },
            source = { url = "https://example.com/{tag}.tar.gz", archive = "tar.gz", strip_prefix = "tool-{version}" },
            build = { backend = "go", go = { binaries = { tool = "./cmd" } } },
            outputs = { bins = { "tool" }, checks = { { "tool", "--version" } } },
            platforms = { ["x86_64-linux"] = { default_version = "1" } },
            versions = { ["1"] = { digests = { ["x86_64-linux"] = "DIGEST" } } },
        }"#
        .replace("DIGEST", &"a".repeat(64));
        let definition = PackageDefinition::from_lua(&source).unwrap();
        let rendered = definition.to_lua().unwrap();
        let again = PackageDefinition::from_lua(&rendered).unwrap();
        assert_eq!(again.package, definition.package);

        let build =
            &rendered[rendered.find("build = {").unwrap()..rendered.find("outputs").unwrap()];
        for resolved in [
            "url",
            "sha256",
            "strip_prefix",
            "archive",
            "cgo",
            "generate",
        ] {
            assert!(
                !build.contains(resolved),
                "`{resolved}` leaked into:\n{build}"
            );
        }
    }

    #[test]
    fn commands_fill_the_version_and_keep_every_other_brace() {
        let source = r#"return {
            name = "tool", description = "A tool", homepage = "https://example.com",
            default_license = "MIT",
            upstream = { github = "owner/tool", tag = "v{version}" },
            source = { url = "https://example.com/{tag}.tar.gz", archive = "tar.gz", strip_prefix = "tool-{version}" },
            build = {
                backend = "custom",
                steps = {
                    build = { { "make", "-j{jobs}", "VERSION={version}" } },
                    check = { { "make", "check" } },
                    install = { { "sh", "-ec", "cp tool ${HOME} {prefix}/{tag}" } },
                },
            },
            outputs = { bins = { "tool" }, checks = { { "tool", "--is={version}", "{len(x)}" } } },
            platforms = { ["x86_64-linux"] = { default_version = "1.2" } },
            versions = { ["1.2"] = { digests = { ["x86_64-linux"] = "DIGEST" } } },
        }"#
        .replace("DIGEST", &"a".repeat(64));
        let definition = PackageDefinition::from_lua(&source).unwrap();
        let recipe = definition.package.versions["1.2"]
            .for_system("x86_64-linux")
            .unwrap();
        let build = recipe.build.as_ref().unwrap();

        assert_eq!(recipe.checks, [["tool", "--is=1.2", "{len(x)}"]]);
        let steps = build.steps.as_ref().unwrap();
        assert_eq!(steps.build, [["make", "-j{jobs}", "VERSION=1.2"]]);
        assert_eq!(
            steps.install,
            [["sh", "-ec", "cp tool ${HOME} {prefix}/v1.2"]]
        );
        assert!(definition.to_lua().unwrap().contains("--is={version}"));
    }

    #[test]
    fn templates_name_the_parts_of_a_release_version() {
        let source = r#"return {
            name = "tool", description = "A tool", homepage = "https://example.com",
            default_license = "MIT",
            source = { url = "https://example.com/{version}.tar.gz", archive = "tar.gz", strip_prefix = "tool-{version}" },
            build = { backend = "go", go = {
                binaries = { tool = "./cmd" },
                variables = { major = "{major}", minor = "{minor}", patch = "{patch}" },
            } },
            outputs = { bins = { "tool" }, checks = { { "tool", "--minor={minor}" } } },
            platforms = { ["x86_64-linux"] = { default_version = "VERSION" } },
            versions = { ["VERSION"] = { digests = { ["x86_64-linux"] = "DIGEST" } } },
        }"#
        .replace("DIGEST", &"a".repeat(64));
        let definition =
            PackageDefinition::from_lua(&source.replace("VERSION", "1.37.1-rc.1+build")).unwrap();
        let recipe = definition.package.versions["1.37.1-rc.1+build"]
            .for_system("x86_64-linux")
            .unwrap();
        let variables = &recipe
            .build
            .as_ref()
            .unwrap()
            .go
            .as_ref()
            .unwrap()
            .variables;
        assert_eq!(variables.values().collect::<Vec<_>>(), ["1", "37", "1"]);
        assert_eq!(recipe.checks, [["tool", "--minor=37"]]);

        let error = PackageDefinition::from_lua(&source.replace("VERSION", "5.0")).unwrap_err();
        assert!(
            error.contains("`{patch}` needs a version with 3 parts"),
            "{error}"
        );
    }

    #[test]
    fn a_commit_template_needs_each_version_to_record_its_commit() {
        let source = r#"return {
            name = "tool", description = "A tool", homepage = "https://example.com",
            default_license = "MIT",
            source = { url = "https://example.com/{version}.tar.gz", archive = "tar.gz", strip_prefix = "tool-{version}" },
            build = { backend = "go", go = { binaries = { tool = "./cmd" }, variables = { commit = "{commit}" } } },
            outputs = { bins = { "tool" }, checks = { { "tool", "--version" } } },
            platforms = { ["x86_64-linux"] = { default_version = "1" } },
            versions = { ["1"] = { COMMIT digests = { ["x86_64-linux"] = "DIGEST" } } },
        }"#
        .replace("DIGEST", &"a".repeat(64));

        let missing = PackageDefinition::from_lua(&source.replace("COMMIT", "")).unwrap_err();
        assert!(missing.contains("record its `commit`"), "{missing}");

        let short = source.replace("COMMIT", r#"commit = "abc123","#);
        let error = PackageDefinition::from_lua(&short).unwrap_err();
        assert!(error.contains("full lowercase SHA-1"), "{error}");

        let recorded = source.replace("COMMIT", &format!(r#"commit = "{}","#, "f".repeat(40)));
        let definition = PackageDefinition::from_lua(&recorded).unwrap();
        assert!(definition.uses_commit());
        let build = definition.package.versions["1"].platforms["x86_64-linux"]
            .build
            .as_ref()
            .unwrap();
        assert_eq!(
            build.go.as_ref().unwrap().variables["commit"],
            "f".repeat(40)
        );

        let rust = recorded.replace(
            r#"backend = "go", go = { binaries = { tool = "./cmd" }, variables = { commit = "{commit}" } }"#,
            r#"backend = "rust", rust = { packages = { "tool" }, environment = { SOURCE = "{commit}" } }"#,
        );
        let definition = PackageDefinition::from_lua(&rust).unwrap();
        assert!(definition.uses_commit());
        let build = definition.package.versions["1"].platforms["x86_64-linux"]
            .build
            .as_ref()
            .unwrap();
        assert_eq!(
            build.rust.as_ref().unwrap().environment["SOURCE"],
            "f".repeat(40)
        );
    }

    #[test]
    fn a_build_cannot_restate_its_source() {
        for field in [
            r#"url = "https://example.com/x.tar.gz""#,
            r#"sha256 = "aa""#,
            r#"strip_prefix = "x""#,
            r#"archive = "zip""#,
            r#"patches = { "fix.patch" }"#,
        ] {
            let source = HELIUM.replace(
                r#"default_license = "GPL-3.0-only","#,
                &format!(r#"default_license = "GPL-3.0-only", build = {{ backend = "autotools", {field} }},"#),
            );
            let error = PackageDefinition::from_lua(&source).unwrap_err();
            assert!(error.contains("unknown field"), "`{field}`: {error}");
        }
    }

    #[test]
    fn rendering_declares_an_upstream_every_platform_repeats_once() {
        let repeated = r#"return {
            name = "tool", description = "A tool", homepage = "https://example.com",
            default_license = "MIT",
            prebuilt = { github = "owner/tool", asset = "tool-{version}-{target}.tar.gz" },
            outputs = { bins = { "tool" }, checks = { { "tool", "--version" } } },
            platforms = {
                ["aarch64-macos"] = { target = "mac", default_version = "1",
                                      upstream = { github = "owner/tool", tag = "v{version}" } },
                ["x86_64-linux"] = { target = "linux", default_version = "1",
                                     upstream = { github = "owner/tool", tag = "v{version}" } },
            },
            versions = { ["1"] = { digests = { ["aarch64-macos"] = "aa", ["x86_64-linux"] = "bb" } } },
        }"#;
        let definition = PackageDefinition::from_lua(repeated).unwrap();
        let rendered = definition.to_lua().unwrap();
        assert_eq!(rendered.matches("upstream = {").count(), 1, "{rendered}");
        let again = PackageDefinition::from_lua(&rendered).unwrap();
        assert_eq!(again.package, definition.package);
        assert_eq!(again.upstream, definition.upstream);

        let split = PackageDefinition::from_lua(HELIUM).unwrap();
        let rendered = split.to_lua().unwrap();
        assert_eq!(
            rendered.matches("upstream = {").count(),
            2,
            "distinct upstreams stay per platform"
        );
    }

    #[test]
    fn rendering_round_trips_through_the_authored_recipe() {
        let definition = PackageDefinition::from_lua(HELIUM).unwrap();
        let rendered = definition.to_lua().unwrap();
        let again = PackageDefinition::from_lua(&rendered).unwrap();
        assert_eq!(again.package, definition.package);
    }
}
