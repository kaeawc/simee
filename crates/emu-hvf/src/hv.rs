//! The hypervisor itself: the VM, its GIC, vCPUs and the host counter (Apple silicon only).
use std::marker::PhantomData;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{Error, GuestRam, HostRange, Result, ffi};

fn check(call: &'static str, code: i32) -> Result<()> {
    if code == 0 { Ok(()) } else { Err(Error::Hv { call, code }) }
}

static VM_EXISTS: AtomicBool = AtomicBool::new(false);

/// General purpose and control registers (`hv_reg_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reg(pub u32);

impl Reg {
    pub const PC: Reg = Reg(31);
    pub const FPCR: Reg = Reg(32);
    pub const FPSR: Reg = Reg(33);
    pub const CPSR: Reg = Reg(34);

    /// X0..=X30.
    pub fn x(n: u32) -> Reg {
        assert!(n <= 30, "no such general purpose register");
        Reg(n)
    }
}

/// System registers (`hv_sys_reg_t`), encoded as op0:op1:crn:crm:op2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SysReg(pub u16);

impl SysReg {
    pub const MPIDR_EL1: SysReg = SysReg(0xc005);
    pub const SCTLR_EL1: SysReg = SysReg(0xc080);
    pub const CNTV_CTL_EL0: SysReg = SysReg(0xdf19);
    pub const CNTV_CVAL_EL0: SysReg = SysReg(0xdf1a);
}

/// Addresses and sizes of the in-kernel GICv3 after creation.
#[derive(Clone, Copy, Debug)]
pub struct GicInfo {
    pub dist_base: u64,
    pub dist_size: u64,
    pub redist_base: u64,
    /// Size of the whole redistributor region (all vCPUs).
    pub redist_region_size: u64,
    /// First valid SPI INTID and how many follow it.
    pub spi_base: u32,
    pub spi_count: u32,
}

/// The virtual machine. One per process; destroyed on drop.
pub struct Vm {
    granule_kib: u32,
}

/// This host's macOS release (`kern.osproductversion`, e.g. `26.6.2`), `None` if unreadable.
pub fn macos_version() -> Option<(u32, u32, u32)> {
    let mut buf = [0u8; 32];
    let mut len = buf.len();
    // SAFETY: a NUL-terminated name; `buf` is `len` writable bytes and `len` is updated to the
    // bytes written; no new value is set.
    let rc = unsafe { libc::sysctlbyname(c"kern.osproductversion".as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
    if rc != 0 {
        return None;
    }
    parse_version(std::str::from_utf8(&buf[..len.min(buf.len())]).ok()?.trim_end_matches('\0'))
}

/// `major[.minor[.patch]]` as numbers (`14.10` is after `14.2`).
fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.trim().split('.').map(|p| p.parse::<u32>().ok());
    let v = (parts.next()??, parts.next().unwrap_or(Some(0))?, parts.next().unwrap_or(Some(0))?);
    parts.next().is_none().then_some(v)
}

fn show((major, minor, patch): (u32, u32, u32)) -> String {
    match (minor, patch) {
        (0, 0) => major.to_string(),
        (_, 0) => format!("{major}.{minor}"),
        _ => format!("{major}.{minor}.{patch}"),
    }
}

/// The in-kernel GIC's functions, or why they're missing: the framework has had them since
/// macOS 15.0 (`API_AVAILABLE(macos(15.0))` in `hv_gic.h`).
fn gic() -> Result<&'static ffi::GicFns> {
    let host = macos_version();
    match (&ffi::late().gic, host) {
        (Some(g), Some(v)) if v >= (15, 0, 0) => Ok(g),
        (Some(g), None) => Ok(g),
        (_, host) => Err(Error::Unsupported {
            what: "the in-kernel GICv3 (hv_gic_create), which the Android board's interrupts need",
            needs: "15",
            host: host.map_or_else(|| "an unknown release".to_owned(), show),
        }),
    }
}

