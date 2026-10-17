use std::path::{Path, PathBuf};

use owo_colors::OwoColorize;
use rootbeer_core::profile::{ProfileError, ProfileErrorContext};
use rootbeer_core::{ExecutionHandler, Op, OpResult};

/// Apply the rootbeer configuration
#[derive(clap::Args, Debug)]
pub struct Args {
    /// Perform a dry run without making any changes
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Overwrite existing files/directories when creating symlinks
    #[arg(short, long)]
    pub force: bool,

    /// Refuse to update rootbeer.lock; fail if it is missing or stale
    #[arg(long)]
    pub locked: bool,

    /// Use only cached package resolutions and contents; allow lock reconciliation
    #[arg(long)]
    pub offline: bool,

    /// Refresh existing package resolutions, respecting explicit version pins
    #[arg(long, conflicts_with_all = ["locked", "offline"])]
    pub update: bool,

    /// Path to a .lua script to execute (default: data_dir/source/rootbeer.lua)
    #[arg(short, long)]
    pub script: Option<PathBuf>,

    /// Configuration profile input; used by `strategy = "cli"` or custom
    /// strategies that call `ctx.cli()`.
    #[arg(short = 'p', long)]
    pub profile: Option<String>,
}

struct CliHandler;

impl ExecutionHandler for CliHandler {
    fn on_start(&mut self, op: &Op) {
        match op {
            Op::Exec { cmd, args, .. } => {
                let display = std::iter::once(cmd.as_str())
                    .chain(args.iter().map(|s| s.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ");

                eprintln!("  {} `{display}`", "exec".cyan());
            }
            Op::WriteFile { source, .. } => {
                // Announce secret-backed fetches before they run so users
                // know why apply is pausing (Touch ID, network, etc.).
                if let Some(label) = source.fetch_label() {
                    eprintln!("  {} {label}", "fetch".cyan());
                }
            }
            _ => {}
        }
    }

    fn on_output(&mut self, line: &str) {
        eprintln!("    {}", line.dimmed());
    }

    fn on_result(&mut self, result: &OpResult) {
        match result {
            OpResult::FileWritten { path, bytes } => {
                let suffix = match bytes {
                    Some(n) => format!("({n} bytes)"),
                    None => "(deferred)".to_string(),
                };
                eprintln!("  {} {} {suffix}", "write".green(), path.display());
            }
            OpResult::SymlinkCreated { src, dst } => {
                eprintln!(
                    "  {} {} -> {}",
                    "link".green(),
                    dst.display(),
                    src.display()
                );
            }
            OpResult::SymlinkUnchanged { dst } => {
                eprintln!("  {} {} (unchanged)", "skip".dimmed(), dst.display());
            }
            OpResult::SymlinkOverwritten { src, dst } => {
                eprintln!(
                    "  {} {} -> {}",
                    "force".yellow(),
                    dst.display(),
                    src.display()
                );
            }
            OpResult::FileCopied { src, dst } => {
                eprintln!(
                    "  {} {} <- {}",
                    "copy".green(),
                    dst.display(),
                    src.display()
                );
            }
            OpResult::FileCopySkipped { dst } => {
                eprintln!("  {} {} (exists)", "skip".dimmed(), dst.display());
            }
            OpResult::CommandRan { cmd, status } => {
                if *status == 0 {
                    eprintln!("  {} `{cmd}`", "done".green());
                } else {
                    eprintln!("  {} `{cmd}` (exit {status})", "fail".red());
                }
            }
            OpResult::Chmodded { path, mode } => {
                eprintln!("  {} {} ({:o})", "chmod".green(), path.display(), mode);
            }
            OpResult::RemoteUpdated { from, to } => {
                eprintln!("  {} {from} -> {to}", "remote".green());
            }
            OpResult::RemoteUnchanged { url } => {
                eprintln!("  {} {url} (unchanged)", "skip".dimmed());
            }
            OpResult::PackageRealized {
                name,
                version,
                store_path,
            } => {
                if let Some(store_path) = store_path {
                    eprintln!(
                        "  {} {name}@{version} -> {}",
                        "package".green(),
                        store_path.display()
                    );
                } else {
                    eprintln!("  {} {name}@{version}", "package".green());
                }
            }
            OpResult::PackageUnchanged { name, version } => {
                eprintln!("  {} {name}@{version} (unchanged)", "skip".dimmed());
            }
            OpResult::PackagePlanned { spec } => {
                eprintln!("  {} {spec}", "package".green());
            }
        }
    }
}

pub fn run(args: Args, lua_dir: Option<&PathBuf>) {
    if args.update && (args.locked || args.offline) {
        eprintln!("error: --update cannot be combined with --locked or --offline");
        std::process::exit(1);
    }

    let script = args.script.unwrap_or_else(rootbeer_core::script_path);

    if !script.exists() {
        eprintln!("error: script not found: {}", script.display());
        std::process::exit(1);
    }

    let mut opts = rootbeer_core::Options::from_script(&script).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });

    opts.mode = if args.dry_run {
        rootbeer_core::Mode::DryRun
    } else {
        rootbeer_core::Mode::Apply
    };
    opts.force = args.force;
    opts.profile = args.profile;
    opts.package_lock = rootbeer_core::PackageLockOptions {
        should_update: args.update,
        is_locked: args.locked,
        is_offline: args.offline,
    };
    let package_lock = opts.package_lock;
    let script_dir = opts.script_dir.clone();

    if let Some(lua_dir) = lua_dir {
        opts.lua_dir = lua_dir.clone();
    }

    if let Err(e) = crate::drv::check_lock(&script_dir) {
        eprintln!("{} error: {e}", "✗".red().bold());
        std::process::exit(1);
    }

    let pipeline = rootbeer_core::Pipeline::new(opts);

    eprintln!(
        "applying ({}){}",
        pipeline.mode(),
        if pipeline.force() { " [force]" } else { "" }
    );

    let plan_result = {
        let _progress = crate::progress::Progress::new("evaluating configuration");
        pipeline.plan()
    };

    let mut planned = plan_result.unwrap_or_else(|e| {
        match &e {
            rootbeer_core::Error::Profile(pe) => {
                eprintln!("{} {pe}", "✗".red().bold());
                if let Some(hint) = profile_hint(pe) {
                    eprintln!("{hint}");
                }
            }
            _ => eprintln!("{} error: {e}", "✗".red().bold()),
        }
        std::process::exit(1);
    });

    // Before the rest, so commands the configuration runs can use them.
    let mut should_link_apps = false;
    if !args.dry_run {
        let catalog = planned.package_catalog().map(Path::to_path_buf);
        let configured = crate::drv::configure(
            planned.take_packages(),
            catalog.as_deref(),
            &script_dir,
            &package_lock,
            &Default::default(),
        )
        .and_then(|configured| crate::drv::profile::keep_rootbeer().map(|()| configured))
        .unwrap_or_else(|e| {
            eprintln!("{} error: {e}", "✗".red().bold());
            std::process::exit(1);
        });

        for skipped in &configured.skipped {
            eprintln!("warning: {skipped}, so rb skipped it");
        }

        should_link_apps = configured.should_link_apps;
    }

    let result = planned.execute(&mut CliHandler);

    match result {
        Ok(report) => {
            let has_failed = report.results.iter().any(
                |result| matches!(result, OpResult::CommandRan { status, .. } if *status != 0),
            );

            if should_link_apps && has_failed {
                eprintln!("warning: a command failed, so apps weren't linked");
            }

            if should_link_apps && !has_failed {
                if let Err(e) = crate::drv::link_default_apps() {
                    eprintln!("{} error: {e}", "✗".red().bold());
                    std::process::exit(1);
                }
            }

            eprintln!(
                "{} done ({} operations)",
                "✓".green().bold(),
                report.results.len()
            );
            crate::drv::migrate::hint();
        }
        Err(e) => {
            eprintln!("{} error: {e}", "✗".red().bold());
            std::process::exit(1);
        }
    }
}

