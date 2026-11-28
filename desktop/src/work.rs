//! Background work for the window, so `poll` never waits on a device, a socket or an encoder:
//! [`Worker`] runs jobs in order on a thread that owns what they need (an Android device's
//! console), and [`Outbox`] carries their status lines back to `poll`, waking the window.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};

/// Status lines that finished background work leaves for `poll`, which shows the newest.
#[derive(Clone, Default)]
pub struct Outbox {
    lines: Arc<Mutex<Vec<String>>>,
    waker: Option<rune::Waker>,
}

impl Outbox {
    pub fn new(waker: Option<rune::Waker>) -> Outbox {
        Outbox { lines: Arc::default(), waker }
    }

    /// Leaves a line and wakes the window.
    pub fn post(&self, line: String) {
        if let Ok(mut lines) = self.lines.lock() {
            lines.push(line);
        }
        if let Some(w) = &self.waker {
            w();
        }
    }

    pub fn take(&self) -> Vec<String> {
        self.lines.lock().map(|mut l| std::mem::take(&mut *l)).unwrap_or_default()
    }
}

/// What a job reports: a status line, nothing, or an error (also shown).
pub type Outcome = Result<Option<String>, String>;
type Job<T> = Box<dyn FnOnce(&T) -> Outcome + Send>;

/// Jobs queued or running at most: past this a stalled device (a console session can take
/// 12 s to fail) refuses more work rather than piling it up.
pub const QUEUE_LIMIT: usize = 32;

/// Jobs run one at a time, in the order they were queued, on a thread that owns a `T`. When the
/// worker drops (its screen is no longer shown), the jobs still queued are dropped unrun and the
/// one running finishes without posting: nothing reports on a device that isn't shown.
pub struct Worker<T> {
    jobs: Option<mpsc::Sender<Job<T>>>,
    pending: Arc<AtomicUsize>,
    cancelled: Arc<AtomicBool>,
    outbox: Outbox,
}

impl<T: Send + 'static> Worker<T> {
    pub fn start(name: &str, owned: T, outbox: Outbox) -> Worker<T> {
        let (tx, rx) = mpsc::channel::<Job<T>>();
        let pending = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicBool::new(false));
        let (left, stop, posts) = (pending.clone(), cancelled.clone(), outbox.clone());
        let spawned = std::thread::Builder::new().name(name.into()).spawn(move || {
            for job in rx {
                let outcome = (!stop.load(Ordering::Acquire)).then(|| job(&owned));
                match outcome {
                    Some(Ok(Some(line)) | Err(line)) if !stop.load(Ordering::Acquire) => posts.post(line),
                    _ => {}
                }
                // Idle only once the line is posted.
                left.fetch_sub(1, Ordering::AcqRel);
            }
        });
        Worker { jobs: spawned.is_ok().then_some(tx), pending, cancelled, outbox }
    }

    /// Queues `job` behind the ones already queued; `false` (and the job dropped, which the
    /// status line says) when [`QUEUE_LIMIT`] jobs are already waiting or the thread is gone.
    pub fn run(&self, job: impl FnOnce(&T) -> Outcome + Send + 'static) -> bool {
        let Some(jobs) = &self.jobs else { return false };
        if self.pending.fetch_add(1, Ordering::AcqRel) >= QUEUE_LIMIT {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            self.outbox.post(format!("the device isn't answering: {QUEUE_LIMIT} actions are waiting, this one was dropped"));
            return false;
        }
        if jobs.send(Box::new(job)).is_err() {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        true
    }

    /// No job queued or running: whatever the jobs sent has been sent.
    pub fn idle(&self) -> bool {
        self.pending.load(Ordering::Acquire) == 0
    }
}

