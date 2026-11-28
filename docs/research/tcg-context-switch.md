# What QEMU TCG's flush on every guest context switch costs software mode

Research for [#227](https://github.com/kaeawc/simee/issues/227) (refs [#55](https://github.com/kaeawc/simee/issues/55)),
the rank 2 item of [software-mode-profile.md](software-mode-profile.md): in software mode every guest CR3 write
drops QEMU's soft TLB and jump cache, because TCG has no PCID. All numbers are from one host on 2026-11-27.

## Summary

- **The bound is real per switch.** A pipe ping-pong between two guest processes runs at 3.3k round trips a second;
  between two threads of one process (a context switch with no CR3 write) at 9.6k. A CR3 write costs about 100 µs
  of vCPU time with a 4 KiB working set and about 150 µs with 512 KiB (section 2).
- **It is a small share of a whole guest.** The guest writes CR3 about 125 times a second idle, about 1,000 times
  a second swiping the launcher and about 150,000 times in a warm boot. At 100 to 150 µs each that is 3 to 4% of
  the boot's vCPU time and 1 to 2% of an idle core. The profile's estimate of a 1.3× faster guest (boot 170 s to
  130 s) is not supported (section 4).
- **A prototype that keeps up to 8 PCIDs' soft TLBs and jump caches removes the ping-pong cost** (process
  ping-pong 3.3k to 8.5k round trips a second, level with threads) **and does nothing measurable for a boot or the
  launcher**: boot 165.2 s (11 boots) against 168.0 s (5 boots), vCPU seconds to boot 370 against 374. Idle host
  CPU falls 10% (0.60 to 0.53 cores). A first version that kept every bank's write-protection current made the
  boot 10 to 16% slower (section 6).
- **Upstream has nothing.** QEMU 11.1.3 and master (2026-11-26) list PCID and INVPCID as unsupported by TCG
  (section 5).
- **Recommendation: stop.** Do not ship a patched QEMU through `EMU_QEMU` for this. Keep the patch as a record;
  an upstream RFC is optional and low value for simee (section 7).

The profile's rank 2 attribution also needs two corrections (section 3): finding the next block is 32 to 36% of
vCPU time in a loop that makes no context switches at all, and "TLB partial flushes" counts MMU-index tables, not
flushes.

## 1. Method and host

