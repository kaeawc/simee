//! A running device's CPU, over adb, for a settle wait: one probe reads the guest's uptime,
//! whether it finished booting, and every process's accumulated CPU time from `/proc/<pid>/stat`
//! (kernel threads included), in one `adb shell` call.
//!
//! The probe's own shell and the `cat` it runs are left out, so sampling doesn't make the device
//! look busier than it is. adbd, which serves the probe, is counted: a few milliseconds a sample.

use crate::apps::Adb;

/// What one probe of a running device read.
#[derive(Clone, Debug, PartialEq)]
pub struct CpuSample {
    /// The guest's `/proc/uptime`, seconds: when the sample was taken, on the guest's clock.
    pub uptime_s: f64,
    /// `sys.boot_completed` is 1.
    pub boot_completed: bool,
    /// Every process but the probe's own.
    pub procs: Vec<ProcCpu>,
}

impl CpuSample {
    /// Whether a process of this name is running (`system_server`, `com.android.systemui`).
    pub fn running(&self, name: &str) -> bool {
        self.procs.iter().any(|p| p.name == name)
    }
}

/// One guest process's CPU so far.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcCpu {
    pub pid: i32,
    pub ppid: i32,
    /// The kernel's name for it (`comm`, at most 15 bytes).
    pub name: String,
    /// User plus system CPU time since it started, seconds.
    pub cpu_s: f64,
}

/// The probe: the shell's own pid (to leave it and its `cat` out), the guest's uptime, the boot
/// flag, then every process's stat line. A process that exits between the glob and the read is
/// skipped by `cat` (its error dropped); `true` makes that no failure.
pub const PROBE: &str = "echo @self $$; cat /proc/uptime; getprop sys.boot_completed; cat /proc/[0-9]*/stat 2>/dev/null; true";

/// Linux's `USER_HZ`, the unit of `/proc/<pid>/stat`'s CPU times: 100 on every Android ABI.
const USER_HZ: f64 = 100.0;

impl Adb {
    /// One [`PROBE`] of the device: a single `adb shell` call.
    pub fn cpu_sample(&self) -> Result<CpuSample, String> {
        let (_, out) = self.run(&["shell", PROBE], self.deadlines.query)?;
        parse_sample(&out).ok_or_else(|| crate::apps::failure("the CPU probe", &out))
    }
}

/// [`PROBE`]'s output as a sample; `None` when it has no `@self` line or no uptime (adb printed
/// an error instead).
pub fn parse_sample(text: &str) -> Option<CpuSample> {
    let mut lines = text.lines().map(str::trim);
    let me: i32 = lines.next()?.strip_prefix("@self ")?.trim().parse().ok()?;
    let uptime_s: f64 = lines.next()?.split_whitespace().next()?.parse().ok()?;
    let boot_completed = lines.next()? == "1";
    let procs = lines.filter_map(parse_stat).filter(|p| p.pid != me && p.ppid != me).collect();
    Some(CpuSample { uptime_s, boot_completed, procs })
}

/// One `/proc/<pid>/stat` line. The name is in parentheses and may hold spaces and parentheses
/// itself, so the fields are counted from the last `)`.
fn parse_stat(line: &str) -> Option<ProcCpu> {
    let (head, rest) = line.rsplit_once(')')?;
    let (pid, name) = head.split_once(" (")?;
    // After the name: state (field 3), ppid (4), ... utime (14), stime (15).
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let num = |i: usize| fields.get(i).and_then(|f| f.parse::<u64>().ok());
    Some(ProcCpu {
        pid: pid.trim().parse().ok()?,
        ppid: fields.get(1)?.parse().ok()?,
        name: name.to_owned(),
        cpu_s: (num(11)? + num(12)?) as f64 / USER_HZ,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What an API 36 guest's probe prints (trimmed): the probe's shell is 7000, its `cat` 7001.
    const PROBE_OUT: &str = "@self 7000
4217.43 15890.12
1
1 (init) S 0 1 1 0 -1 4194560 31264 1031570 213 2042 163 640 2513 2766 20 0 1 0 2 11186176 2826 18446744073709551615 1 1 0 0 0 0 0 0 0
2 (kthreadd) S 0 0 0 0 -1 2129984 0 0 0 0 0 12 0 0 20 0 1 0 2 0 0 18446744073709551615 0 0 0 0 0 0 0 0 0
612 (system_server) S 310 310 0 0 -1 1077936448 512033 0 4209 0 31710 9050 0 0 18 -2 213 0 1034 0 0
900 (ndroid.systemui) S 310 310 0 0 -1 1077936448 9 0 0 0 1200 300 0 0 20 0 50 0 1100 0 0
950 (weird ) name)) S 1 950 0 0 -1 0 0 0 0 0 7 3 0 0 20 0 1 0 1200 0 0
7000 (sh) S 206 7000 0 0 -1 4194560 300 0 0 0 1 1 0 0 20 0 1 0 421743 0 0
7001 (cat) R 7000 7000 0 0 -1 4194560 120 0 0 0 0 1 0 0 20 0 1 0 421744 0 0
";

    #[test]
    fn a_probe_reads_uptime_the_boot_flag_and_each_processes_cpu() {
        let s = parse_sample(PROBE_OUT).expect("sample");
        assert_eq!((s.uptime_s, s.boot_completed), (4217.43, true));
        let pids: Vec<i32> = s.procs.iter().map(|p| p.pid).collect();
        assert_eq!(pids, [1, 2, 612, 900, 950], "the probe's shell and its cat are left out");
        let server = &s.procs[2];
        assert_eq!((server.name.as_str(), server.ppid), ("system_server", 310));
        // utime 31710 + stime 9050 ticks at 100 Hz.
        assert!((server.cpu_s - 407.6).abs() < 1e-9, "{}", server.cpu_s);
        assert!(s.running("system_server") && !s.running("SpringBoard"));
    }

    #[test]
    fn a_name_with_spaces_and_parentheses_is_read_from_the_last_one() {
        let s = parse_sample(PROBE_OUT).expect("sample");
        let weird = s.procs.iter().find(|p| p.pid == 950).expect("950");
        assert_eq!((weird.name.as_str(), weird.ppid), ("weird ) name)", 1));
        assert!((weird.cpu_s - 0.1).abs() < 1e-9);
    }

    #[test]
    fn a_guest_still_booting_and_an_adb_error_read_as_such() {
        let booting = PROBE_OUT.replacen("\n1\n", "\n\n", 1);
        assert!(!parse_sample(&booting).expect("sample").boot_completed, "an empty sys.boot_completed");
        assert_eq!(parse_sample("adb: device offline"), None);
        assert_eq!(parse_sample("@self 1\nnot a number\n"), None);
        // A torn line (a process that exited mid-read) is skipped, not fatal.
        let torn = format!("{PROBE_OUT}8000 (gone) S 1\n");
        assert_eq!(parse_sample(&torn).expect("sample").procs.len(), 5);
    }
}
