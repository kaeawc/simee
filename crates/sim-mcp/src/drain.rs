//! Shutting down on SIGINT or SIGTERM (#388): stop taking tool calls, let the ones in flight end.
//!
//! [`Drain`] is shared by every server of the process (stdio's one, and each of HTTP's), so the
//! transports need nothing of their own: the first signal's handler calls [`Drain::finish`],
//! which marks the process draining, waits up to a bound for the calls in flight (each stops at
//! its next safe point, as `sim-api` cancels every call once a signal arrives), and says whether
//! they all ended. Meanwhile a `tools/call` isn't run: it is answered `cancelled: server shutting
//! down`, and a question waiting for the user's answer is dropped with the same words. The
//! transports go on answering until the process exits, so every reply reaches the client first.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

/// How long a shutdown gives the calls in flight to finish, or to stop at a safe point, before it
/// stops the streams and exits anyway: the bound `simee serve` uses (`sim-api`'s `DRAIN`).
pub const DRAIN: Duration = Duration::from_secs(10);

/// What a call that the shutdown cancelled, or never ran, answers.
pub use sim_api::shutdown::SHUTTING_DOWN;

/// The calls in flight, and whether the process is draining.
#[derive(Default)]
pub struct Drain {
    begun: AtomicBool,
    running: Mutex<usize>,
    idle: Condvar,
}

static SHARED: OnceLock<Arc<Drain>> = OnceLock::new();

/// A call in flight: counted until it is dropped, which is after its reply is sent.
pub struct Held(Arc<Drain>);

impl Drop for Held {
    fn drop(&mut self) {
        let mut running = self.0.running();
        *running = running.saturating_sub(1);
        if *running == 0 {
            self.0.idle.notify_all();
        }
    }
}

impl Drain {
    /// What every server made without its own uses: the process's.
    pub fn shared() -> Arc<Drain> {
        SHARED.get_or_init(Arc::default).clone()
    }

    fn running(&self) -> MutexGuard<'_, usize> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the process is shutting down: this drain has begun, or `sim-api` saw the signal.
    pub fn draining(&self) -> bool {
        self.begun.load(Ordering::SeqCst) || sim_api::shutdown::shutting_down()
    }

    /// Counts a call about to run; `None` once the process is draining (the call isn't run).
    pub fn enter(self: &Arc<Drain>) -> Option<Held> {
        let mut running = self.running();
        if self.draining() {
            return None;
        }
        *running += 1;
        Some(Held(self.clone()))
    }

    /// Marks the process draining, then waits up to `limit` for the calls in flight to end;
    /// whether they all did.
    pub fn finish(&self, limit: Duration) -> bool {
        let mut running = self.running();
        // Under the lock the calls are counted by, so every call is either counted before this
        // or refused after it.
        self.begun.store(true, Ordering::SeqCst);
        let until = Instant::now() + limit;
        while *running > 0 {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            running = self.idle.wait_timeout(running, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        true
    }

    /// A call's outcome, saying so when the shutdown cancelled it: an error that ends a call once
    /// the process is draining is the shutdown's.
    pub fn outcome(&self, result: Result<Value, String>) -> Result<Value, String> {
        match result {
            Err(e) if self.draining() => Err(format!("{SHUTTING_DOWN} ({e})")),
            r => r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: Duration = Duration::from_millis(50);
    /// A deadline for a hang, never a measure of speed.
    const DEADLINE: Duration = Duration::from_secs(60);

    #[test]
    fn calls_enter_until_the_drain_begins_and_not_after() {
        let drain = Arc::new(Drain::default());
        let held = drain.enter().expect("enters");
        assert!(!drain.draining());
        assert!(!drain.finish(SHORT), "a call is still in flight");
        assert!(drain.draining() && drain.enter().is_none(), "no call enters once draining");
        drop(held);
        assert!(drain.finish(DEADLINE), "nothing in flight now");
    }

    #[test]
    fn finish_waits_for_the_calls_in_flight_and_returns_when_the_last_ends() {
        let drain = Arc::new(Drain::default());
        let (a, b) = (drain.enter().expect("a"), drain.enter().expect("b"));
        let waiting = {
            let drain = drain.clone();
            std::thread::spawn(move || drain.finish(DEADLINE))
        };
        let until = Instant::now() + DEADLINE;
        while !drain.draining() {
            assert!(Instant::now() < until, "finish never began");
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(a);
        assert!(!waiting.is_finished(), "one call still runs");
        drop(b);
        assert!(waiting.join().expect("joins"), "all ended in time");
    }

    #[test]
    fn an_error_ending_a_call_while_draining_says_the_shutdown_cancelled_it() {
        let drain = Arc::new(Drain::default());
        assert_eq!(drain.outcome(Err("boom".into())), Err("boom".into()), "not draining: as it was");
        drain.finish(SHORT);
        assert_eq!(drain.outcome(Err("boom".into())), Err(format!("{SHUTTING_DOWN} (boom)")));
        assert_eq!(drain.outcome(Ok(Value::Null)), Ok(Value::Null), "a call that finished is not cancelled");
    }
}
