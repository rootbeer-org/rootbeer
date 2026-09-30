use std::io::{self, Write};
use std::path::PathBuf;

use clap::{Args as ClapArgs, Subcommand};
use rootbeer_packaging::{PackageCatalog, PackageDefinition};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Read package definitions from a PDR checkout
    #[arg(long, global = true)]
    catalog: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Plan the work this machine builds, reuses, or recovers, and every unpublished dependency
    Plan {
        #[arg(required = true)]
        packages: Vec<String>,
        #[arg(short, long, default_value = "plan.json")]
        output: PathBuf,
        /// Where records of reused results are saved for publication
        #[arg(long)]
        records: Option<PathBuf>,
        /// Identity of this machine's image; detected by default
        #[arg(long)]
        context: Option<String>,
    },
    /// Build one task of a plan, installing the dependency builds earlier jobs made
    Build {
        plan: PathBuf,
        task: String,
        #[arg(short, long, default_value = "result")]
        output: PathBuf,
        /// Directory holding the handed-in dependency builds
        #[arg(long, default_value = "dependency-builds")]
        dependencies: PathBuf,
        /// Persistent build result cache
        #[arg(long)]
        cache: Option<PathBuf>,
        /// Identity of this machine's image; detected by default
        #[arg(long)]
        context: Option<String>,
    },
    /// Steps of the GitHub Actions workflows
    Ci {
        #[command(subcommand)]
        command: crate::ci::Ci,
    },
    /// Approve one qualified package and prepare its signed package release
    Release {
        #[arg(long)]
        receipt: PathBuf,
        /// GHCR repository, such as owner/packages/tool
        #[arg(long)]
        registry: String,
        #[arg(long)]
        output: PathBuf,
        /// Publisher's Ed25519 PKCS#8 DER key
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        public_key: String,
        /// Require these planned inputs before signing
        #[arg(long)]
        input_key: Option<String>,
        /// Publication time in Unix seconds, when backfilling history; defaults to now
        #[arg(long)]
        published: Option<u64>,
    },
    /// Upload a signed package release to GHCR without rebuilding or signing again
    Push {
        release: PathBuf,
        /// The registry repository the release was prepared for
        #[arg(long)]
        registry: String,
        #[arg(long)]
        public_key: String,
    },
    /// Publish a single discovery manifest from independently signed package records
    PublishRecords {
        #[arg(long)]
        record: Vec<String>,
        #[arg(long)]
        records: Option<PathBuf>,
        #[arg(long)]
        site: PathBuf,
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        public_key: String,
    },
    /// Add inferred update rules to copies of the selected catalog's GitHub packages
    /// Check tracked upstreams, caching metadata and reporting independent failures
    Updates {
        #[arg(long)]
        cache: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Discover GitHub releases and generate complete candidate package definitions
    /// List canonical names, approved defaults, and descriptions
    List,
    /// Show a package's identity and version recipes, accepting aliases
    Show { name: String },
    /// Validate the selected catalog and print its digest
    Check,
    /// Rewrite every recipe in the layout discovery writes, so its updates never reflow a file
    Format {
        /// Fail, listing the recipes that differ, instead of rewriting them
        #[arg(long)]
        check: bool,
    },
    /// Write the expanded catalog as deterministic JSON to stdout
    Catalog,
    /// Hash a build environment specification and write its lock to stdout
    PinEnvironment { specification: PathBuf },
    /// Print this engine's generation and the fields naming it in cache compatibility records
    EngineGeneration,
    /// Audit native loader references in an installed package directory
    Audit {
        directory: PathBuf,
        /// Build receipt declaring the installed package's runtime closure
        #[arg(long)]
        receipt: Option<PathBuf>,
    },
    /// Show the dependency graph a package builds with
    Graph { name: String },
    /// Prepare a source or upstream binary recipe as an installable local artifact
    Prepare {
        name: String,
        /// Pinned tools, SDK/sysroot inputs, and build variables
        #[arg(long)]
        environment: Option<PathBuf>,
        /// Deny network access and restrict filesystem access to build inputs and scratch
        #[arg(long, requires = "environment")]
        isolate: bool,
        #[arg(long)]
        output: PathBuf,
        /// Compiler jobs; defaults to all available CPU cores
        #[arg(short, long, default_value_t = std::thread::available_parallelism().map_or(1, usize::from))]
        jobs: usize,
        /// Persistent build result cache
        #[arg(long, requires = "cache_context")]
        cache: Option<PathBuf>,
        /// Identity of the host image, SDK, and toolchain
        #[arg(long, requires = "cache")]
        cache_context: Option<String>,
        /// Rebuild dependencies and refresh cached results
        #[arg(long, requires = "cache")]
        recheck: bool,
        /// Maximum seconds for each configure, build, check, or install command
        #[arg(long, default_value_t = 1200)]
        phase_timeout: u64,
        #[command(flatten)]
        published: Published,
        /// Install a dependency from this `prepare` output instead of compiling it
        #[arg(long = "dependency-artifact")]
        dependency_artifacts: Vec<PathBuf>,
    },
}