| | |
| --- | --- |
| Host | Firecracker VM, Ubuntu 22.04, kernel 6.1.189, no `/dev/kvm`, 4 vCPUs of an Intel Xeon at 2.2 GHz (the profile's CPU), 3.9 GiB RAM (cgroup limit 3.3 GiB, no swap; about 1 GiB free while a guest ran), 20 GB disk |
| QEMU | 11.1.3 from the release tarball (sha256 5a8289c7…), `--target-list=x86_64-softmmu --enable-slirp` and no docs, GUI, tools or user mode; the same flags as the profile |
| emu | release build of main at 486be48; `-no-window -no-audio`, QEMU through an `EMU_QEMU` wrapper that adds a monitor socket |
| Guest | API 36 `default` x86_64 r2, kernel 6.6.66, AVD `hw.cpu.ncore=4`, `hw.ramSize=1536`, 720×1280; QEMU's command line is emu's: `-accel tcg,thread=multi -cpu max,-erms,-fsrm,-fzrm,-fsrs,-fsrc` |
| Profiling | `perf` 5.15 (`linux-tools-generic`), `perf record -F 997 -p <qemu pid>` (499 Hz for whole boots); vCPU threads are `CPU n/TCG`; "guest code" is the `[JIT]` mapping |
| Load | nothing ran but emu and QEMU. The 1-minute load average was 0.32 to 0.58 when a cycle started (one: 1.40) and 2.3 to 3.2 when it ended, after the swipes and the ping-pong |

Differences from the profile's host: 3.9 GiB of RAM against 7.8, so QEMU's default translation cache is
`min(1 GiB, host RAM / 8)` = 492 MiB here (`tcg/region.c`; `info jit` reads 516,165,632 bytes), not 1 GiB. The
first boot of this AVD took 366.8 s and a warm boot 154 to 181 s, close to the profile's 360 s and 172.6 s, so
the host runs the same workload at about the same speed.

The ping-pong is [tcg-pingpong.c](tcg-pingpong.c), a static binary pushed to `/data/local/tmp`. A round trip is
four pipe syscalls and one byte each way. `proc` is two processes pinned to one vCPU (every message is a context
switch with a CR3 write), `thread` two threads of one process pinned the same (a switch with no CR3 write),
`single` one process doing the same four syscalls on its own pipes (no switch). An optional working set makes each
side touch one byte on each of N private pages per message, so a flushed TLB has something to refill.
[tcg-mmstress.c](tcg-mmstress.c) is a correctness check: workers that mmap, fork (copy-on-write), mprotect, mremap
and munmap while verifying every page.

Per run the harness read, from the host: QEMU's per-thread `utime+stime`, the monitor's `info jit` counters, and
from the guest `/proc/stat` `ctxt`. It ran each cycle as: warm boot, idle to 300 s after `boot completed`
(CPU over 180 to 300 s), 10 launcher swipes up and down (`scripts/android-jank.sh`), the ping-pong, `mmstress`.

## 2. The bound: pipe ping-pong

Median of 5 warm boots of unpatched QEMU (round trips a second; the range is in brackets). A round trip has 2.2
guest context switches (`ctxt` per round trip).

| Working set | single process | two threads | two processes | µs per round trip (single / thread / proc) | CR3 write, µs (proc − thread) / 2 |
| --- | --- | --- | --- | --- | --- |
| none | 58.6k (55.8–61.5k) | 9.6k (7.9–11.3k) | 3.3k (3.2–3.3k) | 17 / 105 / 306 | 100 |
| 128 pages (512 KiB) | 51.0k (46.9–53.9k) | 8.7k (8.0–9.9k) | 2.4k (2.3–2.6k) | 20 / 115 / 410 | 148 |

A first guest session, 2 to 3 runs each: 32 pages 60.0k / 8.1k / 3.0k, 512 pages (2 MiB) 37.5k / 5.7k / 1.5k, so
the cost grows with the working set: 263 µs a switch at 512 pages. Two threads cost 44 µs more a switch than one
process with no switch: that is the guest scheduler's path through TCG, which a tagged TLB can't help.

Where the host time goes (`perf`, vCPU threads, share of their samples; first guest session, one run each):

| | guest code | finding the next block (`helper_lookup_tb_ptr`, `qht_*`, `tb_lookup_cmp`, `x86_get_tb_cpu_state`) | soft MMU (`probe_access_internal`, `mmu_translate`, `tlb_set_page_full`, `victim_tlb_hit`, `do_ld_*`) | flush (`tlb_flush*`, `tcg_flush_jmp_cache`, `tlb_reset_dirty`) | other QEMU, libc, host kernel, translating |
| --- | --- | --- | --- | --- | --- |
| single process | 32% | 36% | 13% | 0.8% | 19% |
| two threads | 33% | 30% | 15% | 0.6% | 22% |
| two processes | 19% | 21% (helper 8.0, `qht_lookup_custom` 5.5, `tb_htable_lookup` 3.2) | 27% | 2.2% | 31% |

The flush itself is cheap (2%, `tcg_flush_jmp_cache` 0.9%: only the dirty MMU indexes are cleared). What a flush
costs is the refill after it: the soft MMU share doubles and the jump cache misses fall through to the hash table.

## 3. Two corrections to the profile

1. **Finding the next block is not the flush.** The single-process loop never switches and still spends 36% of
   vCPU time there (`helper_lookup_tb_ptr` alone 22%): every syscall return and indirect jump goes through it. The
   25 to 30% in section 3 of the profile is mostly that. In a whole boot, with 3.3 times fewer flushes (section 6),
   `qht_lookup_custom` fell only 4% and `tb_htable_lookup` 8%.
2. **"TLB partial flushes" counts tables.** QEMU adds the number of MMU-index tables a flush cleared
   (`ctpop(to_clean)` in `tlb_flush_by_mmuidx_async_work`), not the number of flushes. A CR3 write cleared about 3.8
   tables in the ping-pong (410k counted for about 105k CR3 writes) and about 4.7 on an idle guest (584 a second counted
   against 125 CR3 writes a second). The profile's match of 570 flushes to 564 context switches a second was two
   unrelated rates meeting; most idle context switches (kernel threads, the same address space) write no CR3
   (`log mmu` counted 2,195 CR3 writes in 20 s of idle, 110 a second, against 560 to 575 switches).

## 4. How much of a real guest it is

CR3 writes, from `log mmu` on a settled guest and from the patched build's `PCID switches` counter (one per CR3
write):