/// The GIC's functions on a hot path (an SPI, a PPI): no version read once they're there.
fn gic_created() -> Result<&'static ffi::GicFns> {
    match &ffi::late().gic {
        Some(g) => Ok(g),
        None => gic(),
    }
}

/// Whether this host can map guest memory at 4 KiB granularity (macOS 26.0+:
/// `hv_vm_config_set_ipa_granule`); [`Vm::granule_kib`] says what a VM got.
pub fn granule_4k_supported() -> bool {
    ffi::late().set_ipa_granule.is_some() && macos_version().is_none_or(|v| v >= (26, 0, 0))
}

/// VM-wide settings fixed at creation.
#[derive(Clone, Copy, Debug)]
pub struct VmConfig {
    /// Guest physical address bits (clamped to what the host supports).
    pub ipa_bits: u32,
    /// Map guest memory at 4 KiB granularity instead of the 16 KiB default (macOS 26+). Needed
    /// for host-visible GPU memory, which the guest maps at 4 KiB-aligned offsets.
    pub granule_4k: bool,
}

impl Default for VmConfig {
    fn default() -> Self {
        VmConfig { ipa_bits: 40, granule_4k: true }
    }
}

impl Vm {
    pub fn new() -> Result<Vm> {
        Vm::with_config(VmConfig::default())
    }

    pub fn with_config(cfg: VmConfig) -> Result<Vm> {
        if VM_EXISTS.swap(true, Ordering::SeqCst) {
            return Err(Error::AlreadyCreated);
        }
        let late = ffi::late();
        let want_4k = cfg.granule_4k && granule_4k_supported();
        let mut granule_kib = 16;
        // SAFETY: plain FFI calls on a config object we own; the framework retains it. Without
        // the configuration API (before macOS 13) a null config selects the defaults.
        let rc = unsafe {
            let config = match &late.vm_config {
                Some(vc) => {
                    let config = (vc.create)();
                    let mut max = 0u32;
                    if (vc.get_max_ipa_size)(&mut max) == 0 {
                        let _ = (vc.set_ipa_size)(config, cfg.ipa_bits.min(max));
                    }
                    if let Some(set) = late.set_ipa_granule.filter(|_| want_4k)
                        && set(config, ffi::IPA_GRANULE_4KB) == 0
                    {
                        granule_kib = 4;
                    }
                    config
                }
                None => std::ptr::null_mut(),
            };
            ffi::hv_vm_create(config)
        };
        if rc != 0 {
            VM_EXISTS.store(false, Ordering::SeqCst);
            return check("hv_vm_create", rc).map(|()| unreachable!());
        }
        Ok(Vm { granule_kib })
    }

    /// The granule guest memory is mapped at: 4 KiB when asked for and the host has it (macOS
    /// 26+), else the 16 KiB default.
    pub fn granule_kib(&self) -> u32 {
        self.granule_kib
    }

    /// Creates the in-kernel GICv3. Must run before any vCPU is created. Before macOS 15 the
    /// framework has none: [`Error::Unsupported`] says so, before any GIC call.
    pub fn create_gic(&self, dist_base: u64, redist_base: u64) -> Result<GicInfo> {
        let g = gic()?;
        let mut dist_size = 0usize;
        let mut redist_region_size = 0usize;
        let (mut spi_base, mut spi_count) = (0u32, 0u32);
        // SAFETY: out-pointers reference live locals; the config object is owned by the
        // framework and intentionally leaked (one per process lifetime).
        unsafe {
            check("hv_gic_get_distributor_size", (g.get_distributor_size)(&mut dist_size))?;
            check("hv_gic_get_redistributor_region_size", (g.get_redistributor_region_size)(&mut redist_region_size))?;
            check("hv_gic_get_spi_interrupt_range", (g.get_spi_interrupt_range)(&mut spi_base, &mut spi_count))?;
            let cfg = (g.config_create)();
            check("hv_gic_config_set_distributor_base", (g.config_set_distributor_base)(cfg, dist_base))?;
            check("hv_gic_config_set_redistributor_base", (g.config_set_redistributor_base)(cfg, redist_base))?;
            check("hv_gic_create", (g.create)(cfg))?;
        }
        Ok(GicInfo {
            dist_base,
            dist_size: dist_size as u64,
            redist_base,
            redist_region_size: redist_region_size as u64,
            spi_base,
            spi_count,
        })
    }

