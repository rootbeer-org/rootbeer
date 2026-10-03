mod cache;
pub mod consumer;
#[cfg(test)]
mod consumer_test;
mod environment;
pub use environment::{verify_environment, BuildEnvironment};
mod plan;
mod relocate;
pub use cache::{BuildCache, BuildSession};
pub use plan::BuildPlan;

mod archive;
pub mod audit;
mod backend;
mod runner;
mod sandbox;
pub mod scheduler;
pub use archive::pack;
pub use runner::{run, run_with_execution, run_with_sandbox};
pub use sandbox::Sandbox;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Command;
use std::time::Duration;

use rootbeer_package::download::DownloadCache;
use rootbeer_package::*;

use rootbeer_store_legacy::{hash_file, hash_tree, Store};

pub mod dependencies;
#[cfg(test)]
mod libraries_test;

/// An engine as fingerprinted by its build script: shared build code and each backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generation {
    pub shared: String,
    pub autotools: String,
    pub custom: String,
    pub go: String,
    pub rust: String,
    pub zig: String,
}

impl Generation {
    pub fn current() -> Self {
        Self {
            shared: env!("ROOTBEER_ENGINE_IDENTITY").into(),
            autotools: env!("ROOTBEER_BACKEND_AUTOTOOLS").into(),
            custom: env!("ROOTBEER_BACKEND_CUSTOM").into(),
            go: env!("ROOTBEER_BACKEND_GO").into(),
            rust: env!("ROOTBEER_BACKEND_RUST").into(),
            zig: env!("ROOTBEER_BACKEND_ZIG").into(),
        }
    }

    /// Identifies shared build behavior and the selected backend, independently of other backends.
    pub fn engine_identity(&self, backend: Option<&BuildBackend>) -> String {
        let implementation = match backend {
            Some(BuildBackend::Autotools) => &self.autotools,
            Some(BuildBackend::Custom) => &self.custom,
            Some(BuildBackend::Go) => &self.go,
            Some(BuildBackend::Rust) => &self.rust,
            Some(BuildBackend::Zig) => &self.zig,
            None => "",
        };
        rootbeer_store_legacy::hash_bytes(
            format!("rootbeer-engine-v2\0{}\0{implementation}", self.shared).as_bytes(),
        )
    }

    /// The fields a `scripts/cache-compatibility` record lists for this engine as a predecessor.
    pub fn record(&self) -> String {
        [
            &self.shared,
            &self.autotools,
            &self.custom,
            &self.go,
            &self.rust,
            &self.zig,
        ]
        .map(String::as_str)
        .join(" ")
    }
}

pub fn engine_identity(backend: Option<&BuildBackend>) -> String {
    Generation::current().engine_identity(backend)
}

/// Digest of this engine's shared code and every backend, which compatibility records guard on.
pub fn engine_generation() -> &'static str {
    env!("ROOTBEER_ENGINE_GENERATION")
}

/// Reviewed predecessor engines whose results remain valid, guarded by this exact generation.
pub fn compatible_generations() -> Vec<Generation> {
    env!("ROOTBEER_COMPATIBLE_GENERATIONS")
        .split(',')
        .filter(|record| !record.is_empty())
        .map(|record| {
            let fields: Vec<&str> = record.split(':').collect();
            Generation {
                shared: fields[0].into(),
                autotools: fields[1].into(),
                custom: fields[2].into(),
                go: fields[3].into(),
                rust: fields[4].into(),
                zig: fields[5].into(),
            }
        })
        .collect()
}

/// Resource limits and storage locations for a build execution.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub environment: Option<BuildEnvironmentLock>,
    pub is_isolated: bool,
    pub jobs: usize,
    pub downloads: PathBuf,
    pub cache: Option<BuildCache>,
    pub session: Option<BuildSession>,
    pub phase_timeout: Duration,
    pub execution: Execution,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            environment: None,
            is_isolated: false,
            jobs: 2,
            downloads: rootbeer_store_legacy::state_dir().join("downloads"),
            cache: None,
            session: None,
            phase_timeout: Duration::from_secs(1200),
            execution: Execution::default(),
        }
    }
}

