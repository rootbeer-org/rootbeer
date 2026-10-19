use super::install::{self, Source};
use rustix::process::geteuid;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{self, Write};

#[derive(Serialize)]
struct Listing {
    targets_version: u64,
    expires: String,
    aliases: BTreeMap<String, String>,
    packages: BTreeMap<String, Value>,
}

pub(super) fn catalog(source: &Source) -> Result<(), String> {
    let index = install::open_index(source, geteuid().is_root())?;
    let packages = index
        .names()
        .into_iter()
        .map(|name| {
            let entry = index
                .entry(&name)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("{name} is listed but has no entry"))?;
            Ok((name, entry))
        })
        .collect::<Result<_, String>>()?;

    let listing = Listing {
        targets_version: index.targets_version(),
        expires: index.targets_expires().to_string(),
        aliases: index.aliases(),
        packages,
    };

    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, &listing).map_err(|error| error.to_string())?;
    writeln!(out).map_err(|error| error.to_string())
}
