//! Remembered stock-emulator lookups, so resolving a device costs a few file reads and one
//! process probe per running emulator (#256).
//!
//! Finding a stock emulator reads each AVD's `hardware-qemu.ini.lock` for a PID, checks that the
//! PID is alive (`kill(pid, 0)`), then asks the host for its command line (to tell a qemu for that
//! AVD from a program that reused the PID) and, when it has no `-port`, for its listening ports.
//! The host answers per PID from the kernel ([`crate::host::Native`]), so a first lookup costs
//! microseconds where a host-wide `ps` and `lsof` cost ~60 ms each.
//!
//! A qemu's command line and console ports never change while it lives, so a kept connection
//! remembers them by PID *and start time*: a PID the kernel hands to another program later starts
//! later, so its entry no longer matches and is asked about again. An emulator is never listed
//! from memory alone: every call reads the locks and checks liveness, so a new emulator is seen
//! at once and a stopped or dead one never is.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use emu_avd::{Env, Probe, Running};

/// What the host says about one process at a time.
pub trait Host: Send + Sync {
    /// When the process started, in any unit that differs between two processes that had the
    /// PID one after the other. `None` when it has gone, or when the host can't say (nothing is
    /// then remembered about it).
    fn start(&self, pid: i32) -> Option<u64>;
    /// Its command line; `None` when it has gone.
    fn args(&self, pid: i32) -> Option<String>;
    /// The TCP ports it listens on.
    fn listening(&self, pid: i32) -> Vec<u16>;
}

/// A qemu process as last seen.
struct Seen {
    start: u64,
    args: String,
    /// Its listening ports, once they include a console and adb pair (a booting emulator may not
    /// listen yet, so an incomplete answer is asked again).
    listening: Option<Vec<u16>>,
}

/// Stock emulators found by earlier lookups, shared by every clone of one [`crate::Android`].
pub struct StockCache {
    host: Arc<dyn Host>,
    by_pid: Mutex<HashMap<i32, Seen>>,
}

impl std::fmt::Debug for StockCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StockCache").field("pids", &self.lock().len()).finish()
    }
}

