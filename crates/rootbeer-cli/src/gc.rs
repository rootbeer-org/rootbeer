use super::profile::{directory, profile_error};
use crate::package::HELPER;
use rootbeer_core::env::{profiles, ON_PATH};
use rootbeer_drv::STORE_ROOT;
use rootbeer_profile::Profile;
use rootbeer_store::{Store, ROOT};
use rustix::process::getuid;
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

#[derive(clap::Args, Debug)]
pub(crate) struct Args {
    /// Also remove your generations older than this, such as `30d`. Each
    /// profile keeps its current generation
    #[arg(long = "older-than", value_parser = days)]
    older_than: Option<Duration>,
}

pub(crate) fn run(args: &Args) -> Result<(), String> {
    crate::package::setup::ensure()?;
    let is_root = getuid().is_root();
    let collector = match is_root {
        true => STORE_ROOT,
        false => HELPER,
    };

    if let Err(error) = fs::metadata(collector) {
        return Err(format!("{collector}: {error}"));
    }

    if let Some(age) = args.older_than {
        prune(age)?;
    }

    if !is_root {
        let status = Command::new(HELPER)
            .arg("gc")
            .status()
            .map_err(|error| format!("{HELPER}: {error}"))?;

        return match status.success() {
            true => Ok(()),
            false => Err(format!("{HELPER} gc {status}")),
        };
    }

    let root = Path::new(ROOT);
    let profiles = root.join("profiles");
    let roots = || rootbeer_profile::roots(&profiles).map_err(profile_error);
    let mut store = Store::open(root).map_err(|error| error.to_string())?;
    let removed = store
        .collect(roots, || {
            eprintln!("waiting for installs and builds to finish")
        })
        .map_err(|error| error.to_string())?;

    for entry in &removed {
        println!("{}", entry.escape_debug());
    }

    println!("removed {} store entries", removed.len());
    Ok(())
}

fn prune(age: Duration) -> Result<(), String> {
    match fs::symlink_metadata(profiles()) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        _ => {}
    }

    let directory = directory(false)?;
    for name in ON_PATH {
        let profile = Profile::open(&directory, name).map_err(profile_error)?;
        let removed = profile.prune(age).map_err(profile_error)?;
        if !removed.is_empty() {
            eprintln!("removed {} generations of {name}", removed.len());
        }
    }

    Ok(())
}

fn days(value: &str) -> Result<Duration, String> {
    value
        .strip_suffix('d')
        .and_then(|days| days.parse::<u64>().ok())
        .and_then(|days| days.checked_mul(24 * 60 * 60))
        .map(Duration::from_secs)
        .ok_or_else(|| format!("{value:?} isn't a number of days, such as 30d"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn ages_are_whole_days() {
        assert_eq!(super::days("30d").unwrap().as_secs(), 30 * 24 * 60 * 60);
        assert_eq!(super::days("0d").unwrap().as_secs(), 0);
        for value in ["30", "d", "-1d", "1.5d", "99999999999999999d"] {
            assert!(super::days(value).is_err(), "{value}");
        }
    }
}
