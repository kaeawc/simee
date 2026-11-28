//! What the server calls to do anything: [`Backend`], which is `sim-api`'s method table in
//! production and a fake in tests, and the worker threads that call it.

use std::cell::RefCell;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use serde_json::Value;

/// One API method call: the method's result, or its error text.
pub trait Backend: Send + Sync + 'static {
    fn call(&self, method: &str, params: &Value) -> Result<Value, String>;

    /// Whether the call `method` with `params` would be allowed now, without making it: what a
    /// policy's backend asks again before it shows more of a result it already read (the next page
    /// of a list). Everything is, unless a policy says otherwise.
    fn permits(&self, _method: &str, _params: &Value) -> Result<(), String> {
        Ok(())
    }
}

thread_local! {
    /// This thread's API session. A session is made on the thread that uses it (the iOS backend's
    /// service connection isn't `Send`), the way `sim-api`'s socket server makes one per connection.
    static SESSION: RefCell<Option<sim_api::Session>> = const { RefCell::new(None) };
}

/// `sim-api`'s methods, run in this process. Each thread that calls gets its own session, kept for
/// the thread's life, so a worker's service connection and per-device input connections stay warm
/// across calls.
pub struct ApiBackend {
    make: Box<dyn Fn() -> sim_api::Session + Send + Sync>,
}

impl ApiBackend {
    /// Sessions that read the SDK, AVD home and simulator service from the environment.
    pub fn new() -> ApiBackend {
        ApiBackend::with_sessions(sim_api::Session::default)
    }

    /// Sessions made by `make` (a test's own SDK and AVD home, say).
    pub fn with_sessions(make: impl Fn() -> sim_api::Session + Send + Sync + 'static) -> ApiBackend {
        ApiBackend { make: Box::new(make) }
    }
}

impl Default for ApiBackend {
    fn default() -> ApiBackend {
        ApiBackend::new()
    }
}

impl Backend for ApiBackend {
    fn call(&self, method: &str, params: &Value) -> Result<Value, String> {
        SESSION.with(|slot| {
            let mut slot = slot.borrow_mut();
            let session = slot.get_or_insert_with(|| (self.make)());
            sim_api::call_method(session, method, params)
        })
    }
}

type Job = Box<dyn FnOnce() + Send>;

/// A fixed set of threads that run requests, so a `boot` that takes a minute doesn't hold up the
/// `screenshot` after it, and each thread's session persists.
pub struct Executor {
    jobs: Sender<Job>,
}

impl Executor {
    /// How many requests can run at once.
    pub const WORKERS: usize = 8;

    pub fn new() -> Executor {
        let (jobs, queue) = channel::<Job>();
        let queue: Arc<Mutex<Receiver<Job>>> = Arc::new(Mutex::new(queue));
        for n in 0..Self::WORKERS {
            let queue = queue.clone();
            let spawned = std::thread::Builder::new().name(format!("simee-mcp-{n}")).spawn(move || {
                loop {
                    // The lock is held only while waiting for a job, not while running it.
                    let job = match queue.lock() {
                        Ok(queue) => queue.recv(),
                        Err(_) => return,
                    };
                    match job {
                        Ok(job) => {
                            // A panicking method fails its request, not the worker.
                            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                        }
                        Err(_) => return,
                    }
                }
            });
            if let Err(e) = spawned {
                eprintln!("simee mcp: cannot start a worker thread: {e}");
            }
        }
        Executor { jobs }
    }

    pub fn submit(&self, job: impl FnOnce() + Send + 'static) {
        // Workers only stop when the executor is dropped, so a send can't fail while it lives.
        let _ = self.jobs.send(Box::new(job));
    }
}

impl Default for Executor {
    fn default() -> Executor {
        Executor::new()
    }
}

#[cfg(test)]
pub mod fake {
    //! A scripted backend for the server's tests.

    use std::collections::HashMap;
    use std::sync::{Condvar, Mutex};

    use serde_json::{Value, json};

    use super::Backend;

    /// A scratch directory of its own, removed on drop.
    pub struct Scratch(pub std::path::PathBuf);

