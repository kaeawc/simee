//! Host processes read through libproc: the host's process table (pid, parent, short name), a
//! process's arguments, and its accumulated CPU time split by QoS. A simulator's processes are
//! ordinary host processes under its `launchd_sim`, so a settle wait reads the table (one cheap
//! syscall per host process) and the CPU of its own device's processes only, instead of spawning
//! a host-wide `ps` (~38 ms of CPU per listing of ~1200 processes, measured by `poll_cost`).
//!
//! A device's process tree ([`tree`], [`task`], [`path`]) and its internet sockets ([`sockets`])
//! come from the same calls, so `processes`, `connections` and `kill` spawn no `ps` or `lsof`.
//!
//! Every call is a plain query of the kernel: a process that has exited (or that this user may not
//! inspect) is `None` or left out, never an error.

use std::ffi::c_void;
use std::sync::OnceLock;

/// A process's accumulated CPU time, nanoseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// User and system time.
    pub cpu_ns: u64,
    /// The part of it that ran at the maintenance or background QoS (what a launchd job with
    /// `ProcessType` Background, or a darwinbg task, runs at: priority 4 on the efficiency cores).
    pub background_ns: u64,
    /// The summed lifetimes (wall time) of the children it has spawned and waited for: every
    /// spawned-and-reaped child adds to it, however short its life, so it tells whether a stretch
    /// of code spawned anything.
    pub reaped_children_ns: u64,
    /// The CPU time (user and system) the children it has reaped used, their own reaped
    /// descendants included.
    pub reaped_children_cpu_ns: u64,
    /// When it started, on the [`now_ns`] clock: with the pid, it names one process (a reused pid
    /// starts later).
    pub start_ns: u64,
    /// Resident memory, bytes.
    pub resident_bytes: u64,
    /// Its physical footprint (what Activity Monitor's Memory column shows): dirty memory,
    /// compressed or swapped pages included.
    pub footprint_bytes: u64,
    /// Bytes read from and written to disk, accumulated.
    pub disk_read_bytes: u64,
    pub disk_written_bytes: u64,
    /// Energy the kernel attributes to it, nanojoules, accumulated (`ri_energy_nj`, macOS 12+);
    /// `None` where the kernel doesn't offer the V6 record.
    pub energy_nj: Option<u64>,
}

/// `rusage_info_v6` (`sys/resource.h`): V4, then `ri_flags` and the V5/V6 fields. libc stops at
/// V4; the layout is checked against the SDK's header below.
#[repr(C)]
struct RusageV6 {
    v4: libc::rusage_info_v4,
    flags: u64,
    user_ptime: u64,
    system_ptime: u64,
    pinstructions: u64,
    pcycles: u64,
    energy_nj: u64,
    penergy_nj: u64,
    rest: [u64; 14],
}

const RUSAGE_INFO_V6: i32 = 6;

// What the SDK's header compiles to: sizeof(rusage_info_v6) 464, offsetof(ri_energy_nj) 336.
const _: () = {
    assert!(size_of::<libc::rusage_info_v4>() == 296);
    assert!(std::mem::offset_of!(RusageV6, energy_nj) == 336);
    assert!(size_of::<RusageV6>() == 464);
};

#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    // libc's bindings are deprecated in favour of the mach2 crate; these two calls aren't worth it.
    fn mach_timebase_info(info: *mut Timebase) -> i32;
    fn mach_absolute_time() -> u64;
}

/// The host's monotonic clock (`mach_absolute_time`, which stops while the host sleeps), in
/// nanoseconds: the clock [`Usage::start_ns`] is on.
pub fn now_ns() -> u64 {
    // SAFETY: mach_absolute_time takes nothing and reads a clock.
    to_ns(unsafe { mach_absolute_time() })
}

/// Mach absolute time units to nanoseconds (1:1 on Intel, 125:3 on Apple silicon).
fn timebase() -> (u64, u64) {
    static TB: OnceLock<(u64, u64)> = OnceLock::new();
    *TB.get_or_init(|| {
        let mut info = Timebase { numer: 0, denom: 0 };
        // SAFETY: `info` is a valid, writable mach_timebase_info_data_t for the call's duration.
        let rc = unsafe { mach_timebase_info(&mut info) };
        if rc == 0 && info.numer != 0 && info.denom != 0 { (u64::from(info.numer), u64::from(info.denom)) } else { (1, 1) }
    })
}

fn to_ns(ticks: u64) -> u64 {
    let (numer, denom) = timebase();
    // In u128 so `ticks * numer` can't overflow.
    (u128::from(ticks) * u128::from(numer) / u128::from(denom)) as u64
}