impl BuildOptions {
    /// Identifies the effective tools, environment, and isolation for a recipe closure.
    pub fn environment_identity(
        &self,
        catalog: &PackageCatalog,
        request: &str,
        context: &str,
    ) -> Result<String, String> {
        self.validate()?;
        let graph = graph::DependencyGraph::new(
            catalog,
            &[request.into()],
            &ResolveContext::current().system,
        )?;
        let recipes = graph
            .order
            .iter()
            .map(|key| graph::find_recipe(catalog, key).map(|(_, _, recipe)| recipe))
            .collect::<Result<Vec<_>, _>>()?;
        if recipes.iter().all(|recipe| recipe.build.is_none()) {
            if let Some(lock) = &self.environment {
                verify_environment(lock)?;
            }
            let bytes = serde_json::to_vec(&(context, self.isolation()?, &self.environment))
                .map_err(|error| error.to_string())?;
            return Ok(rootbeer_store_legacy::hash_bytes(&bytes));
        }
        self.resolve_environment(recipes.iter())?
            .identity(&format!("{context}\0{}", self.isolation()?))
    }

    fn resolve_environment<'a>(
        &self,
        recipes: impl Iterator<Item = &'a CatalogRecipe>,
    ) -> Result<environment::Environment, String> {
        let mut environment = environment::Environment::resolve(self.environment.as_ref())?;
        let mut has_rust = false;
        let mut has_go = false;
        for recipe in recipes {
            if let Some(build) = &recipe.build {
                has_rust |= matches!(build.backend, BuildBackend::Rust);
                has_go |= matches!(build.backend, BuildBackend::Go);
            }
        }
        if has_rust {
            environment = environment.with_rust()?;
        }
        if has_go {
            environment = environment.with_go()?;
        }
        Ok(environment)
    }

    fn isolation(&self) -> Result<String, String> {
        if self.is_isolated {
            return Sandbox::identity();
        }
        Ok("host".into())
    }

    fn validate(&self) -> Result<(), String> {
        self.execution.check().map_err(|error| error.to_string())?;
        if self.is_isolated && self.environment.is_none() {
            return Err("isolated builds require a pinned environment".into());
        }
        if self.jobs == 0 || self.jobs > 64 {
            return Err("jobs must be between 1 and 64".into());
        }
        if self.phase_timeout.is_zero() {
            return Err("phase timeout must be positive".into());
        }
        if self
            .cache
            .as_ref()
            .is_some_and(|cache| cache.context.trim().is_empty())
        {
            return Err(
                "build cache requires an identity for the host image, SDK, and toolchain".into(),
            );
        }
        Ok(())
    }
}

/// Resolves and builds a source recipe using trusted host tools.
pub fn build_package(
    catalog: &PackageCatalog,
    request: &str,
    output: &Path,
    opts: &BuildOptions,
) -> Result<BuildArtifact, String> {
    opts.validate()?;
    BuildPlan::current(catalog, request)?.execute(output, opts)
}

