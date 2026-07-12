mod cli;
mod command;
mod config;
mod credential;
mod datetime;
mod detach;
mod doctor;
mod error;
mod job;
mod process;
mod release;
mod settings;
mod state;
mod tui;
mod version;
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
