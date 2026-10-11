use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};
use std::{fs, io};

use crate::executor::{self, ApplyOptions, ExecutionHandler, ExecutionReport};
use crate::package::lockfile::{LockError, RootbeerLock};
use crate::package::{
    PackageCatalog, PackageIntent, PackageLockBuilder, PackageResolverInputs, Repository,
    ResolverInput,
};
use crate::plan::Op;
use crate::{Error, Runtime};

#[derive(Debug, Default, Clone, Copy)]
pub enum Mode {
    #[default]
    Apply,
    DryRun,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PackageLockOptions {
    pub should_update: bool,
    pub is_locked: bool,
    pub is_offline: bool,
}

impl Display for Mode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Mode::Apply => write!(f, "apply"),
            Mode::DryRun => write!(f, "dry run"),
        }
    }
}

#[derive(Debug)]
pub struct Options {
    pub script_dir: PathBuf,
    pub script_name: String,
    pub lua_dir: PathBuf,
    pub profile: Option<String>,
    pub mode: Mode,
    pub force: bool,
    pub package_lock: PackageLockOptions,
}

impl Options {
    pub fn from_script(script: &Path) -> io::Result<Self> {
        let script = script.canonicalize()?;
        let script_dir = script
            .parent()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "script has no parent directory",
                )
            })?
            .to_path_buf();

        let script_name = script
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "script has no file name"))?
            .to_string_lossy()
            .to_string();

        Ok(Self {
            lua_dir: PathBuf::from(env!("ROOTBEER_LUA_DIR")),
            script_dir,
            script_name,
            profile: None,
            mode: Mode::default(),
            force: false,
            package_lock: PackageLockOptions::default(),
        })
    }
}

/// Entry point: configure the pipeline, then call `.plan()` to evaluate Lua.
pub struct Pipeline {
    opts: Options,
    package_resolver: fn(&PackageResolverInputs) -> crate::package::ResolverStack,
}

impl Pipeline {
    pub fn new(opts: Options) -> Self {
        Self {
            opts,
            package_resolver: crate::package::resolver_stack_for_inputs,
        }
    }

    /// Supplies the consumer resolver without coupling configuration to a build engine.
    pub fn with_package_resolver(
        mut self,
        resolver: fn(&PackageResolverInputs) -> crate::package::ResolverStack,
    ) -> Self {
        self.package_resolver = resolver;
        self
    }

    pub fn mode(&self) -> Mode {
        self.opts.mode
    }

    pub fn force(&self) -> bool {
        self.opts.force
    }

    /// Evaluate the Lua script and advance to the planned phase.
    pub fn plan(self) -> Result<PlannedPipeline, Error> {
        if self.opts.package_lock.should_update
            && (self.opts.package_lock.is_locked || self.opts.package_lock.is_offline)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--update cannot be combined with --locked or --offline",
            )
            .into());
        }
        let tools = std::sync::Arc::new(crate::tools::ToolRuntime::new(
            self.opts.package_lock.is_offline,
        ));
        let runtime = Runtime {
            tools: tools.clone(),
            script_dir: self.opts.script_dir.clone(),
            script_name: self.opts.script_name.clone(),
            lua_dir: self.opts.lua_dir.clone(),
            profile: self.opts.profile.clone(),
        };

        let source = fs::read_to_string(runtime.script_dir.join(&runtime.script_name))?;
        let chunk_name = format!(
            "@{}",
            runtime.script_dir.join(&runtime.script_name).display()
        );
        let vm = crate::lua::Vm::new(runtime)?;
        if let Err(e) = vm.exec(&source, &chunk_name) {
            if let Some(pe) = crate::profile::extract(&e) {
                return Err(pe.into());
            }
            return Err(e.into());
        }

        let package_repository = vm
            .lua
            .app_data_ref::<Repository>()
            .map(|repository| repository.clone());
        let local_catalog = vm
            .lua
            .app_data_ref::<PackageCatalog>()
            .map(|catalog| catalog.clone());
        let ops = vm.drain_ops();

        Ok(PlannedPipeline {
            tools,
            package_resolver: self.package_resolver,
            opts: self.opts,
            package_repository,
            local_catalog,
            ops,
            is_packages_taken: false,
        })
    }
}