fn execute(plan: &BuildPlan, output: &Path, opts: &BuildOptions) -> Result<BuildArtifact, String> {
    opts.validate()?;
    let environment = opts.resolve_environment(plan.recipes.values())?;
    let isolation = opts.isolation()?;
    let context = opts.cache.as_ref().map_or("", |cache| &cache.context);
    let identity = environment.identity(&format!("{context}\0{isolation}"))?;
    let graph = &plan.graph;
    let cache = opts.cache.as_ref();
    fs::create_dir(output)
        .map_err(|e| format!("cannot create build output {}: {e}", output.display()))?;
    let output = output.canonicalize().map_err(|e| e.to_string())?;
    let workspace = tempfile::tempdir_in(&output).map_err(|e| e.to_string())?;
    let host_tools = workspace.path().join("environment-tools");
    environment.stage(&host_tools)?;
    let probe = tempfile::tempdir_in(&output).map_err(|error| error.to_string())?;
    let probe_sandbox = opts
        .is_isolated
        .then(|| Sandbox::new(&environment.lock, [], probe.path()))
        .transpose()?;
    if let Some(sandbox) = &probe_sandbox {
        run_with_execution(
            &[
                environment.lock.tools["sh"]
                    .path
                    .to_string_lossy()
                    .into_owned(),
                "-c".into(),
                "exit 0".into(),
            ],
            probe.path(),
            &BTreeMap::new(),
            &output.join("sandbox.log"),
            Duration::from_secs(10),
            Some(sandbox),
            &opts.execution,
        )
        .map_err(|error| format!("build isolation is unavailable: {error}"))?;
    }
    let store = cache
        .map(|cache| cache.directory.join("store"))
        .unwrap_or_else(|| workspace.path().join("store"));
    fs::create_dir_all(&store).map_err(|error| error.to_string())?;
    let store = store.canonicalize().map_err(|error| error.to_string())?;
    let realizer = PackageRealizer::with_dirs(
        Store::new(store),
        &opts.downloads,
        workspace.path().join("install"),
    )
    .with_execution(opts.execution.clone());
    let mut resolved = BTreeMap::<String, LockedPackage>::new();
    let mut dependency_bins = BTreeMap::new();
    let mut dependency_roots = BTreeMap::<String, PathBuf>::new();
    let mut root_artifact = None;
    for (index, key) in graph.order.iter().enumerate() {
        opts.execution.check().map_err(|error| error.to_string())?;
        let recipe = &plan.recipes[key];
        let is_root = index + 1 == graph.order.len();
        let mut built = None;
        let mut cache_entry = None;
        let mut is_cached = false;
        let destination = if is_root {
            output.clone()
        } else {
            output.join(format!("dependency-{key}"))
        };
        let locked = if let Some(binary) = plan.binaries.get(key) {
            let mut package = binary.clone();
            if let Some(build) = &recipe.build {
                package
                    .runtime_dependencies
                    .extend(runtime_dependencies(build, &resolved));
            }
            package
        } else if let Some(build) = &recipe.build {
            if !is_root {
                fs::create_dir(&destination).map_err(|e| e.to_string())?;
            }
            let dependencies = graph.nodes[key]
                .closure
                .iter()
                .map(|key| (key.clone(), resolved[key].clone()))
                .collect::<BTreeMap<_, _>>();
            let tools = workspace.path().join(format!("tools-{index}"));
            fs::create_dir(&tools).map_err(|e| e.to_string())?;
            for (key, exports) in &graph.nodes[key].exports {
                if exports.has_bins {
                    let bins: &BTreeMap<String, PathBuf> = &dependency_bins[key];
                    for (name, path) in bins {
                        if fs::read_link(tools.join(name)).ok().as_ref() == Some(path) {
                            continue;
                        }
                        symlink(path, tools.join(name)).map_err(|e| {
                            format!("build dependency command collision for {name}: {e}")
                        })?;
                    }
                }
                if exports.has_libraries {
                    dependencies::stage(
                        &dependency_roots[key],
                        &exports.libraries,
                        exports.is_shared,
                        &tools.join(".rootbeer-libraries"),
                    )
                    .map_err(|error| format!("{key}: {error}"))?;
                }
            }
            verify_environment(&environment.lock)?;
            cache_entry = cache
                .map(|cache| {
                    let key = cache::key(
                        key,
                        plan.revisions[key],
                        recipe,
                        &graph.system,
                        &dependencies,
                        &identity,
                        &graph.nodes[key].exports,
                    )?;
                    cache.entry(key, opts.session.as_ref(), &opts.execution)
                })
                .transpose()?;
            let restored = cache_entry
                .as_ref()
                .map(|entry| entry.restore(&destination))
                .transpose()?
                .flatten();
            is_cached = restored.is_some();
            let mut artifact = match restored {
                Some(mut artifact) => {
                    artifact.catalog_sha256 = plan.catalog_sha256.clone();
                    artifact.recipe_sha256 = recipe.sha256();
                    artifact.package.runtime_dependencies =
                        runtime_dependencies(build, &dependencies);
                    artifact.dependencies = dependencies;
                    artifact
                }
                None => compile(
                    plan,
                    key,
                    dependencies,
                    &tools,
                    &destination,
                    opts,
                    (&environment, &host_tools, &dependency_roots),
                )?,
            };
            artifact.environment = Some(environment.lock.clone());
            artifact.qualification_environment = Some(identity.clone());
            artifact.isolation = Some(isolation.clone());
            artifact.resolver_inputs = plan.inputs.clone();
            artifact.published_dependencies = graph.nodes[key]
                .closure
                .iter()
                .filter_map(|dependency| {
                    let record = plan.published.get(dependency)?;
                    Some((dependency.clone(), record.clone()))
                })
                .collect();
            artifact.build_key = cache_entry.as_ref().map(|entry| entry.key().to_string());
            artifact.build_environment = cache.map(|cache| cache.context.clone());
            let locked = artifact.package.clone();
            built = Some(artifact);
            locked
        } else {
            return Err(format!(
                "{key}: build plan has neither source nor a resolved binary"
            ));
        };
        let realized = realizer.realize(&locked).map_err(|e| e.to_string())?;
        if let Some(artifact) = built.as_mut() {
            artifact.runtime_audit_sha256 = Some(audit_output(
                &realized.store_entry.path,
                &destination,
                &runtime_roots(&artifact.package.runtime_dependencies, &dependency_roots)?,
            )?);
        }
        let check_workspace = tempfile::tempdir_in(&output).map_err(|error| error.to_string())?;
        let check_environment =
            environment.variables(&host_tools, &host_tools, check_workspace.path());
        let runtime_paths = runtime_roots(&locked.runtime_dependencies, &dependency_roots)?;
        let check_sandbox = opts
            .is_isolated
            .then(|| {
                Sandbox::new(
                    &environment.lock,
                    std::iter::once(host_tools.clone())
                        .chain(std::iter::once(realized.store_entry.path.clone()))
                        .chain(runtime_paths.values().cloned()),
                    check_workspace.path(),
                )
            })
            .transpose()?;
        for check in &recipe.checks {
            let mut args = check.clone();
            args[0] = realized.bins[&check[0]].to_string_lossy().into_owned();
            run_with_execution(
                &args,
                check_workspace.path(),
                &check_environment,
                &output.join("checks.log"),
                Duration::from_secs(30),
                check_sandbox.as_ref(),
                &opts.execution,
            )?;
        }
        if let Some(artifact) = built {
            verify_environment(&environment.lock)?;
            write_install_files(&artifact, &destination)?;
            if let Some(entry) = &cache_entry {
                if !is_cached {
                    entry.save(&artifact, &destination)?;
                }
                entry.complete();
            }
            fs::write(
                destination.join("receipt.json"),
                serde_json::to_vec_pretty(&artifact).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            if is_root {
                root_artifact = Some(artifact);
            }
        }
        let mut locked = locked;
        locked.output_sha256 = Some(realized.store_entry.output_sha256);
        dependency_bins.insert(key.clone(), realized.bins);
        dependency_roots.insert(key.clone(), realized.store_entry.path);
        resolved.insert(key.clone(), locked);
    }
    root_artifact.ok_or_else(|| "source build produced no artifact".into())
}

fn compile(
    plan: &BuildPlan,
    key: &str,
    dependencies: BTreeMap<String, LockedPackage>,
    tools: &Path,
    output: &Path,
    opts: &BuildOptions,
    execution_environment: (&environment::Environment, &Path, &BTreeMap<String, PathBuf>),
) -> Result<BuildArtifact, String> {
    let (build_environment, host_tools, dependency_roots) = execution_environment;
    let recipe = &plan.recipes[key];
    let request = PackageRequest::parse(key);
    let version = request
        .version
        .as_ref()
        .ok_or("build plan must contain exact versions")?;
    let build = recipe.build.as_ref().ok_or("recipe has no source build")?;
    let downloads = DownloadCache::new(&opts.downloads).with_execution(opts.execution.clone());
    let archive = downloads
        .materialize_verified(&build.url, &build.sha256)
        .map_err(|e| e.to_string())?;
    let workspace = tempfile::tempdir_in(output).map_err(|e| e.to_string())?;
    let workspace_path = workspace.path().canonicalize().map_err(|e| e.to_string())?;
    rootbeer_package::realize::extract_archive(&archive, build.archive, workspace.path())
        .map_err(|e| e.to_string())?;
    let source = workspace
        .path()
        .join(&build.strip_prefix)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if !source.starts_with(&workspace_path) {
        return Err("source directory escapes the archive".into());
    }
    let prefix = workspace_path.join("prefix");
    fs::create_dir(&prefix).map_err(|e| e.to_string())?;
    let mut environment = build_environment.variables(tools, host_tools, &workspace_path);
    let dependency_prefix = tools.join(".rootbeer-libraries");
    dependencies::environment(&dependency_prefix, &mut environment)?;
    if let Some(rust) = &build.rust {
        for (name, value) in &rust.environment {
            environment.insert(name, value.clone());
        }
        environment.insert(
            "CARGO_HOME",
            workspace_path
                .join("cargo-home")
                .to_string_lossy()
                .into_owned(),
        );
        environment.insert(
            "CARGO_TARGET_DIR",
            workspace_path
                .join("cargo-target")
                .to_string_lossy()
                .into_owned(),
        );
        environment.insert("CARGO_INCREMENTAL", "0".into());
        environment.insert(
            "RUSTC",
            build_environment.lock.tools["rustc"]
                .path
                .to_string_lossy()
                .into_owned(),
        );
    }
    if let Some(go) = &build.go {
        environment.extend([
            (
                "GOROOT",
                build_environment.lock.inputs["go-toolchain"]
                    .path
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("GOENV", "off".into()),
            ("GO111MODULE", "on".into()),
            ("GOWORK", "off".into()),
            ("GOTOOLCHAIN", "local".into()),
            ("GOFLAGS", "".into()),
            ("GOEXPERIMENT", go.experiments.join(",")),
            ("GOPRIVATE", "".into()),
            ("GONOPROXY", "".into()),
            ("GONOSUMDB", "".into()),
            ("GOVCS", "*:off".into()),
            (
                "CGO_ENABLED",
                if go.is_cgo_enabled { "1" } else { "0" }.into(),
            ),
            (
                "GOPATH",
                workspace_path.join("go").to_string_lossy().into_owned(),
            ),
            (
                "GOCACHE",
                opts.cache
                    .as_ref()
                    .filter(|cache| !cache.recheck && !opts.is_isolated)
                    .map(|cache| cache.go_compiler_cache(build_environment, &dependencies))
                    .transpose()?
                    .unwrap_or_else(|| workspace_path.join("go-cache"))
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "GOMODCACHE",
                workspace_path
                    .join("go-modules")
                    .to_string_lossy()
                    .into_owned(),
            ),
        ]);
    }
    let mut reads = vec![tools.to_path_buf(), host_tools.to_path_buf()];
    reads.extend(dependencies.keys().map(|key| dependency_roots[key].clone()));
    let sandbox = opts
        .is_isolated
        .then(|| Sandbox::new(&build_environment.lock, reads, &workspace_path))
        .transpose()?;
    let mut toolchain = BTreeMap::from([(
        "rootbeer.host-path".into(),
        if build_environment.is_host {
            "/usr/bin:/bin:/usr/sbin:/sbin"
        } else {
            ""
        }
        .into(),
    )]);
    let zig = tools.join("zig");
    let go = host_tools.join("go");
    let compiler_tools = match build.backend {
        BuildBackend::Autotools | BuildBackend::Custom => vec![
            (environment["CC"].as_str(), "--version"),
            ("make", "--version"),
        ],
        BuildBackend::Rust => {
            for name in ["cargo", "rustc"] {
                if !host_tools.join(name).is_file() {
                    return Err(format!(
                        "Rust builds require a pinned toolchain providing {name}"
                    ));
                }
            }
            vec![
                ("cargo", "--version"),
                ("rustc", "--version"),
                (environment["CC"].as_str(), "--version"),
            ]
        }
        BuildBackend::Go => vec![(go.to_str().ok_or("Go tool path must be UTF-8")?, "version")],
        BuildBackend::Zig => {
            if !zig.is_file() {
                return Err("Zig builds require an exact catalog dependency providing zig".into());
            }
            vec![("zig", "version")]
        }
    };
    for (index, (name, argument)) in compiler_tools.into_iter().enumerate() {
        let log = output.join(format!("tool-{index}.log"));
        run_with_execution(
            &[name.into(), argument.into()],
            &source,
            &environment,
            &log,
            opts.phase_timeout,
            sandbox.as_ref(),
            &opts.execution,
        )?;
        toolchain.insert(
            name.into(),
            fs::read_to_string(log)
                .map_err(|error| error.to_string())?
                .trim()
                .into(),
        );
    }
    let log = output.join("build.log");
    for (index, patch) in build.patches.iter().enumerate() {
        let path = workspace_path.join(format!("patch-{index}.diff"));
        fs::write(&path, patch).map_err(|e| e.to_string())?;
        run_with_execution(
            &[
                build_environment.lock.tools["patch"]
                    .path
                    .to_string_lossy()
                    .into_owned(),
                "--batch".into(),
                "-p1".into(),
                "-i".into(),
                path.to_string_lossy().into_owned(),
            ],
            &source,
            &environment,
            &log,
            Duration::from_secs(30),
            sandbox.as_ref(),
            &opts.execution,
        )?;
    }
    fs::create_dir_all(workspace_path.join("zig-global-cache/tmp")).map_err(|e| e.to_string())?;
    let runtime_dependencies = runtime_dependencies(build, &dependencies);
    let runtime = plan.graph.nodes[key]
        .runtime_closure
        .iter()
        .map(|key| {
            Ok((
                key.clone(),
                rootbeer_package::runtime::store_directory(&dependencies[key])?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let relocation_roots = runtime
        .iter()
        .map(|(key, directory)| (dependency_roots[key].clone(), directory.clone()))
        .collect::<BTreeMap<_, _>>();
    let rpaths = relocate::link_flags(&prefix, &relocation_roots, &build.libraries)?;
    if !rpaths.is_empty() {
        append_flags(&mut environment, "LDFLAGS", &rpaths);
        match build.backend {
            BuildBackend::Rust => {
                let flags = rpaths.iter().map(|flag| format!("-Clink-arg={flag}"));
                append_flags(&mut environment, "RUSTFLAGS", &flags.collect::<Vec<_>>());
            }
            BuildBackend::Go => append_flags(&mut environment, "CGO_LDFLAGS", &rpaths),
            _ => {}
        }
    }
    let downloads_directory = opts
        .downloads
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let phases = backend::plan(
        build,
        &backend::Context {
            prefix: &prefix,
            downloads: &downloads_directory,
            host_tools,
            bins: &recipe.bins.names().into_iter().cloned().collect::<Vec<_>>(),
            dependencies: &dependency_prefix,
            tools,
            workspace: &workspace_path,
            jobs: opts.jobs,
            runtime: &runtime,
        },
    )?;
    for phase in phases {
        let phase_sandbox = if phase.requires_network {
            None
        } else {
            sandbox.as_ref()
        };
        for command in phase.commands {
            if let Err(error) = run_with_execution(
                &command,
                &source,
                &environment,
                &log,
                opts.phase_timeout,
                phase_sandbox,
                &opts.execution,
            ) {
                let retained = workspace.keep();
                return Err(format!(
                    "{}: {error}; build workspace retained at {}",
                    phase.name,
                    retained.display()
                ));
            }
        }
    }
    let bins = recipe.bins.source_paths();
    for (name, path) in &bins {
        let path = prefix
            .join(path)
            .canonicalize()
            .map_err(|e| format!("missing output command {name}: {e}"))?;
        if !path.starts_with(&prefix) || !path.is_file() {
            return Err(format!("invalid output command {name}"));
        }
    }
    if !relocation_roots.is_empty() || relocate::exports_shared(&prefix, &build.libraries) {
        relocate::Relocation {
            prefix: &prefix,
            runtime: &relocation_roots,
        }
        .apply()?;
    }
    dependencies::validate(&prefix, &build.libraries)?;
    let runtime_audit = audit_output(
        &prefix,
        output,
        &runtime_roots(&runtime_dependencies, dependency_roots)?,
    )?;
    let artifact_path = output.join("package.tar.gz");
    pack(&prefix, &artifact_path).map_err(|e| e.to_string())?;
    let artifact = BuildArtifact {
        schema: if runtime_dependencies.is_empty() {
            1
        } else {
            2
        },
        build_key: None,
        recipe_sha256: recipe.sha256(),
        qualification_environment: None,
        build_environment: None,
        environment: None,
        isolation: None,
        runtime_audit_sha256: Some(runtime_audit),
        catalog_sha256: plan.catalog_sha256.clone(),
        revision: plan.revisions[key],
        system: plan.graph.system.clone(),
        build: build.clone(),
        dependencies,
        published_dependencies: BTreeMap::new(),
        resolver_inputs: PackageResolverInputs::default(),
        toolchain,
        package: LockedPackage {
            name: request.name,
            version: version.clone(),
            source: LockedSource::File {
                path: artifact_path.clone(),
                sha256: hash_file(&artifact_path).map_err(|e| e.to_string())?,
            },
            install: LockedInstall::Archive {
                format: ArchiveFormat::TarGz,
                strip_prefix: None,
            },
            provides: Provides {
                apps: recipe.apps.clone(),
                bins: bins.clone(),
            },
            runtime_dependencies,
            output_sha256: Some(hash_tree(&prefix).map_err(|e| e.to_string())?),
        },
    };
    Ok(artifact)
}

fn append_flags(environment: &mut BTreeMap<&str, String>, name: &'static str, flags: &[String]) {
    let existing = environment.get(name).filter(|value| !value.is_empty());
    let value = existing
        .into_iter()
        .chain(flags)
        .cloned()
        .collect::<Vec<_>>();
    environment.insert(name, value.join(" "));
}

fn runtime_dependencies(
    build: &SourceBuild,
    dependencies: &BTreeMap<String, LockedPackage>,
) -> BTreeMap<String, LockedPackage> {
    build
        .dependencies
        .iter()
        .filter(|dependency| dependency.kind().is_runtime())
        .map(|dependency| {
            (
                dependency.package().to_owned(),
                dependencies[dependency.package()].clone(),
            )
        })
        .collect()
}

fn runtime_roots(
    dependencies: &BTreeMap<String, LockedPackage>,
    roots: &BTreeMap<String, PathBuf>,
) -> Result<BTreeMap<PathBuf, PathBuf>, String> {
    let mut result = BTreeMap::new();
    for dependency in dependencies.values() {
        for package in rootbeer_package::runtime::closure(dependency)?
            .into_iter()
            .chain(std::iter::once(dependency))
        {
            let root = roots
                .get(&package.id())
                .ok_or_else(|| format!("{}: runtime root is missing", package.id()))?;
            result.insert(
                rootbeer_package::runtime::store_directory(package)?,
                root.clone(),
            );
        }
    }
    Ok(result)
}

fn audit_output(
    root: &Path,
    output: &Path,
    runtime: &BTreeMap<PathBuf, PathBuf>,
) -> Result<String, String> {
    let report = audit::audit_with_runtime(root, runtime)?;
    let bytes = serde_json::to_vec_pretty(&report).map_err(|error| error.to_string())?;
    fs::write(output.join("runtime-audit.json"), &bytes).map_err(|error| error.to_string())?;
    report.validate()?;
    Ok(rootbeer_store_legacy::hash_bytes(&bytes))
}

fn install_spec(package: &LockedPackage, output: &Path) -> Result<serde_json::Value, String> {
    let source = match &package.source {
        LockedSource::File { path, sha256 } => {
            serde_json::json!({"file": local_archive(path, sha256, output)?, "sha256": sha256})
        }
        LockedSource::Url { url, sha256 } => serde_json::json!({"url": url, "sha256": sha256}),
        LockedSource::Path { .. } => return Err("build install files require archives".into()),
    };
    let runtime = package
        .runtime_dependencies
        .iter()
        .map(|(key, package)| Ok((key.clone(), install_spec(package, output)?)))
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    let mut spec = serde_json::json!({"name": package.name, "version": package.version,
        "source": source,
        "install": {"archive": "tar.gz"}, "bins": package.provides.bins, "output_sha256": package.output_sha256});
    if !package.provides.apps.is_empty() {
        spec["apps"] = serde_json::to_value(&package.provides.apps).map_err(|e| e.to_string())?;
    }
    if !runtime.is_empty() {
        spec["runtime_dependencies"] = serde_json::to_value(runtime).map_err(|e| e.to_string())?;
    }
    Ok(spec)
}

/// The archive relative to `output`, copied in when it lives elsewhere, such as a build cache.
fn local_archive(path: &Path, sha256: &str, output: &Path) -> Result<PathBuf, String> {
    if let Ok(relative) = path.strip_prefix(output) {
        return Ok(relative.to_path_buf());
    }
    let relative = Path::new("runtime").join(format!("{sha256}.tar.gz"));
    fs::create_dir_all(output.join("runtime")).map_err(|e| e.to_string())?;
    fs::copy(path, output.join(&relative)).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(relative)
}

fn write_install_files(artifact: &BuildArtifact, output: &Path) -> Result<(), String> {
    let spec = install_spec(&artifact.package, output)?;
    fs::write(
        output.join("package.json"),
        serde_json::to_vec_pretty(&spec).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    fs::write(
        output.join("install.lua"),
        "local rb = require(\"rootbeer\")\nrb.package(rb.json.read(\"package.json\"))\n",
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod runtime_test;

#[cfg(test)]
#[path = "../../rootbeer-package/tests/support/catalog.rs"]
mod test_catalog;

#[cfg(test)]
#[path = "../../rootbeer-package/tests/support/repository.rs"]
mod test_repository;