/// The accumulated usage of `pid` (CPU time, memory, disk I/O, energy), `None` when it can't be
/// read (exited, or not ours). One `proc_pid_rusage` call: the V6 record, else V4 (no energy).
pub fn usage(pid: i32) -> Option<Usage> {
    let mut v6 = std::mem::MaybeUninit::<RusageV6>::zeroed();
    // SAFETY: `v6` is a writable rusage_info_v6 (layout checked above), the size the V6 flavor
    // fills; the kernel writes at most that many bytes. libc types the buffer as
    // `*mut rusage_info_t` (itself a pointer).
    if unsafe { libc::proc_pid_rusage(pid, RUSAGE_INFO_V6, v6.as_mut_ptr().cast()) } == 0 {
        // SAFETY: zero-initialised and filled by a successful call; every field is a plain integer.
        let v6 = unsafe { v6.assume_init() };
        return Some(Usage { energy_nj: Some(v6.energy_nj), ..from_v4(&v6.v4) });
    }
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: `info` is a writable rusage_info_v4, the size the V4 flavor fills; as above.
    let rc = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, info.as_mut_ptr().cast()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: zero-initialised and filled by a successful call; every field is a plain integer.
    Some(from_v4(&unsafe { info.assume_init() }))
}

fn from_v4(info: &libc::rusage_info_v4) -> Usage {
    Usage {
        cpu_ns: to_ns(info.ri_user_time.saturating_add(info.ri_system_time)),
        background_ns: to_ns(info.ri_cpu_time_qos_maintenance.saturating_add(info.ri_cpu_time_qos_background)),
        reaped_children_ns: to_ns(info.ri_child_elapsed_abstime),
        reaped_children_cpu_ns: to_ns(info.ri_child_user_time.saturating_add(info.ri_child_system_time)),
        start_ns: to_ns(info.ri_proc_start_abstime),
        resident_bytes: info.ri_resident_size,
        footprint_bytes: info.ri_phys_footprint,
        disk_read_bytes: info.ri_diskio_bytesread,
        disk_written_bytes: info.ri_diskio_byteswritten,
        energy_nj: None,
    }
}

/// Every pid on the host.
pub fn all() -> Vec<i32> {
    // SAFETY: a null buffer asks only for the number of pids.
    let estimate = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let mut pids: Vec<i32> = Vec::with_capacity(estimate.max(0) as usize + 256);
    let bytes = (pids.capacity() * size_of::<i32>()).min(i32::MAX as usize) as i32;
    // SAFETY: the buffer is `pids`'s spare capacity, `bytes` long; the kernel writes at most that
    // and returns the number of pids written.
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast::<c_void>(), bytes) };
    if n > 0 {
        // SAFETY: the first `n` pids were written, and `n` is at most the capacity.
        unsafe { pids.set_len((n as usize).min(pids.capacity())) };
    }
    pids
}

/// One row of the host's process table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    /// `p_comm`, up to 16 bytes, NUL-padded ("SpringBoard", "launchd_sim").
    comm: [u8; 16],
}

impl Proc {
    /// The short name (`p_comm`, up to 16 bytes).
    pub fn name(&self) -> &str {
        let end = self.comm.iter().position(|b| *b == 0).unwrap_or(self.comm.len());
        std::str::from_utf8(&self.comm[..end]).unwrap_or("")
    }
}

/// `pid`'s row: parent and short name.
pub fn proc(pid: i32) -> Option<Proc> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdshortinfo>::zeroed();
    let size = size_of::<libc::proc_bsdshortinfo>() as i32;
    // SAFETY: `info` is a writable proc_bsdshortinfo, `size` bytes, what this flavor fills.
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDT_SHORTBSDINFO, 0, info.as_mut_ptr().cast(), size) };
    if n != size {
        return None;
    }
    // SAFETY: zeroed and filled by a call that wrote the whole struct; every field is plain data.
    let info = unsafe { info.assume_init() };
    let mut comm = [0u8; 16];
    for (d, s) in comm.iter_mut().zip(info.pbsi_comm.iter()) {
        *d = *s as u8;
    }
    Some(Proc { pid, ppid: info.pbsi_ppid as i32, comm })
}

/// The host's process table (processes that exit while it is read are left out).
pub fn list() -> Vec<Proc> {
    all().into_iter().filter_map(proc).collect()
}

/// The process's arguments, NUL-separated, from `KERN_PROCARGS2` (after `argc`: the executable
/// path, then argv). `None` when the process has exited or can't be inspected.
pub fn args(pid: i32) -> Option<Vec<u8>> {
    let mut buf = procargs(pid)?;
    Some(buf.split_off(size_of::<i32>().min(buf.len())))
}

