use colored::*;
use std::process;

fn main() {
    if let Err(e) = org_cli::cli::run() {
        eprintln!("{}: {e}", "Error".bright_red());
        process::exit(1);
    }
}
