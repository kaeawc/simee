//! The device tree handed to the guest kernel.
use emu_fdt::Fdt;

use crate::layout::*;

pub struct BoardInfo<'a> {
    pub cpus: u32,
    pub ram_bytes: u64,
    pub gic_dist: (u64, u64),
    pub gic_redist: (u64, u64),
    pub cmdline: &'a str,
    pub initrd: Option<(u64, u64)>,
    /// Slots that hold a device; the rest are still described (and answer as empty), like stock.
    pub virtio_slots: u32,
    /// A slot described after every other, so the guest probes it last: the second NIC (`eth1`)
    /// must not be probed before the first, whose slot is higher. Still described in place of
    /// its own entry, never twice.
    pub late_slot: Option<u32>,
    /// Describe the PCIe host (goldfish address space lives there).
    pub pci: bool,
    /// Describe the goldfish pipe device.
    pub goldfish_pipe: bool,
    pub rng_seed: [u8; 64],
    pub kaslr_seed: u64,
}

const IRQ_TYPE_LEVEL_HIGH: u32 = 4;
const GIC_SPI: u32 = 0;
const GIC_PPI: u32 = 1;

pub fn build(b: &BoardInfo) -> Result<Vec<u8>, emu_fdt::FdtError> {
    let mut f = Fdt::new();
    let gic = f.alloc_phandle();
    let clk = f.alloc_phandle();

    f.begin_node("");
    f.prop_u32("interrupt-parent", gic);
    f.prop_u32("#size-cells", 2);
    f.prop_u32("#address-cells", 2);
    // What the stock board reports; some HALs read it from /proc/device-tree.
    f.prop_str("compatible", "linux,ranchu");
    f.prop_str("model", "emu-rs");

    f.begin_node("psci");
    f.prop_strs("compatible", &["arm,psci-0.2", "arm,psci"]);
    f.prop_str("method", "hvc");
    f.prop_u32("cpu_suspend", 0xc400_0001);
    f.prop_u32("cpu_off", 0x8400_0002);
    f.prop_u32("cpu_on", 0xc400_0003);
    f.prop_u32("migrate", 0xc400_0005);
    f.end_node();

    f.begin_node("cpus");
    f.prop_u32("#address-cells", 1);
    f.prop_u32("#size-cells", 0);
    for i in 0..b.cpus {
        f.begin_node(&format!("cpu@{i}"));
        f.prop_str("device_type", "cpu");
        f.prop_str("compatible", "arm,arm-v8");
        f.prop_str("enable-method", "psci");
        f.prop_u32("reg", i);
        f.end_node();
    }
    f.end_node();

    f.begin_node(&format!("memory@{RAM_BASE:x}"));
    f.prop_str("device_type", "memory");
    f.prop_reg("reg", &[(RAM_BASE, b.ram_bytes)]);
    f.end_node();

    f.begin_node(&format!("intc@{:x}", b.gic_dist.0));
    f.prop_str("compatible", "arm,gic-v3");
    f.prop_u32("#interrupt-cells", 3);
    f.prop_empty("interrupt-controller");
    f.prop_u32("#address-cells", 2);
    f.prop_u32("#size-cells", 2);
    f.prop_empty("ranges");
    f.prop_reg("reg", &[b.gic_dist, b.gic_redist]);
    f.prop_u32("phandle", gic);
    f.end_node();

    f.begin_node("timer");
    f.prop_strs("compatible", &["arm,armv8-timer", "arm,armv7-timer"]);
    // Secure phys, non-secure phys, virtual, hypervisor.
    let mut cells = Vec::new();
    for ppi in [13, 14, 11, 10] {
        cells.extend_from_slice(&[GIC_PPI, ppi, IRQ_TYPE_LEVEL_HIGH]);
    }
    f.prop_cells("interrupts", &cells);
    f.prop_empty("always-on");
    f.end_node();

    f.begin_node("apb-pclk");
    f.prop_str("compatible", "fixed-clock");
    f.prop_u32("#clock-cells", 0);
    f.prop_u32("clock-frequency", 24_000_000);
    f.prop_str("clock-output-names", "clk24mhz");
    f.prop_u32("phandle", clk);
    f.end_node();

    f.begin_node(&format!("pl011@{UART_BASE:x}"));
    f.prop_strs("compatible", &["arm,pl011", "arm,primecell"]);
    f.prop_reg("reg", &[(UART_BASE, PERIPH_SIZE)]);
    f.prop_cells("interrupts", &[GIC_SPI, UART_SPI, IRQ_TYPE_LEVEL_HIGH]);
    f.prop_cells("clocks", &[clk, clk]);
    f.prop_strs("clock-names", &["uartclk", "apb_pclk"]);
    f.end_node();

    f.begin_node(&format!("pl031@{RTC_BASE:x}"));
    f.prop_strs("compatible", &["arm,pl031", "arm,primecell"]);
    f.prop_reg("reg", &[(RTC_BASE, PERIPH_SIZE)]);
    f.prop_cells("interrupts", &[GIC_SPI, RTC_SPI, IRQ_TYPE_LEVEL_HIGH]);
    f.prop_u32("clocks", clk);
    f.prop_str("clock-names", "apb_pclk");
    f.end_node();

    let in_order = (0..b.virtio_slots).filter(|s| Some(*s) != b.late_slot);
    for slot in in_order.chain(b.late_slot.filter(|s| *s < b.virtio_slots)) {
        let addr = virtio_slot_addr(slot);
        f.begin_node(&format!("virtio_mmio@{addr:x}"));
        f.prop_str("compatible", "virtio,mmio");
        f.prop_reg("reg", &[(addr, VIRTIO_MMIO_STRIDE)]);
        f.prop_cells("interrupts", &[GIC_SPI, VIRTIO_MMIO_SPI_BASE + slot, IRQ_TYPE_LEVEL_HIGH]);
        f.prop_empty("dma-coherent");
        f.end_node();
    }

    f.begin_node(&format!("goldfish_battery@{GOLDFISH_BATTERY_BASE:x}"));
    f.prop_str("compatible", "google,goldfish-battery");
    f.prop_reg("reg", &[(GOLDFISH_BATTERY_BASE, PERIPH_SIZE)]);
    f.prop_cells("interrupts", &[GIC_SPI, GOLDFISH_BATTERY_SPI, IRQ_TYPE_LEVEL_HIGH]);
    f.end_node();

    if b.goldfish_pipe {
        f.begin_node(&format!("goldfish_pipe@{GOLDFISH_PIPE_BASE:x}"));
        f.prop_strs("compatible", &["google,android-pipe", "generic,android-pipe"]);
        f.prop_reg("reg", &[(GOLDFISH_PIPE_BASE, GOLDFISH_PIPE_SIZE)]);
        f.prop_cells("interrupts", &[GIC_SPI, GOLDFISH_PIPE_SPI, IRQ_TYPE_LEVEL_HIGH]);
        f.end_node();
    }

    if b.pci {
        f.begin_node(&format!("pcie@{PCI_MMIO32_BASE:x}"));
        f.prop_str("compatible", "pci-host-ecam-generic");
        f.prop_str("device_type", "pci");
        f.prop_u32("#address-cells", 3);
        f.prop_u32("#size-cells", 2);
        f.prop_reg("reg", &[(PCI_ECAM_BASE, PCI_ECAM_SIZE)]);
        f.prop_cells("bus-range", &[0, 0]);
        f.prop_u32("linux,pci-domain", 0);
        let hi = |v: u64| (v >> 32) as u32;
        let lo = |v: u64| v as u32;
        f.prop_cells(
            "ranges",
            &[
                0x0200_0000,
                hi(PCI_MMIO32_BASE),
                lo(PCI_MMIO32_BASE),
                hi(PCI_MMIO32_BASE),
                lo(PCI_MMIO32_BASE),
                hi(PCI_MMIO32_SIZE),
                lo(PCI_MMIO32_SIZE),
                0x4300_0000,
                hi(PCI_MMIO64_BASE),
                lo(PCI_MMIO64_BASE),
                hi(PCI_MMIO64_BASE),
                lo(PCI_MMIO64_BASE),
                hi(PCI_MMIO64_SIZE),
                lo(PCI_MMIO64_SIZE),
            ],
        );
        f.prop_empty("dma-coherent");
        f.end_node();
    }

    f.begin_node("chosen");
    if b.pci {
        // Keep our BAR assignments (BAR1 is backed by host memory mapped at a fixed address).
        f.prop_u32("linux,pci-probe-only", 1);
    }
    f.prop_str("bootargs", b.cmdline);
    f.prop_str("stdout-path", &format!("/pl011@{UART_BASE:x}"));
    if let Some((start, end)) = b.initrd {
        f.prop_u64("linux,initrd-start", start);
        f.prop_u64("linux,initrd-end", end);
    }
    // Seeds the guest CRNG at boot, so nothing in early userspace blocks waiting for entropy.
    f.prop("rng-seed", &b.rng_seed);
    f.prop_u64("kaslr-seed", b.kaslr_seed);
    f.end_node();

    f.begin_node("firmware");
    f.begin_node("android");
    f.prop_str("compatible", "android,firmware");
    f.prop_str("hardware", "ranchu");
    f.end_node();
    f.end_node();

    f.end_node();
    f.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_and_fits() {
        let dtb = build(&BoardInfo {
            cpus: 4,
            ram_bytes: 2 << 30,
            gic_dist: (GIC_DIST_BASE, 0x10000),
            gic_redist: (GIC_REDIST_BASE, 0x20000 * 4),
            cmdline: "console=ttyAMA0",
            initrd: Some((INITRD_ADDR, INITRD_ADDR + 100)),
            virtio_slots: VIRTIO_MMIO_SLOTS,
            late_slot: None,
            pci: true,
            goldfish_pipe: true,
            rng_seed: [0; 64],
            kaslr_seed: 1,
        })
        .expect("dtb");
        assert!((dtb.len() as u64) < DTB_MAX);
        let text = String::from_utf8_lossy(&dtb);
        assert!(text.contains("virtio_mmio@a003600"));
        assert!(text.contains("arm,gic-v3"));
        assert!(text.contains("pci-host-ecam-generic"));
        assert!(text.contains("google,goldfish-battery"));
    }

    /// Where a slot's node sits in the tree, which is the order the guest probes the devices in.
    fn position(dtb: &[u8], slot: u32) -> usize {
        let text = String::from_utf8_lossy(dtb);
        text.find(&format!("virtio_mmio@{:x}", virtio_slot_addr(slot))).expect("slot described")
    }

    fn board(late_slot: Option<u32>) -> Vec<u8> {
        build(&BoardInfo {
            cpus: 1,
            ram_bytes: 1 << 30,
            gic_dist: (GIC_DIST_BASE, 0x10000),
            gic_redist: (GIC_REDIST_BASE, 0x20000),
            cmdline: "console=ttyAMA0",
            initrd: None,
            virtio_slots: VIRTIO_MMIO_SLOTS,
            late_slot,
            pci: false,
            goldfish_pipe: false,
            rng_seed: [0; 64],
            kaslr_seed: 1,
        })
        .expect("dtb")
    }

    #[test]
    fn the_segment_nic_is_described_after_the_nat_nic_so_it_probes_second() {
        let plain = board(None);
        assert!(position(&plain, slots::SEGMENT) < position(&plain, slots::NET), "address order, as stock");
        let late = board(Some(slots::SEGMENT));
        assert!(position(&late, slots::SEGMENT) > position(&late, slots::NET));
        assert!(position(&late, slots::SEGMENT) > position(&late, 31), "after every slot");
        // Neighbouring behaviour: every slot is still described once, and nothing else moved.
        let text = String::from_utf8_lossy(&late).into_owned();
        assert_eq!(text.matches("virtio_mmio@").count(), VIRTIO_MMIO_SLOTS as usize);
        assert!(position(&late, 0) < position(&late, slots::NET) && position(&late, slots::NET) < position(&late, 31));
        assert_eq!(late.len(), plain.len(), "same nodes, different order");
    }
}
