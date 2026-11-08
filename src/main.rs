//! Git Release Manager (`grm`): installs and updates releases published in Git
//! repositories, as described by one configuration file per release.

// Not all configuration values are used yet.
pub mod backend;
#[allow(dead_code)]
mod config;
pub mod lock;

use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use log::LevelFilter;

use crate::config::Config;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Directory containing the configuration files (`*.toml`).
    ///
    /// [default: `/etc/grm` as root, otherwise `$XDG_CONFIG_HOME/grm` or `~/.config/grm`]
    #[arg(short, long, value_name = "DIR")]
    configs: Option<PathBuf>,

    /// Increase verbosity (-v: info, -vv: debug, -vvv: trace).
    ///
    /// The `RUST_LOG` environment variable takes precedence.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn main() -> ExitCode {
    // Parse arguments
    let args = Args::parse();

    // Configure logging; `parse_default_env` lets RUST_LOG override the level
    let level = match args.verbose {
        0 => LevelFilter::Warn,
        1 => LevelFilter::Info,
        2 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    };

    env_logger::Builder::new()
        .filter_level(level)
        .parse_default_env()
        .format_timestamp(None)
        .format_target(false)
        .format_level(false)
        .init();

    // Load configuration files
    let configs = match Config::load_dir(args.configs.as_deref()) {
        Ok(configs) => configs,
        Err(error) => {
            log::error!("Unable to load configuration: {error}");
            return ExitCode::FAILURE;
        }
    };

    // Process every valid configuration, even if others are invalid, but
    // report the failure through the exit code
    let mut failed = false;
    for config in configs {
        let config = match config.and_then(|config| {
            config.installation.create_dirs()?;
            Ok(config)
        }) {
            Ok(config) => config,
            Err(error) => {
                log::error!("Skipping configuration: {error}");
                failed = true;
                continue;
            }
        };

        log::debug!(
            "Processing config for {}",
            config.installation.path.display()
        );
        log::trace!("{config:?}");
    }

    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
