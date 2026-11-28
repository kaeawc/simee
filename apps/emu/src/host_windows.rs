//! `emu`'s process plumbing on Windows, where `main.rs`'s unix versions don't exist: the stop
//! request comes from the console's control events instead of signals, and there is no
//! open-file limit to raise.
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Mutex, PoisonError};

const CTRL_C_EVENT: u32 = 0;
const CTRL_BREAK_EVENT: u32 = 1;
const CTRL_CLOSE_EVENT: u32 = 2;
const CTRL_LOGOFF_EVENT: u32 = 5;
const CTRL_SHUTDOWN_EVENT: u32 = 6;

#[allow(unsafe_code)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
}

static STOP: Mutex<Option<Sender<i32>>> = Mutex::new(None);

/// The signal number a console control event stands for (what `main.rs`'s receiver carries on
/// unix): SIGINT (2) for Ctrl-C and Ctrl-Break, SIGHUP (1) when the console closes, SIGTERM (15)
/// when the user logs off or the machine shuts down; `None` for an event this doesn't handle.
fn signal_for(event: u32) -> Option<i32> {
    match event {
        CTRL_C_EVENT | CTRL_BREAK_EVENT => Some(2),
        CTRL_CLOSE_EVENT => Some(1),
        CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => Some(15),
        _ => None,
    }
}

/// Runs on a thread the system creates for the event: tells the receiver and says it's handled.
#[allow(unsafe_code)]
unsafe extern "system" fn on_event(event: u32) -> i32 {
    let Some(signal) = signal_for(event) else { return 0 };
    if let Some(tx) = STOP.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
        let _ = tx.send(signal);
    }
    1
}

/// A receiver that fires once the console asks the process to stop (Ctrl-C, Ctrl-Break, closing
/// the window, logoff, shutdown), as `main.rs`'s does for signals on unix.
#[allow(unsafe_code)]
pub fn shutdown_signals() -> Receiver<i32> {
    let (tx, rx) = channel();
    *STOP.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
    // SAFETY: `on_event` has the HandlerRoutine signature and lives for the whole process.
    let _ = unsafe { SetConsoleCtrlHandler(Some(on_event), 1) };
    rx
}

/// Windows has no descriptor limit to raise (sockets are handles): what the limit would allow.
pub fn raise_nofile() -> u64 {
    u64::MAX
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_events_map_to_the_signals_they_stand_for() {
        assert_eq!(signal_for(CTRL_C_EVENT), Some(2));
        assert_eq!(signal_for(CTRL_BREAK_EVENT), Some(2));
        assert_eq!(signal_for(CTRL_CLOSE_EVENT), Some(1));
        assert_eq!(signal_for(CTRL_LOGOFF_EVENT), Some(15));
        assert_eq!(signal_for(CTRL_SHUTDOWN_EVENT), Some(15));
        assert_eq!(signal_for(3), None);
    }

    #[allow(unsafe_code)]
    #[test]
    fn a_control_event_reaches_the_receiver() {
        let rx = shutdown_signals();
        // SAFETY: the handler only touches the process's own state.
        assert_eq!(unsafe { on_event(CTRL_C_EVENT) }, 1);
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)), Ok(2));
        // SAFETY: as above.
        assert_eq!(unsafe { on_event(3) }, 0, "an event it doesn't handle goes on to the next handler");
    }
}
