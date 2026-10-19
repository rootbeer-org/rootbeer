use std::collections::BTreeMap;
use std::path::PathBuf;

use mlua::{Error as LuaError, Lua, Result as LuaResult, Table, Value};

use super::ctx::Ctx;
use super::module::Module;
use super::vm::{profile_bin_path, PackageBins};
use crate::package::PackageRequest;
use crate::plan::{Op, WriteSource};

pub(crate) struct Package;

/// A checkout of recipes, with `packages/` and `host.lua`, set by
/// `rb.package_catalog`.
#[derive(Debug, Clone)]
pub(crate) struct Checkout(pub(crate) PathBuf);

impl Module for Package {
    const NAME: &'static str = "";

    fn build(lua: &Lua, t: &Table) -> LuaResult<()> {
        t.set(
            "package_catalog",
            lua.create_function(|lua, path: String| {
                if lua.app_data_ref::<Checkout>().is_some() {
                    return Err(LuaError::RuntimeError(
                        "declare package_catalog once".into(),
                    ));
                }

                // A missing checkout falls back to the index when applied.
                let path = Ctx::from(lua).resolve(&path);
                let is_checkout = path.join("packages").is_dir() && path.join("host.lua").is_file();
                if path.exists() && !is_checkout {
                    return Err(LuaError::RuntimeError(format!(
                        "{} isn't a checkout of recipes with packages/ and host.lua",
                        path.display()
                    )));
                }

                lua.set_app_data(Checkout(path));
                Ok(())
            })?,
        )?;
        t.set(
            "package_repository",
            lua.create_function(|_, _: Value| -> LuaResult<()> {
                Err(LuaError::RuntimeError(
                    "rb.package_repository's signed repositories are gone, so remove it".into(),
                ))
            })?,
        )?;
        t.set(
            "package",
            lua.create_function(|lua, (spec, opts): (Value, Option<Table>)| {
                let parsed = parse_request(lua, spec, opts)?;
                append_packages(lua, vec![parsed])
            })?,
        )?;
        t.set(
            "packages",
            lua.create_function(|lua, specs: Table| {
                let entries = batch_entries(specs)?;
                let parsed = entries
                    .into_iter()
                    .enumerate()
                    .map(|(index, spec)| {
                        parse_request(lua, spec, None).map_err(|error| {
                            LuaError::RuntimeError(format!(
                                "rb.packages entry {}: {error}",
                                index + 1
                            ))
                        })
                    })
                    .collect::<LuaResult<Vec<_>>>()?;
                append_packages(lua, parsed)
            })?,
        )?;
        t.set(
            "bin_dir",
            lua.create_function(|_, ()| Ok(crate::env::bin_dir().to_string_lossy().into_owned()))?,
        )?;
        t.set(
            "bin_path",
            lua.create_function(|_, value: Value| {
                let name = String::parse(value)?;
                validate_command(&name)?;
                Ok(profile_bin_path(&name).to_string_lossy().into_owned())
            })?,
        )?;

        t.set(
            "which",
            lua.create_function(|lua, value: Value| {
                let bin = String::parse(value)?;
                validate_command(&bin)?;
                let bins = package_bins(lua)?;
                if let Some(path) = bins.get(&bin) {
                    return Ok(Some(path.to_string_lossy().to_string()));
                }

                let path = profile_bin_path(&bin);
                Ok(path.exists().then(|| path.to_string_lossy().to_string()))
            })?,
        )?;

        t.set(
            "env_export",
            lua.create_function(|lua, shell: Option<String>| {
                let shell = shell.as_deref().unwrap_or("sh");
                if !matches!(shell, "sh" | "bash" | "zsh") {
                    return Err(LuaError::RuntimeError(format!(
                        "unsupported env export shell `{shell}`"
                    )));
                }

                let path = crate::env::path();
                Ctx::from(lua).push(Op::WriteFile {
                    path: path.clone(),
                    source: WriteSource::text(crate::env::contents()),
                });

                Ok(path.to_string_lossy().to_string())
            })?,
        )?;

        Ok(())
    }
}

fn append_packages(lua: &Lua, parsed: Vec<(PackageRequest, Vec<String>)>) -> LuaResult<()> {
    let cx = Ctx::from(lua);
    let bins = package_bins(lua)?;
    for (request, names) in parsed {
        for name in names {
            let path = profile_bin_path(&name);
            bins.insert(name, path);
        }
        cx.push(Op::Package { request });
    }

    Ok(())
}

fn package_bins(lua: &Lua) -> LuaResult<mlua::AppDataRef<'_, PackageBins>> {
    lua.app_data_ref::<PackageBins>()
        .ok_or_else(|| LuaError::RuntimeError("package bins aren't set up".into()))
}

