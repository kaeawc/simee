//! emu's shutdown signals under real SIGINT and SIGTERM (`signals/unix.rs`). Windows has no
//! signals; its console events are tested in `src/host_windows.rs`.

#[cfg(unix)]
#[path = "signals/unix.rs"]
mod unix;

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    unix::main()
}

#[cfg(not(unix))]
fn main() {}
