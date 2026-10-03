pub use rootbeer_store_legacy::deterministic;
mod age;
mod executor;
mod lua;
mod one_password;
pub mod package;
mod pipeline;
mod plan;
pub mod profile;
mod tools;
pub use rootbeer_store_legacy as store;

pub use executor::{ExecutionHandler, ExecutionReport, OpResult};
pub use pipeline::{Mode, Options, PackageLockOptions, Pipeline, PlannedPipeline};
pub use plan::{AgeIdentity, Op, WriteSource};
pub use profile::ProfileError;

#[cfg(feature = "embedded-stdlib")]
pub use lua::require::embedded_modules;

/// Returns the compile-time Lua standard library directory path.
pub fn lua_dir() -> PathBuf {
    PathBuf::from(env!("ROOTBEER_LUA_DIR"))
}

use std::fmt::Display;
use std::path::PathBuf;
use std::{error, io};

use package::lockfile::LockError;
use package::LockBuildError;

#[derive(Debug)]
pub(crate) struct Runtime {
    pub tools: std::sync::Arc<tools::ToolRuntime>,
    pub script_dir: PathBuf,
    pub script_name: String,
    pub lua_dir: PathBuf,
    pub profile: Option<String>,
}

fn xdg_dir(env_var: &str, fallback: &str) -> PathBuf {
    if let Some(val) = std::env::var_os(env_var) {
        PathBuf::from(val).join("rootbeer")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(fallback).join("rootbeer")
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }
}

/// User configuration directory (`~/.config/rootbeer`).
/// The user's dotfiles/lua scripts live here.
pub fn config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

/// State directory (`~/.local/state/rootbeer`).
/// Revisions, operation history, and other persistent runtime state.
pub fn state_dir() -> PathBuf {
    rootbeer_store_legacy::state_dir()
}

/// Data directory (`~/.local/share/rootbeer`).
/// Type definitions, extracted stdlib, and other shared data.
pub fn data_dir() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

/// Default path to the user's rootbeer script inside the config directory.
pub fn script_path() -> PathBuf {
    config_dir().join("init.lua")
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Lua(mlua::Error),
    Lock(LockError),
    LockBuild(Box<LockBuildError>),
    Profile(ProfileError),
}

impl Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Lock(e) => write!(f, "{e}"),
            Error::LockBuild(e) => write!(f, "{e}"),
            Error::Profile(e) => write!(f, "{e}"),
            Error::Lua(e) => {
                let msg = e.to_string();
                let msg = msg
                    .strip_prefix("runtime error: ")
                    .unwrap_or(&msg)
                    .split("\nstack traceback:")
                    .next()
                    .unwrap_or(&msg)
                    .replace("@source/", "")
                    .replace("@rootbeer/", "rootbeer.");
                write!(f, "{msg}")
            }
        }
    }
}

impl error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<LockError> for Error {
    fn from(e: LockError) -> Self {
        Error::Lock(e)
    }
}

impl From<LockBuildError> for Error {
    fn from(e: LockBuildError) -> Self {
        Error::LockBuild(Box::new(e))
    }
}

impl From<mlua::Error> for Error {
    fn from(e: mlua::Error) -> Self {
        Error::Lua(e)
    }
}

impl From<ProfileError> for Error {
    fn from(e: ProfileError) -> Self {
        Error::Profile(e)
    }
}
