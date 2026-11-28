//! Long jobs on a device (booting, shutting down, folding, CarPlay), kept per device (#558): each
//! device has at most one, keyed by its UDID or AVD name, runs on a thread of its own and reports
//! on its own. The stage and the toolbar's indicator show the selected device's job only, the
//! list row's mark shows each device's, and work for the whole window (a refresh) has its own
//! indicator in the sidebar. Two boots at once each finish, and fail, by themselves.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// A job's work: what it returns is the device's status line when it fails.
pub type Work = Box<dyn FnOnce() -> Result<(), String> + Send>;

/// Makes the work that boots a device: the live window's starts the simulator service or the
/// emulator; tests and headless scenes give their own.
pub type Booter = Arc<dyn Fn(&sim_core::device::DeviceInfo) -> Option<Work> + Send + Sync>;

/// The label a boot runs under (and what the stage waits on).
pub const BOOTING: &str = "Booting…";

/// One device's job: what it is, and what its thread leaves when it's done.
struct Job {
    label: &'static str,
    done: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
}

/// A job that finished: the device, what it was, and how it went.
#[derive(Debug, PartialEq)]
pub struct Finished {
    pub udid: String,
    pub label: &'static str,
    pub result: Result<(), String>,
}

/// Every device's job in flight.
#[derive(Default)]
pub struct Jobs {
    running: BTreeMap<String, Job>,
}

impl Jobs {
    /// Runs `work` for `udid` on a thread, under `label` ("Booting…"); `waker` wakes the window
    /// when it's done. Refused (`Err` with the running job's label) while that device has a job:
    /// a second one would race the first.
    pub fn start(&mut self, udid: &str, label: &'static str, work: Work, waker: Option<rune::Waker>) -> Result<(), &'static str> {
        if let Some(job) = self.running.get(udid) {
            return Err(job.label);
        }
        let (done, error) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(None)));
        let (d, e) = (done.clone(), error.clone());
        let spawned = std::thread::Builder::new().name(format!("simee-job-{udid}")).spawn(move || {
            if let Err(msg) = work()
                && let Ok(mut slot) = e.lock()
            {
                *slot = Some(msg);
            }
            d.store(true, Ordering::Release);
            if let Some(w) = waker {
                w();
            }
        });
        if let Err(err) = spawned {
            // No thread: the job ends at once, saying why.
            if let Ok(mut slot) = error.lock() {
                *slot = Some(format!("no thread for {label}: {err}"));
            }
            done.store(true, Ordering::Release);
        }
        self.running.insert(udid.to_owned(), Job { label, done, error });
        Ok(())
    }

    /// The job `udid` has running ("Booting…"), if any.
    pub fn label(&self, udid: &str) -> Option<&'static str> {
        self.running.get(udid).map(|j| j.label)
    }

    /// Whether `udid` is booting from here.
    pub fn booting(&self, udid: &str) -> bool {
        self.label(udid) == Some(BOOTING)
    }

    /// Whether any device has a job running.
    pub fn any(&self) -> bool {
        !self.running.is_empty()
    }

    /// The devices other than `except` with a job, and those jobs' labels, in UDID order.
    pub fn others<'a>(&'a self, except: Option<&'a str>) -> impl Iterator<Item = (&'a str, &'static str)> + 'a {
        self.running.iter().filter(move |(u, _)| Some(u.as_str()) != except).map(|(u, j)| (u.as_str(), j.label))
    }

    /// Takes the jobs that are done, each with its own result.
    pub fn finished(&mut self) -> Vec<Finished> {
        let done: Vec<String> = self.running.iter().filter(|(_, j)| j.done.load(Ordering::Acquire)).map(|(u, _)| u.clone()).collect();
        done.into_iter()
            .filter_map(|udid| {
                let job = self.running.remove(&udid)?;
                let error = job.error.lock().ok().and_then(|mut e| e.take());
                Some(Finished { udid, label: job.label, result: error.map_or(Ok(()), Err) })
            })
            .collect()
    }
}

/// What the sidebar's window-wide indicator says about the devices other than the one shown:
/// "1 device booting", "2 devices busy"; `None` when no other device has a job.
pub fn others_summary(jobs: &Jobs, selected: Option<&str>) -> Option<String> {
    let labels: Vec<&str> = jobs.others(selected).map(|(_, l)| l).collect();
    let n = labels.len();
    let devices = if n == 1 { "device" } else { "devices" };
    match n {
        0 => None,
        _ if labels.iter().all(|l| *l == BOOTING) => Some(format!("{n} {devices} booting")),
        _ => Some(format!("{n} {devices} busy")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Work that waits for `gate` and then returns `result`.
    fn gated(result: Result<(), String>) -> (mpsc::Sender<()>, Work) {
        let (tx, rx) = mpsc::channel::<()>();
        (
            tx,
            Box::new(move || {
                rx.recv().ok();
                result
            }),
        )
    }

    fn wait_finished(jobs: &mut Jobs) -> Vec<Finished> {
        let start = Instant::now();
        loop {
            let f = jobs.finished();
            if !f.is_empty() {
                return f;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "a job finished");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Two boots at once: each finishes, and reports, on its own; the other keeps running.
    #[test]
    fn two_devices_boot_at_once_and_each_reports_on_its_own() {
        let mut jobs = Jobs::default();
        let (a, work_a) = gated(Err("A: no display".into()));
        let (b, work_b) = gated(Ok(()));
        jobs.start("A", BOOTING, work_a, None).expect("A starts");
        jobs.start("B", BOOTING, work_b, None).expect("B starts beside it");
        assert!(jobs.booting("A") && jobs.booting("B"));
        assert_eq!(others_summary(&jobs, Some("A")).as_deref(), Some("1 device booting"));
        assert_eq!(others_summary(&jobs, None).as_deref(), Some("2 devices booting"));
        a.send(()).expect("release A");
        assert_eq!(wait_finished(&mut jobs), [Finished { udid: "A".into(), label: BOOTING, result: Err("A: no display".into()) }]);
        assert!(!jobs.booting("A"), "A's job is gone");
        assert!(jobs.booting("B"), "B still boots");
        b.send(()).expect("release B");
        assert_eq!(wait_finished(&mut jobs), [Finished { udid: "B".into(), label: BOOTING, result: Ok(()) }]);
        assert!(!jobs.any());
    }

    /// One job per device: a second on the same device is refused while the first runs, and
    /// another device's isn't.
    #[test]
    fn a_device_runs_one_job_at_a_time() {
        let mut jobs = Jobs::default();
        let (gate, work) = gated(Ok(()));
        jobs.start("A", "Attaching CarPlay…", work, None).expect("starts");
        assert_eq!(jobs.start("A", "Folding…", Box::new(|| Ok(())), None), Err("Attaching CarPlay…"));
        assert!(jobs.start("B", "Folding…", Box::new(|| Ok(())), None).is_ok());
        assert_eq!(others_summary(&jobs, Some("B")).as_deref(), Some("1 device busy"));
        gate.send(()).expect("release");
        let start = Instant::now();
        while jobs.any() {
            jobs.finished();
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
