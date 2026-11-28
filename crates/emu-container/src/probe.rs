//! What this host's kernel offers a container that runs Android's userspace (Redroid's list,
//! docs/research/container-backend.md): binder through binderfs, memfd, cgroups v2, user
//! namespaces, overlayfs. Everything is read through a [`HostView`], so the probe is pure logic that
//! tests run against fake `/proc` and `/sys` trees; [`FsView`] is the real one.
//!
//! Each check is `Ok` or `Missing`, and a missing one carries the one-line fix: a host that fails
//! is told what to change, not just that it failed.

use std::fmt;
use std::path::Path;

/// The files and facts a probe reads. Paths are absolute host paths (`/proc/filesystems`).
pub trait HostView {
    /// A text file's contents; `None` when it doesn't exist or can't be read.
    fn read(&self, path: &str) -> Option<String>;
    /// Whether a path exists (a file, a directory or a device node).
    fn exists(&self, path: &str) -> bool;
    /// Whether `name` is an executable on `PATH`.
    fn on_path(&self, name: &str) -> bool;
    /// Whether `memfd_create(2)` works here (a seccomp filter can deny it).
    fn memfd_create(&self) -> bool;
    /// The effective user id.
    fn euid(&self) -> u32;
    /// The CPU architecture as Rust names it (`x86_64`, `aarch64`).
    fn arch(&self) -> String;
    /// The device names a binderfs mount holds once mounted (in a throwaway user namespace, so no
    /// privilege is needed): `binder-control` is not one. `None` when that couldn't be tried.
    fn binderfs_devices(&self) -> Option<Vec<String>>;
}

/// One thing a container needs from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Feature {
    /// A Linux kernel at all (`/proc/filesystems`).
    Linux,
    /// Binder IPC, reached through binderfs: Android's IPC, with no substitute.
    Binder,
    /// `memfd_create`, which replaces ashmem (`androidboot.use_memfd=true`).
    Memfd,
    /// The unified cgroup hierarchy Android's init mounts its controllers on.
    CgroupsV2,
    /// Memory, CPU and pid controllers, to cap what the container may take from the host.
    CgroupLimits,
    /// User namespaces: the container's root is not the host's.
    UserNamespaces,
    /// Subordinate uid and gid ranges, so Android's many uids exist inside an unprivileged
    /// user namespace.
    SubordinateIds,
    /// overlayfs, for a per-device writable layer over the shared image.
    Overlayfs,
    /// DMA-BUF heaps (or ION): graphics buffers.
    DmaBufHeaps,
}

impl Feature {
    pub fn name(self) -> &'static str {
        match self {
            Feature::Linux => "linux",
            Feature::Binder => "binder",
            Feature::Memfd => "memfd",
            Feature::CgroupsV2 => "cgroups v2",
            Feature::CgroupLimits => "cgroup limits",
            Feature::UserNamespaces => "user namespaces",
            Feature::SubordinateIds => "subordinate ids",
            Feature::Overlayfs => "overlayfs",
            Feature::DmaBufHeaps => "dma-buf heaps",
        }
    }

    /// Whether the container can't run without it. The others degrade it (no resource limits,
    /// graphics may fall back) and are reported without blocking.
    pub fn required(self) -> bool {
        !matches!(self, Feature::CgroupLimits | Feature::DmaBufHeaps)
    }
}

impl fmt::Display for Feature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Ok { detail: String },
    Missing { why: String, fix: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub feature: Feature,
    pub status: Status,
}

impl Check {
    pub fn ok(&self) -> bool {
        matches!(self.status, Status::Ok { .. })
    }

    /// `binder: missing: <why> (fix: <fix>)`, or `binder: ok: <detail>`.
    pub fn line(&self) -> String {
        match &self.status {
            Status::Ok { detail } => format!("{}: ok: {detail}", self.feature),
            Status::Missing { why, fix } => format!("{}: missing: {why} (fix: {fix})", self.feature),
        }
    }
}

/// Every check, in a fixed order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn get(&self, f: Feature) -> Option<&Check> {
        self.checks.iter().find(|c| c.feature == f)
    }

    /// The required checks that failed.
    pub fn blockers(&self) -> Vec<&Check> {
        self.checks.iter().filter(|c| c.feature.required() && !c.ok()).collect()
    }

    /// Whether a container can run here: every required check passed.
    pub fn usable(&self) -> bool {
        self.blockers().is_empty()
    }

    /// Why a container can't run here, with each fix; `None` when it can.
    pub fn explain(&self) -> Option<String> {
        let blockers = self.blockers();
        if blockers.is_empty() {
            return None;
        }
        Some(blockers.iter().map(|c| c.line()).collect::<Vec<_>>().join("; "))
    }

    /// One line per check, for `-accel-check` and `-verbose`.
    pub fn lines(&self) -> Vec<String> {
        self.checks.iter().map(Check::line).collect()
    }
}

