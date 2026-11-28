//! What the host says about one process, read from the kernel without spawning anything: the
//! stock emulator's qemu is found by the PID in its lock, so these ask about that PID alone.
//!
//! A host-wide `ps -axo pid=,args=` costs ~60 ms on a host with ~1300 processes and `lsof -iTCP`
//! about as much, and a one-shot CLI call paid both every time (#256). Linux reads `/proc/<pid>`;
//! macOS reads libproc through `sim-coresim` (`sysctl KERN_PROCARGS2`, `proc_pid_rusage`,
//! `proc_pidfdinfo`), the calls `processes` and `connections` use. Another unix falls back to `ps`
//! and `lsof` for the one PID, and can't say when a process started.

use crate::listing::Host;

/// The host's own process table, read per PID.
pub struct Native;

impl Host for Native {
    fn start(&self, pid: i32) -> Option<u64> {
        imp::start(pid)
    }

    fn args(&self, pid: i32) -> Option<String> {
        imp::args(pid)
    }

    fn listening(&self, pid: i32) -> Vec<u16> {
        imp::listening(pid)
    }
}

/// A `KERN_PROCARGS2` record's argv as the command line `ps` prints: the words joined by spaces.
#[cfg(any(target_os = "macos", test))]
fn procargs_line(buf: &[u8]) -> Option<String> {
    let line = sim_host::usage::argv_from_procargs(buf).join(" ");
    let line = line.trim_end();
    (!line.is_empty()).then(|| line.to_owned())
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashSet;

    /// Field 22 of `/proc/<pid>/stat`, `starttime` (clock ticks after boot): with the PID, it
    /// names one process. The command name (field 2) may hold spaces and parentheses, so the
    /// fields are counted from the last `)`.
    pub fn start(pid: i32) -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        starttime(&stat)
    }

    pub(super) fn starttime(stat: &str) -> Option<u64> {
        // After the `)`: state (field 3), then fields 4.. so `starttime` is the 20th.
        stat.get(stat.rfind(')')? + 1..)?.split_whitespace().nth(19)?.parse().ok()
    }

    /// `/proc/<pid>/cmdline`'s NUL-separated words, joined by spaces; `None` for a process that
    /// has gone (or a kernel thread, which has none).
    pub fn args(pid: i32) -> Option<String> {
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let words: Vec<String> =
            raw.split(|&b| b == 0).filter(|w| !w.is_empty()).map(|w| String::from_utf8_lossy(w).into_owned()).collect();
        (!words.is_empty()).then(|| words.join(" "))
    }

    /// The TCP ports `pid` listens on: its socket inodes (`/proc/<pid>/fd`) found in the LISTEN
    /// rows of its network namespace's `tcp` and `tcp6` tables.
    pub fn listening(pid: i32) -> Vec<u16> {
        let inodes: HashSet<u64> = std::fs::read_dir(format!("/proc/{pid}/fd"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let target = std::fs::read_link(e.path()).ok()?;
                target.to_str()?.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
            })
            .collect();
        if inodes.is_empty() {
            return Vec::new();
        }
        ["tcp", "tcp6"]
            .iter()
            .filter_map(|t| std::fs::read_to_string(format!("/proc/{pid}/net/{t}")).ok())
            .flat_map(|table| listen_rows(&table, &inodes))
            .collect()
    }

    pub(super) fn listen_rows(table: &str, inodes: &HashSet<u64>) -> Vec<u16> {
        table
            .lines()
            .skip(1)
            .filter_map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                // `sl local_address rem_address st tx:rx tr:when retrnsmt uid timeout inode`; 0A is LISTEN.
                let (local, state, inode) = (f.get(1)?, f.get(3)?, f.get(9)?);
                (*state == "0A" && inodes.contains(&inode.parse().ok()?))
                    .then(|| u16::from_str_radix(local.rsplit(':').next()?, 16).ok())
                    .flatten()
            })
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn starttime_is_counted_from_the_last_parenthesis() {
            let stat = "4242 (qemu (x) sys) S 1 4242 4242 0 -1 4194560 100 0 0 0 5 3 0 0 20 0 12 0 987654 1000 200 18446744073709551615 0";
            assert_eq!(starttime(stat), Some(987654));
            assert_eq!(starttime("4242 (short"), None);
        }

        #[test]
        fn listen_rows_keep_only_this_process_listeners() {
            let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
                0: 0100007F:15B2 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 111 1 0 100 0 0 10 0\n\
                1: 0100007F:15B3 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 222 1 0 100 0 0 10 0\n\
                2: 0100007F:15B2 0100007F:C000 01 00000000:00000000 00:00000000 00000000  1000        0 111 1 0 100 0 0 10 0\n\
                3: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 333 1 0 100 0 0 10 0\n";
            let ours: HashSet<u64> = [111, 222].into();
            assert_eq!(listen_rows(table, &ours), [5554, 5555], "an established row and another process's listener are left out");
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use sim_coresim::procs;

    pub fn start(pid: i32) -> Option<u64> {
        procs::usage(pid).map(|u| u.start_ns)
    }

    pub fn args(pid: i32) -> Option<String> {
        super::procargs_line(&procs::procargs(pid)?)
    }

    pub fn listening(pid: i32) -> Vec<u16> {
        const IPPROTO_TCP: i32 = 6;
        const TSI_S_LISTEN: i32 = 1;
        procs::sockets(pid)
            .into_iter()
            .filter(|s| s.protocol == IPPROTO_TCP && s.tcp_state == Some(TSI_S_LISTEN))
            .map(|s| s.local.port())
            .collect()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use std::process::Command;

    pub fn start(_: i32) -> Option<u64> {
        None
    }

    pub fn args(pid: i32) -> Option<String> {
        let out = Command::new("ps").args(["-o", "args=", "-p", &pid.to_string()]).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        (out.status.success() && !text.is_empty()).then_some(text)
    }

    /// `lsof`'s lines end with `TCP 127.0.0.1:5750 (LISTEN)`.
    pub fn listening(pid: i32) -> Vec<u16> {
        let out = Command::new("lsof").args(["-nP", "-a", "-iTCP", "-sTCP:LISTEN", "-p", &pid.to_string()]).output();
        let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        text.lines()
            .filter(|l| l.contains("(LISTEN)"))
            .filter_map(|l| l.split_whitespace().rev().nth(1)?.rsplit(':').next()?.parse().ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(argc: i32, exe: &str, padding: usize, argv: &[&str], env: &[&str]) -> Vec<u8> {
        let mut b = argc.to_ne_bytes().to_vec();
        b.extend(exe.as_bytes());
        b.extend(std::iter::repeat_n(0, padding));
        for w in argv.iter().chain(env) {
            b.extend(w.as_bytes());
            b.push(0);
        }
        b
    }

    /// The host's own answers about real processes, on every host with a native `Host`: on macOS
    /// this is the libproc path, which a wrong answer would show as every stock emulator `Shutdown`.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn this_process_reads_back_as_itself() {
        let me = std::process::id() as i32;
        let start = Native.start(me).expect("start");
        assert_eq!(Native.start(me), Some(start), "stable while it lives");
        let exe = std::env::current_exe().expect("exe");
        let args = Native.args(me).expect("args");
        assert!(args.contains(&*exe.file_name().expect("name").to_string_lossy()), "{args}");
        let a = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let quiet = std::net::TcpStream::connect(a.local_addr().expect("addr")).expect("connect");
        let ports = Native.listening(me);
        assert!(ports.contains(&a.local_addr().expect("addr").port()), "{ports:?}");
        assert!(!ports.contains(&quiet.local_addr().expect("addr").port()), "a connected socket doesn't listen: {ports:?}");
        #[cfg(target_os = "linux")]
        if let Ok(b) = std::net::TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, 0)) {
            assert!(Native.listening(me).contains(&b.local_addr().expect("addr").port()));
        }
    }

    /// A process that has exited, once reaped, is nothing to the host: not a stale answer. Its PID
    /// may be handed to another process meanwhile, which is a different process by its start time.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn a_gone_process_reads_as_nothing() {
        let mut child = std::process::Command::new("sleep").arg("60").spawn().expect("sleep");
        let pid = child.id() as i32;
        // Between fork and exec the child still has its parent's command line.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !Native.args(pid).is_some_and(|a| a.starts_with("sleep") || a.contains("/sleep ")) {
            assert!(std::time::Instant::now() < deadline, "the child never exec'd sleep: {:?}", Native.args(pid));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let started = Native.start(pid).expect("start");
        child.kill().expect("kill");
        child.wait().expect("wait");
        match Native.start(pid) {
            None => assert_eq!((Native.args(pid), Native.listening(pid)), (None, Vec::new())),
            Some(now) => assert_ne!(now, started, "the PID was reused: another process, started later"),
        }
    }

    /// A stock qemu end to end through the real host: a process whose command line is a qemu's
    /// (`-avd NAME -port N`) and whose PID is in the AVD's lock lists as running, with those
    /// ports; once it exits it doesn't. On macOS this is the libproc path against a real process.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn a_process_with_a_qemu_command_line_is_found_through_the_real_host() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let root = std::env::temp_dir().join(format!("sa-host-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("lane.avd");
        std::fs::create_dir_all(&dir).expect("avd dir");
        std::fs::write(root.join("lane.ini"), format!("path={}\n", dir.display())).expect("pointer");
        // `sh -c 'read x' qemu-fake -avd lane -port 5900` blocks on its stdin with those words in
        // its command line.
        let mut qemu = Command::new("sh")
            .args(["-c", "read x", "qemu-fake", "-avd", "lane", "-port", "5900"])
            .stdin(Stdio::piped())
            .spawn()
            .expect("sh");
        std::fs::write(dir.join("hardware-qemu.ini.lock"), format!("{}\0", qemu.id())).expect("lock");
        let stock = crate::listing::StockCache::new(std::sync::Arc::new(Native));
        let avds = [("lane", dir.as_path())];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let found = loop {
            let found = stock.instances(avds, &[]);
            if !found.is_empty() || std::time::Instant::now() > deadline {
                break found;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(
            found.iter().map(|r| (r.avd_name.as_str(), r.console_port, r.adb_port, r.pid)).collect::<Vec<_>>(),
            [("lane", 5900, 5901, qemu.id() as i32)]
        );
        assert_eq!(stock.instances(avds, &[]), found, "and again, from memory");
        qemu.stdin.take().expect("stdin").write_all(b"\n").ok();
        qemu.wait().expect("wait");
        assert!(stock.instances(avds, &[]).is_empty(), "an exited process isn't listed");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_procargs_record_reads_as_the_command_line_without_the_environment() {
        let b = record(
            5,
            "/sdk/emulator/qemu/darwin-aarch64/qemu-system-aarch64-headless",
            3,
            &["qemu-system-aarch64-headless", "-avd", "Pixel_9", "-port", "5554"],
            &["HOME=/Users/me", "PATH=/usr/bin"],
        );
        assert_eq!(procargs_line(&b).as_deref(), Some("qemu-system-aarch64-headless -avd Pixel_9 -port 5554"));
        // Fewer words than argc says (a process still exec'ing) is what there is; none is nothing.
        assert_eq!(procargs_line(&record(0, "/x", 1, &[], &[])), None);
        assert_eq!(procargs_line(&[1, 0]), None);
        assert_eq!(procargs_line(&record(2, "/bin/sh", 1, &["sh"], &[])).as_deref(), Some("sh"));
    }
}
