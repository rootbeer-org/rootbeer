use std::collections::BTreeMap;
use std::fs;
use std::process::Command;

use rootbeer_package::download::DownloadCache;
use rootbeer_package::*;
use rootbeer_store_legacy::hash_file;

/// Resolves Rootbeer itself and nothing else; see [`rootbeer_package::self_update`].
pub fn self_update_resolver_stack(inputs: &PackageResolverInputs) -> ResolverStack {
    let mut stack = ResolverStack::new().with_implicit_resolver("rootbeer");
    if let Some(pin) = inputs.repository() {
        stack.push(rootbeer_package::self_update::Resolver::new(
            pin,
            rootbeer_store_legacy::state_dir().join("downloads"),
        ));
    }
    stack
}

/// Builds an installation resolver that prefers published artifacts and can execute source recipes.
pub fn resolver_stack_for_inputs(inputs: &PackageResolverInputs) -> ResolverStack {
    let mut stack = backend_stack().with_implicit_resolver("rootbeer");
    if inputs.repository().is_some() || inputs.local_catalog().is_some() {
        stack.push(SourceResolver::with_inputs(
            inputs,
            rootbeer_store_legacy::state_dir(),
        ));
    }
    stack
}

/// Installs published packages or builds recipes from local definitions and the repository.
pub struct SourceResolver {
    state: std::path::PathBuf,
    repository: Option<RepositoryResolver>,
    inputs: PackageResolverInputs,
}

impl SourceResolver {
    /// Keeps source downloads, build results, and diagnostics under the caller's state directory.
    pub fn with_inputs(
        inputs: &PackageResolverInputs,
        state: impl Into<std::path::PathBuf>,
    ) -> Self {
        let state = state.into();
        Self {
            repository: inputs
                .repository()
                .map(|pin| RepositoryResolver::with_cache(pin, state.join("downloads"), false)),
            inputs: inputs.clone(),
            state,
        }
    }
}

impl PackageResolver for SourceResolver {
    fn name(&self) -> &str {
        "rootbeer"
    }

    fn resolve(
        &self,
        request: &PackageRequest,
        context: &ResolveContext,
    ) -> Result<Option<PackageResolution>, String> {
        if request.asset.is_some() || !request.bins.is_empty() {
            return Err("PDR packages do not accept asset or command overrides".into());
        }
        if let Some(selection) = &request.source {
            selection.validate()?;
            if request.version.is_some() && !matches!(selection, SourceSelection::Release) {
                return Err("Git refs cannot be combined with a release version".into());
            }
        }
        let local = self
            .inputs
            .local_catalog()
            .filter(|catalog| catalog.find(&request.name).is_some());
        if local.is_none() && request.source.is_none() {
            if let Some(repository) = &self.repository {
                return repository.resolve(request, context);
            }
        }
        let repository_catalog = if local.is_none_or(PackageCatalog::requires_pdr) {
            let repository = self
                .repository
                .as_ref()
                .ok_or("this request requires a package repository")?;
            let mut names = vec![request.name.as_str()];
            if let Some(local) = local {
                names.extend(missing_dependencies(local));
            }
            Some(
                repository.catalog(
                    names
                        .into_iter()
                        .filter(|name| local.is_none_or(|local| local.find(name).is_none())),
                )?,
            )
        } else {
            None
        };
        let mut catalog = match (local, &repository_catalog) {
            (Some(local), None) => local.clone(),
            (_, Some(published)) => published.clone(),
            _ => return Err("no package catalog selected".into()),
        };
        if let Some(local) = local {
            catalog.packages.extend(local.packages.clone());
        }
        catalog.validate()?;
        let package = catalog
            .find(&request.name)
            .ok_or_else(|| format!("unknown package `{}`", request.name))?;
        let version = match request.version.as_deref() {
            Some(version) => version,
            None => package
                .default_version_for(&context.system)
                .ok_or_else(|| format!("{} does not support {}", package.name, context.system))?,
        };
        let key = format!("{}@{version}", package.name);
        if local.is_some()
            && request.source.is_none()
            && package.versions.get(version).is_some_and(|entry| {
                entry
                    .for_system(&context.system)
                    .is_some_and(|recipe| recipe.build.is_none())
            })
        {
            return catalog::CatalogResolver::new(&catalog, &self.inputs, backend_stack())
                .resolve(request, context);
        }
        if context != &ResolveContext::current() {
            return Err("local source builds require the host platform".into());
        }
        let entry = package
            .versions
            .get(version)
            .ok_or_else(|| format!("{key}: no approved recipe"))?;
        let mut recipe = entry
            .for_system(&context.system)
            .ok_or_else(|| format!("{key}: no source recipe for {}", context.system))?
            .clone();
        let original = recipe.clone();
        let published = rootbeer_package::CatalogVersion {
            platforms: BTreeMap::new(),
            ..entry.clone()
        };
        let build = recipe
            .build
            .as_mut()
            .ok_or_else(|| format!("{key} is prebuilt-only; no source build is declared"))?;
        let recipe_sha256 = original.sha256();
        let state = &self.state;
        let downloads = state.join("downloads");
        let mut version = version.to_string();
        let mut git_commit = None;
        if let Some(selection) = request
            .source
            .as_ref()
            .filter(|selection| !matches!(selection, SourceSelection::Release))
        {
            let git = build.git.as_ref().ok_or("this source recipe does not declare a Git repository for HEAD, tag, or revision builds")?;
            let commit = resolve_commit(git, selection)?;
            let (_, repository) = github::repository(&git.github)?;
            build.url = format!("https://github.com/{}/archive/{commit}.tar.gz", git.github);
            build.sha256 = DownloadCache::new(&downloads)
                .materialize(&build.url, None)
                .map_err(|error| error.to_string())?
                .sha256;
            build.archive = ArchiveFormat::TarGz;
            build.strip_prefix = format!("{repository}-{commit}").into();
            version = format!("git-{commit}");
            git_commit = Some(commit);
        }
        build.validate()?;
        let source_url = build.url.clone();
        let source_sha256 = build.sha256.clone();
        recipe.source = None;
        recipe.asset = None;
        recipe.sha256 = None;
        recipe.mirror = false;
        let name = package.name.clone();
        let catalog_sha256 = catalog.sha256();
        let entry = catalog.packages.get_mut(&name).unwrap();
        let mut platforms = entry
            .versions
            .get(&version)
            .map(|entry| entry.platforms.clone())
            .unwrap_or_default();
        platforms.insert(context.system.clone(), recipe);
        entry.versions.insert(
            version.clone(),
            rootbeer_package::CatalogVersion {
                platforms,
                ..published
            },
        );
        entry
            .default_versions
            .insert(context.system.clone(), version.clone());
        catalog.validate()?;
        let runs = state.join("source-builds");
        fs::create_dir_all(&runs).map_err(|error| error.to_string())?;
        let run = tempfile::tempdir_in(&runs)
            .map_err(|error| error.to_string())?
            .keep();
        let artifact = crate::build_package(
            &catalog,
            &format!("{name}@{version}"),
            &run.join("output"),
            &crate::BuildOptions {
                downloads,
                cache: Some(crate::BuildCache {
                    directory: state.join("builds"),
                    context: "consumer-v1".into(),
                    recheck: false,
                }),
                ..Default::default()
            },
        )?;
        let proof = SourceBuildProof {
            repository: repository_catalog
                .as_ref()
                .and(self.inputs.repository().cloned()),
            local_catalog_sha256: local.map(PackageCatalog::sha256),
            catalog_sha256,
            recipe_sha256,
            source_url,
            source_sha256,
            git_commit,
            build_key: artifact
                .build_key
                .clone()
                .ok_or("source build did not record its cache identity")?,
            receipt_sha256: hash_file(run.join("output/receipt.json"))
                .map_err(|error| error.to_string())?,
        };
        Ok(Some(PackageResolution::new(
            artifact.package,
            ResolutionProof::SourceBuild(proof),
        )))
    }
}