    /// Maps `ram` into the guest at its base address, readable, writable and executable.
    pub fn map_ram(&self, ram: &GuestRam) -> Result<()> {
        const RWX: u64 = 0b111;
        // SAFETY: `ram` owns a page-aligned mapping of exactly `len` bytes that outlives the VM
        // mapping as long as callers keep it alive, which `Machine` does (it holds an Arc).
        check("hv_vm_map", unsafe { ffi::hv_vm_map(ram.host_ptr().cast(), ram.base(), ram.len() as usize, RWX) })?;
        ram.set_hv_mapped(RWX);
        Ok(())
    }

    /// Maps host memory the VMM doesn't own as guest memory (e.g. a GPU renderer's
    /// host-visible allocation) at `ipa`. Callable from any thread while the VM exists.
    ///
    /// The caller must keep `[host, host + len)` mapped until [`Vm::unmap_raw`]; the guest
    /// reads and writes it directly.
    pub fn map_raw(host: HostRange, ipa: u64) -> Result<()> {
        const RW: u64 = 0b011;
        // SAFETY: the range comes from a live host mapping (see the contract above).
        check("hv_vm_map", unsafe { ffi::hv_vm_map(host.ptr.cast(), ipa, host.len, RW) })
    }

    pub fn unmap_raw(ipa: u64, len: usize) -> Result<()> {
        // SAFETY: plain FFI call; unmapping a range only affects the guest view.
        check("hv_vm_unmap", unsafe { ffi::hv_vm_unmap(ipa, len) })
    }

    /// Raises or lowers a shared peripheral interrupt. Callable from any thread.
    pub fn set_spi(intid: u32, level: bool) -> Result<()> {
        let g = gic_created()?;
        // SAFETY: plain FFI call on the process-wide GIC.
        check("hv_gic_set_spi", unsafe { (g.set_spi)(intid, level) })
    }

    /// `GICD_IROUTER<intid>`: which vCPU the guest routes SPI `intid` to. Callable from any
    /// thread.
    pub fn spi_router(intid: u32) -> Result<u64> {
        if !(32..1020).contains(&intid) {
            return Err(Error::Invalid("SPI INTID must be in 32..1020"));
        }
        let g = gic_created()?;
        let mut v = 0;
        // SAFETY: plain FFI call on the process-wide GIC; out-pointer references a live local.
        check("hv_gic_get_distributor_reg", unsafe { (g.get_distributor_reg)(ffi::GICD_IROUTER32 + ((intid - 32) * 8) as u16, &mut v) })?;
        Ok(v)
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        // SAFETY: plain FFI call. Errors (e.g. vCPUs still alive at process exit) are ignored.
        unsafe { ffi::hv_vm_destroy() };
        VM_EXISTS.store(false, Ordering::SeqCst);
    }
}

/// Why `Vcpu::run` returned.
#[derive(Clone, Copy, Debug)]
pub enum Exit {
    /// Another thread called [`VcpuHandle::kick`].
    Canceled,
    /// The guest trapped: `syndrome` is ESR_EL2, `ipa` the faulting guest physical address.
    Exception {
        syndrome: u64,
        virtual_address: u64,
        ipa: u64,
    },
    /// The virtual timer fired; it stays masked until [`Vcpu::set_vtimer_mask`]`(false)`.
    VtimerActivated,
    Unknown(u32),
}

/// A virtual CPU. Hypervisor.framework requires all calls for a vCPU to come from the thread
/// that created it, so this type is neither `Send` nor `Sync`.
pub struct Vcpu {
    id: u64,
    exit: *const ffi::VcpuExit,
    _not_send: PhantomData<*const ()>,
}

