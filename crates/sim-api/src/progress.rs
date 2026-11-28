//! What a long call tells its caller while it runs, and how it hears that the caller gave up
//! (#297). A front end (the MCP server) puts [`CallHooks`] on the thread that makes a call; the
//! call reports its steps through them ([`call_step`]: devices booted of a `boot_many`, the phases
//! of an `install`) and its waits ([`call_progress`]), and asks [`call_cancelled`] at its safe
//! points. [`interruptible`] runs a backend call on a thread of its own, so a call that can't look
//! at the flag itself (a boot in flight) is let go of at once, and undoes what it finishes after.
//! Without hooks (the CLI, the socket API) a call runs as it always has, until the process shuts
//! down: then [`call_cancelled`] is true for every call.

use std::cell::RefCell;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

/// One thing a call says about how far it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    /// What it does now: `waiting` (for admission), `booting`, `booted`, `installing`, ...
    pub phase: String,
    /// Units done (devices of a `boot_many`, phases of an `install`) of `total`.
    pub done: u64,
    /// How many units the call counts; `None` for a note on what it waits on, which counts nothing.
    pub total: Option<u64>,
    pub message: String,
}

/// What the caller of a call on this thread hands it: a cancel flag the call polls at its safe
/// points, and where it says how far it is (MCP's progress and task status).
pub struct CallHooks {
    pub cancelled: Box<dyn Fn() -> bool>,
    pub progress: Box<dyn Fn(&Progress)>,
}

thread_local! {
    static HOOKS: RefCell<Option<CallHooks>> = const { RefCell::new(None) };
}

