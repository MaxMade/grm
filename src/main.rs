//! Git Release Manager (`grm`): installs and updates releases published in Git
//! repositories, as described by one configuration file per release.

pub mod backend;
mod config;
pub mod lock;
pub mod state;

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use log::LevelFilter;

use crate::{config::Config, state::State};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Directory containing the configuration files (`*.toml`).
    ///
    /// [default: `/etc/grm` as root, otherwise `$XDG_CONFIG_HOME/grm` or `~/.config/grm`]
    #[arg(short, long, value_name = "DIR", global = true)]
    configs: Option<PathBuf>,

    /// Increase verbosity (-v: info, -vv: debug, -vvv: trace).
    ///
    /// Only affects messages of grm itself. Settings for modules in the
    /// `RUST_LOG` environment variable take precedence, e.g. `grm=warn`, and
    /// can enable messages of dependencies, e.g. `ureq=debug`.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List all packages with their installed versions.
    List,

    /// Install the newest release of a package that is not installed yet.
    Install {
        /// Name of the package (`name` in its configuration).
        package: String,
    },

    /// Update all installed packages to their newest releases.
    Update,

    /// Remove an installed package.
    Remove {
        /// Name of the package (`name` in its configuration).
        package: String,
    },
}

fn main() -> ExitCode {
    // Parse arguments
    let args = Args::parse();

    // Configure logging for messages of grm only, since dependencies (e.g.
    // ureq, rustls) log thousands of lines at debug and trace level.
    // `parse_default_env` applies RUST_LOG afterwards, so its settings for
    // modules take precedence (e.g. `grm=warn`) and can enable dependencies
    // (e.g. `ureq=debug`). A plain level (e.g. `warn`) only applies to
    // modules without a setting, i.e. not to grm.
    let level = match args.verbose {
        0 => LevelFilter::Warn,
        1 => LevelFilter::Info,
        2 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    };

    env_logger::Builder::new()
        .filter_level(LevelFilter::Off)
        .filter_module(env!("CARGO_CRATE_NAME"), level)
        .parse_default_env()
        .format_timestamp(None)
        .format_target(false)
        .format_level(false)
        .init();

    // Load configuration files
    let configs = match Config::load_dir(args.configs.as_deref()) {
        Ok(configs) => configs,
        Err(error) => {
            log::error!("Unable to load configurations: {error}");
            return ExitCode::FAILURE;
        }
    };

    // Use every valid configuration, even if others are invalid, but report
    // the failure through the exit code. The directories are needed for the
    // lock file and the record of installed versions.
    let mut failed = false;
    let configs = configs.into_iter().filter_map(|config| {
        match config.and_then(|config| {
            config.installation.create_dirs()?;
            Ok(config)
        }) {
            Ok(config) => Some(config),
            Err(error) => {
                log::error!("Skipping configuration: {error}");
                failed = true;
                None
            }
        }
    });

    let mut state = match State::new(configs) {
        Ok(state) => state,
        Err(error) => {
            log::error!("Unable to create local state: {error}");
            return ExitCode::FAILURE;
        }
    };

    let success = match &args.command {
        Command::List => state.list(),
        Command::Install { package } => match state.install(package) {
            Ok(()) => true,
            Err(error) => {
                log::error!("{package}: {error}");
                false
            }
        },
        Command::Update => state.update(),
        Command::Remove { package } => match state.remove(package) {
            Ok(()) => true,
            Err(error) => {
                log::error!("{package}: {error}");
                false
            }
        },
    };

    if success && !failed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parse(args: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from([&["grm"], args].concat())
    }

    #[test]
    fn cli_is_consistent() {
        Args::command().debug_assert();
    }

    #[test]
    fn parses_commands() {
        assert!(matches!(parse(&["list"]).unwrap().command, Command::List));
        assert!(matches!(
            parse(&["update"]).unwrap().command,
            Command::Update
        ));
        assert!(matches!(
            parse(&["install", "lego"]).unwrap().command,
            Command::Install { package } if package == "lego"
        ));
        assert!(matches!(
            parse(&["remove", "lego"]).unwrap().command,
            Command::Remove { package } if package == "lego"
        ));
    }

    #[test]
    fn global_options_work_before_and_after_command() {
        for args in [
            &["-vv", "-c", "/etc/x", "update"][..],
            &["update", "-vv", "--configs", "/etc/x"],
        ] {
            let args = parse(args).unwrap();
            assert_eq!(args.verbose, 2);
            assert_eq!(
                args.configs.as_deref(),
                Some(std::path::Path::new("/etc/x"))
            );
        }

        // clap does not add up counts on both sides of the command
        assert_eq!(parse(&["-v", "update", "-v"]).unwrap().verbose, 1);
    }

    #[test]
    fn rejects_invalid_usage() {
        for args in [
            &[][..],
            &["install"],
            &["remove"],
            &["install", "a", "b"],
            &["upgrade"],
            &["list", "extra"],
        ] {
            assert!(parse(args).is_err(), "accepted {args:?}");
        }
    }
}
