//! SIGINT and SIGTERM quit the app the way ⌘Q does (#553): every window's `App::on_quit` runs
//! (settings saved, capture threads and owned streams stopped), then the event loop ends.
//!
//! [`watch`] puts `signal-hook`'s iterator on a thread of its own: the signal handler only writes
//! to signal-hook's self-pipe, and that thread calls `quit`, which for the app is
//! `rune::AppHandle::quit`: it works from any thread and wakes the event loop. A second signal
//! while the first quit is still running exits at once with 128 + the signal, so a stuck
//! shutdown never needs SIGKILL. Windows has no signals here: Ctrl-C is left as it was.

#[cfg(unix)]
use signal_hook::consts::{SIGINT, SIGTERM};

/// Watches SIGINT and SIGTERM from now on; the first calls `quit`. Call it once per process.
#[cfg(unix)]
pub fn watch(quit: impl Fn() + Send + 'static) -> std::io::Result<()> {
    watch_signals(&[SIGINT, SIGTERM], quit, |sig| std::process::exit(128 + sig))
}

/// Windows has no signals, and Ctrl-C keeps its default.
#[cfg(not(unix))]
pub fn watch(_quit: impl Fn() + Send + 'static) -> std::io::Result<()> {
    Ok(())
}

/// [`watch`] for `signals`, with what a second signal does as `again` (a test cannot exit).
#[cfg(unix)]
fn watch_signals(signals: &[i32], quit: impl Fn() + Send + 'static, again: impl Fn(i32) + Send + 'static) -> std::io::Result<()> {
    let mut signals = signal_hook::iterator::Signals::new(signals)?;
    std::thread::Builder::new().name("simee-signals".into()).spawn(move || {
        let mut asked = false;
        for sig in signals.forever() {
            if asked {
                again(sig);
            } else {
                asked = true;
                quit();
            }
        }
    })?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use signal_hook::consts::{SIGUSR1, SIGUSR2};
    use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    #[test]
    fn the_first_signal_quits_and_a_second_takes_the_exit() {
        // SIGUSR1 and SIGUSR2 stand in for SIGINT and SIGTERM: the test process must survive.
        let quits = Arc::new(AtomicUsize::new(0));
        let again = Arc::new(AtomicI32::new(0));
        let (tx, rx) = mpsc::channel();
        let (q, a) = (quits.clone(), again.clone());
        let tx2 = tx.clone();
        watch_signals(
            &[SIGUSR1, SIGUSR2],
            move || {
                q.fetch_add(1, Ordering::SeqCst);
                tx.send(()).unwrap();
            },
            move |sig| {
                a.store(sig, Ordering::SeqCst);
                tx2.send(()).unwrap();
            },
        )
        .unwrap();
        signal_hook::low_level::raise(SIGUSR1).unwrap();
        rx.recv_timeout(Duration::from_secs(5)).expect("the first signal quits");
        assert_eq!(quits.load(Ordering::SeqCst), 1);
        assert_eq!(again.load(Ordering::SeqCst), 0);
        signal_hook::low_level::raise(SIGUSR2).unwrap();
        rx.recv_timeout(Duration::from_secs(5)).expect("the second signal takes the exit");
        assert_eq!(quits.load(Ordering::SeqCst), 1, "a second signal does not quit again");
        assert_eq!(again.load(Ordering::SeqCst), SIGUSR2);
    }
}
