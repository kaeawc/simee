//! The launcher's helper as an executable (see `simee_emu_container::helper`): for tests and for
//! packaging next to `emu`, which can instead run the same code under a hidden subcommand.
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    simee_emu_container::helper::main(&args)
}
