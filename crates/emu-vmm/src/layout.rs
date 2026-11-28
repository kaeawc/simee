//! Guest physical memory map.
//!
//! Matches the stock Android emulator's arm64 board ("ranchu", a QEMU `virt` derivative) so the
//! unmodified system images find their devices where they expect them: the fstab names
//! `a003600.virtio_mmio` (boot device) and `a003c00.virtio_mmio` (metadata) by address. The one
//! deliberate difference is the interrupt controller: a GICv3 handled inside Hypervisor.framework
//! instead of a userspace GICv2, which removes an exit per interrupt acknowledge and EOI.

pub const GIC_DIST_BASE: u64 = 0x0800_0000;
pub const GIC_REDIST_BASE: u64 = 0x080a_0000;
/// Redistributors must end before the first peripheral.
pub const GIC_REDIST_LIMIT: u64 = 0x0900_0000;

pub const UART_BASE: u64 = 0x0900_0000;
pub const UART_SPI: u32 = 1;
pub const RTC_BASE: u64 = 0x0901_0000;
pub const RTC_SPI: u32 = 2;
pub const PERIPH_SIZE: u64 = 0x1000;

pub const VIRTIO_MMIO_BASE: u64 = 0x0a00_0000;
pub const VIRTIO_MMIO_STRIDE: u64 = 0x200;
pub const VIRTIO_MMIO_SLOTS: u32 = 32;
/// SPI of slot 0; slot N uses `VIRTIO_MMIO_SPI_BASE + N`.
pub const VIRTIO_MMIO_SPI_BASE: u32 = 32;

pub const RAM_BASE: u64 = 0x4000_0000;
/// Device tree blob, at the start of RAM.
pub const DTB_ADDR: u64 = RAM_BASE;
pub const DTB_MAX: u64 = 0x20_0000;
/// Kernel image (2 MiB aligned, after the DTB).
pub const KERNEL_ADDR: u64 = RAM_BASE + 0x20_0000;
/// Initrd, far enough above the kernel for its image, BSS and early page tables.
pub const INITRD_ADDR: u64 = RAM_BASE + 0x0800_0000;

/// Host-visible GPU memory (virtio-gpu shared memory region 1): blobs the guest maps get
/// mapped here, above RAM, inside a 36-bit guest physical address space.
pub const GPU_SHM_BASE: u64 = 0xc_0000_0000;
pub const GPU_SHM_SIZE: u64 = 8 << 30;

/// Goldfish pipe (stock address and interrupt).
pub const GOLDFISH_BATTERY_BASE: u64 = 0x0a04_0000;
pub const GOLDFISH_BATTERY_SPI: u32 = 17;
pub const GOLDFISH_PIPE_BASE: u64 = 0x0a06_0000;
pub const GOLDFISH_PIPE_SIZE: u64 = 0x2000;
pub const GOLDFISH_PIPE_SPI: u32 = 20;

/// PCIe (ECAM) host, like stock: config space, a 32-bit MMIO window and a 64-bit window.
pub const PCI_ECAM_BASE: u64 = 0x3f00_0000;
pub const PCI_ECAM_SIZE: u64 = 0x0100_0000;
pub const PCI_MMIO32_BASE: u64 = 0x1000_0000;
pub const PCI_MMIO32_SIZE: u64 = 0x2eff_0000;
pub const PCI_MMIO64_BASE: u64 = 0x8_0000_0000;
pub const PCI_MMIO64_SIZE: u64 = 0x4_0000_0000;
/// Goldfish address space: control registers (BAR0) and host-memory area (BAR1, 16 GiB).
pub const GOLDFISH_AS_REGS: u64 = PCI_MMIO32_BASE;
pub const GOLDFISH_AS_AREA: u64 = PCI_MMIO64_BASE;
pub const GOLDFISH_AS_AREA_SIZE: u64 = 16 << 30;

/// Converts an SPI number (as written in the device tree) to a GIC interrupt ID.
pub const fn spi_intid(spi: u32) -> u32 {
    32 + spi
}

pub fn virtio_slot_addr(slot: u32) -> u64 {
    VIRTIO_MMIO_BASE + u64::from(slot) * VIRTIO_MMIO_STRIDE
}