/// Runs `f` (calls on this thread) with `hooks` in place; the previous ones come back after.
pub fn with_call_hooks<R>(hooks: CallHooks, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<CallHooks>);
    impl Drop for Restore {
        fn drop(&mut self) {
            HOOKS.with(|h| *h.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(HOOKS.with(|h| h.borrow_mut().replace(hooks)));
    f()
}

/// Whether a caller watches the call on this thread (it can cancel it and hears its steps).
pub(crate) fn has_call_hooks() -> bool {
    HOOKS.with(|h| h.borrow().is_some())
}

/// Whether the caller of the call on this thread cancelled it, or the process is shutting down
/// (a SIGINT or SIGTERM, [`crate::shutdown`]): every call in flight stops at its next safe point.
pub fn call_cancelled() -> bool {
    crate::shutdown::shutting_down() || HOOKS.with(|h| h.borrow().as_ref().is_some_and(|h| (h.cancelled)()))
}

fn report(p: &Progress) {
    HOOKS.with(|h| {
        if let Some(h) = h.borrow().as_ref() {
            (h.progress)(p);
        }
    });
}

/// Tells the caller of the call on this thread what it is waiting on (counting nothing).
pub fn call_progress(message: &str) {
    report(&Progress { phase: "waiting".into(), done: 0, total: None, message: message.to_owned() });
}

/// Tells the caller of the call on this thread that it is at `phase`, `done` of `total` units.
pub fn call_step(phase: &str, done: u64, total: u64, message: impl Into<String>) {
    report(&Progress { phase: phase.to_owned(), done, total: Some(total), message: message.into() });
}

/// What a backend's boot window (`boot_many`: at most `concurrency` devices booting at once, the
/// next started as soon as any finishes) tells and asks its caller about each device, from the
/// window's own threads (#297). `i` is the device's place in the call's `devices`.
pub(crate) trait BootHook: Sync {
    /// Asked before device `i` starts: `Err(why)` starts no more (each device not started gets
    /// `why` as its error).
    fn start(&self, i: usize) -> Result<(), String>;
    /// Asked just before the backend boots device `i`, which is off at that instant (iOS: shut
    /// down; Android: no emulator runs it): `Ok` and the call has started it, so it is the call's
    /// to shut down if cancelled; `Err(why)` boots it not (`why` is its error). Never asked for a
    /// device that was booted or booting when its turn came: the call only waits on that one.
    fn launching(&self, i: usize) -> Result<(), String>;
    /// Device `i` finished, booted or not: its entry in `boot_many`'s `devices`.
    fn finished(&self, i: usize, device: &serde_json::Value);
}

/// The hook of a backend `boot_many` called directly: every device starts, nothing is told.
pub(crate) struct Unwatched;

impl BootHook for Unwatched {
    fn start(&self, _: usize) -> Result<(), String> {
        Ok(())
    }

    fn launching(&self, _: usize) -> Result<(), String> {
        Ok(())
    }

    fn finished(&self, _: usize, _: &serde_json::Value) {}
}

/// What a single `boot` asks before it launches its device ([`launching`]).
type LaunchHook = Box<dyn Fn() -> Result<(), String>>;

thread_local! {
    static LAUNCH: RefCell<Option<LaunchHook>> = const { RefCell::new(None) };
}

/// Runs `f` (a single `boot` on this thread) with `hook` as what [`launching`] asks; the previous
/// one comes back after.
pub(crate) fn with_launch_hook<R>(hook: impl Fn() -> Result<(), String> + 'static, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<LaunchHook>);
    impl Drop for Restore {
        fn drop(&mut self) {
            LAUNCH.with(|h| *h.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(LAUNCH.with(|h| h.borrow_mut().replace(Box::new(hook))));
    f()
}

/// What a single `boot` on this thread asks just before the backend boots its device, which is
/// off at that instant, as [`BootHook::launching`] does for a `boot_many`'s: `Err(why)` boots it
/// not. Without a hook (an unwatched boot) it boots.
pub(crate) fn launching() -> Result<(), String> {
    LAUNCH.with(|h| h.borrow().as_ref().map_or(Ok(()), |hook| hook()))
}

/// How often a call whose work runs on another thread looks at its caller's cancel flag.
const WATCH: Duration = Duration::from_millis(20);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// How an [`interruptible`] job hands its result back: to the caller, or, once the caller has let
/// go of it, back to the job, which undoes it.
pub(crate) struct Handoff<T> {
    gone: Mutex<bool>,
    tx: Sender<T>,
}

impl<T> Handoff<T> {
    fn new() -> (Handoff<T>, Receiver<T>) {
        let (tx, rx) = channel();
        (Handoff { gone: Mutex::new(false), tx }, rx)
    }

    /// Hands `result` to the caller; `Some(result)` back when the caller has gone (cancelled), for
    /// the job to undo. Sent under the lock the caller takes to leave, so it is one or the other.
    pub(crate) fn give(&self, result: T) -> Option<T> {
        let gone = lock(&self.gone);
        if *gone {
            return Some(result);
        }
        let _ = self.tx.send(result);
        None
    }

    /// The caller leaves: a result already given is still taken, else the job keeps its own.
    fn leave(&self, rx: &Receiver<T>) -> Option<T> {
        *lock(&self.gone) = true;
        rx.try_recv().ok()
    }
}

/// Runs `job` on a thread of its own while this thread watches the caller's cancel flag: the result
/// the job gives, or `None` as soon as the caller cancels (checked every 20 ms). A job given back
/// its result by [`Handoff::give`] undoes it: its caller is gone. Without hooks on this thread
/// (nothing can cancel), runs `job` here.
pub(crate) fn interruptible<T: Send + 'static>(name: &str, job: impl FnOnce(&Handoff<T>) + Send + 'static) -> Result<Option<T>, String> {
    interruptible_watching(name, job, || ())
}

/// [`interruptible`], calling `watch` on this thread each time it looks at the cancel flag (every
/// 20 ms while the job runs elsewhere), so the call can report what its job has done so far: the
/// progress hooks belong to this thread. Without hooks it is never called.
pub(crate) fn interruptible_watching<T: Send + 'static>(
    name: &str,
    job: impl FnOnce(&Handoff<T>) + Send + 'static,
    mut watch: impl FnMut(),
) -> Result<Option<T>, String> {
    let (handoff, rx) = Handoff::new();
    if !has_call_hooks() {
        job(&handoff);
        return rx.try_recv().map(Some).map_err(|_| format!("`{name}` gave no result"));
    }
    let handoff = std::sync::Arc::new(handoff);
    let theirs = handoff.clone();
    let thread = std::thread::Builder::new()
        .name(format!("simee-{name}"))
        .spawn(move || job(&theirs))
        .map_err(|e| format!("cannot start a thread for `{name}`: {e}"))?;
    let ended = || format!("`{name}` ended without a result (its thread panicked)");
    loop {
        let got = rx.recv_timeout(WATCH);
        if got.is_err() {
            watch();
        }
        match got {
            Ok(result) => return Ok(Some(result)),
            // This thread's handoff holds a sender too, so the end is the thread's, not the channel's.
            Err(RecvTimeoutError::Timeout) if thread.is_finished() => return rx.try_recv().map(Some).map_err(|_| ended()),
            Err(RecvTimeoutError::Timeout) if call_cancelled() => return Ok(handoff.leave(&rx)),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err(ended()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    use super::*;

    /// Hooks whose cancel flag is `flag`, recording every step in `heard`.
    fn hooks(flag: &Arc<AtomicBool>, heard: &Arc<Mutex<Vec<Progress>>>) -> CallHooks {
        let (flag, heard) = (flag.clone(), heard.clone());
        CallHooks { cancelled: Box::new(move || flag.load(Ordering::Relaxed)), progress: Box::new(move |p| lock(&heard).push(p.clone())) }
    }

    #[test]
    fn steps_and_notes_reach_the_hooks_of_their_thread_only() {
        let (flag, heard) = (Arc::new(AtomicBool::new(false)), Arc::default());
        call_step("booting", 0, 2, "not heard: no hooks");
        assert!(!has_call_hooks() && !call_cancelled());
        with_call_hooks(hooks(&flag, &heard), || {
            assert!(has_call_hooks());
            call_step("booting", 1, 2, "a booted");
            call_progress("waiting at 5 s");
            std::thread::spawn(|| call_step("booting", 2, 2, "another thread's call")).join().expect("thread");
        });
        assert!(!has_call_hooks(), "the hooks are gone after the call");
        let heard = lock(&heard).clone();
        assert_eq!(
            heard,
            [
                Progress { phase: "booting".into(), done: 1, total: Some(2), message: "a booted".into() },
                Progress { phase: "waiting".into(), done: 0, total: None, message: "waiting at 5 s".into() },
            ]
        );
    }

    #[test]
    fn an_interruptible_job_returns_its_result_or_is_let_go_of_at_once_and_undoes_it() {
        // Without hooks it runs on this thread.
        let here = std::thread::current().id();
        let got = interruptible("t", move |h: &Handoff<_>| {
            h.give(std::thread::current().id() == here);
        });
        assert_eq!(got, Ok(Some(true)));
        let (flag, heard) = (Arc::new(AtomicBool::new(false)), Arc::default());
        // With hooks and no cancel: the job's result.
        let got = with_call_hooks(hooks(&flag, &heard), || {
            interruptible("t", |h: &Handoff<u32>| {
                h.give(7);
            })
        });
        assert_eq!(got, Ok(Some(7)));
        // Cancelled while the job runs: let go of within the watch interval, and the job, finishing
        // later, gets its result back to undo.
        let (release, wait) = channel::<()>();
        let (undone_tx, undone) = channel::<u32>();
        let cancel = flag.clone();
        let t = Instant::now();
        let got = with_call_hooks(hooks(&flag, &heard), || {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                cancel.store(true, Ordering::Relaxed);
            });
            interruptible("t", move |h: &Handoff<u32>| {
                wait.recv().ok();
                if let Some(r) = h.give(9) {
                    undone_tx.send(r).ok();
                }
            })
        });
        assert_eq!(got, Ok(None));
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        release.send(()).expect("release");
        assert_eq!(undone.recv_timeout(Duration::from_secs(5)), Ok(9), "the job undoes what it finished after its caller left");
        // A job that panics is an error, not a hang.
        flag.store(false, Ordering::Relaxed);
        let got = with_call_hooks(hooks(&flag, &heard), || interruptible("t", |_: &Handoff<u32>| panic!("boom")));
        assert!(got.expect_err("panicked").contains("panicked"));
    }

    #[test]
    fn a_result_given_as_the_caller_leaves_is_one_or_the_other() {
        let (h, rx) = Handoff::<u32>::new();
        assert_eq!(h.give(1), None, "handed to the caller");
        assert_eq!(h.leave(&rx), Some(1), "taken on the way out");
        assert_eq!(h.give(2), Some(2), "after the caller left, back to the job");
    }
}
