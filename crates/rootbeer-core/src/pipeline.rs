use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};
use std::{fs, io};

use crate::executor::{self, ExecutionHandler, ExecutionReport};
use crate::package::PackageRequest;
use crate::plan::Op;
use crate::tools::{Install, ToolRuntime};
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
    install: Option<Install>,
}

impl Pipeline {
    pub fn new(opts: Options) -> Self {
        Self {
            opts,
            install: None,
        }
    }

    /// Supplies how the packages rb's own features run, such as `op`, get
    /// installed.
    pub fn with_tools(mut self, install: Install) -> Self {
        self.install = Some(install);
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
        let tools = std::sync::Arc::new(ToolRuntime::new(
            self.install,
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

        let package_catalog = vm
            .lua
            .app_data_ref::<crate::lua::Checkout>()
            .map(|checkout| checkout.0.clone());
        let ops = vm.drain_ops();

        Ok(PlannedPipeline {
            tools,
            opts: self.opts,
            package_catalog,
            ops,
        })
    }
}

/// A pipeline that has been planned — ops are collected, ready to execute.
pub struct PlannedPipeline {
    tools: std::sync::Arc<ToolRuntime>,
    package_catalog: Option<PathBuf>,
    opts: Options,
    ops: Vec<Op>,
}

impl PlannedPipeline {
    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    /// The checkout of recipes `rb.package_catalog` named, whose packages
    /// stand in for the index's.
    pub fn package_catalog(&self) -> Option<&Path> {
        self.package_catalog.as_deref()
    }

    /// Removes the package ops from the plan, which the caller installs.
    pub fn take_packages(&mut self) -> Vec<PackageRequest> {
        let (packages, ops) = std::mem::take(&mut self.ops)
            .into_iter()
            .partition::<Vec<_>, _>(|op| matches!(op, Op::Package { .. }));

        self.ops = ops;
        packages
            .into_iter()
            .filter_map(|op| match op {
                Op::Package { request } => Some(request),
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
                executor::apply_with_tools(&self.tools, &self.ops, self.opts.force, handler)?
            }
            Mode::DryRun => executor::dry_run(&self.ops, handler),
        };

        Ok(report)
    }
}
