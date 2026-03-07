//! CLI argument definitions using clap.

use clap::{Parser, Subcommand};

/// Query and edit org-mode files from the command line.
#[derive(Parser, Debug)]
#[command(name = "org-cli")]
#[command(version, about, long_about = None)]
pub struct Args {
    /// Turn general verbose logging
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Configure component wise logging
    #[arg(long, short = 'l', action = clap::ArgAction::Append)]
    pub log: Option<Vec<String>>,

    /// prepend time to each log line
    #[arg(long)]
    pub log_time: bool,

    /// Subcommand to execute
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// Available commands
#[derive(Subcommand, Debug)]
pub enum Commands {
    // Commands will be added here as we implement them
}
