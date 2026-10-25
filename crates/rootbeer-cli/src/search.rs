use crate::package::install::{self, Source};
use crate::package::this_platform;
use rustix::process::geteuid;
use std::collections::BTreeSet;

#[derive(clap::Args, Debug)]
pub(crate) struct Args {
    /// Words every matching package name contains
    #[arg(required = true)]
    terms: Vec<String>,
}

/// Matches names and aliases only, since descriptions would mean fetching
/// every package instead of only the matches.
pub(crate) fn search(args: &Args) -> Result<(), String> {
    let index = install::open_index(&Source::default(), geteuid().is_root())?;
    let platform = this_platform()?;
    let terms: Vec<String> = args.terms.iter().map(|term| term.to_lowercase()).collect();
    let is_match = |name: &str| terms.iter().all(|term| name.contains(term.as_str()));
    let mut matches: BTreeSet<String> = index
        .names()
        .into_iter()
        .filter(|name| is_match(name))
        .collect();

    for (alias, name) in index.aliases() {
        if is_match(&alias) {
            matches.insert(name);
        }
    }

    for name in &matches {
        let Some(package) = index.package(name).map_err(|error| error.to_string())? else {
            continue;
        };

        let Some(version) = package.default.get(&platform) else {
            continue;
        };

        println!("{name}\t{version}\t{}", package.description);
    }

    Ok(())
}