/// A pipeline that has been planned — ops are collected, ready to execute.
pub struct PlannedPipeline {
    tools: std::sync::Arc<crate::tools::ToolRuntime>,
    package_resolver: fn(&PackageResolverInputs) -> crate::package::ResolverStack,
    /// Set by `rb.package_repository`; otherwise the official repository is used.
    package_repository: Option<Repository>,
    local_catalog: Option<PackageCatalog>,
    opts: Options,
    ops: Vec<Op>,
    is_packages_taken: bool,
}

impl PlannedPipeline {
    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    /// Whether the script called `rb.package_catalog` or
    /// `rb.package_repository`.
    pub fn has_package_sources(&self) -> bool {
        self.package_repository.is_some() || self.local_catalog.is_some()
    }

    /// Removes the package ops from the plan. Execution then leaves
    /// `rootbeer.lock` and the legacy store to the caller.
    pub fn take_packages(&mut self) -> Vec<PackageIntent> {
        self.is_packages_taken = true;
        let (packages, ops) = std::mem::take(&mut self.ops)
            .into_iter()
            .partition::<Vec<_>, _>(|op| matches!(op, Op::Package { .. }));

        self.ops = ops;
        packages
            .into_iter()
            .filter_map(|op| match op {
                Op::Package { intent } => Some(intent),
                _ => None,
            })
            .collect()
    }

    pub fn mode(&self) -> Mode {
        self.opts.mode
    }

    pub fn force(&self) -> bool {
        self.opts.force
    }

    /// Execute the planned operations, reporting progress to the handler.
    pub fn execute(self, handler: &mut impl ExecutionHandler) -> Result<ExecutionReport, Error> {
        let report = match self.opts.mode {
            Mode::Apply => {
                let ops = self.locked_ops_for_apply(&mut |notice| handler.on_output(notice))?;
                executor::apply_with_options(
                    &self.tools,
                    &ops,
                    self.opts.force,
                    handler,
                    ApplyOptions {
                        package_offline: self.opts.package_lock.is_offline,
                        is_packages_taken: self.is_packages_taken,
                    },
                )?
            }
            Mode::DryRun => executor::dry_run(&self.ops, handler),
        };

        Ok(report)
    }

    fn locked_ops_for_apply(&self, notice: &mut dyn FnMut(&str)) -> Result<Vec<Op>, Error> {
        let path = self.opts.script_dir.join("rootbeer.lock");
        if self.is_packages_taken {
            return Ok(self.ops.clone());
        }

        if !crate::package::lockfile::has_package_ops(&self.ops) && !path.exists() {
            return Ok(self.ops.clone());
        }
        let mut is_salvaged = false;
        let existing = if path.exists() {
            let lock;
            (lock, is_salvaged) = RootbeerLock::read_compatible(&path)?;
            if is_salvaged && self.opts.package_lock.is_locked {
                return Err(io::Error::other(
                    "rootbeer.lock is from an older rootbeer; run without --locked to update it",
                )
                .into());
            }
            if is_salvaged {
                notice("updating rootbeer.lock from an older rootbeer");
            }
            Some(lock)
        } else {
            None
        };

        if self.opts.package_lock.is_locked {
            let Some(lock) = existing.as_ref() else {
                return Err(LockError::MissingLockfile { path }.into());
            };

            if !self.lock_matches_plan(lock)? {
                return Err(LockError::StaleLockfile { path }.into());
            }

            return crate::package::lockfile::apply_to_ops(lock, &self.ops).map_err(Into::into);
        }

        if !self.opts.package_lock.should_update && !is_salvaged {
            if let Some(lock) = existing.as_ref() {
                if self.lock_matches_plan(lock)? {
                    match crate::package::lockfile::apply_to_ops(lock, &self.ops) {
                        Ok(ops) => return Ok(ops),
                        Err(
                            LockError::MissingPackage { .. } | LockError::PackageChanged { .. },
                        ) => {}
                        Err(err) => return Err(err.into()),
                    }
                }
            }
        }

        let should_refresh = self.opts.package_lock.should_update;
        let mut inputs = if should_refresh {
            PackageResolverInputs::default()
        } else {
            existing
                .as_ref()
                .map(|lock| lock.inputs.clone())
                .unwrap_or_default()
        };
        inputs.resolvers.remove("local");
        if let Some(catalog) = &self.local_catalog {
            inputs.resolvers.insert(
                "local".into(),
                ResolverInput::LocalCatalog(Box::new(catalog.clone())),
            );
        }
        if self.has_canonical_requests() {
            let repository =
                Repository::chosen(self.package_repository.as_ref()).map_err(io::Error::other)?;
            let is_locked = inputs
                .repository()
                .is_some_and(|pin| pin.repository() == repository);
            if should_refresh || !is_locked {
                let state = crate::state_dir();
                let selection = if self.opts.package_lock.is_offline {
                    repository.select_offline(&state)
                } else {
                    repository.select(&state, should_refresh)
                }
                .map_err(io::Error::other)?;
                if let Some(message) = &selection.notice {
                    notice(message);
                }
                inputs
                    .resolvers
                    .insert("rootbeer".into(), ResolverInput::Repository(selection.pin));
            }
        } else {
            inputs.resolvers.remove("rootbeer");
        }
        let builder = PackageLockBuilder::new_with_inputs(
            (self.package_resolver)(&inputs),
            if self.opts.package_lock.is_offline {
                crate::package::PackageRealizer::offline(crate::store::Store::default())
            } else {
                crate::package::PackageRealizer::default()
            },
            crate::package::ResolveContext::current(),
            inputs,
        );
        let input = builder.lock_input_from_ops(&self.ops);
        let lock = if should_refresh {
            builder.build_with_previous(&input, existing.as_ref())?
        } else {
            builder.reconcile(&input, existing.as_ref(), self.opts.package_lock.is_offline)?
        };
        lock.write(&path)?;
        Ok(crate::package::lockfile::apply_to_ops(&lock, &self.ops)?)
    }

