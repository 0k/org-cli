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
        Some(def::Commands::Backlinks {
            target,
            paths,
            nul,
            json,
            first,
        }) => {
            let opts = action::backlinks::BacklinksOptions {
                target,
                paths,
                nul,
                json,
                first,
            };
            let exit_code = action::backlinks::run(opts)?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
        Some(def::Commands::Headings {
            paths,
            exclude,
            filter,
            ignore_case,
            has_property,
            keyword,
            todo,
            todo_keywords,
            nul,
            json,
        }) => {
            let opts = action::headings::HeadingsOptions {
                paths,
                exclude_globs: exclude,
                filter,
                ignore_case,
                has_property,
                keyword,
                todo,
                todo_keywords,
                nul,
                json,
            };
            let exit_code = action::headings::run(opts)?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
        Some(def::Commands::Todos {
            paths,
            search,
            keyword,
            no_sync,
            db,
            nul,
            json,
        }) => {
            let opts = action::todos::TodosOptions {
                paths,
                search,
                keyword,
                no_sync,
                db,
                nul,
                json,
            };
            let exit_code = action::todos::run(opts)?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
        Some(def::Commands::Clock { action: clock_cmd }) => match clock_cmd {
            def::ClockCommands::Check {
                paths,
                exclude_globs,
                nul,
                json,
                first,
            } => {
                let opts = action::clock::CheckOptions {
                    paths,
                    exclude_globs,
                    nul,
                    json,
                    first,
                };
                let exit_code = action::clock::run_check(opts)?;
                if exit_code != 0 {
                    std::process::exit(exit_code);
                }
                Ok(())
            }
            def::ClockCommands::Fix {
                paths,
                exclude_globs,
                write,
                json,
            } => {
                let opts = action::clock::FixOptions {
                    paths,
                    exclude_globs,
                    write,
                    json,
                };
                let exit_code = action::clock::run_fix(opts)?;
                if exit_code != 0 {
                    std::process::exit(exit_code);
                }
                Ok(())
            }
            def::ClockCommands::List {
                paths,
                exclude_globs,
                filter,
                ignore_case,
                has_property,
                json,
            } => {
                let opts = action::clock::ListOptions {
                    paths,
                    exclude_globs,
                    filter,
                    ignore_case,
                    has_property,
                    json,
                };
                let exit_code = action::clock::run_list(opts)?;
                if exit_code != 0 {
                    std::process::exit(exit_code);
                }
                Ok(())
            }
            def::ClockCommands::Add {
                file,
                heading,
                start,
                end,
                tz,
                seconds,
                grid,
                write,
                json,
            } => {
                let opts = action::clock::AddOptions {
                    file,
                    heading,
                    start,
                    end,
                    tz,
                    seconds,
                    grid,
                    write,
                    json,
                };
                let exit_code = action::clock::run_add(opts)?;
                if exit_code != 0 {
                    std::process::exit(exit_code);
                }
                Ok(())
            }
            def::ClockCommands::Fmt {
                start,
                end,
                tz,
                seconds,
                grid,
            } => action::clock::run_fmt(action::clock::FmtOptions {
                start,
                end,
                tz,
                seconds,
                grid,
            }),
        },

        #[cfg(feature = "server")]
        Some(def::Commands::Daemon { paths, addr, db }) => {
            let opts = action::daemon::DaemonOptions { paths, addr, db };
            let exit_code = action::daemon::run(opts)?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
    }
}