fn ok(feature: Feature, detail: impl Into<String>) -> Check {
    Check { feature, status: Status::Ok { detail: detail.into() } }
}

fn missing(feature: Feature, why: impl Into<String>, fix: impl Into<String>) -> Check {
    Check { feature, status: Status::Missing { why: why.into(), fix: fix.into() } }
}

/// Runs every check against `view`. When the host isn't Linux nothing else is asked.
pub fn probe(view: &dyn HostView) -> Report {
    let Some(filesystems) = view.read("/proc/filesystems") else {
        return Report {
            checks: vec![missing(
                Feature::Linux,
                "no /proc/filesystems: this is not a Linux host, or /proc isn't mounted",
                "run on a Linux host with /proc mounted (on a Mac, emu uses Hypervisor.framework instead)",
            )],
        };
    };
    let release = view.read("/proc/sys/kernel/osrelease").map(|s| s.trim().to_string()).unwrap_or_default();
    Report {
        checks: vec![
            ok(Feature::Linux, if release.is_empty() { "kernel".to_string() } else { format!("kernel {release}") }),
            binder(view, &filesystems, &release),
            memfd(view),
            cgroups_v2(view),
            cgroup_limits(view),
            user_namespaces(view),
            subordinate_ids(view),
            overlayfs(&release, &filesystems),
            dma_buf_heaps(view),
        ],
    }
}

/// Whether `/proc/filesystems` lists `name` (lines are `nodev\tname` or `\tname`).
fn has_filesystem(filesystems: &str, name: &str) -> bool {
    filesystems.lines().any(|l| l.split_whitespace().last() == Some(name))
}

const BINDER_DEVICES: [&str; 3] = ["binder", "hwbinder", "vndbinder"];

/// Loading the module with the devices it should create, and keeping that across reboots: the
/// module is loaded by `modules-load.d`, and its `devices=` option is a `modprobe.d` line.
const BINDER_MODPROBE: &str = "sudo modprobe binder_linux devices=binder,hwbinder,vndbinder";
const BINDER_PERSIST: &str = "to keep it across reboots, put `binder_linux` in /etc/modules-load.d/binder.conf and `options binder_linux devices=binder,hwbinder,vndbinder` in /etc/modprobe.d/binder.conf";

fn binder(view: &dyn HostView, filesystems: &str, release: &str) -> Check {
    if has_filesystem(filesystems, "binder") {
        return match view.binderfs_devices() {
            // A binderfs mounts with the devices the kernel was told to create. A plain
            // `modprobe binder_linux` (Ubuntu) tells it none, and Android has nothing to talk to.
            Some(have) if !BINDER_DEVICES.iter().all(|d| have.iter().any(|h| h == d)) => missing(
                Feature::Binder,
                format!(
                    "binderfs is registered but a test mount holds {} (needs binder, hwbinder and vndbinder)",
                    if have.is_empty() { "no devices".to_string() } else { have.join(", ") }
                ),
                format!(
                    "unload the module and reload it with its devices: `sudo modprobe -r binder_linux && {BINDER_MODPROBE}` ({BINDER_PERSIST}); built into the kernel, boot with binder.devices=binder,hwbinder,vndbinder"
                ),
            ),
            Some(_) => ok(Feature::Binder, "binderfs is registered and mounts with binder, hwbinder and vndbinder"),
            None => ok(Feature::Binder, "binderfs is registered (its device list couldn't be test-mounted here; the launcher checks it)"),
        };
    }
    let module = ["", ".zst", ".xz", ".gz"]
        .iter()
        .any(|ext| view.exists(&format!("/lib/modules/{release}/kernel/drivers/android/binder_linux.ko{ext}")));
    if module {
        return missing(
            Feature::Binder,
            "binderfs isn't registered, but this kernel ships the binder_linux module",
            format!("{BINDER_MODPROBE} ({BINDER_PERSIST})"),
        );
    }
    if view.exists(&format!("/lib/modules/{release}")) {
        return missing(
            Feature::Binder,
            format!("binderfs isn't registered and the binder_linux module isn't installed for kernel {release}"),
            format!(
                "on Ubuntu, `sudo apt install linux-modules-extra-{release}` and then `{BINDER_MODPROBE}`; elsewhere build the redroid-modules DKMS package"
            ),
        );
    }
    missing(
        Feature::Binder,
        "this kernel has no binder (not in /proc/filesystems) and can't load modules",
        "boot a kernel with CONFIG_ANDROID_BINDER_IPC=y, CONFIG_ANDROID_BINDERFS=y and CONFIG_ANDROID_BINDER_DEVICES=\"binder,hwbinder,vndbinder\" (or CONFIG_MODULES=y and the redroid-modules package)",
    )
}

