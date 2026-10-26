//! `utitherm-cli` — automation interface for the UTi260B+ camera.

use std::process::ExitCode;

fn main() -> ExitCode {
    match utitherm::cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}
