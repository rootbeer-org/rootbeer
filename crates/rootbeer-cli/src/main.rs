mod apply;
mod cd;
mod drv;
mod edit;
mod init;
mod newer;
mod progress;
mod remote;
mod typegen;

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

    /// Run a package's command without installing it into a profile
    Run(drv::run::Args),

    /// Search the signed package index by name
    Search(drv::search::Args),

    /// Install packages into your user profile, without a Lua configuration
    Use(drv::profile::UseArgs),

    /// Remove packages from your user profile
    Unuse {
        #[arg(required = true)]
        names: Vec<String>,
    },

    /// Switch your user profile back to its previous generation
    Rollback,

    /// List your user profile's generations, marking the current one
    Generations,

    /// Create or load a rootbeer configuration in the source directory
    Init(init::Args),

    /// Open a shell in the rootbeer source directory
    Cd,

    /// Open the rootbeer source directory in $VISUAL/$EDITOR
    Edit,

    /// Apply the rootbeer configuration
    Apply(apply::Args),

    /// Remove store outputs no generation of any profile on this machine holds
    Gc(drv::gc::Args),

    /// Print shell setup that puts your profiles on PATH (sh, Bash, or Zsh)
    Env,

    /// Move your packages from the old /opt/rootbeer store into /opt/rb, then
    /// release what it holds for you
    Migrate(drv::migrate::Args),

    /// View or change the git remote protocol for the source directory
    Remote(remote::Args),

    /// Update the rootbeer in your user profile
    #[command(visible_alias = "update")]
    SelfUpdate,

    /// Inspect package derivations and keys (catalog maintainers)
    #[command(hide = true)]
    Drv(drv::Args),
}

impl Commands {
    /// Whether a newer rb may be fetched to run this in its place. Offline
    /// commands can't fetch.
    fn can_relaunch(&self) -> bool {
        !matches!(self, Commands::Apply(args) if args.offline)
    }
}

fn main() {
    let cli = Cli::parse();
    rootbeer_core::package::progress::observe(progress::report);
    rootbeer_core::package::newer::observe(newer::relaunch);
    if cli.command.can_relaunch() {
        newer::allow();
    }

    let result = match cli.command {
        Commands::Licenses => {
            print!(
                "lzma-rust2\n{}\nzip\n{}",
                include_str!("../../../licenses/lzma-rust2.txt"),
                include_str!("../../../licenses/zip.txt")
            );
            Ok(())
        }
        Commands::Init(args) => {
            init::run(args);
            Ok(())
        }
        Commands::Run(args) => drv::run::run(args),
        Commands::Search(args) => drv::search::search(&args),
        Commands::Use(args) => drv::profile::add(&args)
            .and_then(|()| drv::profile::keep_rootbeer())
            .inspect(|()| drv::migrate::hint()),
        Commands::Unuse { names } => drv::profile::remove(&names),
        Commands::Rollback => drv::profile::rollback(),
        Commands::Generations => drv::profile::generations(),
        Commands::Cd => {
            cd::run();
            Ok(())
        }
        Commands::Edit => {
            edit::run();
            Ok(())
        }
        Commands::Apply(args) => {
            apply::run(args, cli.lua_dir.as_ref());
            Ok(())
        }
        Commands::Gc(args) => drv::gc::run(&args),
        Commands::Env => {
            print!("{}", rootbeer_core::env::contents());
            Ok(())
        }
        Commands::Migrate(args) => drv::migrate::migrate(&args),
        Commands::Remote(args) => {
            remote::run(args);
            Ok(())
        }
        Commands::SelfUpdate => drv::profile::self_update(),
        Commands::Drv(args) => {
            drv::run(args);
            Ok(())
        }
    };

    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
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