impl Vcpu {
    pub fn new() -> Result<Vcpu> {
        let mut id = 0u64;
        let mut exit: *const ffi::VcpuExit = std::ptr::null();
        // SAFETY: out-pointers reference live locals; a null config selects the defaults.
        check("hv_vcpu_create", unsafe { ffi::hv_vcpu_create(&mut id, &mut exit, std::ptr::null_mut()) })?;
        Ok(Vcpu { id, exit, _not_send: PhantomData })
    }

    pub fn handle(&self) -> VcpuHandle {
        VcpuHandle(self.id)
    }

    pub fn reg(&self, reg: Reg) -> Result<u64> {
        let mut v = 0;
        // SAFETY: called on the owning thread; out-pointer references a live local.
        check("hv_vcpu_get_reg", unsafe { ffi::hv_vcpu_get_reg(self.id, reg.0, &mut v) })?;
        Ok(v)
    }

    pub fn set_reg(&self, reg: Reg, value: u64) -> Result<()> {
        // SAFETY: called on the owning thread.
        check("hv_vcpu_set_reg", unsafe { ffi::hv_vcpu_set_reg(self.id, reg.0, value) })
    }

    pub fn sys_reg(&self, reg: SysReg) -> Result<u64> {
        let mut v = 0;
        // SAFETY: called on the owning thread; out-pointer references a live local.
        check("hv_vcpu_get_sys_reg", unsafe { ffi::hv_vcpu_get_sys_reg(self.id, reg.0, &mut v) })?;
        Ok(v)
    }

    pub fn set_sys_reg(&self, reg: SysReg, value: u64) -> Result<()> {
        // SAFETY: called on the owning thread.
        check("hv_vcpu_set_sys_reg", unsafe { ffi::hv_vcpu_set_sys_reg(self.id, reg.0, value) })
    }

    pub fn set_vtimer_mask(&self, masked: bool) -> Result<()> {
        // SAFETY: called on the owning thread.
        check("hv_vcpu_set_vtimer_mask", unsafe { ffi::hv_vcpu_set_vtimer_mask(self.id, masked) })
    }

    /// Guest CNTVCT_EL0 = host counter − `offset`.
    pub fn set_vtimer_offset(&self, offset: u64) -> Result<()> {
        // SAFETY: called on the owning thread.
        check("hv_vcpu_set_vtimer_offset", unsafe { ffi::hv_vcpu_set_vtimer_offset(self.id, offset) })
    }

    /// Makes a private peripheral interrupt pending on this vCPU's redistributor.
    pub fn set_ppi_pending(&self, intid: u32) -> Result<()> {
        if !(16..32).contains(&intid) {
            return Err(Error::Invalid("PPI INTID must be in 16..32"));
        }
        let g = gic_created()?;
        // SAFETY: called on the owning thread; GICR_ISPENDR0 is write-1-to-set.
        check("hv_gic_set_redistributor_reg", unsafe { (g.set_redistributor_reg)(self.id, ffi::GICR_ISPENDR0, 1 << intid) })
    }

    /// Enters the guest until it traps, its timer fires, or it is kicked.
    pub fn run(&self) -> Result<Exit> {
        // SAFETY: called on the owning thread.
        check("hv_vcpu_run", unsafe { ffi::hv_vcpu_run(self.id) })?;
        // SAFETY: the framework keeps `exit` valid for the vCPU's lifetime and only writes it
        // during hv_vcpu_run, which has returned.
        let exit = unsafe { &*self.exit };
        Ok(match exit.reason {
            0 => Exit::Canceled,
            1 => Exit::Exception { syndrome: exit.syndrome, virtual_address: exit.virtual_address, ipa: exit.physical_address },
            2 => Exit::VtimerActivated,
            other => Exit::Unknown(other),
        })
    }
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        // SAFETY: called on the owning thread (the type is !Send).
        unsafe { ffi::hv_vcpu_destroy(self.id) };
    }
}

/// Sendable reference to a vCPU, good for forcing it out of the guest.
#[derive(Clone, Copy, Debug)]
pub struct VcpuHandle(u64);

