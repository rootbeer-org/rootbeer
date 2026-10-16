use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use mlua::{Lua, Result};

use crate::lua::module::install;
#[cfg(all(feature = "embedded-stdlib", not(debug_assertions)))]
use crate::lua::require::EmbeddedRequirer;
use crate::lua::require::FsRequirer;
use crate::lua::{fs, package, secret, sys, writer};
use crate::plan::Op;
use crate::profile::{self, ProfileContext};
use crate::Runtime;

#[derive(Debug, Default)]
pub(crate) struct Run(Mutex<Vec<Op>>);

impl Run {
    pub fn lock(&self) -> MutexGuard<'_, Vec<Op>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn into_ops(self) -> Vec<Op> {
        self.0.into_inner().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Debug, Default)]
pub(crate) struct PackageBins(Mutex<BTreeMap<String, PathBuf>>);

impl PackageBins {
    pub fn insert(&self, name: String, path: PathBuf) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name, path);
    }

    pub fn get(&self, name: &str) -> Option<PathBuf> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }
}

pub(crate) fn profile_bin_path(bin: &str) -> PathBuf {
    crate::env::bin_dir().join(bin)
}

pub(crate) struct Vm {
    pub lua: Lua,
}

impl Vm {
    pub fn new(runtime: Runtime) -> Result<Self> {
        let lua = Lua::new();

        let lua_dir = runtime.lua_dir.clone();
        let script_dir = runtime.script_dir.clone();
        let cli_profile = runtime.profile.clone();
        let source_dir = script_dir.to_string_lossy().to_string();

        lua.set_app_data(runtime);
        lua.set_app_data(Run::default());
        lua.set_app_data(PackageBins::default());
        lua.set_app_data(ProfileContext::new(cli_profile));

        let rb = lua.create_table()?;
        install::<fs::Fs>(&lua, &rb)?;
        install::<writer::Writer>(&lua, &rb)?;
        install::<package::Package>(&lua, &rb)?;
        install::<sys::Sys>(&lua, &rb)?;
        install::<secret::Secret>(&lua, &rb)?;
        install::<profile::Profile>(&lua, &rb)?;
        rb.set("source_dir", source_dir)?;

        lua.globals().set("rootbeer", &rb)?;
        lua.register_module("@rootbeer", rb)?;

        wire_require(&lua, lua_dir, script_dir)?;

        Ok(Self { lua })
    }

    pub fn exec(&self, source: &str, name: &str) -> Result<()> {
        self.lua.load(source).set_name(name).exec()
    }

    pub fn drain_ops(self) -> Vec<Op> {
        self.lua
            .remove_app_data::<Run>()
            .unwrap_or_default()
            .into_ops()
    }
}

/// Wrap require() to accept standard Lua dot-separated paths
/// (e.g. require("rootbeer.git")) in addition to Luau's native
/// @-prefixed paths (e.g. require("@rootbeer/git")). Luau's C++
/// layer rejects paths that don't start with @, ./, or ../ so the
/// translation must happen before the path reaches it.
fn wire_require(lua: &Lua, lua_dir: PathBuf, script_dir: PathBuf) -> Result<()> {
    let inner = make_require_fn(lua, lua_dir, script_dir)?;

    let require_fn = lua.create_function(move |_lua, path: String| {
        let translated = if path.starts_with('@') || path.starts_with("../") {
            path
        } else if path == "rootbeer" || path.starts_with("rootbeer.") {
            format!("@{}", path.replace('.', "/"))
        } else if let Some(rest) = path.strip_prefix("./") {
            format!("@source/{rest}")
        } else {
            format!("@source/{}", path.replace('.', "/"))
        };

        inner.call::<mlua::Value>(translated)
    })?;
    lua.globals().set("require", require_fn)?;
    Ok(())
}

fn make_require_fn(lua: &Lua, lua_dir: PathBuf, script_dir: PathBuf) -> Result<mlua::Function> {
    // In debug builds, always use the filesystem so Lua changes don't
    // require a Rust recompile. In release builds with embedded-stdlib,
    // serve the stdlib from memory unless --lua-dir was explicitly set.
    #[cfg(all(feature = "embedded-stdlib", not(debug_assertions)))]
    {
        let default_dir = PathBuf::from(env!("ROOTBEER_LUA_DIR"));
        if lua_dir == default_dir {
            return lua.create_require_function(EmbeddedRequirer::new(script_dir));
        }
    }

    lua.create_require_function(FsRequirer::new(lua_dir, script_dir))
}