    fn lock_matches_plan(&self, lock: &RootbeerLock) -> Result<bool, Error> {
        if lock.inputs.configured_repository() != self.package_repository
            || lock.inputs.local_catalog() != self.local_catalog.as_ref()
        {
            return Ok(false);
        }
        let Some(expected) = lock.input_fingerprint.as_deref() else {
            let package_count = self
                .ops
                .iter()
                .filter(|op| matches!(op, Op::Package { .. } | Op::RealizePackage { .. }))
                .count();
            return Ok(package_count == lock.packages.len());
        };

        let builder = PackageLockBuilder::current_system_with_inputs(lock.inputs.clone());
        let input = builder.lock_input_from_ops(&self.ops);
        let actual = builder.fingerprint_input(&input)?;
        Ok(actual == expected)
    }

    fn has_canonical_requests(&self) -> bool {
        self.ops.iter().any(|op| {
            matches!(op,
                Op::Package { intent: PackageIntent::Request(request) }
                    if request.resolver.as_deref().is_none_or(|name| name == "rootbeer")
                        && self.local_catalog.as_ref().is_none_or(|catalog| {
                            catalog.find(&request.name).is_none() || catalog.requires_pdr()
                        })
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::executor::OpResult;
    use crate::package::{LockedInstall, LockedPackage, LockedSource, PackageIntent, Provides};

    use super::*;

    #[derive(Default)]
    struct Recorder;

    impl ExecutionHandler for Recorder {
        fn on_start(&mut self, _op: &Op) {}
        fn on_output(&mut self, _line: &str) {}
        fn on_result(&mut self, _result: &OpResult) {}
    }

    fn opts(script_dir: PathBuf, mode: Mode) -> Options {
        Options {
            script_dir,
            script_name: "init.lua".to_string(),
            lua_dir: PathBuf::from("lua"),
            profile: None,
            mode,
            force: false,
            package_lock: PackageLockOptions::default(),
        }
    }

    fn package() -> LockedPackage {
        LockedPackage {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            source: LockedSource::Path {
                path: PathBuf::from("demo"),
                sha256: "source".to_string(),
            },
            install: LockedInstall::Directory { strip_prefix: None },
            provides: Provides {
                apps: Default::default(),
                bins: BTreeMap::new(),
            },
            runtime_dependencies: Default::default(),
            output_sha256: None,
        }
    }

    fn package_with_source_path(path: PathBuf) -> LockedPackage {
        let mut package = package();
        package.source = LockedSource::Path {
            path,
            sha256: "source".to_string(),
        };
        package
    }

    #[test]
    fn apply_attempts_to_build_lock_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let planned = PlannedPipeline {
            tools: Default::default(),
            package_resolver: crate::package::resolver_stack_for_inputs,
            package_repository: None,
            local_catalog: None,
            opts: opts(tmp.path().to_path_buf(), Mode::Apply),
            ops: vec![Op::Package {
                intent: PackageIntent::Locked(package()),
            }],
            is_packages_taken: false,
        };

        let err = planned.execute(&mut Recorder).unwrap_err();

        assert!(matches!(err, Error::LockBuild(_)));
    }

    #[test]
    fn apply_uses_locked_package_facts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut locked = package();
        locked.output_sha256 = Some("output".to_string());
        RootbeerLock::from_packages([locked])
            .unwrap()
            .write(tmp.path().join("rootbeer.lock"))
            .unwrap();
        let planned = PlannedPipeline {
            tools: Default::default(),
            package_resolver: crate::package::resolver_stack_for_inputs,
            package_repository: None,
            local_catalog: None,
            opts: opts(tmp.path().to_path_buf(), Mode::Apply),
            ops: vec![Op::Package {
                intent: PackageIntent::Locked(package()),
            }],
            is_packages_taken: false,
        };

        let ops = planned.locked_ops_for_apply(&mut |_| {}).unwrap();

        let [Op::RealizePackage { package }] = ops.as_slice() else {
            panic!("expected package op");
        };
        assert_eq!(package.output_sha256.as_deref(), Some("output"));
    }

    #[test]
    fn apply_rebuilds_lock_when_input_fingerprint_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let planned_package = package_with_source_path(tmp.path().join("missing-source"));
        let mut locked = planned_package.clone();
        locked.output_sha256 = Some("output".to_string());
        RootbeerLock::from_packages([locked])
            .unwrap()
            .with_input_fingerprint("stale")
            .write(tmp.path().join("rootbeer.lock"))
            .unwrap();
        let planned = PlannedPipeline {
            tools: Default::default(),
            package_resolver: crate::package::resolver_stack_for_inputs,
            package_repository: None,
            local_catalog: None,
            opts: opts(tmp.path().to_path_buf(), Mode::Apply),
            ops: vec![Op::Package {
                intent: PackageIntent::Locked(planned_package),
            }],
            is_packages_taken: false,
        };

        let err = planned.locked_ops_for_apply(&mut |_| {}).unwrap_err();

        assert!(matches!(err, Error::LockBuild(_)));
    }

    #[test]
    fn locked_mode_requires_existing_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let mut opts = opts(tmp.path().to_path_buf(), Mode::Apply);
        opts.package_lock = PackageLockOptions {
            is_locked: true,
            ..Default::default()
        };
        let planned = PlannedPipeline {
            tools: Default::default(),
            package_resolver: crate::package::resolver_stack_for_inputs,
            package_repository: None,
            local_catalog: None,
            opts,
            ops: vec![Op::Package {
                intent: PackageIntent::Locked(package()),
            }],
            is_packages_taken: false,
        };

        let err = planned.locked_ops_for_apply(&mut |_| {}).unwrap_err();

        assert!(matches!(
            err,
            Error::Lock(LockError::MissingLockfile { .. })
        ));
    }

