mod apply;
mod bootstrap;
mod cd;
mod drv;
mod edit;
mod gc;
mod init;
mod newer;
mod progress;
mod remote;
mod run;
mod search;
mod typegen;
mod update;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "rb",
    version = env!("RB_VERSION"),
    about,
    long_about = None,
    max_term_width = 80
)]
/// A command-line tool to deterministically manage your system using Lua!
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Enable additional debug output
    #[arg(short, long, default_value_t = false)]
    verbose: bool,

    /// Suppress all output except errors
    #[arg(short, long, default_value_t = false)]
    quiet: bool,

    /// Path to the lua/ standard library directory
    #[arg(long)]
    lua_dir: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Show license notices for package backend libraries
    Licenses,

    /// Run a package command without a configuration or permanent installation
    Run(run::RunArgs),

    /// Search published packages in the distribution manifest
    Search(search::Args),

    /// Install packages for your user without a Lua configuration
    Use(run::UseArgs),

    /// Remove packages from your user profile, retaining cached downloads
    Unuse(run::UnuseArgs),

    /// Create or load a rootbeer configuration in the source directory
    Init(init::Args),

    /// Open a shell in the rootbeer source directory
    Cd,

    /// Open the rootbeer source directory in $VISUAL/$EDITOR
    Edit,

    /// Apply the rootbeer configuration
    Apply(apply::Args),

    /// Delete store entries no profile or configuration uses
    Gc,

    /// Print shell setup for installed packages (sh, Bash, or Zsh)
    Env,

    /// View or change the git remote protocol for the source directory
    Remote(remote::Args),

    /// Update Rootbeer through its installation owner
    #[command(visible_alias = "update")]
    SelfUpdate,

    /// Inspect package derivations and keys (catalog maintainers)
    #[command(hide = true)]
    Drv(drv::Args),
}

impl Commands {
    fn needs_store(&self) -> bool {
        matches!(
            self,
            Commands::Run(_)
                | Commands::Use(_)
                | Commands::Unuse(_)
                | Commands::Apply(_)
                | Commands::Gc
                | Commands::SelfUpdate
        )
    }

    /// Whether `rb` may install itself before running, which needs the network.
    fn should_adopt(&self) -> bool {
        match self {
            Commands::Run(args) => !args.offline,
            Commands::Use(args) => {
                !args.offline && !args.packages.iter().any(|package| package == "rootbeer")
            }
            Commands::Apply(args) => !args.offline,
            _ => false,
        }
    }

    /// Whether a newer rb may be fetched to run this in its place. Offline
    /// commands can't fetch, and a fetched rb can't update the installed one.
    fn can_relaunch(&self) -> bool {
        match self {
            Commands::Run(args) => !args.offline,
            Commands::Use(args) => !args.offline,
            Commands::Apply(args) => !args.offline,
            Commands::SelfUpdate => false,
            _ => true,
        }
    }
}

fn main() {
    let cli = Cli::parse();
    rootbeer_core::package::progress::observe(progress::report);
    rootbeer_core::package::newer::observe(newer::relaunch);
    if cli.command.can_relaunch() {
        newer::allow();
    }
    if cli.command.needs_store() {
        if let Err(error) = bootstrap::ensure() {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
    if cli.command.should_adopt() {
        bootstrap::adopt();
    }

    match cli.command {
        Commands::Licenses => print!(
            "lzma-rust2\n{}\nzip\n{}",
            include_str!("../../../licenses/lzma-rust2.txt"),
            include_str!("../../../licenses/zip.txt")
        ),
        Commands::Init(args) => init::run(args),
        Commands::Run(args) => run::run(args),
        Commands::Use(args) => run::install(args),
        Commands::Search(args) => search::run(args),
        Commands::Unuse(args) => run::uninstall(args),
        Commands::Cd => cd::run(),
        Commands::Edit => edit::run(),
        Commands::Apply(args) => apply::run(args, cli.lua_dir.as_ref()),
        Commands::Gc => gc::run(),
        Commands::Env => print!("{}", rootbeer_core::package::profile::env_contents()),
        Commands::Remote(args) => remote::run(args),
        Commands::SelfUpdate => update::run(),
        Commands::Drv(args) => drv::run(args),
    }
}

#[cfg(test)]
mod command_tests {
    use super::*;

    #[test]
    fn self_update_keeps_legacy_alias() {
        for command in ["self-update", "update"] {
            assert!(matches!(
                Cli::try_parse_from(["rb", command]).unwrap().command,
                Commands::SelfUpdate
            ));
        }
    }

    #[test]
    fn apply_controls_are_independent() {
        let cli = Cli::try_parse_from(["rb", "apply", "--locked", "--offline"]).unwrap();
        let Commands::Apply(args) = cli.command else {
            panic!("expected apply")
        };
        assert!(args.locked && args.offline && !args.update);
        for flag in ["--locked", "--offline"] {
            assert!(Cli::try_parse_from(["rb", "apply", "--update", flag]).is_err());
        }
    }
}
