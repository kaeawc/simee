//! Raw Hypervisor.framework declarations (arm64). Signatures follow the macOS SDK headers.
//!
//! The functions every Apple silicon macOS has (11.0, per the headers' `API_AVAILABLE`) are
//! linked. The newer ones are looked up at run time ([`late`]), because a linked symbol the
//! framework lacks stops dyld from loading the binary at all: emu would die before `main` on an
//! older macOS instead of saying what it needs. Those are the VM configuration (macOS 13.0), the
//! in-kernel GIC (15.0) and the IPA granule (26.0).
use std::ffi::c_void;
use std::sync::OnceLock;

/// `hv_vcpu_exit_t`.
#[repr(C)]
pub struct VcpuExit {
    pub reason: u32,
    pub syndrome: u64,
    pub virtual_address: u64,
    pub physical_address: u64,
}

#[repr(C)]
pub struct MachTimebaseInfo {
    pub numer: u32,
    pub denom: u32,
}

/// `HV_GIC_REDISTRIBUTOR_REG_GICR_ISPENDR0`.
pub const GICR_ISPENDR0: u32 = 0x10200;
/// `HV_GIC_DISTRIBUTOR_REG_GICD_IROUTER32`; IROUTER<n> follows at 8-byte steps (register offsets).
pub const GICD_IROUTER32: u16 = 0x6100;
/// `HV_IPA_GRANULE_4KB`.
pub const IPA_GRANULE_4KB: u32 = 0;

#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    pub fn hv_vm_create(config: *mut c_void) -> i32;
    pub fn hv_vm_unmap(ipa: u64, size: usize) -> i32;
    pub fn hv_vm_destroy() -> i32;
    pub fn hv_vm_map(addr: *mut c_void, ipa: u64, size: usize, flags: u64) -> i32;

    pub fn hv_vcpu_create(vcpu: *mut u64, exit: *mut *const VcpuExit, config: *mut c_void) -> i32;
    pub fn hv_vcpu_destroy(vcpu: u64) -> i32;
    pub fn hv_vcpu_run(vcpu: u64) -> i32;
    pub fn hv_vcpus_exit(vcpus: *const u64, count: u32) -> i32;
    pub fn hv_vcpu_get_reg(vcpu: u64, reg: u32, value: *mut u64) -> i32;
    pub fn hv_vcpu_set_reg(vcpu: u64, reg: u32, value: u64) -> i32;
    pub fn hv_vcpu_get_sys_reg(vcpu: u64, reg: u16, value: *mut u64) -> i32;
    pub fn hv_vcpu_set_sys_reg(vcpu: u64, reg: u16, value: u64) -> i32;
    pub fn hv_vcpu_set_vtimer_mask(vcpu: u64, masked: bool) -> i32;
    pub fn hv_vcpu_set_vtimer_offset(vcpu: u64, offset: u64) -> i32;
}

unsafe extern "C" {
    pub fn mach_absolute_time() -> u64;
    pub fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

/// Looks up `$name` in the loaded images; `None` when this macOS's framework doesn't export it.
macro_rules! sym {
    ($name:ident: fn($($arg:ty),*) -> $ret:ty) => {{
        // SAFETY: a NUL-terminated literal name; RTLD_DEFAULT searches every loaded image, which
        // includes Hypervisor.framework (linked above). A non-null result is that exported C
        // function, whose signature the SDK header fixes as the transmute target.
        unsafe {
            let p = libc::dlsym(libc::RTLD_DEFAULT, concat!(stringify!($name), "\0").as_ptr().cast());
            (!p.is_null()).then(|| std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) -> $ret>(p))
        }
    }};
}

/// `hv_vm_config_*` (macOS 13.0).
pub struct VmConfigFns {
    pub create: unsafe extern "C" fn() -> *mut c_void,
    pub get_max_ipa_size: unsafe extern "C" fn(*mut u32) -> i32,
    pub set_ipa_size: unsafe extern "C" fn(*mut c_void, u32) -> i32,
}

/// `hv_gic_*` (macOS 15.0).
pub struct GicFns {
    pub config_create: unsafe extern "C" fn() -> *mut c_void,
    pub config_set_distributor_base: unsafe extern "C" fn(*mut c_void, u64) -> i32,
    pub config_set_redistributor_base: unsafe extern "C" fn(*mut c_void, u64) -> i32,
    pub create: unsafe extern "C" fn(*mut c_void) -> i32,
    pub get_distributor_size: unsafe extern "C" fn(*mut usize) -> i32,
    pub get_redistributor_region_size: unsafe extern "C" fn(*mut usize) -> i32,
    pub get_spi_interrupt_range: unsafe extern "C" fn(*mut u32, *mut u32) -> i32,
    pub set_spi: unsafe extern "C" fn(u32, bool) -> i32,
    pub get_distributor_reg: unsafe extern "C" fn(u16, *mut u64) -> i32,
    pub set_redistributor_reg: unsafe extern "C" fn(u64, u32, u64) -> i32,
}

/// The functions newer than macOS 11, each group present only when all of it is.
pub struct Late {
    pub vm_config: Option<VmConfigFns>,
    /// `hv_vm_config_set_ipa_granule` (macOS 26.0).
    pub set_ipa_granule: Option<unsafe extern "C" fn(*mut c_void, u32) -> i32>,
    pub gic: Option<GicFns>,
}

/// Looked up once per process.
pub fn late() -> &'static Late {
    static LATE: OnceLock<Late> = OnceLock::new();
    LATE.get_or_init(|| Late {
        vm_config: (|| {
            Some(VmConfigFns {
                create: sym!(hv_vm_config_create: fn() -> *mut c_void)?,
                get_max_ipa_size: sym!(hv_vm_config_get_max_ipa_size: fn(*mut u32) -> i32)?,
                set_ipa_size: sym!(hv_vm_config_set_ipa_size: fn(*mut c_void, u32) -> i32)?,
            })
        })(),
        set_ipa_granule: sym!(hv_vm_config_set_ipa_granule: fn(*mut c_void, u32) -> i32),
        gic: (|| {
            Some(GicFns {
                config_create: sym!(hv_gic_config_create: fn() -> *mut c_void)?,
                config_set_distributor_base: sym!(hv_gic_config_set_distributor_base: fn(*mut c_void, u64) -> i32)?,
                config_set_redistributor_base: sym!(hv_gic_config_set_redistributor_base: fn(*mut c_void, u64) -> i32)?,
                create: sym!(hv_gic_create: fn(*mut c_void) -> i32)?,
                get_distributor_size: sym!(hv_gic_get_distributor_size: fn(*mut usize) -> i32)?,
                get_redistributor_region_size: sym!(hv_gic_get_redistributor_region_size: fn(*mut usize) -> i32)?,
                get_spi_interrupt_range: sym!(hv_gic_get_spi_interrupt_range: fn(*mut u32, *mut u32) -> i32)?,
                set_spi: sym!(hv_gic_set_spi: fn(u32, bool) -> i32)?,
                get_distributor_reg: sym!(hv_gic_get_distributor_reg: fn(u16, *mut u64) -> i32)?,
                set_redistributor_reg: sym!(hv_gic_set_redistributor_reg: fn(u64, u32, u64) -> i32)?,
            })
        })(),
    })
}