/// The process's whole `KERN_PROCARGS2` record: `argc` (native-endian `i32`), the executable path,
/// NUL padding, argv, then the environment, each string NUL-terminated (`sim_debug::process::
/// ProcArgs` reads it). `None` when the process has exited or can't be inspected.
pub fn procargs(pid: i32) -> Option<Vec<u8>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut len: libc::size_t = 0;
    // SAFETY: a null old pointer asks for the size only; `mib` and `len` are valid.
    let rc = unsafe { libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) };
    if rc != 0 || len < size_of::<i32>() {
        return None;
    }
    let mut buf = vec![0u8; len];
    // SAFETY: `buf` is `len` bytes, writable; the kernel writes at most `len` and updates it.
    let rc = unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
    if rc != 0 {
        return None;
    }
    buf.truncate(len);
    Some(buf)
}

/// The `launchd_sim` of the device whose data directory contains `udid` (its arguments name the
/// device's `launchd_bootstrap.plist`), if one is running.
pub fn launchd_sim(udid: &str) -> Option<i32> {
    let needle = format!("/{udid}/");
    all().into_iter().find(|&pid| is_launchd_sim(pid) && args(pid).is_some_and(|a| contains(&a, needle.as_bytes())))
}

/// Whether `pid` is a `launchd_sim` (a cached root is checked with this before reuse).
pub fn is_launchd_sim(pid: i32) -> bool {
    proc(pid).is_some_and(|p| p.name() == "launchd_sim")
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// The pids whose parent is `pid`.
pub fn children(pid: i32) -> Vec<i32> {
    // Most processes have a few children; grow the buffer only when a call fills it.
    let mut cap = 64usize;
    loop {
        let mut pids: Vec<i32> = Vec::with_capacity(cap);
        let bytes = (cap * size_of::<i32>()).min(i32::MAX as usize) as i32;
        // SAFETY: the buffer is `pids`'s spare capacity, `bytes` long; the kernel writes at most
        // that and libproc returns the number of pids written.
        let n = unsafe { libc::proc_listchildpids(pid, pids.as_mut_ptr().cast::<c_void>(), bytes) };
        if n <= 0 {
            return Vec::new();
        }
        let n = n as usize;
        if n < cap || cap >= 1 << 16 {
            // SAFETY: the first `n` pids were written, and `n` is at most the capacity.
            unsafe { pids.set_len(n.min(cap)) };
            return pids;
        }
        cap *= 4;
    }
}

/// `root` and every descendant of it, `root` first, then breadth first, from one read of the
/// host's process table. (A [`children`] call per process of the tree costs 8× as much: each
/// one walks the kernel's whole process list, 3.7 ms against 0.47 ms for a 207-process iPhone
/// on a host of ~1200 processes.)
pub fn tree(root: i32) -> Vec<i32> {
    let mut kids: std::collections::HashMap<i32, Vec<i32>> = std::collections::HashMap::new();
    for p in list() {
        if p.pid != p.ppid {
            kids.entry(p.ppid).or_default().push(p.pid);
        }
    }
    let mut out = vec![root];
    let mut seen = std::collections::HashSet::from([root]);
    let mut i = 0;
    while i < out.len() {
        for &child in kids.get(&out[i]).map_or(&[][..], Vec::as_slice) {
            // A pid reused while the table is read could show up twice; never loop on it.
            if seen.insert(child) {
                out.push(child);
            }
        }
        i += 1;
    }
    out
}

/// What `ps` reads of one process: parent, names, start time, resident size and the scheduler's
/// CPU usage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub pid: i32,
    pub ppid: i32,
    /// `pbi_name` (up to 32 bytes, what `lsof` prints), or `p_comm` when that is empty.
    pub name: String,
    /// Start time, seconds and microseconds since the Unix epoch.
    pub start_s: u64,
    pub start_us: u64,
    /// Resident memory, bytes.
    pub resident_bytes: u64,
    /// The sum of its threads' scheduler CPU usage (a decaying average the scheduler keeps per
    /// thread), thousandths of a CPU (`TH_USAGE_SCALE`): what `ps`'s `%cpu` is, ×10. Idle threads
    /// are left out, as `ps` leaves them out.
    pub cpu_usage: u32,
}

/// `pid`'s [`Task`], `None` when it has exited (or is a zombie, with no task left to read).
pub fn task(pid: i32) -> Option<Task> {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskallinfo>::zeroed();
    let size = size_of::<libc::proc_taskallinfo>() as i32;
    // SAFETY: `info` is a writable proc_taskallinfo, `size` bytes, what this flavor fills.
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKALLINFO, 0, info.as_mut_ptr().cast(), size) };
    if n != size {
        return None;
    }
    // SAFETY: zeroed and filled by a call that wrote the whole struct; every field is plain data.
    let info = unsafe { info.assume_init() };
    let name = c_name(&info.pbsd.pbi_name);
    Some(Task {
        pid,
        ppid: info.pbsd.pbi_ppid as i32,
        name: if name.is_empty() { c_name(&info.pbsd.pbi_comm) } else { name },
        start_s: info.pbsd.pbi_start_tvsec,
        start_us: info.pbsd.pbi_start_tvusec,
        resident_bytes: info.ptinfo.pti_resident_size,
        cpu_usage: cpu_usage(pid, info.ptinfo.pti_threadnum.max(0) as usize),
    })
}