fn parse_request(
    lua: &Lua,
    spec: Value,
    opts: Option<Table>,
) -> LuaResult<(PackageRequest, Vec<String>)> {
    let (request, opts, allowed) = match spec {
        Value::String(value) => (value.to_str()?.to_string(), opts, &["asset", "bins"][..]),
        Value::Table(table) if opts.is_none() => {
            let request = required(&table, "request")?;
            (request, Some(table), &["request", "asset", "bins"][..])
        }
        Value::Table(_) => {
            return Err(LuaError::RuntimeError(
                "package options require a request string".into(),
            ))
        }
        other => {
            return Err(LuaError::RuntimeError(format!(
                "rb.package expected a request string or table, got {}",
                other.type_name()
            )))
        }
    };

    let mut request = PackageRequest::parse(&request).map_err(LuaError::RuntimeError)?;
    if let Some(opts) = opts {
        fields(&opts, allowed)?;
        let asset = optional::<String>(&opts, "asset")?;
        let bins = optional::<Table>(&opts, "bins")?;
        match &mut request {
            PackageRequest::Release {
                asset: wanted_asset,
                bins: wanted_bins,
                ..
            } => {
                if asset.as_ref().is_some_and(|asset| asset.trim().is_empty()) {
                    return Err(LuaError::RuntimeError(
                        "package asset must be nonempty".into(),
                    ));
                }

                *wanted_asset = asset;
                if let Some(bins) = bins {
                    *wanted_bins = parse_bins(bins)?;
                }
            }
            PackageRequest::Index { .. } if asset.is_some() || bins.is_some() => {
                return Err(LuaError::RuntimeError(
                    "package asset and bins options require a github: request".into(),
                ));
            }
            PackageRequest::Index { .. } => {}
        }
    }

    let cx = Ctx::from(lua);
    let mut names: Vec<String> = match &request {
        PackageRequest::Release { bins, .. } => bins.keys().cloned().collect(),
        PackageRequest::Index { .. } => Vec::new(),
    };
    names.extend(request_bins(&cx, &request));
    for name in &names {
        validate_command(name)?;
    }

    Ok((request, names))
}

fn batch_entries(table: Table) -> LuaResult<Vec<Value>> {
    let mut entries = BTreeMap::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair?;
        let index = match key {
            Value::Integer(index) if index > 0 => index as usize,
            Value::Number(index)
                if index >= 1.0 && index.fract() == 0.0 && index < usize::MAX as f64 =>
            {
                index as usize
            }
            _ => {
                return Err(LuaError::RuntimeError(
                    "rb.packages expects a dense list with positive integer keys".into(),
                ))
            }
        };
        entries.insert(index, value);
    }
    if !entries.keys().copied().eq(1..=entries.len()) {
        return Err(LuaError::RuntimeError(
            "rb.packages expects a dense list without missing entries".into(),
        ));
    }
    Ok(entries.into_values().collect())
}

/// The commands `rootbeer.lock` recorded for a request, on this machine's
/// platform.
fn request_bins(cx: &Ctx<'_>, request: &PackageRequest) -> Vec<String> {
    let name = match request {
        PackageRequest::Release { repository, .. } => rootbeer_profile::release_name(repository),
        PackageRequest::Index { name, .. } => Some(name.clone()),
    };
    let Some(name) = name else {
        return Vec::new();
    };

    let path = cx.runtime.script_dir.join("rootbeer.lock");
    let Ok(Some(lock)) = rootbeer_profile::Lock::read(&path) else {
        return Vec::new();
    };

    let platform = format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS);
    lock.packages
        .get(&name)
        .and_then(|platforms| platforms.get(&platform))
        .map(|locked| locked.bins.iter().cloned().collect())
        .unwrap_or_default()
}

fn fields(table: &Table, allowed: &[&str]) -> LuaResult<()> {
    for pair in table.pairs::<Value, Value>() {
        let (key, _) = pair?;
        let Value::String(key) = key else {
            return Err(LuaError::RuntimeError(
                "package fields must be named strings".into(),
            ));
        };
        let key = key.to_str()?;
        if !allowed.contains(&key.as_ref()) {
            return Err(LuaError::RuntimeError(format!(
                "unknown package field `{key}`"
            )));
        }
    }
    Ok(())
}

fn validate_command(name: &str) -> LuaResult<()> {
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.contains(['/', '\\'])
        || name.chars().any(char::is_control)
    {
        return Err(LuaError::RuntimeError(
            "command name must be one nonempty normal filename".into(),
        ));
    }
    Ok(())
}

fn relative_path(path: &str) -> LuaResult<PathBuf> {
    let path = PathBuf::from(path);
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(LuaError::RuntimeError(
            "package install paths must be nonempty relative paths without traversal".into(),
        ));
    }
    Ok(path)
}

fn parse_bins(bins: Table) -> LuaResult<BTreeMap<String, PathBuf>> {
    let mut out = BTreeMap::new();
    for pair in bins.pairs::<Value, Value>() {
        let (name, path) = pair?;
        let name = String::parse(name)?;
        validate_command(&name)?;
        out.insert(name, relative_path(&String::parse(path)?)?);
    }

    Ok(out)
}

trait PackageField: Sized {
    fn parse(value: Value) -> LuaResult<Self>;
}

impl PackageField for String {
    fn parse(value: Value) -> LuaResult<Self> {
        match value {
            Value::String(value) => Ok(value.to_str()?.to_string()),
            other => Err(LuaError::RuntimeError(format!(
                "expected string, got {}",
                other.type_name()
            ))),
        }
    }
}

impl PackageField for Table {
    fn parse(value: Value) -> LuaResult<Self> {
        match value {
            Value::Table(value) => Ok(value),
            other => Err(LuaError::RuntimeError(format!(
                "expected table, got {}",
                other.type_name()
            ))),
        }
    }
}

fn required<T: PackageField>(table: &Table, field: &str) -> LuaResult<T> {
    T::parse(table.raw_get(field)?).map_err(|error| {
        LuaError::RuntimeError(format!(
            "package field `{field}` is required or invalid: {error}"
        ))
    })
}

fn optional<T: PackageField>(table: &Table, field: &str) -> LuaResult<Option<T>> {
    match table.raw_get(field)? {
        Value::Nil => Ok(None),
        value => T::parse(value).map(Some).map_err(|error| {
            LuaError::RuntimeError(format!("invalid package field `{field}`: {error}"))
        }),
    }
}