/// A PDR whose published builds may stand in for compiling dependencies.
#[derive(ClapArgs, Debug)]
struct Published {
    /// Take dependencies from this PDR root's published builds instead of compiling them
    #[arg(long, requires = "pdr_public_key")]
    pdr: Option<String>,
    /// Key the PDR root and every record taken from it must verify against
    #[arg(long, requires = "pdr")]
    pdr_public_key: Option<String>,
    /// Read this exact root instead of the current one, as a builder reading its planner's root
    #[arg(long, requires = "pdr")]
    pdr_root: Option<String>,
}

impl Published {
    fn resolver(
        &self,
    ) -> Result<Option<rootbeer_packaging::repository::RepositoryResolver>, String> {
        let (Some(url), Some(public_key)) = (&self.pdr, &self.pdr_public_key) else {
            return Ok(None);
        };
        let repository = rootbeer_packaging::repository::Repository {
            url: url.clone(),
            public_key: public_key.clone(),
        };
        repository.validate()?;
        let pin = match &self.pdr_root {
            Some(root) if rootbeer_packaging::is_sha256(root) => {
                rootbeer_packaging::repository::RepositoryPin {
                    url: url.clone(),
                    public_key: public_key.clone(),
                    root: root.clone(),
                }
            }
            Some(_) => return Err("--pdr-root must be a root digest".into()),
            None => {
                repository
                    .select(&rootbeer_packaging::state_dir(), true)?
                    .pin
            }
        };
        Ok(Some(
            rootbeer_packaging::repository::RepositoryResolver::new(&pin),
        ))
    }
}

