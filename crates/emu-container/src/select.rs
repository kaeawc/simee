//! Which backend runs an Android guest on a Linux host (#210): KVM first; then the container,
//! only on a host without usable KVM that has what a container needs; then TCG software mode.
//! `-backend kvm|container|tcg` overrides the order, and then the answer is that backend or the
//! reason it can't run: an explicit request never falls back to another.
//!
//! The one function `apps/emu` calls is [`choose`]; [`select`] is the same decision over facts the
//! caller has already gathered.

use std::fmt;
use std::str::FromStr;

use crate::probe::{FsView, HostView, probe};

/// What can run a guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Backend {
    /// emu's KVM backend: the guest's CPU on `/dev/kvm`.
    Kvm,
    /// Android's userspace in a container on the host kernel.
    Container,
    /// QEMU's TCG software mode.
    Tcg,
}

/// Automatic selection tries these in order.
pub const ORDER: [Backend; 3] = [Backend::Kvm, Backend::Container, Backend::Tcg];

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Kvm => "kvm",
            Backend::Container => "container",
            Backend::Tcg => "tcg",
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Backend {
    type Err = Error;

    /// `kvm`, `container` or `tcg` (any case); `auto` is "no override" and the caller's `None`.
    fn from_str(s: &str) -> Result<Self, Error> {
        ORDER.into_iter().find(|b| b.name().eq_ignore_ascii_case(s)).ok_or_else(|| Error::Unknown(s.to_string()))
    }
}

/// `Ok` when the backend can run this guest here, else why not and how to fix it.
pub type Availability = Result<(), String>;

/// What each backend can do on this host, for one guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    pub kvm: Availability,
    pub container: Availability,
    pub tcg: Availability,
}

impl Facts {
    fn of(&self, b: Backend) -> &Availability {
        match b {
            Backend::Kvm => &self.kvm,
            Backend::Container => &self.container,
            Backend::Tcg => &self.tcg,
        }
    }
}

/// The chosen backend and the reasons behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub backend: Backend,
    /// `-verbose`'s line: what was chosen and why, naming each backend that was passed over.
    pub why: String,
    /// The backends tried first and why each was skipped, in order.
    pub skipped: Vec<(Backend, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("-backend {0}: expected kvm, container, tcg or auto")]
    Unknown(String),
    #[error("-backend {backend}: {why}")]
    Unavailable { backend: Backend, why: String },
    #[error("no backend can run this guest here: {}", .0.iter().map(|(b, w)| format!("{b}: {w}")).collect::<Vec<_>>().join("; "))]
    NoneAvailable(Vec<(Backend, String)>),
}

/// The decision. `want` is `-backend`'s value, `None` for automatic.
pub fn select(want: Option<Backend>, facts: &Facts) -> Result<Selection, Error> {
    if let Some(backend) = want {
        return match facts.of(backend) {
            Ok(()) => Ok(Selection { backend, why: format!("{backend} (-backend {backend})"), skipped: vec![] }),
            Err(why) => Err(Error::Unavailable { backend, why: why.clone() }),
        };
    }
    let mut skipped = Vec::new();
    for backend in ORDER {
        match facts.of(backend) {
            Ok(()) => {
                let why = if skipped.is_empty() {
                    format!("{backend} (first choice)")
                } else {
                    format!("{backend} ({})", skipped.iter().map(|(b, w)| format!("{b} unavailable: {w}")).collect::<Vec<_>>().join("; "))
                };
                return Ok(Selection { backend, why, skipped });
            }
            Err(why) => skipped.push((backend, why.clone())),
        }
    }
    Err(Error::NoneAvailable(skipped))
}

/// The Android ABI of an AVD (`abi.type` or the system image's `x86_64`, `arm64-v8a`) as the host
/// architecture that runs it natively (`std::env::consts::ARCH`'s spelling).
pub fn abi_arch(abi: &str) -> Option<&'static str> {
    match abi {
        "x86_64" => Some("x86_64"),
        "arm64-v8a" => Some("aarch64"),
        _ => None,
    }
}

/// Whether the container backend can run a guest of `abi` on the host `view` describes: the
/// kernel has what a container needs, and the guest is the host's own architecture (a container
/// runs the image's binaries on the host's CPU, so there is no emulating a different one).
pub fn container_availability(view: &dyn HostView, abi: &str) -> Availability {
    let guest = abi_arch(abi).ok_or_else(|| format!("no container image for ABI {abi}"))?;
    let host = view.arch();
    if guest != host {
        return Err(format!(
            "the guest is {abi} and this host is {host}: a container runs on the host's CPU, so only {} guests",
            host_abi(&host)
        ));
    }
    probe(view).explain().map_or(Ok(()), Err)
}