impl StockCache {
    pub fn new(host: Arc<dyn Host>) -> StockCache {
        StockCache { host, by_pid: Mutex::default() }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<i32, Seen>> {
        self.by_pid.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The stock emulators running among `avds` (name and directory, as [`Env::content_dir`]
    /// resolves them; left out: those `skip` names), asking the host only about a PID not seen at
    /// its current start time. Forgets emulators no lock names any more.
    pub fn instances<'a>(&self, avds: impl IntoIterator<Item = (&'a str, &'a Path)>, skip: &[Running]) -> Vec<Running> {
        self.scan(avds, skip, true)
    }

    /// [`StockCache::instances`] for the one AVD `name`: its lock only.
    pub fn instance(&self, env: &Env, name: &str, skip: &[Running]) -> Option<Running> {
        let dir = env.content_dir(name).ok()?;
        self.scan([(name, dir.as_path())], skip, false).pop()
    }

    fn scan<'a>(&self, avds: impl IntoIterator<Item = (&'a str, &'a Path)>, skip: &[Running], forget: bool) -> Vec<Running> {
        let mut seen = self.lock();
        let touched = std::cell::RefCell::new(HashSet::new());
        let table = std::cell::RefCell::new(std::mem::take(&mut *seen));
        let args = |pid: i32| {
            touched.borrow_mut().insert(pid);
            let start = self.host.start(pid);
            if let (Some(start), Some(s)) = (start, table.borrow().get(&pid))
                && s.start == start
            {
                return Some(s.args.clone());
            }
            let a = self.host.args(pid)?;
            // Only a qemu's command line is final (a launcher about to exec one is asked again),
            // and only when the PID kept its process while it was read.
            if a.contains("qemu")
                && let Some(start) = start
                && self.host.start(pid) == Some(start)
            {
                table.borrow_mut().insert(pid, Seen { start, args: a.clone(), listening: None });
            }
            Some(a)
        };
        let listening = |pid: i32| {
            let start = self.host.start(pid);
            let known = table.borrow().get(&pid).filter(|s| start == Some(s.start)).and_then(|s| s.listening.clone());
            if let Some(ports) = known {
                return ports;
            }
            let ports = self.host.listening(pid);
            if emu_avd::console_port_from_listening(&ports).is_some()
                && let Some(s) = table.borrow_mut().get_mut(&pid).filter(|s| start == Some(s.start))
            {
                s.listening = Some(ports.clone());
            }
            ports
        };
        let found = Env::stock_instances_in_with(avds, skip, &Probe { args: &args, listening: &listening });
        *seen = table.into_inner();
        if forget {
            let touched = touched.into_inner();
            seen.retain(|pid, _| touched.contains(pid));
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    /// What a fake process looks like to the host.
    #[derive(Clone)]
    struct Proc {
        start: u64,
        args: String,
        ports: Vec<u16>,
    }

    /// A host whose process table the test writes, counting how often it is asked.
    #[derive(Default)]
    struct FakeHost {
        procs: Mutex<BTreeMap<i32, Proc>>,
        /// Whether the host can't say when a process started.
        no_start: AtomicBool,
        /// Whether every `start` answer differs (a PID that changed process while it was read).
        shifting: AtomicBool,
        shifts: AtomicU64,
        args: AtomicUsize,
        lsof: AtomicUsize,
    }

    impl Host for FakeHost {
        fn start(&self, pid: i32) -> Option<u64> {
            let start = self.procs.lock().expect("procs").get(&pid)?.start;
            if self.no_start.load(Ordering::SeqCst) {
                return None;
            }
            Some(if self.shifting.load(Ordering::SeqCst) { start + 1000 * self.shifts.fetch_add(1, Ordering::SeqCst) } else { start })
        }
        fn args(&self, pid: i32) -> Option<String> {
            self.args.fetch_add(1, Ordering::SeqCst);
            self.procs.lock().expect("procs").get(&pid).map(|p| p.args.clone())
        }
        fn listening(&self, pid: i32) -> Vec<u16> {
            self.lsof.fetch_add(1, Ordering::SeqCst);
            self.procs.lock().expect("procs").get(&pid).map(|p| p.ports.clone()).unwrap_or_default()
        }
    }

    impl FakeHost {
        /// Process `pid`, started at tick `start`, as a qemu for `avd`, with or without `-port`.
        fn qemu(&self, pid: u32, start: u64, avd: &str, port: Option<u16>) {
            let args = match port {
                Some(p) => format!("/sdk/emulator/qemu/darwin-aarch64/qemu-system-aarch64-headless -avd {avd} -port {p}"),
                None => format!("/sdk/emulator/qemu/darwin-aarch64/qemu-system-aarch64-headless -avd {avd} -no-window"),
            };
            self.procs.lock().expect("procs").insert(pid as i32, Proc { start, args, ports: Vec::new() });
        }
        fn other(&self, pid: u32, start: u64, args: &str) {
            self.procs.lock().expect("procs").insert(pid as i32, Proc { start, args: args.into(), ports: Vec::new() });
        }
        fn listen(&self, pid: u32, ports: &[u16]) {
            self.procs.lock().expect("procs").get_mut(&(pid as i32)).expect("process").ports = ports.to_vec();
        }
        /// (command lines, listening sockets) asked for.
        fn asked(&self) -> (usize, usize) {
            (self.args.load(Ordering::SeqCst), self.lsof.load(Ordering::SeqCst))
        }
    }

    struct Home(PathBuf);

    impl Drop for Home {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// An AVD home with AVDs `names`, none running.
    fn home(tag: &str, names: &[&str]) -> (Home, Env) {
        let root = std::env::temp_dir().join(format!("sa-listing-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for name in names {
            let dir = root.join(format!("{name}.avd"));
            std::fs::create_dir_all(&dir).expect("avd dir");
            std::fs::write(root.join(format!("{name}.ini")), format!("path={}\n", dir.display())).expect("pointer");
        }
        let env = Env { sdk_root: None, avd_home: root.clone() };
        (Home(root), env)
    }

    /// Stock's lock: the qemu PID, then a NUL.
    fn lock(env: &Env, name: &str, pid: u32) {
        std::fs::write(env.avd_home.join(format!("{name}.avd/hardware-qemu.ini.lock")), format!("{pid}\0")).expect("lock");
    }

    fn unlock(env: &Env, name: &str) {
        std::fs::remove_file(env.avd_home.join(format!("{name}.avd/hardware-qemu.ini.lock"))).expect("unlock");
    }

    fn names(found: &[Running]) -> Vec<&str> {
        found.iter().map(|r| r.avd_name.as_str()).collect()
    }

    /// A process of our own that lives until killed, standing in for a qemu.
    fn process() -> std::process::Child {
        std::process::Command::new("sleep").arg("60").spawn().expect("sleep")
    }

    fn stop(mut child: std::process::Child) {
        child.kill().expect("kill");
        child.wait().expect("wait");
    }

    fn cache(host: &Arc<FakeHost>) -> StockCache {
        StockCache::new(host.clone())
    }

    fn all(stock: &StockCache, env: &Env, skip: &[Running]) -> Vec<Running> {
        stock.instances(avds(env).iter().map(|(n, d)| (n.as_str(), d.as_path())), skip)
    }

    fn avds(env: &Env) -> Vec<(String, PathBuf)> {
        env.list_avds().into_iter().filter_map(|n| Some((n.clone(), env.content_dir(&n).ok()?))).collect()
    }

    #[test]
    fn a_known_emulator_is_listed_again_without_asking_the_host_about_it() {
        let (_home, env) = home("hit", &["a", "b", "idle"]);
        let (qa, qb) = (process(), process());
        lock(&env, "a", qa.id());
        lock(&env, "b", qb.id());
        let host = Arc::new(FakeHost::default());
        host.qemu(qa.id(), 100, "a", Some(5750));
        // `b` was started without `-port`: its listening sockets say.
        host.qemu(qb.id(), 200, "b", None);
        host.listen(qb.id(), &[5563, 5562, 51000]);
        let stock = cache(&host);
        let first = all(&stock, &env, &[]);
        assert_eq!(names(&first), ["a", "b"]);
        assert_eq!(host.asked(), (2, 1), "each process's command line once, and the one without -port its sockets");
        for _ in 0..3 {
            assert_eq!(all(&stock, &env, &[]), first);
        }
        assert_eq!(host.asked(), (2, 1), "known qemus are not asked about again");
        // One device: answered the same way, no host question either.
        let a = stock.instance(&env, "a", &[]).expect("a");
        assert_eq!((a.console_port, a.adb_port, a.pid), (5750, 5751, qa.id() as i32));
        assert_eq!(stock.instance(&env, "b", &[]).map(|r| r.console_port), Some(5562));
        assert_eq!(host.asked(), (2, 1));
        stop(qa);
        stop(qb);
    }

    #[test]
    fn an_emulator_that_stops_is_gone_at_the_next_lookup() {
        let (_home, env) = home("stop", &["a", "b"]);
        let (qa, qb) = (process(), process());
        lock(&env, "a", qa.id());
        lock(&env, "b", qb.id());
        let host = Arc::new(FakeHost::default());
        host.qemu(qa.id(), 100, "a", Some(5750));
        host.qemu(qb.id(), 200, "b", Some(5562));
        let stock = cache(&host);
        assert_eq!(names(&all(&stock, &env, &[])), ["a", "b"]);
        // `a` shuts down cleanly: its lock goes, then its process.
        unlock(&env, "a");
        stop(qa);
        assert_eq!(names(&all(&stock, &env, &[])), ["b"]);
        assert!(stock.instance(&env, "a", &[]).is_none());
        assert_eq!(stock.lock().len(), 1, "what no lock names any more is forgotten");
        // `b` dies without removing its lock: a dead PID is never an emulator, listed or looked up.
        let pid_b = qb.id();
        stop(qb);
        assert!(stock.instance(&env, "b", &[]).is_none(), "a remembered emulator whose PID died");
        assert!(all(&stock, &env, &[]).is_empty());
        // A new emulator between calls is found by the next one.
        let qc = process();
        lock(&env, "a", qc.id());
        host.qemu(qc.id(), 300, "a", Some(5760));
        let found = all(&stock, &env, &[]);
        assert_eq!(names(&found), ["a"]);
        assert_eq!(found[0].console_port, 5760);
        assert_ne!(found[0].pid, pid_b as i32);
        stop(qc);
    }

    #[test]
    fn a_reused_pid_is_not_the_emulator_remembered_under_it() {
        let (_home, env) = home("reuse", &["a", "b"]);
        let q = process();
        lock(&env, "a", q.id());
        let host = Arc::new(FakeHost::default());
        host.qemu(q.id(), 100, "a", Some(5750));
        let stock = cache(&host);
        assert_eq!(names(&all(&stock, &env, &[])), ["a"]);
        // The emulator died without removing its lock and the kernel gave its PID to a vim: the
        // PID lives, with a later start, and is no qemu.
        host.other(q.id(), 900, "/usr/bin/vim notes.txt");
        assert!(all(&stock, &env, &[]).is_empty(), "same PID, new process");
        assert!(stock.instance(&env, "a", &[]).is_none());
        // Handed to a qemu of another AVD, or to one for this AVD on other ports: what that
        // process says, not what was remembered.
        host.qemu(q.id(), 950, "b", Some(5800));
        assert!(all(&stock, &env, &[]).is_empty(), "a qemu, but for b");
        host.qemu(q.id(), 990, "a", Some(5900));
        let found = all(&stock, &env, &[]);
        assert_eq!(found.first().map(|r| r.console_port), Some(5900), "the old ports are not remembered");
        stop(q);
    }

    #[test]
    fn a_pid_that_is_not_a_qemu_for_the_avd_is_asked_about_every_time() {
        let (_home, env) = home("notqemu", &["a"]);
        let other = process();
        lock(&env, "a", other.id());
        let host = Arc::new(FakeHost::default());
        host.other(other.id(), 100, "/usr/bin/vim notes.txt");
        let stock = cache(&host);
        assert!(all(&stock, &env, &[]).is_empty());
        assert!(all(&stock, &env, &[]).is_empty());
        assert_eq!(host.asked().0, 2, "a non-qemu answer is not remembered");
        // A launcher that has since exec'd its qemu is found.
        host.qemu(other.id(), 101, "a", Some(5770));
        assert_eq!(names(&all(&stock, &env, &[])), ["a"]);
        stop(other);
    }

    #[test]
    fn nothing_is_remembered_about_a_process_whose_start_the_host_cannot_say() {
        let (_home, env) = home("nostart", &["a"]);
        let q = process();
        lock(&env, "a", q.id());
        let host = Arc::new(FakeHost::default());
        host.qemu(q.id(), 100, "a", Some(5750));
        host.no_start.store(true, Ordering::SeqCst);
        let stock = cache(&host);
        assert_eq!(names(&all(&stock, &env, &[])), ["a"]);
        assert_eq!(names(&all(&stock, &env, &[])), ["a"]);
        assert_eq!(host.asked().0, 2, "asked each time: without a start time a reused PID couldn't be told");
        assert!(stock.lock().is_empty());
        // A PID that changes process between the two start reads isn't remembered either.
        host.no_start.store(false, Ordering::SeqCst);
        host.shifting.store(true, Ordering::SeqCst);
        assert_eq!(names(&all(&stock, &env, &[])), ["a"]);
        assert!(stock.lock().is_empty());
        stop(q);
    }

    #[test]
    fn ports_are_asked_again_until_a_console_and_adb_pair_listens() {
        let (_home, env) = home("ports", &["a"]);
        let q = process();
        lock(&env, "a", q.id());
        let host = Arc::new(FakeHost::default());
        host.qemu(q.id(), 100, "a", None);
        let stock = cache(&host);
        // Booting: nothing listens yet, so it isn't listed.
        assert!(all(&stock, &env, &[]).is_empty());
        host.listen(q.id(), &[5554, 5555]);
        assert_eq!(all(&stock, &env, &[]).first().map(|r| r.console_port), Some(5554));
        assert_eq!(host.asked(), (1, 2), "the command line was remembered, the empty ports were not");
        assert_eq!(all(&stock, &env, &[]).len(), 1);
        assert_eq!(host.asked(), (1, 2), "and now the ports are");
        stop(q);
    }

    #[test]
    fn the_listing_matches_an_unremembered_one() {
        let (_home, env) = home("same", &["a", "b", "dead", "idle", "simee"]);
        let (qa, qb, qs) = (process(), process(), process());
        lock(&env, "a", qa.id());
        lock(&env, "b", qb.id());
        lock(&env, "dead", 999_999_999);
        lock(&env, "simee", qs.id());
        let host = Arc::new(FakeHost::default());
        host.qemu(qa.id(), 1, "a", Some(5750));
        host.qemu(qb.id(), 2, "b", None);
        host.listen(qb.id(), &[5567, 5566]);
        host.qemu(qs.id(), 3, "simee", Some(5600));
        let skip = [Running { pid: 1, avd_name: "simee".into(), console_port: 5600, adb_port: 5601, avd_dir: None, simee: true }];
        let plain = env.stock_instances_among_with(
            &env.list_avds(),
            &skip,
            &Probe { args: &|pid| host.args(pid), listening: &|pid| host.listening(pid) },
        );
        assert_eq!(names(&plain), ["a", "b"]);
        let stock = cache(&host);
        assert_eq!(all(&stock, &env, &skip), plain);
        assert_eq!(all(&stock, &env, &skip), plain, "remembered");
        assert!(stock.instance(&env, "simee", &skip).is_none(), "a skipped AVD is simee's");
        // Another AVD home's lookups never answer for this one.
        let other = Env { sdk_root: None, avd_home: Path::new("/nonexistent-avd-home").to_owned() };
        assert!(stock.instance(&other, "a", &[]).is_none());
        stop(qa);
        stop(qb);
        stop(qs);
    }
}
