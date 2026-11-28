//! emu's shutdown signals: SIGINT, SIGTERM and SIGHUP stop the machine the way the console's
//! `kill` does, a second one exits at once, and the process exits with 128 + the signal (130 for
//! SIGINT, 143 for SIGTERM) (#388).
//!
//! No handler runs: the signals are blocked in every thread and a thread of their own `sigwait`s
//! for them, so a native library that installs handlers of its own (the GPU renderer does) can't
//! swallow them. (`signal-hook`, which `simee serve` uses, installs a handler the renderer would
//! replace.) Built into emu and into `tests/signals.rs`, which sends real signals to a child.
// Only a host that boots stops a machine on a signal.
#![cfg_attr(
    not(any(all(target_os = "macos", target_arch = "aarch64"), all(target_os = "linux", target_arch = "x86_64"))),
    allow(dead_code)
)]

use std::process::ExitCode;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Blocks SIGINT/SIGTERM/SIGHUP in this thread (and so in every thread created after it,
/// including native libraries' threads) and returns a receiver that gets each one that arrives.
/// A dedicated thread `sigwait`s for them, so a library that installs its own handlers (the GPU
/// renderer does) can't swallow the shutdown signal.
#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn shutdown_signals() -> std::sync::mpsc::Receiver<i32> {
    // SAFETY: plain libc signal-mask calls on a stack-allocated, initialized sigset.
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::sigaddset(&mut set, sig);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    };
    // Native libraries may set these signals to SIG_IGN (the GPU renderer does), and ignored
    // signals are discarded even while blocked. Keep their disposition at SIG_DFL: blocked
    // everywhere, the default action never runs, and sigwait receives them.
    let started = std::time::Instant::now();
    let _ = std::thread::Builder::new().name("signal-guard".into()).spawn(move || {
        emu_hvf::host::set_qos(emu_hvf::host::Qos::Utility);
        loop {
            for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                // SAFETY: querying and restoring a disposition with zero-initialized sigactions.
                unsafe {
                    let mut cur: libc::sigaction = std::mem::zeroed();
                    if libc::sigaction(sig, std::ptr::null(), &mut cur) == 0 && cur.sa_sigaction != libc::SIG_DFL {
                        let mut dfl: libc::sigaction = std::mem::zeroed();
                        dfl.sa_sigaction = libc::SIG_DFL;
                        libc::sigaction(sig, &dfl, std::ptr::null_mut());
                    }
                }
            }
            std::thread::sleep(signal_guard_interval(started.elapsed()));
        }
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = std::thread::Builder::new().name("signals".into()).spawn(move || {
        emu_hvf::host::set_qos(emu_hvf::host::Qos::Utility);
        let mut sig = 0;
        // SAFETY: `set` is a valid sigset; sigwait writes the delivered signal number.
        while unsafe { libc::sigwait(&set, &mut sig) } == 0 {
            if tx.send(sig).is_err() {
                return;
            }
        }
    });
    rx
}

/// Windows has no signals: the console's control events (Ctrl-C, closing the window, logoff,
/// shutdown) arrive on the same receiver as the signal numbers they stand for.
#[cfg(windows)]
pub(crate) use crate::host_windows::shutdown_signals;

/// While native libraries may still be installing handlers, the signal guard checks often.
/// The renderer, which does, initializes in the first seconds (beside kernel load, hundreds of
/// ms) and once per process (guest resets reuse it); the window leaves room for a cold disk and
/// the first boot's GPU device setup.
#[cfg(unix)]
pub(crate) const SIGNAL_GUARD_FAST: std::time::Duration = std::time::Duration::from_secs(60);

/// How long the signal guard sleeps between checks, `since_start` after it started: 200 ms in
/// [`SIGNAL_GUARD_FAST`], then 5 s (an idle emulator's guard wakes 0.2 times a second, not 5).
#[cfg(unix)]
pub(crate) fn signal_guard_interval(since_start: std::time::Duration) -> std::time::Duration {
    if since_start < SIGNAL_GUARD_FAST { std::time::Duration::from_millis(200) } else { std::time::Duration::from_secs(5) }
}

