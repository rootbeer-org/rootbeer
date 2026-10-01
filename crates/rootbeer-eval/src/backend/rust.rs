use super::{Script, env, lines};
use crate::Host;
use crate::recipe::{Bins, Build};
use crate::template::{Values, path};
use std::collections::BTreeMap;

// The host toolchain stand-in, set by eval alone.
const RESERVED_ENV: [&str; 2] = ["RB_RUST", "RB_SYSTEM"];

pub(super) fn build(
    values: &Values,
    build: &Build,
    bins: Option<&Bins>,
    host: &Host,
) -> Result<Script, String> {
    let rust = build
        .rust
        .as_ref()
        .ok_or("a rust build needs rust settings")?;

    let names = bins.map(Bins::names).unwrap_or_default();
    if names.is_empty() {
        return Err("a rust build needs outputs.bins".into());
    }

    if let Some(name) = rust
        .environment
        .keys()
        .find(|name| RESERVED_ENV.contains(&name.as_str()))
    {
        return Err(format!("environment variable {name} is reserved"));
    }

    let mut variables = rust
        .environment
        .iter()
        .map(|(name, value)| Ok((name.clone(), values.literal(value)?)))
        .collect::<Result<BTreeMap<_, _>, String>>()?;

    // TODO: Depend on a catalog rust by key instead of the host
    variables.extend(env(&[("CARGO_INCREMENTAL", "0"), ("RB_RUST", &host.rust)]));
    let mut selection = rust
        .packages
        .iter()
        .flat_map(|package| ["--package".to_string(), package.clone()])
        .collect::<Vec<_>>();

    if !rust.features.is_empty() {
        selection.extend(["--features".into(), rust.features.join(",")]);
    }

    if rust.no_default_features {
        selection.push("--no-default-features".into());
    }

    selection.extend(
        names
            .iter()
            .flat_map(|name| ["--bin".to_string(), name.to_string()]),
    );

    let mut script = lines(&[
        r#"export CARGO_HOME="${TMPDIR}/cargo-home" CARGO_TARGET_DIR="${TMPDIR}/cargo-target""#,
    ]);

    script.push(format!(
        r#"cargo build --frozen --release --jobs "${{jobs}}" --config "${{vendor}}/config.toml" {}"#,
        values.command(&selection)?
    ));

    script.push(r#"mkdir -p "${out}/bin""#.into());
    script.extend(names.iter().map(|name| {
        format!(
            "cp {} {}",
            path("CARGO_TARGET_DIR", &format!("release/{name}")),
            path("out", &format!("bin/{name}"))
        )
    }));

    Ok(Script {
        env: variables,
        lines: script,
    })
}

pub(super) fn vendor(host: &Host) -> Script {
    Script {
        env: env(&[("RB_RUST", &host.rust)]),
        lines: lines(&[
            r#"export CARGO_HOME="${TMPDIR}/cargo-home""#,
            "test -f Cargo.lock",
            r#"mkdir -p "${out}""#,
            r#"cargo vendor --locked --versioned-dirs "${out}/vendor" > "${out}/config.toml""#,
        ]),
    }
}