    #[test]
    fn locked_mode_rejects_stale_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let mut locked = package();
        locked.output_sha256 = Some("output".to_string());
        RootbeerLock::from_packages([locked])
            .unwrap()
            .with_input_fingerprint("stale")
            .write(tmp.path().join("rootbeer.lock"))
            .unwrap();
        let mut opts = opts(tmp.path().to_path_buf(), Mode::Apply);
        opts.package_lock = PackageLockOptions {
            is_locked: true,
            ..Default::default()
        };
        let planned = PlannedPipeline {
            tools: Default::default(),
            package_resolver: crate::package::resolver_stack_for_inputs,
            package_repository: None,
            local_catalog: None,
            opts,
            ops: vec![Op::Package {
                intent: PackageIntent::Locked(package()),
            }],
            is_packages_taken: false,
        };

        let err = planned.locked_ops_for_apply(&mut |_| {}).unwrap_err();

        assert!(matches!(err, Error::Lock(LockError::StaleLockfile { .. })));
    }

    #[test]
    fn taken_packages_leave_rootbeer_lock_unread() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("rootbeer.lock"), r#"{"schema": 4}"#).unwrap();
        let mut planned = plan_script(
            root.path(),
            r#"local rb = require("rootbeer")
rb.package("jq")
rb.package("fd@10.2.0")"#,
        );

        let taken = planned.take_packages();
        let names = taken
            .iter()
            .map(|intent| match intent {
                PackageIntent::Request(request) => request.name.as_str(),
                PackageIntent::Locked(package) => package.name.as_str(),
            })
            .collect::<Vec<_>>();
        assert_eq!(names, ["jq", "fd"]);
        assert!(planned.ops().is_empty());
        assert!(planned
            .locked_ops_for_apply(&mut |_| {})
            .unwrap()
            .is_empty());
    }

    fn plan_script(directory: &Path, source: &str) -> PlannedPipeline {
        let script = directory.join("init.lua");
        fs::write(&script, source).unwrap();
        let mut options = Options::from_script(&script).unwrap();
        options.package_lock = PackageLockOptions {
            is_locked: true,
            is_offline: true,
            ..Default::default()
        };
        Pipeline::new(options).plan().unwrap()
    }

    #[test]
    fn local_recipe_changes_invalidate_locked_plans_without_selecting_the_registry() {
        let root = tempfile::tempdir().unwrap();
        let recipes = root.path().join("packages");
        fs::create_dir(&recipes).unwrap();
        let recipe = format!(
            r#"return {{
            name = "demo", description = "Local demo",
            homepage = "https://example.invalid/demo", default_license = "MIT",
            prebuilt = {{ github = "owner/demo", tag = "v{{version}}", asset = "demo.tar.gz" }},
            outputs = {{ bins = {{ "demo" }}, checks = {{ {{ "demo", "--version" }} }} }},
            platforms = {{ ["{system}"] = {{ default_version = "1" }} }},
            versions = {{ ["1"] = {{ revision = 1, digests = {{ ["{system}"] = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" }} }} }},
        }}"#,
            system = crate::package::ResolveContext::current().system
        );
        fs::write(recipes.join("demo.lua"), &recipe).unwrap();
        let script =
            "local rb = require('rootbeer'); rb.package_catalog('packages'); rb.package('demo')";
        let planned = plan_script(root.path(), script);
        assert!(!planned.has_canonical_requests());
        let mut inputs = PackageResolverInputs::default();
        inputs.resolvers.insert(
            "local".into(),
            ResolverInput::LocalCatalog(Box::new(planned.local_catalog.clone().unwrap())),
        );
        let builder = PackageLockBuilder::current_system_with_inputs(inputs.clone());
        let fingerprint = builder
            .fingerprint_input(&builder.lock_input_from_ops(&planned.ops))
            .unwrap();
        let lock = RootbeerLock::from_packages([])
            .unwrap()
            .with_resolver_inputs(inputs)
            .with_input_fingerprint(fingerprint);
        assert!(planned.lock_matches_plan(&lock).unwrap());
        lock.write(root.path().join("rootbeer.lock")).unwrap();
        fs::write(
            recipes.join("demo.lua"),
            recipe.replace("revision = 1", "revision = 2"),
        )
        .unwrap();
        let changed = plan_script(root.path(), script);
        assert!(!changed.lock_matches_plan(&lock).unwrap());
        assert!(matches!(
            changed.locked_ops_for_apply(&mut |_| {}).unwrap_err(),
            Error::Lock(LockError::StaleLockfile { .. })
        ));
        let removed = plan_script(
            root.path(),
            "local rb = require('rootbeer'); rb.package('demo')",
        );
        assert!(removed.has_canonical_requests());
        assert!(!removed.lock_matches_plan(&lock).unwrap());
        let mixed = plan_script(
            root.path(),
            &(script.to_string() + "; rb.package('registry-tool')"),
        );
        assert!(mixed.has_canonical_requests());
    }

    fn repository_script(public_key: &str) -> String {
        format!(
            "local rb = require('rootbeer')\nrb.package_repository({{url='https://unavailable.invalid/current.json', public_key='{public_key}'}})\nrb.package('new-tool')"
        )
    }

    fn locked_repository(planned: &PlannedPipeline) -> PackageResolverInputs {
        let repository = planned.package_repository.clone().unwrap();
        let mut inputs = PackageResolverInputs::default();
        inputs.resolvers.insert(
            "rootbeer".into(),
            ResolverInput::Repository(crate::package::RepositoryPin {
                url: repository.url,
                public_key: repository.public_key,
                root: "c".repeat(64),
            }),
        );
        inputs
    }

    #[test]
    fn repository_planning_is_offline_and_a_new_repository_invalidates_locks() {
        let root = tempfile::tempdir().unwrap();
        let source = repository_script(&"a".repeat(64));
        let planned = plan_script(root.path(), &source);
        assert!(planned.has_canonical_requests());
        let inputs = locked_repository(&planned);
        let builder = PackageLockBuilder::current_system_with_inputs(inputs.clone());
        let fingerprint = builder
            .fingerprint_input(&builder.lock_input_from_ops(&planned.ops))
            .unwrap();
        let mut lock = RootbeerLock::from_packages(std::iter::empty::<LockedPackage>())
            .unwrap()
            .with_input_fingerprint(fingerprint);
        lock.inputs = inputs;
        assert!(planned.lock_matches_plan(&lock).unwrap());
        lock.write(root.path().join("rootbeer.lock")).unwrap();
        let changed = plan_script(root.path(), &repository_script(&"b".repeat(64)));
        assert!(!changed.lock_matches_plan(&lock).unwrap());
        assert!(matches!(
            changed.locked_ops_for_apply(&mut |_| {}).unwrap_err(),
            Error::Lock(LockError::StaleLockfile { .. })
        ));
        let removed = plan_script(
            root.path(),
            "local rb = require('rootbeer'); rb.package('new-tool')",
        );
        assert!(!removed.lock_matches_plan(&lock).unwrap());
        let mixed = plan_script(
            root.path(),
            &(source + "\nrb.package('github:owner/repo@1')"),
        );
        assert!(mixed.has_canonical_requests());
    }

    #[test]
    fn repository_declarations_require_a_key_and_precede_packages() {
        let root = tempfile::tempdir().unwrap();
        let declaration = format!(
            "rb.package_repository({{url='https://example.org/current.json', public_key='{}'}})",
            "a".repeat(64)
        );
        for source in [
            format!("{declaration}; {declaration}"),
            format!("rb.package('age'); {declaration}"),
            declaration.replace(&"a".repeat(64), "invalid"),
            declaration.replace("https://", "http://"),
            declaration.replace("current.json", "packages"),
            "rb.package_repository({url='https://example.org/current.json'})".into(),
        ] {
            let script = root.path().join("init.lua");
            fs::write(&script, format!("local rb = require('rootbeer'); {source}")).unwrap();
            assert!(
                Pipeline::new(Options::from_script(&script).unwrap())
                    .plan()
                    .is_err(),
                "{source}"
            );
        }
    }

    #[test]
    fn a_matching_lock_replays_without_fetching_its_repository() {
        use crate::package::lockfile::PackageLockEntry;
        use crate::package::{
            PackageRecordProof, PackageRequest, PackageResolution, ResolutionProof, ResolveContext,
        };
        let root = tempfile::tempdir().unwrap();
        let mut planned = plan_script(
            root.path(),
            &repository_script(&"a".repeat(64)).replace("new-tool", "demo"),
        );
        let context = ResolveContext::current();
        let entry = PackageLockEntry::resolved(
            &PackageRequest::parse("demo"),
            &context,
            PackageResolution::new(
                package(),
                ResolutionProof::PackageRecord(PackageRecordProof {
                    record: "b".repeat(64),
                    public_key: "a".repeat(64),
                    system: context.system.clone(),
                }),
            ),
        )
        .unwrap();
        let mut lock = RootbeerLock::from_package_entries([entry]).unwrap();
        lock.inputs = locked_repository(&planned);
        let builder = PackageLockBuilder::current_system_with_inputs(lock.inputs.clone());
        lock.input_fingerprint = Some(
            builder
                .fingerprint_input(&builder.lock_input_from_ops(&planned.ops))
                .unwrap(),
        );
        lock.write(root.path().join("rootbeer.lock")).unwrap();
        for mode in [
            PackageLockOptions::default(),
            PackageLockOptions {
                is_locked: true,
                ..Default::default()
            },
            PackageLockOptions {
                is_offline: true,
                ..Default::default()
            },
        ] {
            planned.opts.package_lock = mode;
            let ops = planned
                .locked_ops_for_apply(&mut |_| {
                    panic!("matching lock attempted repository selection")
                })
                .unwrap();
            assert!(matches!(ops.as_slice(), [Op::RealizePackage { .. }]));
        }
    }
    #[test]
    fn offline_reconciles_removal_but_locked_offline_rejects_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("rootbeer.lock");
        RootbeerLock::from_packages([package()])
            .unwrap()
            .with_input_fingerprint("stale")
            .write(&path)
            .unwrap();
        let original = fs::read(&path).unwrap();
        let mut options = opts(root.path().into(), Mode::Apply);
        options.package_lock = PackageLockOptions {
            is_offline: true,
            is_locked: true,
            should_update: false,
        };
        let mut planned = PlannedPipeline {
            tools: Default::default(),
            package_resolver: crate::package::resolver_stack_for_inputs,
            package_repository: None,
            local_catalog: None,
            opts: options,
            ops: vec![],
            is_packages_taken: false,
        };
        assert!(planned.locked_ops_for_apply(&mut |_| {}).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        planned.opts.package_lock.is_locked = false;
        assert!(planned
            .locked_ops_for_apply(&mut |_| {})
            .unwrap()
            .is_empty());
        assert!(RootbeerLock::read(&path).unwrap().packages.is_empty());
    }

    #[test]
    fn lock_entries_from_another_rootbeer_resolve_again_unless_locked() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("rootbeer.lock");
        let lock = r#"{
            "schema": 2,
            "inputs": { "resolvers": { "aqua": { "type": "aqua_registry" } } },
            "resolutions": { "abc": { "package": "aqua:tool@1", "proof": { "type": "gone" } } },
            "packages": {}
        }"#;
        for is_locked in [false, true] {
            fs::write(&path, lock).unwrap();
            let mut options = opts(root.path().into(), Mode::Apply);
            options.package_lock.is_locked = is_locked;
            let planned = PlannedPipeline {
                tools: Default::default(),
                package_resolver: crate::package::resolver_stack_for_inputs,
                package_repository: None,
                local_catalog: None,
                opts: options,
                ops: vec![],
                is_packages_taken: false,
            };
            let mut notices = Vec::new();
            let result =
                planned.locked_ops_for_apply(&mut |notice| notices.push(notice.to_owned()));

            if is_locked {
                let error = result.unwrap_err().to_string();
                assert!(error.contains("older rootbeer"), "{error}");
                assert_eq!(fs::read_to_string(&path).unwrap(), lock);
                continue;
            }
            result.unwrap();
            assert_eq!(notices, ["updating rootbeer.lock from an older rootbeer"]);
            assert!(RootbeerLock::read(&path)
                .unwrap()
                .inputs
                .resolvers
                .is_empty());
        }
    }

    #[test]
    fn malformed_lock_is_preserved_in_every_mode() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("rootbeer.lock");
        fs::write(&path, "{\n<<<<<<< conflict").unwrap();
        for package_lock in [
            PackageLockOptions::default(),
            PackageLockOptions {
                should_update: true,
                ..Default::default()
            },
            PackageLockOptions {
                is_offline: true,
                ..Default::default()
            },
            PackageLockOptions {
                is_locked: true,
                ..Default::default()
            },
        ] {
            let mut options = opts(root.path().into(), Mode::Apply);
            options.package_lock = package_lock;
            let planned = PlannedPipeline {
                tools: Default::default(),
                package_resolver: crate::package::resolver_stack_for_inputs,
                package_repository: None,
                local_catalog: None,
                opts: options,
                ops: vec![],
                is_packages_taken: false,
            };
            assert!(planned
                .locked_ops_for_apply(&mut |_| {})
                .unwrap_err()
                .to_string()
                .contains("invalid package lock"));
            assert_eq!(fs::read_to_string(&path).unwrap(), "{\n<<<<<<< conflict");
        }
    }
}