| Phase | CR3 writes | Context switches |
| --- | --- | --- |
| Idle, 180–300 s after boot | 125 a second (14,991 in 120 s) | 560 to 575 a second |
| Launcher swipes over adb (44 s) | about 1,000 a second (43k to 46k) | 2,118 a second |
| Warm boot to `boot completed` | 130k to 163k (about 900 a second) | 303k to 383k |

The ceiling for a boot: 150,000 CR3 writes, 70% of which the patched build avoids a flush for (partial flushes fall
3.3×), at 100 to 150 µs, is 11 to 16 vCPU-seconds of the 370 a boot uses: 3 to 4%, 5 to 7 s of 165. With the
ping-pong's 512-page switch (263 µs) it would be 7%. An idle guest: 125 × 100 to 150 µs = 0.013 to 0.019 cores of
0.6. Boot-to-boot spread is ±9 s, and about 5% of vCPU seconds, so a gain of this size can't show in a boot; the
measured idle gain is larger than this ceiling (below), so real processes' switches cost more than the ping-pong's.

## 5. QEMU's options and what upstream has

- `one-insn-per-tb: off` (`info jit`); `split-wx` defaults to off unless QEMU is built with TCG debugging
  (`accel/tcg/tcg-all.c`), and only adds a second mapping of the code buffer; `tb-size` and a larger jump cache
  were measured in the profile (no gain, slower). None changes what a CR3 write does.
- `-cpu max,+pcid` on an unpatched QEMU 11.1.3 is refused: `warning: TCG doesn't support requested feature:
  CPUID[eax=01h].ECX.pcid [bit 17]`, and the guest's `/proc/cpuinfo` has no `pcid` (checked on this guest).
- `target/i386/cpu.c` in 11.1.3 and in master (9751c62358, 2026-11-26) lists `CPUID_EXT_PCID`, `CPUID_EXT_TSC_DEADLINE_TIMER`
  and `CPUID_7_0_EBX_INVPCID` under "missing" for TCG (they are shown only to user-mode emulation), and
  `cpu_x86_update_cr3` in `target/i386/helper.c` is still `env->cr[3] = new_cr3; tlb_flush(...)`. Searching the
  GitHub mirror's commits for `pcid` and `invpcid` finds the 2012 to 2018 CPU model definitions and nothing for TCG.