fn c_name(chars: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = chars.iter().map(|c| *c as u8).take_while(|b| *b != 0).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The sum of `pid`'s non-idle threads' `pth_cpu_usage` (`threads`: how many it has, to size the
/// list; a thread started meanwhile is missed, as `ps` would miss it).
fn cpu_usage(pid: i32, threads: usize) -> u32 {
    let mut handles: Vec<u64> = Vec::with_capacity(threads + 16);
    let bytes = (handles.capacity() * size_of::<u64>()).min(i32::MAX as usize) as i32;
    // SAFETY: the buffer is `handles`'s spare capacity, `bytes` long; the kernel writes at most
    // that many bytes of thread handles and returns how many bytes it wrote.
    let n = unsafe { libc::proc_pidinfo(pid, PROC_PIDLISTTHREADS, 0, handles.as_mut_ptr().cast(), bytes) };
    if n <= 0 {
        return 0;
    }
    // SAFETY: the first `n` bytes (whole handles) were written, at most the capacity.
    unsafe { handles.set_len((n as usize / size_of::<u64>()).min(handles.capacity())) };
    let mut total = 0u32;
    for handle in handles {
        let mut info = std::mem::MaybeUninit::<libc::proc_threadinfo>::zeroed();
        let size = size_of::<libc::proc_threadinfo>() as i32;
        // SAFETY: `info` is a writable proc_threadinfo, `size` bytes, what this flavor fills for
        // the thread `handle` names.
        let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTHREADINFO, handle, info.as_mut_ptr().cast(), size) };
        if n != size {
            continue; // the thread ended meanwhile
        }
        // SAFETY: zeroed and filled by a call that wrote the whole struct; plain data.
        let info = unsafe { info.assume_init() };
        if info.pth_flags & libc::TH_FLAGS_IDLE == 0 {
            total = total.saturating_add(info.pth_cpu_usage.max(0) as u32);
        }
    }
    total
}

/// `PROC_PIDLISTTHREADS` (`sys/proc_info.h`; libc doesn't name it).
const PROC_PIDLISTTHREADS: i32 = 6;

