//! `cargo xtask <task>`: repository automation that must run on every host, Windows included.
//! Today there is one task, `check`; see [`xtask::usage`].
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match xtask::run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(xtask::Failure::Usage(message)) => {
            eprintln!("{message}");
            ExitCode::from(2)
        }
        Err(xtask::Failure::Check) => ExitCode::FAILURE,
    }
}
