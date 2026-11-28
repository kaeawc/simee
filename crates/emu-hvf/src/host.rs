//! Host file and thread operations that std and rustix don't wrap.
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::os::fd::AsRawFd;

/// A macOS quality-of-service class for a thread. The scheduler places and clocks threads by it:
/// a process launched from a background-clamped parent (a CI agent, a daemon) otherwise runs
/// every thread at that parent's QoS, which keeps it on efficiency cores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Qos {
    UserInteractive,
    /// Work something waits on: vCPUs and the device workers they start.
    UserInitiated,
    /// Housekeeping no guest or client waits on.
    Utility,
    /// Never for anything a device waits on: it may not run for long stretches.
    Background,
}

#[cfg(target_os = "macos")]
impl Qos {
    fn class(self) -> libc::qos_class_t {
        match self {
            Qos::UserInteractive => libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE,
            Qos::UserInitiated => libc::qos_class_t::QOS_CLASS_USER_INITIATED,
            Qos::Utility => libc::qos_class_t::QOS_CLASS_UTILITY,
            Qos::Background => libc::qos_class_t::QOS_CLASS_BACKGROUND,
        }
    }
}

/// Sets the calling thread's QoS class. A no-op off macOS.
///
/// It is not inherited: a thread created afterwards with `std::thread::spawn` starts at
/// `QOS_CLASS_DEFAULT` (`sys/qos.h`: "the default QOS class for threads created by
/// pthread_create() without an attribute specifying a QOS class"), so each thread that matters
/// sets its own.
pub fn set_qos(qos: Qos) {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: plain value arguments; affects only the calling thread's scheduling.
        let _ = unsafe { libc::pthread_set_qos_class_self_np(qos.class(), 0) };
    }
    #[cfg(not(target_os = "macos"))]
    let _ = qos;
}

/// The calling thread's requested QoS class; `None` for `QOS_CLASS_DEFAULT`, unspecified, or off
/// macOS.
pub fn qos() -> Option<Qos> {
    #[cfg(target_os = "macos")]
    {
        let mut class = libc::qos_class_t::QOS_CLASS_UNSPECIFIED;
        let mut prio = 0;
        // SAFETY: both out-pointers reference live locals; pthread_self is always valid.
        let rc = unsafe { libc::pthread_get_qos_class_np(libc::pthread_self(), &mut class, &mut prio) };
        if rc != 0 {
            return None;
        }
        [Qos::UserInteractive, Qos::UserInitiated, Qos::Utility, Qos::Background].into_iter().find(|q| q.class() as u32 == class as u32)
    }
    #[cfg(not(target_os = "macos"))]
    None
}

/// Deallocates `len` bytes of `file` at `offset` with `fcntl(F_PUNCHHOLE)`. The range reads back
/// as zeroes and the file's length is unchanged. `offset` and `len` must be multiples of the
/// filesystem block size (4096 on APFS); filesystems without hole support fail with `ENOTSUP`.
#[cfg(target_os = "macos")]
pub fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<()> {
    let too_big = |_| io::Error::new(io::ErrorKind::InvalidInput, "punch range exceeds off_t");
    let arg = libc::fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: libc::off_t::try_from(offset).map_err(too_big)?,
        fp_length: libc::off_t::try_from(len).map_err(too_big)?,
    };
    loop {
        // SAFETY: F_PUNCHHOLE reads one fpunchhole_t through the pointer, which references a live
        // local for the duration of the call; the descriptor is owned by `file`, which outlives it.
        let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &arg as *const libc::fpunchhole_t) };
        if rc != -1 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Deallocates `len` bytes of `file` at `offset` with `fallocate(PUNCH_HOLE | KEEP_SIZE)`: the
/// range reads back as zeroes and the file's length is unchanged. Filesystems without hole
/// support fail with `EOPNOTSUPP`.
#[cfg(target_os = "linux")]
pub fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<()> {
    let too_big = |_| io::Error::new(io::ErrorKind::InvalidInput, "punch range exceeds off_t");
    let offset = libc::off_t::try_from(offset).map_err(too_big)?;
    let len = libc::off_t::try_from(len).map_err(too_big)?;
    loop {
        // SAFETY: plain integer arguments; the descriptor is owned by `file`, which outlives the call.
        let rc = unsafe { libc::fallocate(file.as_raw_fd(), libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, offset, len) };
        if rc != -1 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Whether process `pid` exists and hasn't exited.
#[cfg(windows)]
pub fn process_alive(pid: u32) -> bool {
    crate::win::process_alive(pid)
}

/// Windows has no `fallocate`; a sparse range would take `FSCTL_SET_ZERO_DATA` on a file marked
/// sparse, which nothing here does yet. Always `Unsupported`, which callers treat as a
/// filesystem without holes.
#[cfg(windows)]
pub fn punch_hole(_file: &File, _offset: u64, _len: u64) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};

    use super::*;

    /// A thread's QoS reads back, and a thread it spawns afterwards does NOT inherit it (it starts
    /// at QOS_CLASS_DEFAULT): device workers started from a vCPU thread must set their own.
    #[test]
    fn qos_is_set_per_thread_and_not_inherited() {
        let (own, child) = std::thread::spawn(|| {
            set_qos(Qos::UserInitiated);
            let own = qos();
            let child = std::thread::spawn(qos).join().expect("child");
            (own, child)
        })
        .join()
        .expect("thread");
        if cfg!(target_os = "macos") {
            assert_eq!(own, Some(Qos::UserInitiated));
        } else {
            assert_eq!(own, None, "a no-op off macOS");
        }
        assert_eq!(child, None, "a spawned thread starts at QOS_CLASS_DEFAULT");
        for q in [Qos::UserInteractive, Qos::Utility, Qos::Background] {
            let back = std::thread::spawn(move || {
                set_qos(q);
                qos()
            })
            .join()
            .expect("thread");
            assert_eq!(back, cfg!(target_os = "macos").then_some(q));
        }
    }

    #[cfg(windows)]
    #[test]
    fn this_process_is_alive_and_an_exited_child_is_not() {
        assert!(process_alive(std::process::id()));
        let mut child = std::process::Command::new("cmd").args(["/C", "exit 0"]).spawn().expect("spawn");
        let pid = child.id();
        child.wait().expect("wait");
        assert!(!process_alive(pid), "an exited process is gone even while a handle to it is open");
    }

    #[test]
    fn punched_range_reads_zero_and_keeps_length() {
        let path = std::env::temp_dir().join(format!("emu-hvf-punch-{}", std::process::id()));
        let mut f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).expect("create");
        f.write_all(&vec![0xab; 3 * 4096]).expect("fill");
        let punched = punch_hole(&f, 4096, 4096);
        let mut back = Vec::new();
        f.seek(SeekFrom::Start(0)).expect("seek");
        f.read_to_end(&mut back).expect("read");
        let _ = std::fs::remove_file(&path);
        if let Err(e) = punched {
            // tmpfs and some overlay filesystems have no holes; nothing to check there.
            #[cfg(unix)]
            assert_eq!(e.raw_os_error(), Some(libc::EOPNOTSUPP), "{e}");
            #[cfg(windows)]
            assert_eq!(e.kind(), io::ErrorKind::Unsupported, "{e}");
            return;
        }
        assert_eq!(back.len(), 3 * 4096, "length unchanged");
        assert!(back[..4096].iter().all(|&b| b == 0xab));
        assert!(back[4096..8192].iter().all(|&b| b == 0), "hole reads as zeroes");
        assert!(back[8192..].iter().all(|&b| b == 0xab));
    }
}
