mod cli;
mod command;
mod config;
mod datetime;
mod detach;
mod doctor;
mod error;
mod job;
mod process;
mod state;
mod tui;
mod worker;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::Cli;

fn main() -> ExitCode {
    match cli::run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