fn memfd(view: &dyn HostView) -> Check {
    if view.memfd_create() {
        ok(Feature::Memfd, "memfd_create works")
    } else {
        missing(
            Feature::Memfd,
            "memfd_create fails (kernel before 3.17, or a seccomp filter denies it)",
            "run on a kernel 3.17 or newer and outside a sandbox that filters memfd_create",
        )
    }
}

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

fn cgroups_v2(view: &dyn HostView) -> Check {
    if view.exists(&format!("{CGROUP_ROOT}/cgroup.controllers")) {
        ok(Feature::CgroupsV2, "the unified hierarchy is mounted at /sys/fs/cgroup")
    } else {
        missing(
            Feature::CgroupsV2,
            "no unified cgroup hierarchy at /sys/fs/cgroup (cgroup.controllers is absent)",
            "boot with systemd.unified_cgroup_hierarchy=1 (or `mount -t cgroup2 none /sys/fs/cgroup`)",
        )
    }
}

/// The controllers a container is capped with (hw.ramSize, hw.cpu.ncore, a pid limit).
const LIMIT_CONTROLLERS: [&str; 3] = ["memory", "cpu", "pids"];

fn cgroup_limits(view: &dyn HostView) -> Check {
    let Some(have) = view.read(&format!("{CGROUP_ROOT}/cgroup.controllers")) else {
        return missing(Feature::CgroupLimits, "no cgroup v2 hierarchy to read controllers from", "see the cgroups v2 check");
    };
    let have: Vec<&str> = have.split_whitespace().collect();
    let lacking: Vec<&str> = LIMIT_CONTROLLERS.iter().copied().filter(|c| !have.contains(c)).collect();
    if lacking.is_empty() {
        ok(Feature::CgroupLimits, "memory, cpu and pids controllers are available")
    } else {
        missing(
            Feature::CgroupLimits,
            format!("the cgroup v2 root doesn't offer {}", lacking.join(", ")),
            "delegate them to your user (systemd: Delegate=memory cpu pids in a user@.service drop-in) or run as root",
        )
    }
}

fn first_line(view: &dyn HostView, path: &str) -> Option<String> {
    view.read(path).map(|s| s.trim().to_string())
}

fn user_namespaces(view: &dyn HostView) -> Check {
    if !view.exists("/proc/self/ns/user") {
        return missing(
            Feature::UserNamespaces,
            "this kernel is built without user namespaces (no /proc/self/ns/user)",
            "boot a kernel with CONFIG_USER_NS=y",
        );
    }
    if first_line(view, "/proc/sys/user/max_user_namespaces").as_deref() == Some("0") {
        return missing(Feature::UserNamespaces, "user.max_user_namespaces is 0", "sudo sysctl -w user.max_user_namespaces=15000");
    }
    // Root is held to these too: the container's process drops to its own unprivileged id before
    // it creates the namespace, so root's exemption from them wouldn't apply to it.
    if first_line(view, "/proc/sys/kernel/unprivileged_userns_clone").as_deref() == Some("0") {
        return missing(
            Feature::UserNamespaces,
            "kernel.unprivileged_userns_clone is 0, so only a process with CAP_SYS_ADMIN may create a user namespace, and the container's process runs without it",
            "sudo sysctl -w kernel.unprivileged_userns_clone=1",
        );
    }
    if first_line(view, "/proc/sys/kernel/apparmor_restrict_unprivileged_userns").as_deref() == Some("1") {
        return missing(
            Feature::UserNamespaces,
            "AppArmor strips capabilities from unprivileged user namespaces (kernel.apparmor_restrict_unprivileged_userns=1), so mounts inside one fail",
            "sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0",
        );
    }
    ok(Feature::UserNamespaces, "available to unprivileged processes")
}