impl<T> Drop for Worker<T> {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

/// Where a screenshot taken now is saved: in `folder` (the settings' screenshot folder) while it
/// is a directory, else the Desktop, or the temp dir without a home.
pub fn screenshot_path(folder: Option<&std::path::Path>) -> PathBuf {
    let desktop = || std::env::var("HOME").map(|h| PathBuf::from(h).join("Desktop")).unwrap_or_else(|_| std::env::temp_dir());
    let dir = folder.filter(|f| f.is_dir()).map_or_else(desktop, std::path::Path::to_path_buf);
    let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    unique_path(&dir, &format!("simee-{millis}"))
}

/// `<dir>/<stem>.png`, or `<stem>-2.png`, `-3`… when that name is taken: quick shots stack in the
/// preview, so two in the same moment must not overwrite one file.
fn unique_path(dir: &std::path::Path, stem: &str) -> PathBuf {
    let first = dir.join(format!("{stem}.png"));
    if !first.exists() {
        return first;
    }
    (2..).map(|n| dir.join(format!("{stem}-{n}.png"))).find(|p| !p.exists()).expect("a free name")
}

#[cfg(test)]
mod screenshot_name_tests {
    #[test]
    fn a_taken_name_gets_a_suffix_instead_of_being_overwritten() {
        let dir = std::env::temp_dir().join(format!("simee-shot-name-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let a = super::unique_path(&dir, "simee-1");
        std::fs::write(&a, b"x").expect("write");
        let b = super::unique_path(&dir, "simee-1");
        assert_ne!(a, b);
        assert!(b.ends_with("simee-1-2.png"), "{}", b.display());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Encodes a frame as PNG and writes it to `path` on its own thread; the result goes to `outbox`,
/// and the saved PNG to `saved` (for the screenshot preview).
pub fn save_png(frame: sim_core::frame::Frame, path: PathBuf, outbox: Outbox, saved: impl FnOnce(PathBuf, Vec<u8>) + Send + 'static) {
    let spawned = std::thread::Builder::new().name("simee-screenshot".into()).spawn({
        let outbox = outbox.clone();
        move || match write_png(&frame, &path) {
            Ok(png) => {
                outbox.post(format!("saved {}", path.display()));
                saved(path, png);
            }
            Err(e) => outbox.post(format!("screenshot failed: {e}")),
        }
    });
    if let Err(e) = spawned {
        outbox.post(format!("screenshot failed: {e}"));
    }
}

fn write_png(frame: &sim_core::frame::Frame, path: &std::path::Path) -> Result<Vec<u8>, String> {
    let png = sim_core::frame::encode_png(frame)?;
    std::fs::write(path, &png).map_err(|e| e.to_string())?;
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn jobs_run_in_order_off_the_calling_thread_and_report_through_the_outbox() {
        let woken = Arc::new(AtomicUsize::new(0));
        let w = woken.clone();
        let outbox = Outbox::new(Some(Arc::new(move || {
            w.fetch_add(1, Ordering::Relaxed);
        })));
        let caller = std::thread::current().id();
        let worker = Worker::start("test-worker", Mutex::new(Vec::new()), outbox.clone());
        let (release, gate) = mpsc::channel::<()>();
        // A slow job holds the queue; the ones behind it wait their turn.
        worker.run(move |log: &Mutex<Vec<u32>>| {
            gate.recv().ok();
            log.lock().map_err(|e| e.to_string())?.push(1);
            Ok(None)
        });
        worker.run(move |log| {
            assert_ne!(std::thread::current().id(), caller, "jobs run on the worker's thread");
            log.lock().map_err(|e| e.to_string())?.push(2);
            Err("console: no reply".into())
        });
        worker.run(|log| Ok(Some(format!("done {:?}", log.lock().map_err(|e| e.to_string())?))));
        assert!(!worker.idle(), "three jobs pending");
        release.send(()).expect("release");
        let start = std::time::Instant::now();
        while !worker.idle() || woken.load(Ordering::Relaxed) < 2 {
            assert!(start.elapsed() < Duration::from_secs(5), "jobs finished");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(outbox.take(), ["console: no reply", "done [1, 2]"]);
        assert!(outbox.take().is_empty());
    }

    /// A stalled device holds the queue: past [`QUEUE_LIMIT`] jobs are refused (and the status
    /// line says so); once the worker drops (its screen is gone), the jobs still queued never run
    /// and the one running posts nothing.
    #[test]
    fn a_full_queue_refuses_work_and_a_dropped_worker_stays_quiet() {
        let outbox = Outbox::default();
        let worker = Worker::start("test-bound", (), outbox.clone());
        let (release, gate) = mpsc::channel::<()>();
        let (started, running) = mpsc::channel::<()>();
        assert!(worker.run(move |()| {
            started.send(()).ok();
            gate.recv().ok();
            Ok(Some("rotated the old device".into()))
        }));
        running.recv().expect("first job running");
        let ran = Arc::new(AtomicUsize::new(0));
        for _ in 1..QUEUE_LIMIT {
            let r = ran.clone();
            assert!(worker.run(move |()| {
                r.fetch_add(1, Ordering::Relaxed);
                Ok(Some("queued".into()))
            }));
        }
        assert!(!worker.run(|()| Ok(None)), "the queue is full");
        assert_eq!(outbox.take().len(), 1, "one line says an action was dropped");
        let pending = worker.pending.clone();
        drop(worker);
        release.send(()).expect("release");
        let start = std::time::Instant::now();
        while pending.load(Ordering::Acquire) > 0 {
            assert!(start.elapsed() < Duration::from_secs(5), "drained");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(ran.load(Ordering::Relaxed), 0, "queued jobs for a gone screen don't run");
        assert!(outbox.take().is_empty(), "nothing posted for a gone screen");
    }

    #[test]
    fn screenshots_are_written_off_the_ui_thread() {
        let path = std::env::temp_dir().join(format!("rs-shot-{}.png", std::process::id()));
        let outbox = Outbox::default();
        save_png(sim_core::frame::Frame { width: 2, height: 1, rgba: vec![255; 8] }, path.clone(), outbox.clone(), |_, _| {});
        let start = std::time::Instant::now();
        let line = loop {
            if let Some(l) = outbox.take().pop() {
                break l;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "saved");
            std::thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(line, format!("saved {}", path.display()));
        assert_eq!(std::fs::read(&path).map(|b| b[..8].to_vec()).ok(), Some(b"\x89PNG\r\n\x1a\n".to_vec()));
        let _ = std::fs::remove_file(path);
    }
}