fn profile_hint(err: &ProfileError) -> Option<String> {
    match err {
        ProfileError::Required {
            context,
            active,
            profiles,
        } if !profiles.is_empty() => match *context {
            ProfileErrorContext::Cli => {
                let valid = format!("rb apply --profile <{}>", profiles.join("|"));
                let mut out = match active {
                    Some(name) => format!(
                        "  {} add profile '{}' to {} or pass one of: {}",
                        "hint:".dimmed(),
                        name.cyan(),
                        "rb.profile.define(...)".cyan(),
                        valid.cyan()
                    ),
                    None => format!("  {} {}", "hint:".dimmed(), valid.cyan()),
                };
                if let Some(name) = active {
                    append_profile_suggestion(&mut out, name, profiles);
                }
                Some(out)
            }
            ProfileErrorContext::Strategy => active.as_ref().map(|name| {
                let mut out = format!(
                    "  {} return one of <{}> from the profile strategy or add '{}' to {}",
                    "hint:".dimmed(),
                    profiles.join("|"),
                    name.cyan(),
                    "rb.profile.define(...)".cyan()
                );
                append_profile_suggestion(&mut out, name, profiles);
                out
            }),
            ProfileErrorContext::Reference(api) => active.as_ref().map(|name| {
                let mut out = format!(
                    "  {} add '{}' to {} or fix the {} profile name",
                    "hint:".dimmed(),
                    name.cyan(),
                    "rb.profile.define(...)".cyan(),
                    api.cyan()
                );
                append_profile_suggestion(&mut out, name, profiles);
                out
            }),
            ProfileErrorContext::Value(api) => Some(match active {
                Some(name) => format!(
                    "  {} add {} = ... or default = ... to {}",
                    "hint:".dimmed(),
                    name.cyan(),
                    api.cyan()
                ),
                None => format!(
                    "  {} add default = ... to {} or choose an active profile",
                    "hint:".dimmed(),
                    api.cyan()
                ),
            }),
        },
        ProfileError::NoMatch {
            strategy,
            value,
            profiles,
        } if !profiles.is_empty() => match (*strategy, value.as_deref()) {
            ("hostname", Some(hostname)) => Some(format!(
                "  {} add hostname '{}' to a matcher in {} or add one fallback profile with {}",
                "hint:".dimmed(),
                hostname.cyan(),
                "rb.profile.define(...)".cyan(),
                "{}".cyan()
            )),
            ("user", Some(user)) => Some(format!(
                "  {} add user '{}' to a matcher in {} or add one fallback profile with {}",
                "hint:".dimmed(),
                user.cyan(),
                "rb.profile.define(...)".cyan(),
                "{}".cyan()
            )),
            ("command", _) => Some(format!(
                "  {} add an installed command/path matcher in {} or add one fallback profile with {}",
                "hint:".dimmed(),
                "rb.profile.define(...)".cyan(),
                "{}".cyan()
            )),
            _ => Some(format!(
                "  {} adjust the '{}' strategy matchers in {}",
                "hint:".dimmed(),
                strategy.cyan(),
                "rb.profile.define(...)".cyan()
            )),
        },
        _ => None,
    }
}

fn append_profile_suggestion(out: &mut String, name: &str, profiles: &[String]) {
    if let Some(suggestion) = closest_match(name, profiles) {
        out.push_str(&format!(" (did you mean '{}'?)", suggestion.cyan()));
    }
}

fn closest_match<'a>(name: &str, candidates: &'a [String]) -> Option<&'a str> {
    let target = name.to_lowercase();
    candidates
        .iter()
        .map(|c| (levenshtein(&target, &c.to_lowercase()), c.as_str()))
        .filter(|(d, c)| *d <= (c.len() / 2).max(2))
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        curr[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profiles(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn hostname_no_match_hint_matches_hostname_strategy() {
        let hint = profile_hint(&ProfileError::NoMatch {
            strategy: "hostname",
            value: Some("work-mbp".to_string()),
            profiles: profiles(&["personal"]),
        })
        .expect("expected hint");

        assert!(hint.contains("hostname"));
        assert!(hint.contains("work-mbp"));
        assert!(!hint.contains("ctx.cli"));
        assert!(!hint.contains("--profile"));
    }

    #[test]
    fn reference_hint_does_not_suggest_cli_profile_flag() {
        let hint = profile_hint(&ProfileError::Required {
            context: ProfileErrorContext::Reference("rb.profile.when"),
            active: Some("work".to_string()),
            profiles: profiles(&["personal"]),
        })
        .expect("expected hint");

        assert!(hint.contains("rb.profile.when"));
        assert!(hint.contains("rb.profile.define"));
        assert!(!hint.contains("rb apply --profile"));
    }

    #[test]
    fn cli_unknown_hint_mentions_declaring_or_passing_known_profile() {
        let hint = profile_hint(&ProfileError::Required {
            context: ProfileErrorContext::Cli,
            active: Some("work".to_string()),
            profiles: profiles(&["personal"]),
        })
        .expect("expected hint");

        assert!(hint.contains("add profile"));
        assert!(hint.contains("rb.profile.define"));
        assert!(hint.contains("rb apply --profile <personal>"));
    }
}