/// The first `/etc/subuid`-style line for `uid` or its login name: `(start, count)`.
pub fn subordinate_range(view: &dyn HostView, file: &str, uid: u32) -> Option<(u32, u32)> {
    let name = view.read("/etc/passwd").and_then(|p| {
        p.lines().find_map(|l| {
            let mut f = l.split(':');
            let (name, _, id) = (f.next()?, f.next()?, f.next()?);
            (id.parse::<u32>().ok()? == uid).then(|| name.to_string())
        })
    });
    view.read(file)?.lines().find_map(|l| {
        let mut f = l.split(':');
        let who = f.next()?;
        let (start, count) = (f.next()?.trim().parse().ok()?, f.next()?.trim().parse().ok()?);
        (who == uid.to_string() || Some(who) == name.as_deref()).then_some((start, count))
    })
}

/// Android needs more than the one uid an unprivileged user namespace maps by default.
const MIN_SUBORDINATE_IDS: u32 = 65536;

fn subordinate_ids(view: &dyn HostView) -> Check {
    let euid = view.euid();
    if euid == 0 {
        // Root writes the maps itself and never maps container root to host root: it takes its own
        // subordinate range or a dedicated one (launch::IdMap::for_host).
        return ok(Feature::SubordinateIds, "running as root: the container's ids map to a range of their own, not to root's");
    }
    for tool in ["newuidmap", "newgidmap"] {
        if !view.on_path(tool) {
            return missing(
                Feature::SubordinateIds,
                format!("{tool} isn't on PATH, so an unprivileged user can't map more than its own uid"),
                "sudo apt install uidmap (shadow's newuidmap and newgidmap), or run as root",
            );
        }
    }
    for (file, kind) in [("/etc/subuid", "uid"), ("/etc/subgid", "gid")] {
        match subordinate_range(view, file, euid) {
            Some((_, count)) if count >= MIN_SUBORDINATE_IDS => {}
            Some((_, count)) => {
                return missing(
                    Feature::SubordinateIds,
                    format!("{file} gives this user {count} {kind}s; Android needs {MIN_SUBORDINATE_IDS}"),
                    format!("sudo usermod --add-sub{kind}s 100000-165535 $USER"),
                );
            }
            None => {
                return missing(
                    Feature::SubordinateIds,
                    format!("{file} has no range for this user"),
                    format!("sudo usermod --add-sub{kind}s 100000-165535 $USER"),
                );
            }
        }
    }
    ok(Feature::SubordinateIds, "newuidmap, newgidmap and subordinate ranges are set up")
}

/// The oldest kernel whose overlayfs can be mounted inside a user namespace (`userxattr`).
pub const OVERLAY_USERNS_KERNEL: (u32, u32) = (5, 11);

/// `6.8.0-147-generic` -> (6, 8).
pub fn kernel_version(release: &str) -> Option<(u32, u32)> {
    let mut it = release.trim().split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty());
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

fn overlayfs(release: &str, filesystems: &str) -> Check {
    if !has_filesystem(filesystems, "overlay") {
        return missing(
            Feature::Overlayfs,
            "overlay isn't in /proc/filesystems",
            "sudo modprobe overlay (or boot a kernel with CONFIG_OVERLAY_FS=y)",
        );
    }
    match kernel_version(release) {
        Some(v) if v < OVERLAY_USERNS_KERNEL => missing(
            Feature::Overlayfs,
            format!(
                "overlay is registered, but kernel {}.{} can't mount it in a user namespace (the `userxattr` option arrived in Linux {}.{})",
                v.0, v.1, OVERLAY_USERNS_KERNEL.0, OVERLAY_USERNS_KERNEL.1
            ),
            format!("boot Linux {}.{} or newer", OVERLAY_USERNS_KERNEL.0, OVERLAY_USERNS_KERNEL.1),
        ),
        _ => ok(Feature::Overlayfs, "overlay is registered"),
    }
}

fn dma_buf_heaps(view: &dyn HostView) -> Check {
    if view.exists("/dev/dma_heap/system") {
        ok(Feature::DmaBufHeaps, "/dev/dma_heap/system exists")
    } else if view.exists("/dev/ion") {
        ok(Feature::DmaBufHeaps, "/dev/ion exists")
    } else {
        missing(
            Feature::DmaBufHeaps,
            "no /dev/dma_heap/system and no /dev/ion",
            "sudo modprobe system_heap (CONFIG_DMABUF_HEAPS_SYSTEM); software rendering may still boot without it",
        )
    }
}

