//! CLI dispatch and argument handling.

mod def;

pub use def::Args;

pub mod log;

use clap::Parser;

/// Setup logging and color output based on CLI arguments.
fn setup_logging_and_colors(cli: &def::Args) -> Result<(), String> {
    let logs = cli.log.clone().unwrap_or_default();
    let logs = logs
        .iter()
        .flat_map(|log| log.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<&str>>();

    log::setup(cli.verbose, logs, cli.log_time)?;

    Ok(())
}

/// Run the CLI application.
pub fn run() -> Result<(), String> {
    let args = Args::parse();
    setup_logging_and_colors(&args)?;

    match args.command {
        None => {
            println!("org-cli: Query and edit org-mode files");
            println!("Run with --help for usage information.");
            Ok(())
        }
    }
}