    impl Scratch {
        pub fn new(tag: &str) -> Scratch {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("simee-mcp-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch");
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A latch: calls to a method named in `block` wait until it is released.
    #[derive(Default)]
    pub struct Gate {
        open: Mutex<bool>,
        cv: Condvar,
    }

    impl Gate {
        pub fn release(&self) {
            *self.open.lock().expect("gate") = true;
            self.cv.notify_all();
        }

        fn wait(&self) {
            let mut open = self.open.lock().expect("gate");
            while !*open {
                open = self.cv.wait(open).expect("gate");
            }
        }
    }

    /// Answers methods from a table, records every call, and can hold a method until its gate opens.
    #[derive(Default)]
    pub struct Fake {
        pub answers: Mutex<HashMap<String, Result<Value, String>>>,
        pub calls: Mutex<Vec<(String, Value)>>,
        pub gates: Mutex<HashMap<String, std::sync::Arc<Gate>>>,
        /// What a method tells its caller it waits on (as admission does), before its gate.
        pub says: Mutex<HashMap<String, String>>,
        /// Whether the caller had cancelled each call once its gate opened: (method, cancelled).
        pub saw_cancel: Mutex<Vec<(String, bool)>>,
        /// Each call's method once it took its answer, under the answers' lock, so no later
        /// [`Fake::answer`] can be what that call returned.
        pub answered: Mutex<Vec<String>>,
    }

    impl Fake {
        pub fn answer(&self, method: &str, result: Result<Value, String>) {
            self.answers.lock().expect("answers").insert(method.to_owned(), result);
        }

        pub fn gate(&self, method: &str) -> std::sync::Arc<Gate> {
            let gate = std::sync::Arc::new(Gate::default());
            self.gates.lock().expect("gates").insert(method.to_owned(), gate.clone());
            gate
        }

        /// How many calls of `method` have taken their answer. [`Fake::called`] counts a call once
        /// it starts, before a gate and its hooks: a test that changes the answer next must wait on
        /// this instead, or the call it waited for may return the new answer.
        pub fn answered(&self, method: &str) -> usize {
            self.answered.lock().expect("answered").iter().filter(|m| *m == method).count()
        }

        pub fn called(&self, method: &str) -> Vec<Value> {
            self.calls.lock().expect("calls").iter().filter(|(m, _)| m == method).map(|(_, p)| p.clone()).collect()
        }

        /// A fake with the listings every test reads: two devices, one booted.
        pub fn with_devices() -> Fake {
            let fake = Fake::default();
            fake.answer(
                "devices",
                Ok(json!([
                    {"udid": "phone", "name": "phone", "state": "Booted", "platform": "android", "form_factor": "phone"},
                    {"udid": "0000AAAA-BBBB-CCCC-DDDD-EEEEFFFF0000", "name": "iPhone 17", "state": "Shutdown", "platform": "ios",
                     "form_factor": "phone"},
                ])),
            );
            fake
        }
    }

    impl Backend for Fake {
        fn call(&self, method: &str, params: &Value) -> Result<Value, String> {
            self.calls.lock().expect("calls").push((method.to_owned(), params.clone()));
            let says = self.says.lock().expect("says").get(method).cloned();
            if let Some(says) = says {
                sim_api::call_progress(&says);
            }
            let gate = self.gates.lock().expect("gates").get(method).cloned();
            if let Some(gate) = gate {
                gate.wait();
            }
            self.saw_cancel.lock().expect("saw_cancel").push((method.to_owned(), sim_api::call_cancelled()));
            let answers = self.answers.lock().expect("answers");
            self.answered.lock().expect("answered").push(method.to_owned());
            answers.get(method).cloned().unwrap_or(Ok(Value::Null))
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_fake_counts_a_call_answered_only_once_it_took_its_answer() {
        let fake = Arc::new(fake::Fake::default());
        fake.answer("info", Ok(json!("before")));
        let gate = fake.gate("info");
        let caller = fake.clone();
        let call = std::thread::spawn(move || caller.call("info", &json!({})));
        while fake.called("info").is_empty() {
            std::thread::yield_now();
        }
        // Started but not answered: an answer set now is the one it returns.
        assert_eq!(fake.answered("info"), 0);
        fake.answer("info", Ok(json!("after")));
        gate.release();
        assert_eq!(call.join().expect("call"), Ok(json!("after")));
        assert_eq!(fake.answered("info"), 1);
    }

    #[test]
    fn the_api_backend_keeps_a_session_per_thread_and_runs_methods() {
        let made = Arc::new(Mutex::new(0));
        let counter = made.clone();
        let backend = Arc::new(ApiBackend::with_sessions(move || {
            *counter.lock().expect("count") += 1;
            sim_api::Session::default()
        }));
        for _ in 0..3 {
            let help = backend.call("help", &Value::Null).expect("help");
            assert!(help.as_array().is_some_and(|m| m.iter().any(|x| x["method"] == "tap")));
        }
        assert_eq!(*made.lock().expect("count"), 1, "one session for this thread");
        let other = backend.clone();
        std::thread::spawn(move || other.call("help", &json!({})).expect("help")).join().expect("thread");
        assert_eq!(*made.lock().expect("count"), 2, "another thread, another session");
        assert!(backend.call("nope", &Value::Null).expect_err("unknown").contains("unknown method"));
    }

    #[test]
    fn the_executor_runs_jobs_in_parallel_and_survives_a_panic() {
        let executor = Executor::new();
        let (tx, rx) = channel();
        executor.submit(|| panic!("a method panicked"));
        // More jobs than workers still finish, and two of them wait for each other: they run at once.
        let barrier = Arc::new(std::sync::Barrier::new(2));
        for _ in 0..2 {
            let (tx, barrier) = (tx.clone(), barrier.clone());
            executor.submit(move || {
                barrier.wait();
                tx.send(1).ok();
            });
        }
        for n in 0..Executor::WORKERS + 2 {
            let tx = tx.clone();
            executor.submit(move || {
                tx.send(10 + n).ok();
            });
        }
        let done: Vec<usize> =
            (0..Executor::WORKERS + 4).map(|_| rx.recv_timeout(std::time::Duration::from_secs(10)).expect("job")).collect();
        assert_eq!(done.len(), Executor::WORKERS + 4);
    }
}