/// The real host.
pub struct FsView;

impl HostView for FsView {
    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn exists(&self, path: &str) -> bool {
        Path::new(path).exists()
    }

    fn on_path(&self, name: &str) -> bool {
        let path = std::env::var("PATH").unwrap_or_default();
        path.split(':').filter(|d| !d.is_empty()).any(|d| Path::new(d).join(name).is_file())
    }

    #[cfg(target_os = "linux")]
    fn memfd_create(&self) -> bool {
        rustix::fs::memfd_create("simee-probe", rustix::fs::MemfdFlags::CLOEXEC).is_ok()
    }

    #[cfg(not(target_os = "linux"))]
    fn memfd_create(&self) -> bool {
        false
    }

    fn euid(&self) -> u32 {
        rustix::process::geteuid().as_raw()
    }

    fn arch(&self) -> String {
        std::env::consts::ARCH.to_string()
    }

    /// Mounts binderfs in a throwaway user namespace (`unshare`) and lists it.
    fn binderfs_devices(&self) -> Option<Vec<String>> {
        let dir = std::env::temp_dir().join(format!("simee-binderfs-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let out = std::process::Command::new("unshare")
            .args(["--user", "--map-root-user", "--mount", "--", "sh", "-c", "mount -t binder binder \"$1\" && ls \"$1\"", "sh"])
            .arg(&dir)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok();
        let _ = std::fs::remove_dir(&dir);
        let out = out.filter(|o| o.status.success())?;
        Some(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && *l != "binder-control" && *l != "features")
                .map(String::from)
                .collect(),
        )
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use super::HostView;
    use std::collections::{BTreeMap, BTreeSet};

    /// A host described as data: files with contents, bare paths, tools on `PATH`.
    #[derive(Default, Clone)]
    pub struct FakeView {
        pub files: BTreeMap<String, String>,
        pub paths: BTreeSet<String>,
        pub tools: BTreeSet<String>,
        pub memfd: bool,
        pub euid: u32,
        pub arch: String,
        /// What a test mount of binderfs holds; `None` is "couldn't be tried".
        pub binderfs: Option<Vec<String>>,
    }

    impl FakeView {
        pub fn file(mut self, path: &str, body: &str) -> Self {
            self.files.insert(path.into(), body.into());
            self
        }
        pub fn path(mut self, path: &str) -> Self {
            self.paths.insert(path.into());
            self
        }
        pub fn tool(mut self, name: &str) -> Self {
            self.tools.insert(name.into());
            self
        }
        pub fn euid(mut self, euid: u32) -> Self {
            self.euid = euid;
            self
        }

        /// A host where everything a container needs is there, as an Ubuntu 24.04 cloud VM with
        /// `linux-modules-extra` loaded: root, binderfs registered, unified cgroups.
        pub fn container_host() -> Self {
            FakeView {
                memfd: true,
                arch: "x86_64".into(),
                binderfs: Some(["binder", "hwbinder", "vndbinder"].map(String::from).to_vec()),
                ..Default::default()
            }
            .file("/proc/filesystems", "nodev\tsysfs\nnodev\tproc\nnodev\tcgroup2\nnodev\toverlay\nnodev\tbinder\n\text4\n")
            .file("/proc/sys/kernel/osrelease", "6.8.0-147-generic\n")
            .file("/sys/fs/cgroup/cgroup.controllers", "cpuset cpu io memory hugetlb pids rdma misc\n")
            .path("/proc/self/ns/user")
            .file("/proc/sys/user/max_user_namespaces", "63704\n")
            .path("/dev/dma_heap")
            .path("/dev/dma_heap/system")
            .path("/proc/kcore")
            .path("/proc/keys")
            .path("/proc/timer_list")
            .path("/proc/sched_debug")
            .path("/proc/acpi")
            .path("/proc/scsi")
            .path("/proc/bus")
            .path("/proc/fs")
            .path("/proc/irq")
            .path("/proc/sys")
            .path("/proc/sysrq-trigger")
            .path("/sys/firmware")
        }
    }

    impl HostView for FakeView {
        fn read(&self, path: &str) -> Option<String> {
            self.files.get(path).cloned()
        }
        fn exists(&self, path: &str) -> bool {
            self.files.contains_key(path) || self.paths.contains(path)
        }
        fn on_path(&self, name: &str) -> bool {
            self.tools.contains(name)
        }
        fn memfd_create(&self) -> bool {
            self.memfd
        }
        fn euid(&self) -> u32 {
            self.euid
        }
        fn arch(&self) -> String {
            self.arch.clone()
        }
        fn binderfs_devices(&self) -> Option<Vec<String>> {
            self.binderfs.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeView;
    use super::*;

    fn status(r: &Report, f: Feature) -> &Status {
        &r.get(f).unwrap_or_else(|| panic!("no {f} check")).status
    }

    fn fix_of(r: &Report, f: Feature) -> String {
        match status(r, f) {
            Status::Missing { fix, .. } => fix.clone(),
            Status::Ok { detail } => panic!("{f} is ok: {detail}"),
        }
    }

    #[test]
    fn a_complete_host_is_usable_and_every_check_is_ok() {
        let r = probe(&FakeView::container_host());
        assert!(r.usable(), "{:?}", r.explain());
        assert!(r.checks.iter().all(Check::ok), "{:#?}", r.lines());
        assert_eq!(r.explain(), None);
        assert_eq!(r.get(Feature::Linux).map(Check::line), Some("linux: ok: kernel 6.8.0-147-generic".into()));
    }

    #[test]
    fn not_linux_asks_nothing_else() {
        let r = probe(&FakeView::default());
        assert_eq!(r.checks.len(), 1);
        assert!(!r.usable());
        assert!(fix_of(&r, Feature::Linux).contains("Linux host"));
    }

    #[test]
    fn binder_is_found_in_proc_filesystems_whatever_the_flag_column_says() {
        for line in ["nodev\tbinder\n", "\tbinder\n", "nodev   binder\n"] {
            let v = FakeView::container_host().file("/proc/filesystems", &format!("nodev\toverlay\n{line}"));
            assert!(probe(&v).get(Feature::Binder).is_some_and(Check::ok), "{line:?}");
        }
        // A filesystem merely named like binder doesn't count.
        let v = FakeView::container_host().file("/proc/filesystems", "nodev\toverlay\nnodev\tbinderfs_stats\n");
        assert!(!probe(&v).get(Feature::Binder).is_some_and(Check::ok));
    }

    #[test]
    fn binder_module_installed_but_not_loaded_says_modprobe() {
        let v = FakeView::container_host()
            .file("/proc/filesystems", "nodev\toverlay\n")
            .path("/lib/modules/6.8.0-147-generic/kernel/drivers/android/binder_linux.ko.zst");
        let r = probe(&v);
        assert!(fix_of(&r, Feature::Binder).starts_with("sudo modprobe binder_linux devices=binder,hwbinder,vndbinder"));
        assert!(!r.usable());
    }

    #[test]
    fn binder_module_not_installed_says_modules_extra_for_this_kernel() {
        let v = FakeView::container_host().file("/proc/filesystems", "nodev\toverlay\n").path("/lib/modules/6.8.0-147-generic");
        let fix = fix_of(&probe(&v), Feature::Binder);
        assert!(fix.contains("linux-modules-extra-6.8.0-147-generic"), "{fix}");
        assert!(fix.contains("redroid-modules"), "{fix}");
    }

    #[test]
    fn binder_on_a_kernel_without_modules_says_which_config_lines() {
        // This is the Hive VM: no binder, CONFIG_MODULES is not set, no /lib/modules.
        let v = FakeView::container_host().file("/proc/filesystems", "nodev\toverlay\nnodev\tcgroup2\n");
        let fix = fix_of(&probe(&v), Feature::Binder);
        for want in ["CONFIG_ANDROID_BINDER_IPC=y", "CONFIG_ANDROID_BINDERFS=y", "binder,hwbinder,vndbinder"] {
            assert!(fix.contains(want), "{fix}");
        }
    }

    #[test]
    fn memfd_missing_is_a_blocker_with_a_fix() {
        let mut v = FakeView::container_host();
        v.memfd = false;
        let r = probe(&v);
        assert!(!r.usable());
        assert!(fix_of(&r, Feature::Memfd).contains("3.17"));
    }

    #[test]
    fn cgroups_v2_needs_the_unified_hierarchy() {
        let mut v = FakeView::container_host();
        v.files.remove("/sys/fs/cgroup/cgroup.controllers");
        let r = probe(&v);
        assert!(fix_of(&r, Feature::CgroupsV2).contains("unified_cgroup_hierarchy"));
        assert!(!r.usable());
    }

    #[test]
    fn missing_cgroup_controllers_degrade_without_blocking() {
        let v = FakeView::container_host().file("/sys/fs/cgroup/cgroup.controllers", "cpuset cpu\n");
        let r = probe(&v);
        let Status::Missing { why, .. } = status(&r, Feature::CgroupLimits) else { panic!("limits should be missing") };
        assert!(why.contains("memory, pids"), "{why}");
        assert!(r.usable(), "advisory checks don't block");
        assert!(r.blockers().is_empty());
    }

    #[test]
    fn dma_buf_heaps_are_advisory_and_ion_counts() {
        let mut v = FakeView::container_host();
        v.paths.remove("/dev/dma_heap/system");
        let r = probe(&v);
        assert!(!r.get(Feature::DmaBufHeaps).is_some_and(Check::ok));
        assert!(r.usable());
        assert!(probe(&v.path("/dev/ion")).get(Feature::DmaBufHeaps).is_some_and(Check::ok));
    }

    #[test]
    fn overlayfs_missing_blocks() {
        let v = FakeView::container_host().file("/proc/filesystems", "nodev\tbinder\n");
        let r = probe(&v);
        assert!(fix_of(&r, Feature::Overlayfs).contains("modprobe overlay"));
        assert!(!r.usable());
    }

    #[test]
    fn user_namespaces_each_way_to_be_off() {
        let off = |v: FakeView| fix_of(&probe(&v), Feature::UserNamespaces);
        let mut no_ns = FakeView::container_host();
        no_ns.paths.remove("/proc/self/ns/user");
        assert!(off(no_ns).contains("CONFIG_USER_NS"));
        assert!(off(FakeView::container_host().file("/proc/sys/user/max_user_namespaces", "0\n")).contains("max_user_namespaces=15000"));
        // Root is held to the unprivileged-namespace sysctls as well: its container runs as a
        // plain user, so they apply to it.
        for euid in [0, 1000] {
            assert!(
                off(FakeView::container_host().euid(euid).file("/proc/sys/kernel/unprivileged_userns_clone", "0\n"))
                    .contains("unprivileged_userns_clone=1"),
                "euid {euid}"
            );
            assert!(
                off(FakeView::container_host().euid(euid).file("/proc/sys/kernel/apparmor_restrict_unprivileged_userns", "1\n"))
                    .contains("apparmor_restrict_unprivileged_userns=0"),
                "euid {euid}"
            );
        }
    }

    fn unprivileged() -> FakeView {
        FakeView::container_host()
            .euid(1000)
            .file("/etc/passwd", "root:x:0:0::/root:/bin/bash\nci:x:1000:1000::/home/ci:/bin/bash\n")
            .file("/etc/subuid", "ci:100000:65536\n")
            .file("/etc/subgid", "1000:100000:65536\n")
            .tool("newuidmap")
            .tool("newgidmap")
    }

    #[test]
    fn unprivileged_with_ranges_and_helpers_is_usable() {
        let r = probe(&unprivileged());
        assert!(r.usable(), "{:?}", r.explain());
        // subuid by login name, subgid by numeric id: both forms are read
        assert_eq!(subordinate_range(&unprivileged(), "/etc/subuid", 1000), Some((100000, 65536)));
        assert_eq!(subordinate_range(&unprivileged(), "/etc/subgid", 1000), Some((100000, 65536)));
        assert_eq!(subordinate_range(&unprivileged(), "/etc/subuid", 1001), None);
    }

    #[test]
    fn unprivileged_without_the_helpers_or_ranges_is_told_what_to_install() {
        let mut v = unprivileged();
        v.tools.remove("newgidmap");
        assert!(fix_of(&probe(&v), Feature::SubordinateIds).contains("uidmap"));
        let v = unprivileged().file("/etc/subuid", "ci:100000:1000\n");
        let fix = fix_of(&probe(&v), Feature::SubordinateIds);
        assert!(fix.contains("--add-subuids"), "{fix}");
        let v = unprivileged().file("/etc/subgid", "someone-else:100000:65536\n");
        assert!(fix_of(&probe(&v), Feature::SubordinateIds).contains("--add-subgids"));
        assert!(!probe(&v).usable());
    }

    #[test]
    fn explain_lists_every_blocker_with_its_fix() {
        let v = FakeView::container_host().file("/proc/filesystems", "nodev\tsysfs\n");
        let text = probe(&v).explain().expect("blocked");
        assert!(text.contains("binder: missing"), "{text}");
        assert!(text.contains("overlayfs: missing"), "{text}");
        assert!(text.contains("(fix: "), "{text}");
        assert!(!text.contains("dma-buf"), "advisory checks aren't blockers: {text}");
    }

    #[test]
    fn every_missing_check_has_a_non_empty_fix() {
        // The strictest host: nothing at all but /proc/filesystems.
        let r = probe(&FakeView::default().file("/proc/filesystems", "nodev\tsysfs\n").euid(1000));
        let mut missing = 0;
        for c in &r.checks {
            if let Status::Missing { why, fix } = &c.status {
                missing += 1;
                assert!(!why.is_empty() && fix.len() > 10, "{}", c.line());
            }
        }
        assert!(missing >= 6, "{:#?}", r.lines());
    }

    #[test]
    fn a_binderfs_that_mounts_empty_is_not_usable() {
        // Ubuntu's plain `modprobe binder_linux`: registered, but no devices to talk to.
        let mut v = FakeView::container_host();
        v.binderfs = Some(vec![]);
        let r = probe(&v);
        assert!(!r.usable());
        let Status::Missing { why, fix } = status(&r, Feature::Binder) else { panic!("binder should be missing") };
        assert!(why.contains("no devices"), "{why}");
        assert!(fix.contains("modprobe -r binder_linux") && fix.contains("binder.devices="), "{fix}");
        // A partial set is no better.
        v.binderfs = Some(vec!["binder".into()]);
        let Status::Missing { why, .. } = status(&probe(&v), Feature::Binder).clone() else { panic!("binder should be missing") };
        assert!(why.contains("holds binder (needs"), "{why}");
    }

    #[test]
    fn binder_devices_that_cant_be_test_mounted_pass_on_registration_and_say_so() {
        let mut v = FakeView::container_host();
        v.binderfs = None;
        let r = probe(&v);
        assert!(r.usable());
        assert!(r.get(Feature::Binder).is_some_and(|c| c.line().contains("couldn't be test-mounted")));
    }

    #[test]
    fn the_modprobe_fix_puts_devices_in_modprobe_d_and_the_module_in_modules_load_d() {
        let v = FakeView::container_host()
            .file("/proc/filesystems", "nodev\toverlay\n")
            .path("/lib/modules/6.8.0-147-generic/kernel/drivers/android/binder_linux.ko.zst");
        let fix = fix_of(&probe(&v), Feature::Binder);
        assert!(fix.contains("`binder_linux` in /etc/modules-load.d/binder.conf"), "{fix}");
        assert!(fix.contains("`options binder_linux devices=binder,hwbinder,vndbinder` in /etc/modprobe.d/binder.conf"), "{fix}");
        assert!(!fix.contains("devices=binder,hwbinder,vndbinder` in /etc/modules-load.d"), "{fix}");
    }

    #[test]
    fn kernel_releases_parse_to_major_minor() {
        assert_eq!(kernel_version("6.8.0-147-generic\n"), Some((6, 8)));
        assert_eq!(kernel_version("5.11.0"), Some((5, 11)));
        assert_eq!(kernel_version("6.1.189"), Some((6, 1)));
        assert_eq!(kernel_version(""), None);
        assert_eq!(kernel_version("generic"), None);
    }

    #[test]
    fn overlay_in_a_user_namespace_needs_linux_5_11() {
        let on = |rel: &str| probe(&FakeView::container_host().file("/proc/sys/kernel/osrelease", rel));
        for old in ["5.4.0-170-generic", "5.10.200", "4.19.0"] {
            let r = on(old);
            assert!(!r.usable(), "{old}");
            let fix = fix_of(&r, Feature::Overlayfs);
            assert!(fix.contains("5.11"), "{fix}");
        }
        for fine in ["5.11.0", "5.15.0-1", "6.8.0-147-generic", "7.0.0"] {
            assert!(on(fine).get(Feature::Overlayfs).is_some_and(Check::ok), "{fine}");
        }
        // An unparsable release doesn't block on a guess.
        assert!(on("custom").get(Feature::Overlayfs).is_some_and(Check::ok));
    }

    #[test]
    fn the_real_host_view_answers_without_panicking() {
        // No assertion on what this host has; the probe must run on any machine, Mac included.
        let r = probe(&FsView);
        assert!(!r.checks.is_empty());
        assert_eq!(FsView.arch(), std::env::consts::ARCH);
    }
}
