//! Parsers for what the kernel reports, on fixtures: `/proc/loadavg`, `/proc/meminfo`,
//! `/proc/pressure/*` and the raw structs behind macOS's `vm.loadavg` and `vm.swapusage` sysctls.
//! Pure functions of bytes, so every platform's format is tested on every host.
//!
//! The meminfo and PSI parsing follows spectra's `internal/memstate/linux.go` (`ParseMeminfo`) and
//! `internal/pressure` (MIT, same author).

use crate::{Load, PsiLine, PsiResource};

/// `/proc/loadavg`: `0.52 0.58 0.59 1/467 12345`.
pub fn loadavg(text: &str) -> Option<Load> {
    let mut f = text.split_whitespace();
    let mut next = || f.next()?.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0);
    Some(Load { one: next()?, five: next()?, fifteen: next()? })
}

/// macOS `vm.loadavg`: `struct loadavg { fixpt_t ldavg[3]; long fscale; }`, so three `u32`s, four
/// bytes of padding and an `i64` scale (24 bytes, native endian). Each load is `ldavg / fscale`.
pub fn loadavg_struct(b: &[u8]) -> Option<Load> {
    let fscale = i64::from_ne_bytes(b.get(16..24)?.try_into().ok()?);
    if fscale <= 0 {
        return None;
    }
    let at = |i: usize| Some(f64::from(u32::from_ne_bytes(b.get(i..i + 4)?.try_into().ok()?)) / fscale as f64);
    Some(Load { one: at(0)?, five: at(4)?, fifteen: at(8)? })
}

/// macOS `vm.swapusage`: `struct xsw_usage { u64 total, avail, used; u32 pagesize; bool encrypted }`.
/// Returns (total, used) in bytes.
pub fn swapusage_struct(b: &[u8]) -> Option<(u64, u64)> {
    let total = u64::from_ne_bytes(b.get(0..8)?.try_into().ok()?);
    let used = u64::from_ne_bytes(b.get(16..24)?.try_into().ok()?);
    Some((total, used))
}

/// The `/proc/meminfo` fields the sampler uses, in bytes.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Meminfo {
    pub total: u64,
    pub free: u64,
    pub available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
    /// Compressed swap cache held in RAM (`Zswap`).
    pub zswap: u64,
}

/// `/proc/meminfo` (values in KiB). `None` without a `MemTotal`. A kernel without `MemAvailable`
/// (before 3.14) gets free plus the reclaimable page cache.
pub fn meminfo(text: &str) -> Option<Meminfo> {
    let kib = |key: &str| -> Option<u64> {
        let line = text.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))?;
        line.split_whitespace().next()?.parse::<u64>().ok().map(|v| v.saturating_mul(1024))
    };
    let total = kib("MemTotal").filter(|t| *t > 0)?;
    let free = kib("MemFree").unwrap_or(0);
    let available = kib("MemAvailable").unwrap_or_else(|| free + kib("Cached").unwrap_or(0) + kib("Buffers").unwrap_or(0));
    Some(Meminfo {
        total,
        free,
        available,
        swap_total: kib("SwapTotal").unwrap_or(0),
        swap_free: kib("SwapFree").unwrap_or(0),
        zswap: kib("Zswap").unwrap_or(0),
    })
}