/// The executable `pid` runs, as the kernel resolved it when it was exec'd.
pub fn path(pid: i32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buf` is writable and as long as the length passed; the call writes at most that
    // and returns how many bytes of path it wrote.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    String::from_utf8(buf).ok()
}

/// `pid`'s name as [`Task::name`] has it (`pbi_name`, else `p_comm`), without reading its threads.
pub fn name(pid: i32) -> Option<String> {
    let mut buf = [0u8; 64];
    // SAFETY: `buf` is writable and as long as the length passed; proc_name writes at most that
    // and returns the name's length.
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..(n as usize).min(buf.len())]).into_owned())
}

/// Sends `signal` to `pid` (`kill(2)`; 0 checks that it may be signalled).
pub fn signal(pid: i32, signal: i32) -> std::io::Result<()> {
    // SAFETY: kill(2) takes plain integers and has no memory effects on this process.
    if unsafe { libc::kill(pid, signal) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// One internet (IPv4 or IPv6) socket a process has open: what `lsof -i` lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Socket {
    pub fd: i32,
    /// The IP protocol (`IPPROTO_TCP` 6, `IPPROTO_UDP` 17, ...).
    pub protocol: i32,
    pub local: std::net::SocketAddr,
    pub remote: std::net::SocketAddr,
    /// A TCP socket's state (`TSI_S_*`: 0 closed, 1 listen, ... 4 established ... 10 time wait).
    pub tcp_state: Option<i32>,
}

/// `pid`'s internet sockets, by fd.
pub fn sockets(pid: i32) -> Vec<Socket> {
    fds(pid).into_iter().filter(|f| f.proc_fdtype == libc::PROX_FDTYPE_SOCKET as u32).filter_map(|f| socket(pid, f.proc_fd)).collect()
}

fn fds(pid: i32) -> Vec<libc::proc_fdinfo> {
    // SAFETY: a null buffer asks for the size the list needs.
    let estimate = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if estimate <= 0 {
        return Vec::new();
    }
    // Room for a few more fds opened between the two calls; a buffer that comes back full may
    // have missed some, so read again with four times the room.
    let mut cap = estimate as usize / size_of::<libc::proc_fdinfo>() + 16;
    loop {
        let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(cap);
        let bytes = (cap * size_of::<libc::proc_fdinfo>()).min(i32::MAX as usize) as i32;
        // SAFETY: the buffer is `fds`'s spare capacity, `bytes` long; the kernel writes at most
        // that and returns how many bytes it wrote.
        let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), bytes) };
        if n <= 0 {
            return Vec::new();
        }
        let len = (n as usize / size_of::<libc::proc_fdinfo>()).min(cap);
        if len < cap || cap >= 1 << 20 {
            // SAFETY: the first `len` entries were written, at most the capacity.
            unsafe { fds.set_len(len) };
            return fds;
        }
        cap *= 4;
    }
}

/// `PROC_PIDFDSOCKETINFO` and `INI_IPV4` (`sys/proc_info.h`; libc doesn't name them).
const PROC_PIDFDSOCKETINFO: i32 = 3;
const INI_IPV4: u8 = 0x1;
const SOCKINFO_IN: i32 = 1;
const SOCKINFO_TCP: i32 = 2;

fn socket(pid: i32, fd: i32) -> Option<Socket> {
    let mut info = std::mem::MaybeUninit::<SocketFdinfo>::zeroed();
    let size = size_of::<SocketFdinfo>() as i32;
    // SAFETY: `info` is a writable socket_fdinfo (layout checked against the SDK's header below),
    // `size` bytes, what this flavor fills.
    let n = unsafe { libc::proc_pidfdinfo(pid, fd, PROC_PIDFDSOCKETINFO, info.as_mut_ptr().cast(), size) };
    if n != size {
        return None;
    }
    // SAFETY: zeroed and filled by a call that wrote the whole struct; every field is plain data.
    let info = unsafe { info.assume_init() };
    let psi = &info.psi;
    if psi.soi_family != libc::AF_INET && psi.soi_family != libc::AF_INET6 {
        return None;
    }
    let (ini, tcp_state) = match psi.soi_kind {
        // SAFETY: the kernel filled the union's TCP member (`soi_kind`); it is plain data.
        SOCKINFO_TCP => unsafe { (psi.soi_proto.tcp.tcpsi_ini, Some(psi.soi_proto.tcp.tcpsi_state)) },
        // SAFETY: the kernel filled the union's internet member (`soi_kind`); plain data.
        SOCKINFO_IN => unsafe { (psi.soi_proto.ini, None) },
        _ => return None,
    };
    Some(Socket {
        fd,
        protocol: psi.soi_protocol,
        local: sockaddr(ini.insi_vflag, ini.insi_laddr, ini.insi_lport),
        remote: sockaddr(ini.insi_vflag, ini.insi_faddr, ini.insi_fport),
        tcp_state,
    })
}

/// An address as `in_sockinfo` holds it: an IPv4 address in the last 4 bytes when `vflag` says
/// IPv4, else 16 bytes of IPv6; the port in network byte order in the low 16 bits.
fn sockaddr(vflag: u8, addr: [u8; 16], port: i32) -> std::net::SocketAddr {
    let port = u16::from_be(port as u16);
    if vflag & INI_IPV4 != 0 {
        std::net::SocketAddr::from(([addr[12], addr[13], addr[14], addr[15]], port))
    } else {
        std::net::SocketAddr::from((addr, port))
    }
}

// `struct socket_fdinfo` and what it holds, from `sys/proc_info.h` (libc doesn't define them).
// Only the fields read here are named; the rest keep the layout. The sizes and offsets are
// checked against the macOS SDK's (measured with `offsetof` on macOS 26.6) at compile time.

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)] // layout fields, never read
struct InSockinfo {
    insi_fport: i32,
    insi_lport: i32,
    insi_gencnt: u64,
    insi_flags: u32,
    insi_flow: u32,
    insi_vflag: u8,
    insi_ip_ttl: u8,
    rfu_1: u32,
    insi_faddr: [u8; 16],
    insi_laddr: [u8; 16],
    insi_v4_tos: u8,
    insi_v6: InV6,
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)] // layout fields, never read
struct InV6 {
    in6_hlim: u8,
    in6_cksum: i32,
    in6_ifindex: u16,
    in6_hops: i16,
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)] // layout fields, never read
struct TcpSockinfo {
    tcpsi_ini: InSockinfo,
    tcpsi_state: i32,
    tcpsi_timer: [i32; 4],
    tcpsi_mss: i32,
    tcpsi_flags: u32,
    rfu_1: u32,
    tcpsi_tp: u64,
}

/// `soi_proto`: as large as its largest member (`un_sockinfo`, 528 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)] // layout fields, never read
union SocketProto {
    ini: InSockinfo,
    tcp: TcpSockinfo,
    size: [u64; 66],
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)] // layout fields, never read
struct SocketInfo {
    soi_stat: [u64; 17], // struct vinfo_stat
    soi_so: u64,
    soi_pcb: u64,
    soi_type: i32,
    soi_protocol: i32,
    soi_family: i32,
    soi_options: i16,
    soi_linger: i16,
    soi_state: i16,
    soi_qlen: i16,
    soi_incqlen: i16,
    soi_qlimit: i16,
    soi_timeo: i16,
    soi_error: u16,
    soi_oobmark: u32,
    soi_rcv: [u32; 6], // struct sockbuf_info
    soi_snd: [u32; 6],
    soi_kind: i32,
    rfu_1: u32,
    soi_proto: SocketProto,
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)] // layout fields, never read
struct SocketFdinfo {
    pfi: [u64; 3], // struct proc_fileinfo
    psi: SocketInfo,
}

const _: () = {
    assert!(size_of::<InSockinfo>() == 80);
    assert!(std::mem::offset_of!(InSockinfo, insi_vflag) == 24);
    assert!(std::mem::offset_of!(InSockinfo, insi_faddr) == 32);
    assert!(std::mem::offset_of!(InSockinfo, insi_laddr) == 48);
    assert!(size_of::<TcpSockinfo>() == 120);
    assert!(std::mem::offset_of!(TcpSockinfo, tcpsi_state) == 80);
    assert!(size_of::<SocketProto>() == 528);
    assert!(std::mem::offset_of!(SocketInfo, soi_family) == 160);
    assert!(std::mem::offset_of!(SocketInfo, soi_kind) == 232);
    assert!(std::mem::offset_of!(SocketInfo, soi_proto) == 240);
    assert!(size_of::<SocketInfo>() == 768);
    assert!(std::mem::offset_of!(SocketFdinfo, psi) == 24);
    assert!(size_of::<SocketFdinfo>() == 792);
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Spins until this process has used `secs` more CPU, so a loaded host (where wall time
    /// buys less CPU) can't starve the measurement; gives up after 30 s of wall time.
    fn busy_cpu(secs: f64) {
        let target = cpu_s(libc::RUSAGE_SELF) + secs;
        let give_up = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut x = 0u64;
        while cpu_s(libc::RUSAGE_SELF) < target && std::time::Instant::now() < give_up {
            for _ in 0..10_000 {
                x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(7));
            }
        }
    }

    fn cpu_s(who: i32) -> f64 {
        let mut ru = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: a writable rusage for RUSAGE_SELF or RUSAGE_CHILDREN.
        assert_eq!(unsafe { libc::getrusage(who, ru.as_mut_ptr()) }, 0);
        // SAFETY: filled by the successful call.
        let ru = unsafe { ru.assume_init() };
        let tv = |t: libc::timeval| t.tv_sec as f64 + f64::from(t.tv_usec) / 1e6;
        tv(ru.ru_utime) + tv(ru.ru_stime)
    }

    #[test]
    fn own_usage_matches_getrusage() {
        busy_cpu(0.2);
        let u = usage(std::process::id() as i32).expect("own usage");
        let rusage_s = cpu_s(libc::RUSAGE_SELF);
        let ours = u.cpu_ns as f64 / 1e9;
        assert!(ours >= 0.15, "{ours} s after 0.2 s of busy CPU");
        // Both count the same CPU in different units: within 20 % (other test threads run too).
        assert!((ours - rusage_s).abs() <= 0.2 * rusage_s.max(0.2), "libproc {ours} s, getrusage {rusage_s} s");
        assert!(u.background_ns <= u.cpu_ns);
    }

    #[test]
    fn usage_has_start_memory_disk_and_energy_on_the_now_clock() {
        let me = usage(std::process::id() as i32).expect("own usage");
        let before = now_ns();
        assert!(me.start_ns > 0 && me.start_ns < before, "{} !< {before}", me.start_ns);
        assert!(me.resident_bytes > 1 << 20 && me.footprint_bytes > 0, "{me:?}");
        assert!(me.energy_nj.is_some(), "macOS 12+ has the V6 record");
        busy_cpu(0.3);
        let after = usage(std::process::id() as i32).expect("own usage");
        let (joules, cpu_s) =
            ((after.energy_nj.unwrap_or(0) - me.energy_nj.unwrap_or(0)) as f64 / 1e9, (after.cpu_ns - me.cpu_ns) as f64 / 1e9);
        println!("{joules:.3} J for {cpu_s:.3} CPU-s: {:.2} W per busy core", joules / cpu_s);
        // A macOS VM (GitHub's runners) reports no energy: the counters stay at zero there.
        let in_vm = std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "kern.hv_vmm_present"])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "1");
        assert!(joules > 0.0 || in_vm, "busy CPU costs energy");
        let mut child = std::process::Command::new("/bin/sleep").arg("5").spawn().expect("sleep");
        let kid = usage(child.id() as i32).expect("child usage");
        child.kill().ok();
        child.wait().ok();
        assert!(kid.start_ns >= before && kid.start_ns <= now_ns(), "a child started after `before` is stamped after it");
    }

    #[test]
    fn a_reaped_child_adds_its_lifetime_to_reaped_children() {
        let me = std::process::id() as i32;
        let before = usage(me).expect("usage").reaped_children_ns;
        std::process::Command::new("/usr/bin/true").status().expect("true");
        let after = usage(me).expect("usage").reaped_children_ns;
        assert!(after > before, "{before} -> {after}");
    }

    #[test]
    fn a_spawned_child_is_in_the_table_under_us() {
        let mut child = std::process::Command::new("/bin/sleep").arg("5").spawn().expect("sleep");
        let pid = child.id() as i32;
        let me = std::process::id() as i32;
        let row = list().into_iter().find(|p| p.pid == pid).expect("listed");
        assert_eq!((row.ppid, row.name()), (me, "sleep"));
        assert_eq!(proc(pid), Some(row));
        let a = args(pid).expect("args");
        assert!(contains(&a, b"/bin/sleep\0") && contains(&a, b"\x005"), "{:?}", String::from_utf8_lossy(&a));
        assert!(usage(pid).is_some());
        assert!(!is_launchd_sim(pid));
        child.kill().ok();
        child.wait().ok();
        assert_eq!(proc(pid), None, "an exited process has no row");
        assert_eq!(usage(pid), None, "an exited process has no usage");
    }

    #[test]
    fn a_spawned_child_task_path_and_tree_are_read_and_it_can_be_signalled() {
        let mut child = std::process::Command::new("/bin/sleep").arg("5").spawn().expect("sleep");
        let pid = child.id() as i32;
        let me = std::process::id() as i32;
        assert!(children(me).contains(&pid), "{:?}", children(me));
        let all = tree(me);
        assert_eq!(all[0], me);
        assert!(all.contains(&pid));
        let t = task(pid).expect("task");
        assert_eq!((t.pid, t.ppid, t.name.as_str()), (pid, me, "sleep"));
        assert!(t.resident_bytes > 0);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("now").as_secs();
        assert!(t.start_s <= now && now - t.start_s < 60, "started {} s ago", now - t.start_s);
        assert!(t.start_us < 1_000_000);
        assert!(t.cpu_usage <= 1000, "a sleeping process uses at most a CPU: {}", t.cpu_usage);
        assert_eq!(path(pid).as_deref(), Some("/bin/sleep"));
        assert_eq!(name(pid).as_deref(), Some("sleep"));
        signal(pid, 0).expect("signal 0");
        signal(pid, libc::SIGKILL).expect("SIGKILL");
        child.wait().ok();
        assert_eq!(task(pid), None, "an exited process has no task");
        assert_eq!(path(pid), None);
        assert!(!children(me).contains(&pid));
        assert_eq!(signal(pid, 0).map_err(|e| e.raw_os_error()), Err(Some(libc::ESRCH)));
    }

    #[test]
    fn a_busy_process_has_cpu_usage() {
        busy_cpu(0.5);
        let task = task(std::process::id() as i32).expect("own task");
        assert!(task.cpu_usage > 0, "{task:?}");
    }

    #[test]
    fn own_tcp_and_udp_sockets_are_read_with_their_addresses_and_state() {
        use std::net::{Ipv6Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
        use std::os::fd::AsRawFd;
        let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
        let client = TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        let udp = UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).expect("udp");
        let all = sockets(std::process::id() as i32);
        let find = |fd: i32| *all.iter().find(|s| s.fd == fd).unwrap_or_else(|| panic!("fd {fd} in {all:?}"));
        let unspecified: SocketAddr = "0.0.0.0:0".parse().expect("addr");

        let l = find(listener.as_raw_fd());
        assert_eq!((l.protocol, l.local, l.remote, l.tcp_state), (6, listener.local_addr().expect("addr"), unspecified, Some(1)));
        let c = find(client.as_raw_fd());
        assert_eq!((c.local, c.remote, c.tcp_state), (client.local_addr().expect("addr"), listener.local_addr().expect("addr"), Some(4)));
        let s = find(server.as_raw_fd());
        assert_eq!((s.local, s.remote), (server.local_addr().expect("addr"), client.local_addr().expect("addr")));
        let u = find(udp.as_raw_fd());
        assert_eq!((u.protocol, u.local, u.tcp_state), (17, udp.local_addr().expect("addr"), None));
        assert_eq!(u.remote, "[::]:0".parse::<SocketAddr>().expect("addr"));
        let fds: Vec<i32> = all.iter().map(|s| s.fd).collect();
        let mut sorted = fds.clone();
        sorted.sort_unstable();
        assert_eq!(fds, sorted, "by fd");
    }

    #[test]
    fn the_table_has_every_process_and_no_device_is_found_for_a_made_up_udid() {
        let table = list();
        let me = std::process::id() as i32;
        assert!(table.iter().any(|p| p.pid == 1 && p.name() == "launchd"), "{} rows", table.len());
        assert!(table.iter().any(|p| p.pid == me));
        assert_eq!(launchd_sim("00000000-0000-0000-0000-000000000000"), None);
    }

    #[test]
    fn a_name_stops_at_the_first_nul_and_a_needle_is_found_only_whole() {
        let mut comm = [0u8; 16];
        comm[..11].copy_from_slice(b"SpringBoard");
        assert_eq!(Proc { pid: 1, ppid: 0, comm }.name(), "SpringBoard");
        assert_eq!(Proc { pid: 1, ppid: 0, comm: *b"sixteen-chars-xx" }.name(), "sixteen-chars-xx");
        assert!(contains(b"a /U/ b", b"/U/"));
        assert!(!contains(b"a /U b", b"/U/"));
        assert!(!contains(b"abc", b""));
    }

    /// Host CPU of one settle poll: the host-wide `ps -Ao pid=,ppid=,time=,args=` the settle wait
    /// used to spawn, against what it does now (the process table, then the CPU of a device's
    /// processes; here the CPU of every process this user can read, an upper bound).
    /// `cargo test -p simee-coresim --release poll_cost -- --ignored --nocapture --test-threads=1`
    #[test]
    #[ignore = "a benchmark: spawns ps 20 times"]
    fn poll_cost() {
        let n = 20;
        let per = |s0: f64, t0: std::time::Instant| {
            ((cpu_s(libc::RUSAGE_SELF) - s0) * 1e3 / f64::from(n), t0.elapsed().as_secs_f64() * 1e3 / f64::from(n))
        };

        let (s0, c0, t0) = (cpu_s(libc::RUSAGE_SELF), cpu_s(libc::RUSAGE_CHILDREN), std::time::Instant::now());
        let mut lines = 0;
        for _ in 0..n {
            let out = std::process::Command::new("/bin/ps").args(["-Ao", "pid=,ppid=,time=,args="]).output().expect("ps");
            lines = out.stdout.split(|b| *b == b'\n').count();
        }
        let ps_cpu = (cpu_s(libc::RUSAGE_SELF) - s0 + cpu_s(libc::RUSAGE_CHILDREN) - c0) * 1e3 / f64::from(n);
        let ps_wall = t0.elapsed().as_secs_f64() * 1e3 / f64::from(n);

        let (s0, t0) = (cpu_s(libc::RUSAGE_SELF), std::time::Instant::now());
        let mut rows = 0;
        for _ in 0..n {
            rows = std::hint::black_box(list()).len();
        }
        let (table_cpu, table_wall) = per(s0, t0);

        let pids = all();
        let (s0, t0) = (cpu_s(libc::RUSAGE_SELF), std::time::Instant::now());
        let mut read = 0;
        for _ in 0..n {
            read = pids.iter().filter(|p| usage(**p).is_some()).count();
        }
        let (usage_cpu, usage_wall) = per(s0, t0);

        let (s0, t0) = (cpu_s(libc::RUSAGE_SELF), std::time::Instant::now());
        for _ in 0..n {
            assert_eq!(launchd_sim("00000000-0000-0000-0000-000000000000"), None);
        }
        let (scan_cpu, scan_wall) = per(s0, t0);
        eprintln!(
            "ps listing ({lines} lines): {ps_cpu:.2} ms CPU, {ps_wall:.2} ms wall per poll\n\
             process table ({rows} rows): {table_cpu:.3} ms CPU, {table_wall:.3} ms wall per poll\n\
             CPU of {read} processes: {usage_cpu:.3} ms CPU, {usage_wall:.3} ms wall ({:.2} µs each)\n\
             launchd_sim scan (once per wait): {scan_cpu:.3} ms CPU, {scan_wall:.3} ms wall",
            usage_cpu * 1e3 / read.max(1) as f64,
        );
    }

    /// CPU that ran at background QoS (`taskpolicy -b`, darwinbg) is counted as background; CPU at
    /// the default QoS isn't. Host-only and timing-dependent, so not in the default run.
    #[test]
    #[ignore = "spins two host processes for a second each"]
    fn background_cpu_is_attributed_to_background_qos() {
        let spin = |bg: bool| {
            let mut cmd = if bg {
                let mut c = std::process::Command::new("/usr/sbin/taskpolicy");
                c.args(["-b", "/usr/bin/yes"]);
                c
            } else {
                std::process::Command::new("/usr/bin/yes")
            };
            let mut child = cmd.stdout(std::process::Stdio::null()).spawn().expect("spawn");
            std::thread::sleep(std::time::Duration::from_secs(1));
            let u = usage(child.id() as i32).expect("usage");
            child.kill().ok();
            child.wait().ok();
            u
        };
        let fg = spin(false);
        let bg = spin(true);
        eprintln!("default: {fg:?}\nbackground: {bg:?}");
        assert!(fg.cpu_ns > 100_000_000 && fg.background_ns < fg.cpu_ns / 10, "{fg:?}");
        assert!(bg.cpu_ns > 10_000_000 && bg.background_ns > bg.cpu_ns * 9 / 10, "{bg:?}");
    }
}