fn resolve_commit(source: &GitSource, selection: &SourceSelection) -> Result<String, String> {
    source.validate()?;
    selection.validate()?;
    if let SourceSelection::Revision(commit) = selection {
        return Ok(commit.clone());
    }
    let reference = match selection {
        SourceSelection::Head => source
            .branch
            .as_ref()
            .map(|branch| format!("refs/heads/{branch}"))
            .unwrap_or("HEAD".into()),
        SourceSelection::Branch(branch) => format!("refs/heads/{branch}"),
        SourceSelection::Tag(tag) => format!("refs/tags/{tag}"),
        _ => return Err("a Git source selection is required".into()),
    };
    let mut command = Command::new("git");
    command.args([
        "ls-remote",
        "--exit-code",
        &format!("https://github.com/{}.git", source.github),
        &reference,
    ]);
    if matches!(selection, SourceSelection::Tag(_)) {
        command.arg(format!("{reference}^{{}}"));
    }
    let output = command
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| format!("cannot resolve Git source: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cannot resolve {reference}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    commit_from_refs(&String::from_utf8_lossy(&output.stdout), &reference)
}

fn commit_from_refs(output: &str, reference: &str) -> Result<String, String> {
    let mut commit = None;
    let mut peeled = None;
    for line in output.lines() {
        let Some((sha, name)) = line.split_once('\t') else {
            return Err("invalid Git ref response".into());
        };
        SourceSelection::Revision(sha.into()).validate()?;
        if name == reference {
            commit = Some(sha.to_string());
        }
        if name == format!("{reference}^{{}}") {
            peeled = Some(sha.to_string());
        }
    }
    peeled
        .or(commit)
        .ok_or_else(|| format!("Git ref `{reference}` was not found"))
}

/// Packages a local catalog builds with but does not define.
fn missing_dependencies(local: &PackageCatalog) -> Vec<&str> {
    local
        .packages
        .values()
        .flat_map(|package| package.versions.values())
        .flat_map(|version| version.platforms.values())
        .filter_map(|recipe| recipe.build.as_ref())
        .flat_map(|build| &build.dependencies)
        .map(|dependency| dependency.package())
        .map(|package| package.split_once('@').map_or(package, |(name, _)| name))
        .filter(|name| local.find(name).is_none())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotated_tags_pin_the_commit_and_ignore_other_refs() {
        let object = "a".repeat(40);
        let commit = "b".repeat(40);
        let refs = format!("{object}\trefs/tags/v1\n{commit}\trefs/tags/v1^{{}}\n");
        assert_eq!(commit_from_refs(&refs, "refs/tags/v1").unwrap(), commit);
        assert!(commit_from_refs(&refs, "refs/heads/v1").is_err());
        assert!(commit_from_refs("short\tHEAD\n", "HEAD").is_err());
    }
}