/// Stock slot assignment (from a running stock emulator's `/proc/iomem` and `/sys/block`).
pub mod slots {
    /// virtio-gpu (stock uses goldfish pipe + address space instead).
    pub const GPU: u32 = 23;
    /// virtio-input: touchscreen, keyboard, rotary (stock puts 11 input devices on PCI).
    pub const TOUCH: u32 = 20;
    pub const KEYBOARD: u32 = 21;
    pub const ROTARY: u32 = 22;
    /// virtio-input dual-mode mouse, Android XR images only (stock: on PCI like its other inputs).
    pub const MOUSE: u32 = 17;
    /// virtio-input touchpad (`hw.touchpad0`: the display glasses' temple), a multi-touch device
    /// named `virtio_input_multi_touch_touchpad_0` (stock: on PCI like its other inputs).
    pub const TOUCHPAD: u32 = 16;
    /// Not on the stock board (which puts vsock on PCI); the guest driver doesn't care.
    pub const VSOCK: u32 = 24;
    pub const RNG: u32 = 25;
    pub const NET: u32 = 26;
    /// The second virtio-net, `net-segment`'s NIC on a link subnet (#539). Not on the stock board.
    /// Below `NET`, so the board describes it after the others (`BoardInfo::late_slot`): the guest
    /// then names the NAT's NIC `eth0` (the radio HAL configures that name) and this one `eth1`.
    pub const SEGMENT: u32 = 15;
    /// virtio-console with named ports (`modem`); stock puts its consoles on PCI.
    pub const VPORTS: u32 = 19;
    /// virtio-snd (images that enable `VirtioSndCard`, and Automotive); stock puts it on PCI, and the guest driver
    /// doesn't care which transport.
    pub const SOUND: u32 = 18;
    /// vda: system.img (GPT: vbmeta, super). `androidboot.boot_devices` points here.
    pub const SYSTEM: u32 = 27;
    /// vdb: cache.img.
    pub const CACHE: u32 = 28;
    /// vdc: userdata (the fstab mounts /dev/block/vdc on /data).
    pub const USERDATA: u32 = 29;
    /// vdd: encryptionkey.img (GPT: metadata), mounted by path on /metadata.
    pub const METADATA: u32 = 30;
    /// vde: vendor.img (GPT: vendor).
    pub const VENDOR: u32 = 31;
}

/// Stock's x86 board (`pc`), which software mode gives x86_64 images under QEMU (#55): from a
/// running stock emulator's `/proc/bootconfig` and the images' fstab.
pub mod x86 {
    /// `androidboot.boot_devices`: system (00:03.0) and metadata (00:06.0).
    pub const BOOT_DEVICES: &str = "pci0000:00/0000:00:03.0 pci0000:00/0000:00:06.0";
    /// virtio-rng's PCI slot, after the disks.
    pub const RNG_SLOT: u8 = 8;
    /// The goldfish pipe's PCI function (emu's carrier for it), out of the way of stock's slots.
    pub const PIPE_SLOT: u8 = 0x1e;

    /// The PCI slot of each of the five disks, by serial.
    pub fn disk_slot(serial: &str) -> Option<u8> {
        match serial {
            "system" => Some(3),
            "cache" => Some(4),
            "userdata" => Some(5),
            "metadata" => Some(6),
            "vendor" => Some(7),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_slot_is_taken_once() {
        use slots::*;
        let mut all = [
            GPU, TOUCH, KEYBOARD, ROTARY, MOUSE, TOUCHPAD, VSOCK, RNG, NET, SEGMENT, VPORTS, SOUND, SYSTEM, CACHE, USERDATA, METADATA,
            VENDOR,
        ];
        all.sort_unstable();
        assert!(all.windows(2).all(|w| w[0] != w[1]) && all.iter().all(|s| *s < 32), "{all:?}");
    }

    #[test]
    fn stock_addresses() {
        assert_eq!(virtio_slot_addr(slots::SYSTEM), 0x0a00_3600);
        assert_eq!(virtio_slot_addr(slots::METADATA), 0x0a00_3c00);
        assert_eq!(spi_intid(VIRTIO_MMIO_SPI_BASE + slots::SYSTEM), 91);
    }

    // The initrd area leaves 64 MiB for the decompressed kernel, its BSS and early page tables.
    const _: () = assert!(INITRD_ADDR > KERNEL_ADDR + 0x0400_0000);
}