fn psi_line(rest: &str) -> Option<PsiLine> {
    let (mut avg10, mut avg60, mut avg300, mut total_us) = (None, None, None, 0);
    for kv in rest.split_whitespace() {
        let (k, v) = kv.split_once('=')?;
        match k {
            "avg10" => avg10 = v.parse().ok(),
            "avg60" => avg60 = v.parse().ok(),
            "avg300" => avg300 = v.parse().ok(),
            "total" => total_us = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    Some(PsiLine { avg10: avg10?, avg60: avg60?, avg300: avg300?, total_us })
}

/// One `/proc/pressure/<resource>` file: a `some` line, and a `full` line when the kernel has one
/// (`cpu` has none before Linux 5.13 and no meaningful one since). `None` without a `some` line.
pub fn psi_resource(text: &str) -> Option<PsiResource> {
    let (mut some, mut full) = (None, None);
    for line in text.lines() {
        match line.split_once(' ') {
            Some(("some", rest)) => some = psi_line(rest),
            Some(("full", rest)) => full = psi_line(rest),
            _ => {}
        }
    }
    Some(PsiResource { some: some?, full })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loadavg_text() {
        let l = loadavg("0.52 0.58 1.59 1/467 12345\n").expect("parses");
        assert_eq!((l.one, l.five, l.fifteen), (0.52, 0.58, 1.59));
        assert!(loadavg("0.52 0.58").is_none() && loadavg("").is_none() && loadavg("a b c").is_none());
        assert!(loadavg("-1 0 0").is_none() && loadavg("NaN 0 0").is_none());
    }

    #[test]
    fn loadavg_struct_scales_by_fscale() {
        let mut b = Vec::new();
        for v in [2048u32, 4096, 1024] {
            b.extend_from_slice(&v.to_ne_bytes());
        }
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&2048i64.to_ne_bytes());
        let l = loadavg_struct(&b).expect("parses");
        assert_eq!((l.one, l.five, l.fifteen), (1.0, 2.0, 0.5));
        assert!(loadavg_struct(&b[..20]).is_none(), "truncated");
        b[16..24].copy_from_slice(&0i64.to_ne_bytes());
        assert!(loadavg_struct(&b).is_none(), "zero scale");
    }

    #[test]
    fn swapusage_struct_reads_total_and_used() {
        let mut b = Vec::new();
        for v in [4096u64, 1024, 3072] {
            b.extend_from_slice(&v.to_ne_bytes());
        }
        b.extend_from_slice(&[0; 8]);
        assert_eq!(swapusage_struct(&b), Some((4096, 3072)));
        assert_eq!(swapusage_struct(&b[..16]), None);
    }

    const MEMINFO: &str = "MemTotal:       16384000 kB\nMemFree:         1000000 kB\nMemAvailable:    8192000 kB\n\
        Buffers:          100 kB\nCached:          2000 kB\nSwapTotal:       2048000 kB\nSwapFree:        1024000 kB\nZswap:               512 kB\n";

    #[test]
    fn meminfo_in_bytes() {
        let m = meminfo(MEMINFO).expect("parses");
        assert_eq!((m.total, m.free, m.available), (16_384_000 * 1024, 1_000_000 * 1024, 8_192_000 * 1024));
        assert_eq!((m.swap_total, m.swap_free, m.zswap), (2_048_000 * 1024, 1_024_000 * 1024, 512 * 1024));
    }

    #[test]
    fn meminfo_without_memavailable_adds_the_cache() {
        let m = meminfo("MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 20 kB\nCached: 30 kB\n").expect("parses");
        assert_eq!(m.available, 150 * 1024);
        assert_eq!((m.swap_total, m.zswap), (0, 0));
    }

    #[test]
    fn meminfo_needs_a_total() {
        assert!(meminfo("MemFree: 100 kB\n").is_none());
        assert!(meminfo("MemTotal: 0 kB\n").is_none());
        assert!(meminfo("").is_none());
    }

    #[test]
    fn psi_with_and_without_a_full_line() {
        let both = "some avg10=1.50 avg60=2.00 avg300=3.25 total=12345\nfull avg10=0.10 avg60=0.20 avg300=0.30 total=99\n";
        let r = psi_resource(both).expect("parses");
        assert_eq!((r.some.avg10, r.some.avg60, r.some.avg300, r.some.total_us), (1.5, 2.0, 3.25, 12345));
        assert_eq!(r.full.map(|f| (f.avg10, f.total_us)), Some((0.1, 99)));
        let cpu = psi_resource("some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n").expect("parses");
        assert!(cpu.full.is_none());
        assert!(psi_resource("full avg10=0.10 avg60=0.20 avg300=0.30 total=99\n").is_none(), "no some line");
        assert!(psi_resource("some avg10=x avg60=0 avg300=0 total=0\n").is_none());
        assert!(psi_resource("").is_none());
    }
}