fn host_abi(host: &str) -> &'static str {
    match host {
        "x86_64" => "x86_64",
        "aarch64" => "arm64-v8a",
        _ => "no supported ABI",
    }
}

/// [`select`] with the container's facts read from `view`. `kvm` and `tcg` come from the caller,
/// which already knows how: `emu_kvm::open()` and creating a VM for KVM, a new enough QEMU for TCG.
pub fn select_on(view: &dyn HostView, want: Option<Backend>, abi: &str, kvm: Availability, tcg: Availability) -> Result<Selection, Error> {
    // The probe reads a few files; skip it when the choice can't need it.
    let container = if want.is_none_or(|b| b == Backend::Container) && !(want.is_none() && kvm.is_ok()) {
        container_availability(view, abi)
    } else {
        Err("not probed (another backend was chosen first)".into())
    };
    select(want, &Facts { kvm, container, tcg })
}

/// What `apps/emu` calls: which backend boots an AVD of `abi` on this host, reading the real
/// kernel's `/proc` and `/sys`.
pub fn choose(want: Option<Backend>, abi: &str, kvm: Availability, tcg: Availability) -> Result<Selection, Error> {
    select_on(&FsView, want, abi, kvm, tcg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::fake::FakeView;

    fn up() -> Availability {
        Ok(())
    }
    fn down(why: &str) -> Availability {
        Err(why.to_string())
    }
    fn facts(kvm: bool, container: bool, tcg: bool) -> Facts {
        let f = |ok, name: &str| if ok { up() } else { down(&format!("{name} off")) };
        Facts { kvm: f(kvm, "kvm"), container: f(container, "container"), tcg: f(tcg, "tcg") }
    }

    #[test]
    fn automatic_order_for_every_host_combination() {
        // (kvm, container, tcg) -> the chosen backend, the owner's order: KVM, container, TCG.
        let table = [
            ((true, true, true), Some(Backend::Kvm)),
            ((true, true, false), Some(Backend::Kvm)),
            ((true, false, true), Some(Backend::Kvm)),
            ((true, false, false), Some(Backend::Kvm)),
            ((false, true, true), Some(Backend::Container)),
            ((false, true, false), Some(Backend::Container)),
            ((false, false, true), Some(Backend::Tcg)),
            ((false, false, false), None),
        ];
        for ((k, c, t), want) in table {
            let got = select(None, &facts(k, c, t)).ok().map(|s| s.backend);
            assert_eq!(got, want, "kvm={k} container={c} tcg={t}");
        }
    }

    #[test]
    fn the_selection_names_why_each_earlier_backend_was_skipped() {
        let s = select(None, &facts(false, true, true)).expect("container");
        assert_eq!(s.skipped, vec![(Backend::Kvm, "kvm off".to_string())]);
        assert_eq!(s.why, "container (kvm unavailable: kvm off)");
        let s = select(None, &facts(false, false, true)).expect("tcg");
        assert_eq!(s.skipped.len(), 2);
        assert_eq!(s.why, "tcg (kvm unavailable: kvm off; container unavailable: container off)");
        assert_eq!(select(None, &facts(true, true, true)).expect("kvm").why, "kvm (first choice)");
    }

    #[test]
    fn nothing_available_lists_every_reason() {
        let e = select(None, &facts(false, false, false)).expect_err("none");
        assert_eq!(e.to_string(), "no backend can run this guest here: kvm: kvm off; container: container off; tcg: tcg off");
    }

    #[test]
    fn an_override_is_obeyed_whatever_the_order_would_pick() {
        for b in ORDER {
            let s = select(Some(b), &facts(true, true, true)).expect("override");
            assert_eq!(s.backend, b);
            assert!(s.skipped.is_empty());
            assert_eq!(s.why, format!("{b} (-backend {b})"));
        }
    }

    #[test]
    fn an_override_never_falls_back() {
        // Container asked for, KVM works and container doesn't: an error, not KVM.
        let e = select(Some(Backend::Container), &facts(true, false, true)).expect_err("no fallback");
        assert_eq!(e, Error::Unavailable { backend: Backend::Container, why: "container off".into() });
        assert_eq!(e.to_string(), "-backend container: container off");
        let e = select(Some(Backend::Kvm), &facts(false, true, true)).expect_err("no fallback");
        assert!(e.to_string().starts_with("-backend kvm: "), "{e}");
        assert!(select(Some(Backend::Tcg), &facts(true, true, false)).is_err());
    }

    #[test]
    fn a_forced_container_works_even_with_kvm_present() {
        let s = select(Some(Backend::Container), &facts(true, true, true)).expect("forced");
        assert_eq!(s.backend, Backend::Container);
    }

    #[test]
    fn backend_names_parse_and_print() {
        for (text, b) in [("kvm", Backend::Kvm), ("container", Backend::Container), ("TCG", Backend::Tcg), ("Kvm", Backend::Kvm)] {
            assert_eq!(text.parse::<Backend>(), Ok(b));
        }
        assert_eq!(Backend::Container.to_string(), "container");
        assert_eq!("auto".parse::<Backend>(), Err(Error::Unknown("auto".into())));
        assert_eq!("hvf".parse::<Backend>().expect_err("unknown").to_string(), "-backend hvf: expected kvm, container, tcg or auto");
    }

    #[test]
    fn abis_map_to_host_architectures() {
        assert_eq!(abi_arch("x86_64"), Some("x86_64"));
        assert_eq!(abi_arch("arm64-v8a"), Some("aarch64"));
        assert_eq!(abi_arch("x86"), None);
        assert_eq!(abi_arch("armeabi-v7a"), None);
    }

    #[test]
    fn the_container_needs_a_capable_host_and_a_matching_architecture() {
        let host = FakeView::container_host();
        assert_eq!(container_availability(&host, "x86_64"), Ok(()));
        let arm_guest = container_availability(&host, "arm64-v8a").expect_err("arch mismatch");
        assert!(arm_guest.contains("this host is x86_64") && arm_guest.contains("only x86_64 guests"), "{arm_guest}");
        assert!(container_availability(&host, "x86").expect_err("no image").contains("no container image for ABI x86"));
        let no_binder = FakeView::container_host().file("/proc/filesystems", "nodev\toverlay\n");
        let why = container_availability(&no_binder, "x86_64").expect_err("no binder");
        assert!(why.contains("binder: missing") && why.contains("(fix: "), "{why}");
        let mut arm_host = FakeView::container_host();
        arm_host.arch = "aarch64".into();
        assert_eq!(container_availability(&arm_host, "arm64-v8a"), Ok(()));
    }

    #[test]
    fn select_on_picks_the_container_on_a_binder_host_without_kvm() {
        let host = FakeView::container_host();
        let s = select_on(&host, None, "x86_64", down("no /dev/kvm"), up()).expect("container");
        assert_eq!(s.backend, Backend::Container);
        // With KVM the same host runs KVM, and the container isn't even probed.
        let s = select_on(&host, None, "x86_64", up(), up()).expect("kvm");
        assert_eq!(s.backend, Backend::Kvm);
    }

    #[test]
    fn select_on_a_host_without_binder_falls_to_tcg_and_says_why() {
        let host = FakeView::container_host().file("/proc/filesystems", "nodev\toverlay\n");
        let s = select_on(&host, None, "x86_64", down("no /dev/kvm"), up()).expect("tcg");
        assert_eq!(s.backend, Backend::Tcg);
        assert!(s.why.contains("container unavailable: binder: missing"), "{}", s.why);
    }

    #[test]
    fn select_on_an_arm_guest_on_an_x86_host_is_not_a_container() {
        let s = select_on(&FakeView::container_host(), None, "arm64-v8a", down("no /dev/kvm"), down("x86_64 only")).expect_err("none");
        let text = s.to_string();
        assert!(text.contains("container: the guest is arm64-v8a"), "{text}");
    }

    #[test]
    fn select_on_a_forced_container_reports_the_probe_when_it_fails() {
        let host = FakeView::container_host().file("/proc/filesystems", "nodev\toverlay\n");
        let e = select_on(&host, Some(Backend::Container), "x86_64", up(), up()).expect_err("no binder");
        assert!(e.to_string().starts_with("-backend container: binder: missing"), "{e}");
    }

    #[test]
    fn a_forced_other_backend_doesnt_probe_the_container() {
        // The container fact is a placeholder; KVM is forced and available.
        let s = select_on(&FakeView::default(), Some(Backend::Kvm), "x86_64", up(), up()).expect("kvm");
        assert_eq!(s.backend, Backend::Kvm);
    }

    #[test]
    fn choose_reads_the_real_host_without_panicking() {
        // The answer depends on the machine; with KVM up it is KVM everywhere.
        let s = choose(None, "x86_64", up(), up()).expect("kvm");
        assert_eq!(s.backend, Backend::Kvm);
    }
}