/// The signal that started this process's stop; 0 while none has (a console `kill` or the
/// guest's own power-off stops it with 0).
static STOPPED_BY: AtomicI32 = AtomicI32::new(0);

/// How a stop that a signal started ended before the orderly path got there.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Forced {
    /// A second signal (its number) asked to exit at once.
    Second(i32),
    /// The stop took longer than its limit.
    Overrun,
}

/// The stop watcher: waits for the first signal (`None` once no more can come), records it and
/// runs `stop` (what the console's `kill` does), then waits up to `limit` for the orderly path,
/// which exits the process first when it gets there. What forces the exit otherwise, with the
/// signal whose exit code it is.
pub(crate) fn watch(signals: &Receiver<i32>, limit: Duration, stop: impl FnOnce()) -> Option<(i32, Forced)> {
    let sig = signals.recv().ok()?;
    STOPPED_BY.store(sig, Ordering::SeqCst);
    stop();
    let until = Instant::now() + limit;
    match signals.recv_timeout(limit) {
        Ok(second) => Some((second, Forced::Second(second))),
        Err(RecvTimeoutError::Timeout) => Some((sig, Forced::Overrun)),
        Err(RecvTimeoutError::Disconnected) => {
            std::thread::sleep(until.saturating_duration_since(Instant::now()));
            Some((sig, Forced::Overrun))
        }
    }
}

/// The exit code for a process `sig` stopped: 128 + its number.
pub(crate) fn exit_code(sig: i32) -> i32 {
    128 + sig
}

/// The exit status of a machine that stopped in order: 128 + the signal that stopped it, or
/// success when none did.
pub(crate) fn stopped() -> ExitCode {
    match STOPPED_BY.load(Ordering::SeqCst) {
        0 => ExitCode::SUCCESS,
        sig => ExitCode::from(u8::try_from(exit_code(sig)).unwrap_or(u8::MAX)),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Checks the guard makes in `[from, to)` of its life, following its own schedule.
    #[cfg(unix)]
    fn guard_checks(from: Duration, to: Duration) -> usize {
        let (mut t, mut n) = (Duration::ZERO, 0);
        while t < to {
            if t >= from {
                n += 1;
            }
            t += signal_guard_interval(t);
        }
        n
    }

    #[cfg(unix)]
    #[test]
    fn the_signal_guard_checks_often_at_startup_then_rarely() {
        // A handler a library installs during startup is undone within 200 ms.
        assert_eq!(signal_guard_interval(Duration::ZERO), Duration::from_millis(200));
        assert_eq!(signal_guard_interval(SIGNAL_GUARD_FAST - Duration::from_millis(1)), Duration::from_millis(200));
        assert_eq!(guard_checks(Duration::from_secs(1), Duration::from_secs(2)), 5);
        // Idle afterwards: at most one wakeup in any second (it was 5), and it never stops.
        for start in [SIGNAL_GUARD_FAST, SIGNAL_GUARD_FAST * 2, SIGNAL_GUARD_FAST * 10] {
            assert!(guard_checks(start, start + Duration::from_secs(1)) <= 1, "{start:?}");
            assert!(guard_checks(start, start + Duration::from_secs(10)) >= 1, "{start:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_second_signal_forces_the_exit_and_a_stuck_stop_overruns() {
        assert_eq!((exit_code(libc::SIGINT), exit_code(libc::SIGTERM), exit_code(libc::SIGHUP)), (130, 143, 129));
        // Here, not at the top: `tests/signals.rs` builds this module with `cfg(test)` but no
        // test functions.
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(libc::SIGINT).expect("first");
        tx.send(libc::SIGTERM).expect("second");
        let mut stopped = false;
        assert_eq!(watch(&rx, Duration::from_secs(60), || stopped = true), Some((libc::SIGTERM, Forced::Second(libc::SIGTERM))));
        assert!(stopped, "the first signal stops the machine");
        tx.send(libc::SIGINT).expect("first");
        assert_eq!(watch(&rx, Duration::ZERO, || ()), Some((libc::SIGINT, Forced::Overrun)));
        drop(tx);
        assert_eq!(watch(&rx, Duration::ZERO, || panic!("no signal came")), None);
    }
}
