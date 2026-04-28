//! CLI dispatch and argument handling.

pub mod action;
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
        Some(def::Commands::Check {
            paths,
            nul,
            json,
            first,
            strict,
            strict_headings,
        }) => {
            let opts = action::check::CheckOptions {
                paths,
                nul,
                json,
                first,
                strict,
                strict_headings,
            };
            let exit_code = action::check::run(opts)?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
        Some(def::Commands::Orphans {
            paths,
            nul,
            json,
            first,
            filter_paths,
            include_exts,
            exclude_globs,
            directory_references_all_children,
        }) => {
            let opts = action::orphans::OrphansOptions {
                paths,
                nul,
                json,
                first,
                filter_paths,
                include_exts,
                exclude_globs,
                directory_references_all_children,
            };
            let exit_code = action::orphans::run(opts)?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
    }
}