- The only mailing list discussion found is a February 2021 qemu-discuss thread
  ([question](https://lists.nongnu.org/archive/html/qemu-discuss/2021-02/msg00115.html)): Paolo Bonzini says TCG
  would only need to support the instructions, not tag its TLB; that the flag appeared to work because QEMU does not check
  reserved bits in CR3 and CR4; that PCID without INVPCID makes little sense; and that TCG is a poor platform for
  anything that depends on TLB misses. No patch followed.
- Nothing like it for other targets was looked for. `NB_MMU_MODES` is 22 in 11.1 (x86 uses 8), so tagging by
  MMU index would fit a few PCIDs; the prototype swaps tables instead.
- On the guest side, Linux (since the 2017 PCID series by Andy Lutomirski,
  [v2 00/10](https://groups.google.com/g/linux.kernel/c/6QOsox39mXw)) keeps a few dynamic ASIDs per CPU (6 in the kernels I know; not re-read here), writes CR3
  with bit 63 set to keep a PCID's entries, and flushes a stale one when it is switched back to. This guest
  runs with `meltdown: Not affected`, so it has one PCID per address space.

## 6. The prototype

[patches/tcg-pcid.patch](patches/tcg-pcid.patch), against the 11.1.3 tarball (`patch -p1`, 9 files):

- `target/i386`: `cpu_x86_update_cr3` reads CR4.PCIDE and CR3 bit 63 and calls the new `tlb_switch_pcid(cpu, pcid,
  flush)`; a CR4.PCIDE change flushes everything and makes the current PCID 0; `helper_write_crN` no longer treats bit 63 as a
  reserved bit when PCIDE is on; PCID (not INVPCID) is advertised to system emulation.
- `accel/tcg/cputlb.c`: up to 8 *banks* hold the tables (only the MMU indexes with entries, victim entries
  included) and the jump cache of a PCID, and a switch swaps those with the bank. A switch to an unknown PCID, or with bit 63 clear, flushes the live tables as before. Anything that
  isn't a PCID-aware switch (`tlb_flush*`, CR0/CR4 changes, memory map updates, range flushes) drops every bank;
  INVLPG flushes the page in every bank (it also drops global entries of every PCID); `tb_flush` clears every
  bank's jump cache.
- Write protection of translated code (`tlb_reset_dirty`) needs care. Version 1 scanned every bank's tables on
  every call, which is once per translated code page and touches tables other vCPUs use. Version 2 (the patch)
  remembers one host address range per bank (up to 1 MiB) and applies it when the bank comes back, dropping a bank that
  would need more. `info jit` gains "TLB PCID switches" and "TLB PCID bank hits".

Correctness: about 20 boots to `boot completed` with the PCID guest (Linux sets CR4.PCIDE; 99.7% of switches hit a
bank at idle, 80% in boot), and `mmstress` (workers that fork, write-protect and
remap while checking every page, 8 workers on 4 vCPUs, 20 to 40 s) on every patched cycle: 31.4 million page checks,
0 bad; the unpatched build did 4.4 to 5.0 million a run, also 0 bad. Guest crashes in the log (a Bluetooth abort) also
happen with PCID hidden on the same binary (`-cpu …,-pcid`) and are the known Bluetooth one.

### Results

Same AVD, same host and day, QEMU built once. Unpatched: 5 full cycles (more boots below); version 1: 3; version 2:
2. Ping-pong in round trips a second, medians.

| | unpatched | v1: 8 banks, eager write-protect | v2: 8 banks, deferred (the patch) |
| --- | --- | --- | --- |
| Two processes, no working set | 3.3k | 10.2k | 8.5k |
| Two processes, 128 pages | 2.4k | 8.9k | 8.5k |
| Two threads, none / 128 pages | 9.6k / 8.7k | 10.2k / 9.6k | 9.9k / 9.9k |
| One process, none / 128 pages | 58.6k / 51.0k | 61.1k / 46.6k | 63.1k / 53.4k |
| CR3 cost per switch, none / 128 pages | 100 / 148 µs | 0 / 0 | 8 / 8 µs |
| Idle host CPU, 180–300 s (cores) | 0.60 (0.57–0.60, 5) | 0.54 (0.52–0.55, 3) | 0.53 (0.52–0.54, 2) |
| Idle jump-cache and TLB flushes, 120 s | 70,131 tables | not read | 5,080 tables, 14,991 switches, 99.7% hits |
| Launcher swipes: frames drawn, janky | 120–175, 93–98% | 114–125, 94–96% | 100–161, 94–95% |
| `mmstress`, passes in 20 s | 8,584 | 8,327 | 8,217 |

The swipes don't move (frames per run vary 100 to 175 with the same build). The two-process rate with the patch
equals the threads' within noise; the CR3 cost falls 92 to 95%.

Boot to `sys.boot_completed`, seconds, and QEMU CPU seconds to that point (`utime+stime` of the QEMU process,
read at `boot completed`; "-pcid" is the patched binary with `-cpu …,-pcid`, so no banks are used):

| Build | Boots | Boot, s | QEMU CPU s |
| --- | --- | --- | --- |
| Unpatched | 11 | 165.2 (154.1–181.0) | 370 (344–382, 6 measured) |
| v1, 8 banks | 3 + 1 under `perf` | 180.6–182.4 (and 177–197 in full cycles) | 407–411 |
| v1, PCID hidden | 3 | 153.8–161.6 | 345–361 |
| v1, 1, 2 or 4 banks, jump cache kept or not (one boot each) | 5 | 160.3 (2 banks, jc kept), 169.9–183.2 (others) | 358–416 |
| v2, 8 banks (the patch) | 5 | 168.0 (161.8–175.5) | 363.9–384.2 (3 measured) |

The one config that matched the unpatched boot, 2 banks (160.3 s, 357 CPU s), kept few enough PCIDs to avoid the
cost of the others. Version 1's `perf` (whole boot, vCPU threads) against an unpatched boot: `tlb_reset_dirty*` 5,307
samples against 1,750, guest code +10%, `helper_lookup_tb_ptr` +16%: more host work per guest instruction, from the
extra tables the other vCPUs' write-protect scans pulled through their caches, not more guest work (guest `pgfault`
+2.6%, `LOC` ticks +10% with the longer uptime). v2 removes that and the boot is level with unpatched.

Before and after, `perf` of the two-process ping-pong (vCPU threads): unpatched guest code 20%, lookup 23%, soft MMU
30%, flush 2.3%; v2: guest code 39%, lookup 31%, soft MMU 13%, flush 0.2%. The soft MMU share is what a thread
ping-pong shows (13%). Unpatched two threads for comparison: 34% / 29% / 13% / 0.6%.

## 7. Recommendation

**Stop; don't ship a patched QEMU for this.**

- The win a flush-free soft TLB can give a software-mode device is the idle cost: about 0.06 cores of 0.6 per
  device (10%). Boot (3 to 4% ceiling, not visible in 5 boots against 11) and frame pacing don't move. That
  doesn't pay for asking every software-mode user to build QEMU from source plus a patch (they already must: Ubuntu 22.04's QEMU is 6.2) and to rebase it for every QEMU release.
- The design has a floor under it: translating new code write-protects pages through every vCPU's tables, so
  banks are dropped exactly when a boot needs them (80% bank hits in boot against 99.7% idle), and the first version
  of the same idea made boots slower.
- If idle density across many software-mode devices becomes the goal, the cheaper levers are in the profile's rank
  5 (`hw.lcd.vsync=30`, the sensors' poll rate: 0.04 to 0.12 cores).
- An upstream RFC is optional. It would need INVPCID (Bonzini's point), the CR3 reserved-bit checks
  that currently `cpu_vmexit` where hardware faults, a PCID-aware flush for every `tlb_flush` caller, tests, and a
  case that isn't a pipe ping-pong; the benefit is for fork-heavy TCG workloads (3× on process switching), not
  for simee's guests. Offer it only if someone wants to carry it.

Update software-mode-profile.md's rank 2: the estimate of 1.3× is withdrawn, and its stated mechanism for the
lookup share is wrong (section 3).

## 8. What this doesn't cover

One host (4 vCPUs, 3.9 GiB), one guest image (API 36 `default` x86_64), one QEMU release, a 4-vCPU guest. Boot
spread is ±9 s over the day (the 11 unpatched boots run 154 to 181 s), so a boot gain under about 5% can't be
excluded or confirmed; the patched build has no more than that to give by the arithmetic of section 4. The build
was not run through AutoMobile, a long `monkey` session, a build or database workload, or guests with page-table
isolation (12 PCIDs per CPU would not fit 8 banks). INVPCID isn't implemented and isn't advertised, so a guest
that needs it (Linux uses it when present) gets the slower CR3 paths. `perf` runs are one per row. The first
guest session's ping-pong and perf tables are from the boot before the others, on the same AVD. Host load was
that of the guest alone, 0.3 to 0.6 at the start of a cycle.

## 9. Reproduce

```bash
# QEMU: the release tarball, the profile's configure flags; the patched one adds the patch
patch -p1 < docs/research/patches/tcg-pcid.patch
# emu in software mode, QEMU through EMU_QEMU (a wrapper may add -monitor unix:...,server,nowait)
gcc -O2 -static -o pingpong docs/research/tcg-pingpong.c -lpthread
gcc -O2 -static -o mmstress docs/research/tcg-mmstress.c
adb push pingpong mmstress /data/local/tmp
adb shell '/data/local/tmp/pingpong proc 30000 128; /data/local/tmp/pingpong thread 30000 128; /data/local/tmp/pingpong single 100000 128'
adb shell '/data/local/tmp/mmstress 8 20'
# monitor: info jit  (TLB counters; the patch adds "TLB PCID switches" and "TLB PCID bank hits")
# CR3 writes on an unpatched guest: monitor "logfile f" "log mmu", then count "CR3 update" lines
perf record -F 997 -p <qemu pid> -- sleep 20     # during the ping-pong
```