impl VcpuHandle {
    /// Makes the vCPU's current or next `run` return [`Exit::Canceled`].
    pub fn kick(&self) {
        // SAFETY: hv_vcpus_exit is documented as callable from any thread.
        unsafe { ffi::hv_vcpus_exit(&self.0, 1) };
    }
}

/// [`VcpuHandle::kick`] for several vCPUs in one `hv_vcpus_exit` call (per 64).
pub fn kick_vcpus(handles: impl IntoIterator<Item = VcpuHandle>) {
    let mut ids = [0u64; 64];
    let mut n = 0;
    let flush = |ids: &[u64]| {
        if !ids.is_empty() {
            // SAFETY: `ids` is a live array of `len` vCPU ids; hv_vcpus_exit is documented as
            // callable from any thread.
            unsafe { ffi::hv_vcpus_exit(ids.as_ptr(), ids.len() as u32) };
        }
    };
    for h in handles {
        ids[n] = h.0;
        n += 1;
        if n == ids.len() {
            flush(&ids);
            n = 0;
        }
    }
    flush(&ids[..n]);
}

/// Host counter the guest's virtual timer is derived from (`mach_absolute_time`).
pub fn host_ticks() -> u64 {
    // SAFETY: no arguments, no preconditions.
    unsafe { ffi::mach_absolute_time() }
}

/// Converts host counter ticks to nanoseconds.
pub fn ticks_to_nanos(ticks: u64) -> u64 {
    // The timebase is fixed for the life of the system: ask once.
    static TIMEBASE: OnceLock<(u32, u32)> = OnceLock::new();
    let &(numer, denom) = TIMEBASE.get_or_init(|| {
        let mut info = ffi::MachTimebaseInfo { numer: 0, denom: 0 };
        // SAFETY: out-pointer references a live local.
        unsafe { ffi::mach_timebase_info(&mut info) };
        (info.numer, info.denom)
    });
    if denom == 0 {
        return ticks;
    }
    (u128::from(ticks) * u128::from(numer) / u128::from(denom)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_and_compare_numerically() {
        assert_eq!(parse_version("26.6.2"), Some((26, 6, 2)));
        assert_eq!(parse_version("15"), Some((15, 0, 0)));
        assert_eq!(parse_version("14.2"), Some((14, 2, 0)));
        for bad in ["", "15.", "a", "1.2.3.4", "15.0b1"] {
            assert_eq!(parse_version(bad), None, "{bad:?}");
        }
        assert!(parse_version("14.10").expect("v") > (14, 2, 0));
        assert!(parse_version("14.6.1").expect("v") < (15, 0, 0));
        assert_eq!((show((15, 0, 0)), show((14, 2, 0)), show((26, 0, 1))), ("15".into(), "14.2".into(), "26.0.1".into()));
    }

    #[test]
    fn unsupported_says_what_and_which_release() {
        let e = Error::Unsupported { what: "the in-kernel GICv3 (hv_gic_create)", needs: "15", host: "14.6.1".into() };
        assert_eq!(e.to_string(), "the in-kernel GICv3 (hv_gic_create) needs macOS 15 or later; this Mac runs macOS 14.6.1");
    }

    /// What this host's framework has matches its release: the GIC from 15, the granule from 26.
    #[test]
    fn late_symbols_follow_the_release() {
        let v = macos_version().expect("kern.osproductversion");
        let late = ffi::late();
        assert!(late.vm_config.is_some(), "macOS 13+ (every host simee builds on)");
        assert_eq!(late.gic.is_some(), v >= (15, 0, 0), "{v:?}");
        assert_eq!(late.set_ipa_granule.is_some(), v >= (26, 0, 0), "{v:?}");
        assert_eq!(granule_4k_supported(), v >= (26, 0, 0));
        assert_eq!(gic().is_ok(), v >= (15, 0, 0));
        println!("macOS {}: gic {}, 4 KiB granule {}", show(v), late.gic.is_some(), granule_4k_supported());
    }
}