pub fn run(args: Args) {
    if let Err(error) = execute(args) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn execute(args: Args) -> Result<(), String> {
    let config = crate::config::Config::load()?;
    let catalog_directory = args.catalog.clone().or_else(|| config.catalog.clone());
    let definitions = catalog_directory
        .as_deref()
        .map(PackageDefinition::from_directory)
        .transpose()?;
    let local_catalog = definitions
        .as_ref()
        .map(PackageCatalog::from_definitions)
        .transpose()?;
    let catalog = || {
        local_catalog.as_ref().ok_or_else(|| {
            "this command requires --catalog, or `catalog` in forge.toml, naming a PDR recipe directory".to_string()
        })
    };
    let mut output = io::stdout().lock();
    match args.command {
        Command::Plan {
            packages,
            output: destination,
            records,
            context,
        } => {
            let plan = rootbeer_packaging::work::plan_work(
                catalog()?,
                &packages,
                &context.unwrap_or_else(crate::config::detect_context),
                &config.distribution()?,
                &mut |_| Ok(None),
                records.as_deref(),
            )?;
            std::fs::write(
                &destination,
                serde_json::to_vec_pretty(&plan).map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
            write!(output, "{}", crate::ci::describe(&plan)).map_err(|error| error.to_string())?;
        }
        Command::Build {
            plan,
            task,
            output: destination,
            dependencies,
            cache,
            context,
        } => {
            let plan = rootbeer_packaging::work::WorkPlan::read(&plan)?;
            let cache = cache.unwrap_or_else(|| std::env::temp_dir().join("rootbeer-forge-cache"));
            let outcome = rootbeer_packaging::work::build_task(
                catalog()?,
                &plan,
                &task,
                &destination,
                &dependencies,
                &cache,
                &context.unwrap_or_else(crate::config::detect_context),
            )
            .inspect_err(|_| show_log_tail(&destination))?;
            let summary = serde_json::to_vec_pretty(&outcome).map_err(|error| error.to_string())?;
            std::fs::write(destination.join("build.json"), &summary)
                .map_err(|error| error.to_string())?;
            match outcome {
                rootbeer_packaging::work::Outcome::Built { key } => {
                    writeln!(output, "built {task} as inputs-{key}")
                }
                rootbeer_packaging::work::Outcome::Reused { record, .. } => {
                    writeln!(
                        output,
                        "reused {task}, published after planning as {record}"
                    )
                }
            }
            .map_err(|error| error.to_string())?;
        }
        Command::Ci { command } => crate::ci::run(command, &config, local_catalog.as_ref())?,
        Command::PublishRecords {
            mut record,
            records,
            site,
            key,
            public_key,
        } => {
            if let Some(directory) = records {
                for entry in std::fs::read_dir(directory).map_err(|error| error.to_string())? {
                    let path = entry.map_err(|error| error.to_string())?.path();
                    if path.extension().is_some_and(|value| value == "json") {
                        record.push(path.to_string_lossy().into_owned());
                    }
                }
            }
            let key = std::fs::read(key).map_err(|error| error.to_string())?;
            let count =
                rootbeer_packaging::publish_records(catalog()?, &record, &site, &key, &public_key)?;
            writeln!(output, "published {count} package platforms")
                .map_err(|error| error.to_string())?;
        }
        Command::Release {
            receipt,
            registry,
            output: destination,
            key,
            public_key,
            input_key,
            published,
        } => {
            let published = match published {
                Some(published) => published,
                None => std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|error| error.to_string())?
                    .as_secs(),
            };
            let key_der = std::fs::read(key).map_err(|error| error.to_string())?;
            let reference = rootbeer_packaging::release_package(
                catalog()?,
                &receipt,
                &registry,
                &destination,
                &rootbeer_packaging::Signer {
                    key_der: &key_der,
                    public_key: &public_key,
                    published,
                },
                input_key.as_deref(),
                &Default::default(),
            )?;
            writeln!(
                output,
                "prepared package release\nrecord: {}\nreference: {reference}",
                destination.join("package.json").display(),
            )
            .map_err(|error| error.to_string())?;
        }
        Command::Push {
            release,
            registry,
            public_key,
        } => {
            let reference = rootbeer_packaging::push_package(&release, &registry, &public_key)?;
            writeln!(output, "published {reference}").map_err(|error| error.to_string())?;
        }
        Command::Updates { cache, output } => {
            let definitions = definitions
                .as_ref()
                .ok_or("updates requires --catalog pointing to package definitions")?;
            let report = rootbeer_packaging::discover_updates(definitions, &cache, &output)?;
            eprintln!(
                "{} updates, {} unchanged, {} errors; report: {}",
                report.updated.len(),
                report.unchanged.len(),
                report.errors.len(),
                output.join("summary.md").display()
            );
            if !report.errors.is_empty() {
                return Err("some upstreams failed; see the discovery report".into());
            }
        }
        Command::List => {
            let system = rootbeer_packaging::ResolveContext::current().system;
            for package in catalog()?.packages.values() {
                let Some(version) = package.default_version_for(&system) else {
                    continue;
                };
                writeln!(
                    output,
                    "{}\t{}\t{}\t{}",
                    package.name,
                    version,
                    match package.versions[version].for_system(&system) {
                        Some(recipe) if recipe.build.is_some() => "source",
                        _ => "binary",
                    },
                    package.description
                )
                .map_err(|e| e.to_string())?;
            }
        }
        Command::Show { name } => {
            let package = catalog()?
                .find(&name)
                .ok_or_else(|| format!("unknown catalog package `{name}`"))?;
            writeln!(
                output,
                "{} — {}\n{}\naliases: {}",
                package.name,
                package.description,
                package.homepage,
                package.aliases.join(", ")
            )
            .map_err(|e| e.to_string())?;
            for (system, version) in &package.default_versions {
                writeln!(output, "default for {system}: {version}").map_err(|e| e.to_string())?;
            }
            for (version, entry) in &package.versions {
                writeln!(output, "\n{version} (revision {})", entry.revision)
                    .map_err(|e| e.to_string())?;
                for (system, recipe) in &entry.platforms {
                    writeln!(
                        output,
                        "  {system}: {}\n    commands: {}",
                        recipe
                            .source
                            .as_deref()
                            .unwrap_or("source build (binary not published)"),
                        recipe
                            .bins
                            .names()
                            .into_iter()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
        }
        Command::Check => {
            let catalog = catalog()?;
            catalog.validate()?;
            writeln!(
                output,
                "{} packages; sha256:{}",
                catalog.packages.len(),
                catalog.sha256()
            )
            .map_err(|e| e.to_string())?;
        }
        Command::Format { check } => {
            let directory = catalog_directory
                .as_deref()
                .ok_or("format requires --catalog pointing to a PDR recipe directory")?;
            let definitions = definitions
                .as_ref()
                .ok_or("format requires --catalog pointing to a PDR recipe directory")?;
            let mut differing = Vec::new();
            for (name, definition) in definitions {
                let path = directory.join(format!("{name}.lua"));
                let rendered = definition.to_lua()?;
                if std::fs::read_to_string(&path).map_err(|e| e.to_string())? == rendered {
                    continue;
                }
                if !check {
                    std::fs::write(&path, rendered).map_err(|e| e.to_string())?;
                }
                differing.push(name.as_str());
            }
            if check && !differing.is_empty() {
                return Err(format!(
                    "{} recipes are not formatted; run rootbeer-forge format: {}",
                    differing.len(),
                    differing.join(", ")
                ));
            }
            writeln!(output, "{} recipes reformatted", differing.len())
                .map_err(|e| e.to_string())?;
        }
        Command::Catalog => {
            writeln!(output, "{}", catalog()?.to_json()?).map_err(|e| e.to_string())?
        }
        Command::PinEnvironment { specification } => {
            let specification: rootbeer_packaging::BuildEnvironment = serde_json::from_slice(
                &std::fs::read(specification).map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
            writeln!(
                output,
                "{}",
                serde_json::to_string_pretty(&specification.pin()?)
                    .map_err(|error| error.to_string())?
            )
            .map_err(|error| error.to_string())?;
        }
        Command::EngineGeneration => {
            writeln!(
                output,
                "{}\n{}",
                rootbeer_packaging::engine_generation(),
                rootbeer_packaging::Generation::current().record()
            )
            .map_err(|error| error.to_string())?;
        }
        Command::Audit { directory, receipt } => {
            let report = if let Some(receipt) = receipt {
                let artifact: rootbeer_packaging::BuildArtifact =
                    serde_json::from_slice(&std::fs::read(receipt).map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?;
                rootbeer_packaging::audit::audit_installed(&directory, &artifact.package)?
            } else {
                rootbeer_packaging::audit::audit(&directory)?
            };
            writeln!(
                output,
                "{}",
                serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
            )
            .map_err(|error| error.to_string())?;
            report.validate()?;
        }
        Command::Graph { name } => {
            catalog()?.validate()?;
            let graph = rootbeer_packaging::graph::DependencyGraph::new(
                catalog()?,
                &[name],
                &rootbeer_packaging::ResolveContext::current().system,
            )?;
            writeln!(
                output,
                "{}",
                serde_json::to_string_pretty(&graph).map_err(|error| error.to_string())?
            )
            .map_err(|error| error.to_string())?;
        }
        Command::Prepare {
            name,
            environment,
            isolate,
            output: destination,
            jobs,
            cache,
            cache_context,
            recheck,
            phase_timeout,
            published,
            dependency_artifacts,
        } => {
            let pdr = published.resolver()?;
            let environment = read_environment(environment)?;
            let cache = cache.map(|directory| rootbeer_packaging::BuildCache {
                directory,
                context: cache_context.unwrap(),
                recheck,
            });
            let artifact = rootbeer_packaging::prepare_package(
                catalog()?,
                &name,
                &destination,
                &rootbeer_packaging::BuildOptions {
                    environment,
                    is_isolated: isolate,
                    jobs,
                    cache,
                    phase_timeout: std::time::Duration::from_secs(phase_timeout),
                    ..Default::default()
                },
                pdr.as_ref(),
                &dependency_artifacts,
            )
            .inspect_err(|_| show_log_tail(&destination))?;
            let location = match &artifact.source {
                rootbeer_packaging::LockedSource::Url { url, .. } => url.clone(),
                rootbeer_packaging::LockedSource::File { path, .. }
                | rootbeer_packaging::LockedSource::Path { path, .. } => path.display().to_string(),
            };
            writeln!(
                output,
                "prepared {} for {}\nartifact: {location}",
                artifact.id(),
                rootbeer_packaging::ResolveContext::current().system,
            )
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn read_environment(
    path: Option<PathBuf>,
) -> Result<Option<rootbeer_packaging::BuildEnvironmentLock>, String> {
    path.map(|path| {
        serde_json::from_slice(&std::fs::read(path).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())
    })
    .transpose()
}

/// Prints the end of a failed build's log, which otherwise stays in its output directory.
fn show_log_tail(output: &std::path::Path) {
    const LINES: usize = 80;
    let Ok(log) = std::fs::read_to_string(output.join("build.log")) else {
        return;
    };
    let lines: Vec<_> = log.lines().collect();
    eprintln!("last {} lines of build.log:", LINES.min(lines.len()));
    for line in &lines[lines.len().saturating_sub(LINES)..] {
        eprintln!("  {line}");
    }
}
