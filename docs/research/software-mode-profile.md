# Where software mode spends its time (4 vCPUs, no KVM)

Research for [#55](https://github.com/kaeawc/simee/issues/55): `emu -avd` on a Linux host with no
hardware virtualization and no GPU, profiled phase by phase so the next performance work goes to
the biggest cost. Every run is one AVD on one host over about 6 hours; the tables say how many
boots each number has.

## 1. Environment

One host for every number below, 2026-11-27.

| | |
| --- | --- |
| Host | Firecracker VM, Ubuntu 22.04, kernel 6.1.189, `/dev/kvm` absent, no GPU (Mesa llvmpipe/lavapipe only) |
| CPU | 4 vCPUs of an Intel Xeon at 2.2 GHz (`hypervisor`, `sse4_2`, `avx2`, `avx512f`; no `vmx`/`svm`) |
| Memory / disk | 7.8 GiB RAM, no swap; root disk 9.8 GB at the start (1.6 GB free once the image and renderer were fetched), grown to 20 GB part-way through |
| Toolchain | rustc 1.99.1 (rustup), release build of `emu` and `simee` from main at 5bcc3a8 |
| QEMU | 11.1.3 built from the release tarball (`--target-list=x86_64-softmmu --enable-slirp`, no docs, no GUI), `-j4` in 191 s, installed to a private prefix and passed as `EMU_QEMU`. Ubuntu 22.04's 6.2 is too old (docs/android.md) |
| Image | API 36 `default` x86_64, r2: `system.img` 1.9 GB, `vendor.img` 104 MB, kernel 20 MB (1.7 GB on disk) |
| Renderer | the SDK's Linux `libgfxstream_backend.so` 37.3.10 from `emu -provision-renderer` (255 MB on disk) |
| adb | platform-tools, 22 MB |
| AVD | an AVD named `prof` on that image: `hw.cpu.ncore=4`, `hw.ramSize=1536` (1.5 GB, to leave the 7.8 GiB host room for QEMU, emu, perf and the sampler), 720×1280 at 320 dpi, 2 GB data |
| Profiling | `perf record -a -g` (the 5.15 `perf` binary runs on this 6.1 kernel; `perf_event_paranoid` is -1) and a 2 s per-thread `/proc` sampler (`utime`+`stime` per QEMU/emu thread) |

Fetched, and only that: the system image zip, the renderer archive, platform-tools, the QEMU
source tarball (sha256 5a8289c7…84b22), the Rust toolchain. The documented route was used as
written, except that QEMU is built from source: no QEMU 11 package exists for Ubuntu 22.04.

## 2. Reproducing the published numbers

`docs/benchmarks.md` ("Software mode through `emu -avd`") publishes: warm boot 161.0 s, launcher
swipe frame time p50 / p90 / p99 150 / 300 / 400 ms with 96.7% janky frames, 2.46 host cores while
swiping, 2.14 cores 60–120 s after boot and 0.75 cores 240–300 s after. Here: one uncounted first
boot (fresh userdata), then 3 warm boots of the same AVD, each followed by 2 runs of
`scripts/android-jank.sh emulator-5554` (10 launcher swipes up and down, then the launcher's
`dumpsys gfxinfo`; 6 runs, where the published table has 9). Boot is emu's `boot completed` line,
"launcher" is the first time `dumpsys activity` reports the launcher resumed, host cores are
emu's plus QEMU's `utime+stime` from a 2 s per-thread sampler. Medians; the AVD has 1.5 GB of
RAM, not the published run's 2 GB (the 7.8 GiB host also holds `perf` and the sampler).

| Measure | This host, median (runs) | Published |
| --- | --- | --- |
| First boot of fresh userdata, to `sys.boot_completed` / launcher | 360.2 s / 386.8 s (one run) | 381.4 s |
| Warm boot to `sys.boot_completed` | 172.6 s (179.3, 161.0, 172.6) | 161.0 s |
| Warm boot to the launcher resumed | 187.7 s (197.2, 176.2, 187.7) | not published |
| Host CPU 60–120 s after boot | 2.37 cores (3.78, 2.37, 1.19) | 2.14 |
| Host CPU 180–300 s after boot (the 2 min idle window) | 0.58 cores (0.57, 0.58, 0.58) | 0.73 at 240–300 s |
| Launcher swipes, frame time p50 / p90 / p99 | 150 / 300 / 450 ms | 150 / 300 / 400 |
| Janky frames | 95.5% (93.9, 95.8, 94.9, 96.7, 95.2, 96.7) | 96.7% |
| Frames rendered per run of 20 swipes | 128 first run, 184 second (131, 189 / 118, 182 / 124, 180) | not published |
| Host CPU while swiping | 2.56 cores (3.04, 2.11, 2.95, 2.17, 2.97, 2.19) | 2.46 |

Everything reproduces within the run-to-run spread except idle, which is 0.15 cores lower here
(0.55–0.62 in every idle window of every run in this research except one, against 0.73 published). The first
swipe run of each boot draws about a third fewer frames than the second (it is the coldest
the guest gets) and burns about 0.9 cores more host CPU for them. The 60–120 s
window is the noisiest: 1.19 to 3.78 cores across three boots of the same AVD, because it
catches the guest's post-boot work at different points (system_server, the composer HAL and
Settings were the busiest guest processes there, section 3).

## 3. Where the host CPU goes, by phase

Method. Phases are the ones the published rows use: boot (emu start to `sys.boot_completed`),
60–120 s after, 240–300 s after (idle), and the two swipe sets (10 up and 10 down launcher swipes,
sent either over emu's display socket with `simee swipe`, which starts nothing in the guest, or
over adb with `scripts/android-jank.sh`, which starts one `input` process in the guest per swipe).

- **By thread**: the 2 s per-thread sampler (`utime+stime` of every thread of emu and of its QEMU
  child), on a boot without `perf` running. The vCPU threads are QEMU's `CPU n/TCG`; `Thread<0n>`
  in emu are SwiftShader's four rasterizer workers.
- **By symbol**: `perf record -a -g -F 299` over each phase, `perf report --no-children --sort
  comm,dso,sym`, on a second boot. Samples in the TCG code cache (an anonymous mapping that holds
  the code QEMU generated from the guest's) are one symbol, "translated guest code"; QEMU's own
  functions are grouped by what they do. `perf` didn't change the boot time (170.8 s with it,
  169.1 to 177.7 s without).
- Host CPU "cores" are `samples / total samples × 4`, idle included, so each row sums to 4.

### Threads, cores of the 4 (one boot, no perf)

| Phase | vCPU threads (4× `CPU n/TCG`) | SwiftShader workers (emu `Thread<0n>`) | emu's devices, console, display, GPU decoder | QEMU main loop and helpers | Total |
| --- | --- | --- | --- | --- | --- |
| Boot (168 s) | 2.16 | 0.56 | 0.03 | 0.08 | 2.83 |
| 60–120 s after (60 s) | 1.21 | 0.03 | 0.01 | 0.06 | 1.31 |
| Idle, 240–300 s (60 s) | 0.50 | 0.01 | 0.01 | 0.06 | 0.57 |
| Idle, 180–300 s (the 2 min window) | 0.51 | 0.01 | 0.01 | 0.06 | 0.58 |
| Swipes over the display socket (32 s) | 2.61 | 0.35 | 0.03 | 0.06 | 3.05 |
| Swipes over adb (44 s) | 1.62 | 0.55 | 0.03 | 0.06 | 2.26 |

Top threads by phase (cores): boot: each vCPU 0.53–0.56, each SwiftShader worker 0.14, QEMU main
0.06; idle: vCPUs 0.10–0.14 each, QEMU main 0.06, everything of emu's under 0.01; swipes over the
display socket: each vCPU 0.64–0.66, SwiftShader workers 0.34 in total.

emu's own device threads (vsock, the vhost-user kicks, the GPU, the sensors, the console) are 0.03
cores at their busiest and 0.01 idle: nothing in emu's Rust is a cost. The host never runs out of
cores either: no phase passes 3.05 of 4, and the vCPUs are 65% busy in the display-socket swipes. What bounds
a frame is how fast one guest thread runs, because the guest thread that draws a frame (the
launcher's UI thread, SurfaceFlinger) is one translated thread.

### Symbols, cores of the 4 (second boot, `perf`)

| Phase | Host idle | Translated guest code | TB lookup (`helper_lookup_tb_ptr`, `qht_*`) | Soft MMU (TLB refill, loads and stores) | Translating guest code | Other QEMU | SwiftShader (JIT'd shaders and driver) | QEMU main loop | emu device threads | Other processes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Boot | 1.03 | 0.67 | 0.60 | 0.39 | 0.13 | 0.40 | 0.64 | 0.06 | 0.02 | 0.07 |
| 60–120 s after | 2.67 | 0.39 | 0.31 | 0.30 | 0.03 | 0.16 | 0.02 | 0.06 | 0.01 | 0.05 |
| Idle, 240–300 s | 3.37 | 0.15 | 0.14 | 0.13 | 0.00 | 0.08 | 0.00 | 0.06 | 0.01 | 0.05 |
| Swipes (display socket; this run drew only 7 frames) | 1.08 | 0.76 | 0.65 | 0.58 | 0.22 | 0.40 | 0.19 | 0.06 | 0.01 | 0.06 |
| Swipes (adb) | 1.66 | 0.51 | 0.50 | 0.35 | 0.03 | 0.25 | 0.56 | 0.05 | 0.02 | 0.06 |

Top 10 symbols per phase (cores; `qemu` is QEMU's own code, the rest are named):

| Phase | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Boot | translated guest code 0.67 | SwiftShader shader JIT 0.55 | `helper_lookup_tb_ptr` 0.31 | `qht_lookup_custom` 0.15 | `probe_access_internal` 0.05 | `mmu_translate` 0.05 | `x86_get_tb_cpu_state` 0.04 | `tb_lookup_cmp` 0.04 | `tlb_set_page_full` 0.04 | `tb_htable_lookup` 0.03 |
| 60–120 s after | translated guest code 0.39 | `helper_lookup_tb_ptr` 0.15 | `qht_lookup_custom` 0.09 | `mmu_translate` 0.04 | `probe_access_internal` 0.04 | `tlb_set_page_full` 0.03 | `do_ld_8` 0.02 | `tb_lookup_cmp` 0.02 | `tb_htable_lookup` 0.02 | `do_ld4_mmu` 0.02 |
| Idle | translated guest code 0.15 | `helper_lookup_tb_ptr` 0.07 | `qht_lookup_custom` 0.04 | `probe_access_internal` 0.02 | `mmu_translate` 0.02 | `tlb_set_page_full` 0.01 | `tb_htable_lookup` 0.01 | `_raw_spin_unlock_irqrestore` (kernel) 0.01 | `tb_lookup_cmp` 0.01 | `do_ld_8` 0.01 |
| Swipes (display socket) | translated guest code 0.76 | `helper_lookup_tb_ptr` 0.30 | `qht_lookup_custom` 0.18 | SwiftShader shader JIT 0.17 | `probe_access_internal` 0.08 | `mmu_translate` 0.08 | `tlb_set_page_full` 0.05 | `tb_lookup_cmp` 0.05 | `do_ld4_mmu` 0.05 | `do_ld_8` 0.05 |
| Swipes (adb) | translated guest code 0.51 | SwiftShader shader JIT 0.51 | `helper_lookup_tb_ptr` 0.25 | `qht_lookup_custom` 0.14 | `probe_access_internal` 0.05 | `mmu_translate` 0.05 | `tlb_set_page_full` 0.04 | `tb_lookup_cmp` 0.03 | `do_ld_8` 0.03 | `x86_get_tb_cpu_state` 0.03 |

What the vCPU threads (the biggest consumer in every phase) spend their time on, as a share of
their busy time (the five TCG groups of the table above):

| Phase | Translated guest code | Finding the next block (TB lookup) | Soft MMU | Translating new code | Other QEMU |
| --- | --- | --- | --- | --- | --- |
| Boot | 31% | 27% | 18% | 6% | 18% |
| 60–120 s after | 33% | 26% | 25% | 3% | 13% |
| Idle | 30% | 28% | 26% | 0% | 16% |
| Swipes (display socket) | 29% | 25% | 22% | 8% | 15% |
| Swipes (adb) | 31% | 30% | 21% | 2% | 15% |

In boot the lookup is `helper_lookup_tb_ptr` 14% of vCPU time, `qht_lookup_custom` 7%, then
`tb_lookup_cmp`, `x86_get_tb_cpu_state`, `tb_htable_lookup` and `curr_cflags`; the soft MMU is
`probe_access_internal`, `mmu_translate`, `tlb_set_page_full`, `do_ld_*` and `victim_tlb_hit`.
Only about 30% of the time the vCPUs are busy runs the guest's own code, in every phase; 50 to
55% goes to jumping between translated blocks and to the soft TLB. Translating new code is a
boot-time cost (6 to 8%, and 0 when idle); once the cache is warm the cost of software mode is
dispatch and address translation, per instruction. Section 4 tests why they're so high.

### What the guest is doing

Guest uptime milestones from `logcat -b events` on a boot (ms of uptime; emu's `boot completed`
came 166.0 s after the start, 10 s after the guest's last milestone):

| Milestone | Guest uptime | Step took |
| --- | --- | --- |
| zygote starts (`boot_progress_start`): firmware, kernel, first-stage init, second-stage init | 55.4 s | 55.4 s |
| preload of classes and resources ends | 77.6 s | 22.1 s |
| system_server runs (`system_run`) | 82.6 s | 5.1 s |
| PackageManager scans the system partition and data ready (`pms_ready`) | 100.5 s | 17.9 s |
| ActivityManager ready (`ams_ready`) | 119.2 s | 18.7 s |
| screen enabled | 132.1 s | 12.9 s |
| boot animation stops (`sf_stop_bootanim`) | 155.1 s | 23.0 s |

Busiest guest processes (`dumpsys cpuinfo`, percent of one guest CPU): the 14 s from 16 to 30 s
after boot completes (load average 17): system_server 86%, the ranchu composer HAL 69% (59% user),
Settings 47%, SurfaceFlinger 30%, SystemUI 24%, the launcher 20%. In the idle window: system_server
30%, SystemUI 12%, the composer HAL 10%, SurfaceFlinger 8.3%, the sensors HAL 6.2%, the launcher 4.3%.
One guest thread's speed is what it waits on: system_server's boot is serial.

## 4. Why the vCPUs are slow, and what was tried

### The cause: every guest context switch flushes QEMU's TLB

Section 3 puts about 50% of the vCPUs' busy time in finding the next translated block and in the
soft TLB, and 30% in the guest's own code. QEMU's monitor (`info jit`, through an `emu` run whose
QEMU is a wrapper script that adds `-monitor unix:...`) says why. Guest `/proc/stat` and QEMU's TLB
counters, read together over the same 60 s of an idle guest, 4 minutes after boot complete:

| Over 60 s, idle | Guest context switches (`ctxt`) | QEMU "TLB partial flushes" | "TLB elided flushes" |
| --- | --- | --- | --- |
| Count | 34,143 (564 a second) | 34,172 (570 a second) | 107,956 |

One QEMU TLB flush per guest context switch, to within 0.1%, while "TLB full flushes" stays 0. Over
31 s of launcher swipes (display socket): 64,319 context switches (2,111 a second) and about 136,000
partial flushes. An x86 guest reloads CR3 when it switches address space; QEMU's TCG doesn't
implement PCID (`-cpu max,+pcid` on QEMU 11.1.3: `warning: TCG doesn't support requested feature:
CPUID[eax=01h].ECX.pcid`; the guest's `/proc/cpuinfo` has no `pcid` or `invpcid`), so each switch
drops the vCPU's soft TLB, and with it the per-vCPU jump cache that `helper_lookup_tb_ptr` consults
first. The next few hundred guest instructions refill both: `tlb_set_page_full`, `mmu_translate`,
`probe_access_internal` and `qht_lookup_custom` are the symbols that fill the vCPU rows of section
3. The guest isn't to blame either: its kernel reports `meltdown: Not affected`, so no page-table
isolation adds CR3 writes on every syscall, and `mitigations=off` changed nothing in earlier
measurements (docs/benchmarks.md).

Two more `info jit` readings: the translation cache held 622 MB of its 1,053 MB 4 minutes after
boot complete (1.00 M translated blocks) and 695 MB (1.12 M) after 31 s of swipes, and its flush
count was 1 throughout: translating is a one-off cost after boot, and the cache has room for the
workload of a short session.

### What was tried

Every run: the same AVD and image as section 2 (4 vCPUs, 1.5 GB, 720×1280), one boot per row unless
said, the same swipe scripts. The baseline is the six boots of sections 2 and 3: boot 171.7 s median
(161.0 to 179.3), launcher 186.6 s (176.2 to 197.2), idle 0.57 to 0.60 cores. Boot-to-boot spread
is ±9 s, so a difference under about 10 s is not a result.

| Configuration | Boot / launcher | Idle, 240–300 s | Launcher swipes over adb: frames, p50 / p90, janky | Result |
| --- | --- | --- | --- | --- |
| 3 vCPUs (`hw.cpu.ncore=3`) | 199.3 s / 226.1 s | 0.59 | 174, 200 / 350 ms, 98.3% | boot 16% later |
| 2 vCPUs | 434.7 s / 518.5 s | 0.58 | 161, 150 / 400 ms, 98.1% | boot 2.5× longer |
| QEMU confined to 3 cores (`taskset -c 0-2`), the renderer and emu on the 4th | 180.5 s / 203.0 s | 0.60 | 181, 200 / 350 ms, 97.2% | no change (within noise) |
| `hw.lcd.vsync=30` | 180.8 s / 204.4 s | 0.55 | 175, 150 / 300 ms, 89.7% | idle 0.04 lower; fewer janky frames because there are fewer deadlines |
| 540×960 at 240 dpi | 166.2 s / 196.4 s | 0.59 | 194, 150 / 200 ms, 94.3% | boot 3% earlier (noise); 8% more frames and p90 200 ms against 300 |
| Animations off (window, transition, animator scales 0) | 166.0 s / 191.7 s | 0.59 | 110, 150 / 300 ms, 98.2% | no gain: fewer frames because nothing animates |
| QEMU rebuilt with a 16× larger jump cache (`TB_JMP_CACHE_BITS` 12 → 16), 2 boots | 187.8 / 178.2 s; launcher 220.0 / 199.3 s | 0.82, 0.57 | 173, 150 / 300 ms, 97.1% and 182, 150 / 300 ms, 98.9% | slower: `tcg_flush_jmp_cache` took 0.13 to 0.17 cores (each flush clears 512 KB), the lookup share didn't fall (0.79 against 0.60 cores in boot) |
| 6 vCPUs on the 4 host cores, against a baseline boot from the same state (below) | 167.1 s / 176.1 s, against 149.4 s / 156.1 s | 0.59, against 0.64 | 107, 200 / 300 ms, 99.1%, against 113, 150 / 300 ms, 98.2% | boot 12% later |
| QEMU's translation cache 2 GiB (`tb-size=2048`) instead of the default 1 GiB, against the same state | 144.9 s / 150.8 s, against 143.3 s / 149.2 s | not measured | 12 min of `monkey` instead (below) | no difference |
| `-gpu lavapipe` (`LP_NUM_THREADS` 0 and 2) | emu exited 138 s and 66 s in | | | no boot, as docs/benchmarks.md records for the SDK's lavapipe |

The 6 vCPU and `tb-size` rows, and the baseline they are compared with, ran after the animation
scales were set to 0 for the "animations off" row. They persist in the AVD's userdata, so those
runs, `mon1` above and the `monkey` runs below had the animations off (107 to 113 frames over adb,
against 173 to 194 with them on), until the next `-wipe-data` reset them. Each of these rows is
compared with a boot from its own state: 6 vCPUs against 149.4 s / 156.1 s (idle 0.64, 113 frames, 98.2%
janky), `tb-size` against its own 1 GiB run. A fresh baseline after the reset (a second boot of
the wiped AVD, animations on) took 156.4 s / 175.6 s, idle 0.59, 181 frames, 97.8% janky, which is
the pre-reset baseline again. Boot-to-boot spread across the day was 143 to 187 s.

The first two rows say the guest is bound by single-thread speed, not by cores: 4 vCPUs are 54–65%
busy, and still taking one away costs 16% and two cost 150%. The larger jump cache is the direct
test of the flush explanation: more entries make each flush dearer and don't reduce the misses,
because the flushes empty it either way.

### The translation cache: 1 GiB is full after the boot

`info jit` every 30 s through a 12.5 minute `monkey` run (25% app switches, 700 ms throttle)
that began 2 minutes after boot completed, with QEMU's default 1 GiB cache and with 2 GiB
(`tb-size=2048` in `-accel`, through the wrapper), the same AVD state.

| | 1 GiB (default) | 2 GiB |
| --- | --- | --- |
| Cache in use when `monkey` starts | 500 MB, after 1 flush during the boot | 1,015 MB, 0 flushes |
| Cache after 30 s / 60 s / 150 s of `monkey` | 726 / 894 / 984 MB | 1,183 / 1,288 / 1,367 MB |
| Flushes | 2: one in the boot, one about 160 s into `monkey` (984 MB to 262 MB) | 0 (1,469 MB at the end) |
| Boot / launcher | 143.3 s / 149.2 s | 144.9 s / 150.8 s |
| vCPU host cores per 30 s, `monkey` period | 2.07 to 2.78 | 1.93 to 2.79 |
| Events `monkey` injected, app switches | 99, 192 | 98, 191 |

Booting translates about 1 GB of code (1.65 M blocks), so the default cache fills and flushes once
before the launcher, and once more after about 2.5 minutes of app switching. Neither flush is
visible: the boot is the same length, the vCPU host cores have no dip or step at the flush, and
`monkey` is throttled so it can't show a slower guest. A flush throws away about 1 to 1.5 M
blocks; at the 58 µs of vCPU time a block cost in the swipes (8% of 2.6 vCPU cores over 31 s for
the 111,000 new blocks `info jit` counted) that is about 60 to 90 vCPU-seconds of translation,
15 to 25 s of a 4 vCPU guest (an estimate, spread over the minutes after the flush). No result here
justifies passing `tb-size`.

### The first boot: apexd decompresses 24 APEX files, one at a time

A fresh userdata took 360.2 s to `sys.boot_completed` and 385.8 s to the launcher (median of 4
first boots, 360.1 to 366.7 s and 380.3 to 386.8 s), against 156 to 179 s for a warm one. The
guest's own milestones (`logcat -b events`, uptime) on a first and a warm boot:

| Milestone | First boot | Warm boot |
| --- | --- | --- |
| zygote starts (`boot_progress_start`) | 177.5 s | 52.2 s |
| `system_run` | 203.1 s | 77.6 s |
| `pms_ready` | 233.6 s | 95.3 s |
| `ams_ready` | 301.2 s | 115.0 s |
| `enable_screen` | 324.5 s | 128.2 s |
| boot animation stops | 348.8 s | 144.7 s |

118 of the 125 extra seconds before zygote are one wait: init's `Wait for property
'apexd.status=activated' took 118366ms`. On a first boot `apexd` decompresses the image's 24
compressed APEX files (`.capex`, for example `com.android.art`, `conscrypt`, `adservices`) into
`/data/apex/decompressed`, one after the other (24 `Decompressing` lines from 11:02:48 to
11:04:40), and finds them already there on every later boot. On the first of these boots the
host ran about one vCPU's worth for those 180 s (1.25 to 1.45 cores, against 2.8 in a warm boot). The rest of the extra time is
`system_server` after `pms_ready` (67 s to `ams_ready` against 20 s; a 110 s `main` handler
delivery in `UserManagerService`, logcat's "Slow delivery") and the boot animation.

Cloning a settled AVD skips all of it. `emu -clone-avd prof2 -from prof` took 12.8 s here (a plain
copy of the AVD's 2.1 GB; this host's file system has no reflinks) and
the clone's first boot reached `sys.boot_completed` in 161.1 s and the launcher in 190.4 s (one
boot), against 360 and 386 s for a fresh AVD. docs/farm-android.md already describes clones for
the farm.

### savevm and loadvm

On the software-mode VM (emu's devices attached, the guest at the launcher), the QEMU monitor's
`savevm snap1` answers at once:

```
Error: State blocked by non-migratable device '0000:00:02.0/vhost-user-vsock'
```

`info snapshots`: "There is no snapshot available." QEMU names the first blocker; emu serves the
vsock, the console, net, touch, keyboard, rotary and GPU devices over vhost-user and the goldfish
address space and pipe over vfio-user, and the host-side state behind them (the renderer's
contexts and resources, open pipe connections, adb sessions) lives in emu, not in QEMU. So a booted
snapshot does not restore, and making one restore is work in emu, not a flag. Its payoff would be
the largest of anything in this document: the 172 s boot would become the time to map and
restore 1.5 GB. Section 7 takes it further: the blockers removed, what a restore costs and what it
breaks.

## 5. Ranked bottlenecks

Ranked by what a person waits for, with the evidence above. "Estimate" is derived from the
profile, not measured; nothing here is a number for `docs/benchmarks.md`.

| # | Bottleneck | What it costs the user | Evidence | Controlled in | Experiment and estimated gain |
| --- | --- | --- | --- | --- | --- |
| 1 | The first boot of a fresh AVD: `apexd` decompresses 24 `.capex` files serially, then `system_server` does its first-boot work | 360 s to `sys.boot_completed` and 386 s to the launcher, against 156 to 179 s warm: 190 to 200 s of every new AVD's first start | Section 4: init's wait for `apexd.status=activated` is 118.4 s of the 125 s extra before zygote; `ams_ready` is 67 s after `pms_ready` against 20 s; 4 first boots 360.1 to 366.7 s | `crates/emu-avd/src/lib.rs` creates the AVD, `apps/emu/src/main.rs:161` (`create_avd`) and `apps/emu/src/provision.rs:91` (`-provision ... -create-avd`) never boot it; `-clone-avd` is `apps/emu/src/main.rs:235` | Done: a clone of a settled AVD booted in 161.1 s / 190.4 s (one boot). To ship it: `-create-avd` (or `-provision`) boots the image once and keeps a settled userdata to clone for later AVDs of that image. Gain about 200 s per new AVD after the first; no QEMU or guest change |
| 2 | Guest code runs at a fraction of its speed because every guest context switch flushes QEMU's soft TLB and jump cache (no PCID in TCG) | Everything the guest does: the 156 to 179 s warm boot, about 7 frames a second in the launcher, 95% janky frames | Section 3: 30% of vCPU time is guest code, 25 to 30% finding the next block, 18 to 26% soft MMU. Section 4: 564 context switches a second idle = 570 TLB flushes a second; 2,111 a second swiping; `-cpu max,+pcid` is refused by TCG; a 16× larger jump cache is slower (`tcg_flush_jmp_cache` 0.13 to 0.17 cores) | `crates/emu-vmm/src/soft.rs:25` (`CPU`) and `:96` (`-accel tcg,thread=multi`): QEMU exposes no option for it | Not config-only. Keep the soft TLB and jump cache across a CR3 write by tagging entries with the guest's PCID in QEMU's x86 target (the structure the profile points at), or measure the bound first: a guest pipe ping-pong between two processes (a context switch per message) against the same work in one process. Estimate: removing half of the lookup and soft-MMU share is 25% of vCPU time, so the guest runs about 1.3× faster: boot 170 s to about 130 s. **Measured since (tcg-context-switch.md, #227): withdrawn.** A CR3 write costs 100 to 150 µs, the guest writes CR3 about 125 times a second idle and 150,000 times in a boot, which is a 3 to 4% ceiling; a prototype that keeps 8 PCIDs left boot unchanged (idle host CPU 10% lower). The lookup share is 36% in a loop with no context switches, so it is not mostly the flush |
| 3 | The warm boot is one serial guest job at about half a core per vCPU: kernel and init 52 to 55 s, preload 14 to 22 s, PackageManager 18 s, ActivityManager 19 s, screen and boot animation 36 s | 156 to 179 s to `sys.boot_completed` | Section 3: host CPU never above 2.83 of 4 cores in boot; vCPUs 54 to 65% busy; guest uptime milestones. Section 4: 3 vCPUs cost 16%, 2 cost 150%, 6 cost 12% (4 is right on 4 host cores) | `crates/emu-avd/src/lib.rs:218` (`cpus`, default 2 when the AVD sets no `hw.cpu.ncore`; `emu -create-avd` writes 4) and `crates/emu-vmm/src/soft.rs:99` | A booted snapshot is the only thing that skips it, and QEMU's `savevm` is refused (below): the vsock, console, net, input and GPU devices are emu's vhost-user and vfio-user backends and none is migratable, and the renderer's state is in emu. Estimate if it worked: restore of 1.5 GB plus a cold translation cache, tens of seconds against 170 s. A spike: serialize one device (vsock) through vhost-user's device-state protocol and see how many others block. **Spiked since (section 7, #214): the save and the load work and take 8.5 s; the restored guest's graphics crash because the goldfish address space and the renderer start empty** |
| 4 | SwiftShader draws every frame on the same 4 cores as the vCPUs | The launcher's 7 frames a second: its four rasterizer threads take 0.35 to 0.55 cores while swiping (24% of host CPU over adb) and 0.56 in boot | Section 3: `Thread<0n>` rows; the SwiftShader JIT is the second symbol in boot and in the adb swipes. Section 4: 540×960 at 240 dpi gave 194 frames against 173 to 183 and p90 200 ms against 300 (one boot) | `apps/emu/src/instance.rs:390` (SwiftShader unless `-gpu lavapipe`, which crashed emu 66 s and 138 s in) | Run `lcd540` three times and an AVD with `hw.lcd.vsync=30` together (they were measured separately): if the gains add, a documented software-mode AVD profile (smaller screen, 30 Hz) is a config change. Estimate: 10 to 20% fewer janky frames, not a change in frame rate. **Measured since (section 8, #228):** the gains do not add: the screen lowers the swiping cost (2.43 cores against 2.11 with both, 2.01 as shipped) and the p90 frame time (300 against 250 and 200 ms), with no more frames together, and shipped as `emu -create-avd -profile software-lite` |
| 5 | An idle guest costs 0.58 host cores | A device left running takes a core's worth of a 4-core host | Section 3: vCPUs 0.50 of it, QEMU's main loop 0.06, emu 0.01; the guest does 564 context switches a second (each a flush, rank 2) and takes 60 composer callbacks a second | `apps/emu/src/instance.rs:883` (`androidboot.qemu.vsync` from the AVD's `hw.lcd.vsync`), the AVD's sensor rate | `hw.lcd.vsync=30`: 0.55 against 0.59 here, 0.12 lower in docs/benchmarks.md. Lowering the sensors HAL's 15 Hz poll is a guest change; estimate 0.05 to 0.1 more. Fixing rank 2 lowers this too. **Measured since (section 8):** 30 Hz alone settles at 0.49 cores against 0.59, and with the 540x960 screen at 0.53: the screen adds nothing to the idle cost |

Checked and not bottlenecks (so no work belongs there): emu's own threads (vsock, vhost-user kicks,
GPU decoder, console, display) are at most 0.03 cores, 0.01 idle, and `vhost_user.rs:587` blocks in
`poll` with no timeout, so nothing spins; QEMU's main loop is 0.06 cores; the translation cache
is full after the boot and flushes twice in a 15 minute session with no
measurable effect, so `tb-size` stays unset; the jump cache size, confining QEMU to 3 cores,
switching animations off and 6 vCPUs changed nothing or made things worse; `perf` itself didn't
change the boot time.

What this doesn't cover: one host, one image (API 36 `default` x86_64), one AVD, and mostly one
boot per row, so only the differences above about 10 s or 10% mean anything; boot-to-boot spread
was 143 to 187 s over the day. Guest symbols weren't resolved (the profile is of QEMU, not the
guest); the guest's kernel could show which of its context switches are the periodic ones. The
renderer's cost was measured with SwiftShader only. The API 36 `google_apis` image, arm64 guests
and a host with more than 4 cores weren't run. The per-symbol profile (`perf`) and the
per-thread one come from different boots.

## 6. Could Android run in a container on this host's kernel?

A five-minute check of whether a container backend (an Android userspace on the host's kernel, no
VM) is possible here, run on the same host as section 1 on 2026-11-27 as root. Android's binder
IPC is the gate: a container needs it in the host kernel, as `binder_linux` or built in.

| Check | Result |
| --- | --- |
| Kernel config (`/proc/config.gz`; no `/boot/config-*`) | `# CONFIG_ANDROID_BINDER_IPC is not set`; `# CONFIG_MODULES is not set`; `CONFIG_MEMFD_CREATE=y`. No `BINDERFS`, `ANDROID` (other than the binder line) or `ASHMEM` lines at all: with binder off their options are hidden |
| `binder` in `/proc/filesystems` | no |
| `/lib/modules/$(uname -r)` | does not exist (`/lib/modules` is absent), so no `binder_linux`; the kernel has no module support, so none could be loaded |
| `mkdir /tmp/binderfs-test; unshare -Urm mount -t binder binder /tmp/binderfs-test` | `mount: /tmp/binderfs-test: unknown filesystem type 'binder'.`, exit 32. `unshare -Urm true` alone exits 0, so user and mount namespaces work; binderfs is what's missing. Nothing was mounted; the directory was removed |
| `docker` / `podman` | neither installed |

What it means: this host's kernel has no binder and can't load it, so a container backend can't run
here, and no container runtime was installed or tried. Anything that wants one needs a kernel built
with `CONFIG_ANDROID_BINDER_IPC` and `CONFIG_ANDROID_BINDERFS` (the Firecracker guest kernel here
is the host's to change, not simee's), and the result would be an x86_64 Android on the host's
kernel, so it says nothing about arm64 images.

## 7. Can a booted guest be saved and restored? (#214)

Research for [#214](https://github.com/kaeawc/simee/issues/214): CI's `boot (tcg)` job spends most
of its ~300 s in "Boot, screenshot, tap, stop" (227 s). If a booted guest could be saved once per
image and restored in a pull request, the check would take seconds. Run on 2026-11-28 with the
setup of `.github/workflows/software-mode.yml`, so the numbers are what that job would see on a
host of this size.

**Result: not yet.** QEMU refuses the save as emu runs it today (four blockers, all in how
emu's devices attach to it). With the blockers removed in a patched QEMU and a patched emu, the guest is
saved in 15 s, loaded in 7.5 s and answers on adb 20 s after it resumes, against 163 s for a
warm boot and 374 s for a first boot. But its graphics are dead: the first renderer calls
divide by zero in the guest, the composer HAL, SurfaceFlinger and zygote restart in a loop, and
no screenshot ever comes back. The screenshot and tap check can't pass on this restore, so phase 2
(a CI path that restores one) isn't built.

### Setup

| | |
| --- | --- |
| Host | Firecracker VM, Ubuntu 22.04, kernel 6.1.189, no `/dev/kvm`, 4 vCPUs of an Intel Xeon at 2.2 GHz, 8 GiB RAM, 9.8 GB disk (about 1 GB free during the runs) |
| QEMU | 11.1.3 from the pinned tarball (sha256 5a8289c7…84b22), CI's `configure` line plus `--disable-tools --disable-guest-agent` (disk) |
| emu | this branch's `main` (1c5704e), release build; QEMU through `EMU_QEMU` pointing at a wrapper script that adds `-qmp unix:...` (and `-incoming defer` for the restore) |
| Image | `emu -provision 36 -tag default -abi x86_64` (system image r2, renderer 37.3.10; checksums verified by emu), AVD from `emu -create-avd ci -sysdir ... -no-golden`: 4 vCPUs, 2048 MB RAM, 720×1280, 4 GB data, `-accel off` |

### Why QEMU refuses today

`migrate` and `savevm` through QMP on the unmodified setup, guest booting:

```
State blocked by non-migratable device '0000:00:02.0/vhost-user-vsock'
```

That is the first blocker QEMU names; QEMU 11.1.3's source and the patched runs show the rest.
Each fix hides the next:

| # | Blocker | Where | What fixing it takes |
| --- | --- | --- | --- |
| 1 | `.unmigratable = 1` on `vhost-user-vsock` and on `vhost-user-test-device` (the generic frontend of console, net, touch, keyboard, rotary and GPU) | QEMU `hw/virtio/vhost-user-vsock.c`, `vhost-user-test-device.c` | one line each: save the virtio state (`VMSTATE_VIRTIO_DEVICE`, as `vhost-user-rng` does). Not upstream: needs a QEMU patch, or emu stops using the test device |
| 2 | The GPU's shared memory regions ("devices with VIRTIO Shared Memory Regions do not support migration yet") | `hw/virtio/vhost-user-base.c` | a QEMU patch (the check is a blocker, not a missing feature of the guest's state) |
| 3 | `vhost-user backend lacks VHOST_USER_PROTOCOL_F_LOG_SHMFD`, then `vhost lacks VHOST_F_LOG_ALL` | emu's vhost-user backend (`crates/emu-vmm/src/vhost_user.rs`) | advertise both and accept `SET_LOG_BASE`/`SET_LOG_FD`. Saving a stopped VM needs no dirty log, so emu can accept and ignore it; a live save would need real dirty logging for the guest memory the devices write |
| 4 | `SET_VRING_BASE` with a non-zero index is an error ("only fresh rings are supported") | `vhost_user.rs`, `virtio/queue.rs` | the queue starts at the available and used indices of the restored ring (`Queue::new` always starts at 0); the in-flight descriptors (taken from the available ring, not yet in the used ring) have to be resumed by each device |

`vfio-user` (the goldfish address space and pipe) adds no blocker in QEMU 11.1.3; it adds no state
either, which is the problem below.

### The spike

QEMU patched to drop 1 and 2 (`.fields = {VMSTATE_VIRTIO_DEVICE, VMSTATE_END_OF_LIST()}` in place
of `.unmigratable`, the shared memory blocker removed; QEMU 11.1.3 rebuilt incrementally; QEMU's
own tests not run), and emu patched for 3 and 4 (+20 −12 lines in `vhost_user.rs`,
`virtio/queue.rs`, `virtio/mmio.rs`: `QueueConfig::start`, `SET_LOG_BASE` echoing its payload,
`SET_LOG_FD` ignored, `VHOST_F_LOG_ALL` advertised and masked from the features given to the
device). Nothing else in emu changed, so every device and the renderer start empty on the load.

Save: boot to the launcher (warm boot, 162.8 s), wait 60 s, screenshot (`pre.png`: 222 kB, the
launcher), `stop`, `migrate` to `exec:zstd -T0 -3`, `quit`. Load: a fresh `emu -avd ci` (new
QEMU with `-incoming defer`, the same disks), `migrate-incoming` from `exec:zstd -d -c`, `cont`.

| | Measured |
| --- | --- |
| Guest RAM / what QEMU sent | 2,148,016,128 B / 1,898,493,428 B (88% of the 2 GiB: zero pages aren't sent) |
| Save (`stop` to `completed`) | 14.3 s of QEMU's `total-time`, 15.1 s as seen from the script |
| State file | 517 MB with `zstd -3` (493 MiB) |
| Load (`migrate-incoming` to the VM paused with everything loaded) | 7.5 s; guest running 8.5 s after emu started |
| User data to go with it | `userdata.img` 4 GiB sparse, 545 MB allocated, 570 MB with `zstd -3`; cache and encryption key partitions 0.1 MB. The state is only valid with the disk it was saved with |
| Actions cache cost per key | about 1.09 GB (517 MB + 570 MB); the cache is over its 10 GB limit already, so each key would evict older entries |
| Full boot, same host | 163 s warm, 374 s first boot (a fresh AVD); the CI step takes 227 s per #214 |

### What the restored guest does

| Check | Result |
| --- | --- |
| `adb devices`, 20 s after `cont` | `emulator-5554 device` (the adb transport came back without help) |
| `getprop sys.boot_completed` | `1` (the property was in the saved RAM; not evidence of anything) |
| Screenshot over `display.sock`, 3 tries then 1 more 80 s later | `no frame presented yet`, then `the scanout frame couldn't be read`, every time: **no frame, ever** |
| `simee tap` | exit 0 (the display socket took it; with no screen, nothing shows it reached the guest) |
| Logcat | the guest's clock goes on from the save (01:18:15); 2 s later, `SIGFPE (FPE_INTDIV)` in `AddressSpaceStream::type1Write` in `vulkan.ranchu.so` (SystemUI's RenderThread, then system_server's and the launcher's), then in `rcCompose_enc` inside `android.hardware.graphics.composer3-service.ranchu`; init restarts SurfaceFlinger and zygote (01:18:25); the composer HAL aborts again at 01:18:29 and SurfaceFlinger at 01:18:31. Graphics stays dead |
| emu | `gpu: first command 0x0203 8450 ms after activation`, `gpu reset: 2 mappings removed, 2 contexts and 3 resources left`: a renderer with 2 contexts and 3 resources at the end, where the saved guest's run ended with 14 and 31 |

The divide by zero is most likely the guest reading a size of 0 from the address-space device
it had configured before the save (the crash is in the address-space graphics stream; this spike
didn't confirm what value was zero): the goldfish address space (`GoldfishAs`) and the goldfish pipe
carrier are `vfio-user` devices served by emu, and their state (the claimed address-space blocks,
the pipe command buffers, every open pipe connection) isn't in QEMU's stream. Beyond that the
renderer starts without the guest's contexts, color buffers, blobs and Vulkan objects.

### What a correct restore needs

Each is emu's work; the first two are also QEMU's.

1. **A QEMU that migrates the vhost-user devices**: patches 1 and 2 above carried in the build
   (the workflow already builds QEMU from a pinned tarball and caches it), or serving these
   devices without the generic test device.
2. **emu saves its own state next to QEMU's**, with the VM stopped, and loads it before `cont`:
   the ring positions and in-flight descriptors of every queue; vsock's connection table; the
   console ports' buffers; the net backend's NAT and DHCP state; the input devices' pending events;
   `GoldfishAs`'s blocks; the goldfish pipe's registers and every open pipe, with each service's
   state (adb, qemud services, `refcount`, `GLProcessPipe`, sensors, camera...). Today none of
   these has a serialize function.
3. **The renderer's state.** The SDK's `libgfxstream_backend.so` exports `stream_renderer_suspend`,
   `stream_renderer_snapshot`, `stream_renderer_restore` and `stream_renderer_resume` (nothing in
   `emu-gfxstream` calls them yet, and this spike didn't), next to the whole of the stock
   emulator's snapshot machinery (`androidSnapshot_*`). Whether they save a SwiftShader
   Vulkan device's state (pipelines, memory contents, color buffers) correctly is the
   open question, and the one that decides whether this is a week or months.
4. **A device-set and version contract.** A saved state is only valid for the emu build that wrote
   it: any change to a device's state or to the board (`layout.rs`, the ACPI table, the device
   list) invalidates it.

That last point also limits the CI use. The `boot (tcg)` job runs on a pull request only when the
change reaches `apps/emu` or the crates it builds. Those are the changes the boot is there to
test, and a state saved by `main`'s emu and loaded into the pull request's is a different
emulator reading another one's state. Keying the cache by the emu build (as issue #214 proposes)
means every pull request that triggers the job misses it and does the full boot, and a restored
snapshot would skip exactly the boot code under test (device init, the board, the pipe handshake).
A restore-only path would have to be limited to pull requests that don't change the board or a
device's state, and that is hard to decide mechanically.

### What was not tried

- `mapped-ram` migration (needs multifd and a seekable file; the stream through `zstd` was used),
  a live save with dirty logging, and a different compression level. The save and load times
  above are with the guest stopped, on 4 vCPUs that were otherwise idle.
- The renderer's snapshot calls, the address-space and pipe state, and so a correct restore.
  So there's no figure for what a correct restore costs; the 7.5 s load is a lower bound that
  leaves out the renderer's own load.
- Restoring more than once or on another host: the SIGFPE sequence was seen once, in the one
  restore whose guest ran (an earlier attempt that never sent `cont` is not a result).
- CRIU on emu and QEMU together (checkpointing the process tree, the renderer included): the host
  kernel has no `CONFIG_CHECKPOINT_RESTORE` and no `UNIX_DIAG`, so it couldn't be tried here. It
  would need root and a kernel with both on the CI runner, and the same disk and cache cost.
- The cheaper cut that needs none of the above, from section 4: a CI cache of the *golden user
  data* (`emu-avd`'s `golden`), which skips the first-boot work (a warm boot was 156 to 179 s
  against 360 s on the first host; section 4). That halves the boot, not "well under a minute",
  and its cache cost is the 570 MB of user data. It wasn't measured on CI.

### Reproducing

QEMU as in the workflow; `emu -provision 36 -tag default -abi x86_64`;
`emu -create-avd ci -sysdir system-images/android-36/default/x86_64 -no-golden`; an `EMU_QEMU`
wrapper that is `exec "$QEMU_REAL" "$@" -qmp "unix:$QMP_SOCK,server=on,wait=off" $QEMU_EXTRA`;
QMP `stop`, then `migrate` with `{"uri": "exec:zstd -T0 -3 -q -o state.zst"}`, polling
`query-migrate`, then `quit`; a new emu with `QEMU_EXTRA="-incoming defer"`, `migrate-incoming`
with `{"uri": "exec:zstd -d -c state.zst"}`, `cont`. The unmodified QEMU stops at the first
`migrate` with the vsock error above.

## 8. A smaller screen and 30 Hz together

Ranks 4 and 5 of section 5 each measured one config change once: 540x960 at 240 dpi
(section 4: 8% more frames, p90 200 ms against 300) and `hw.lcd.vsync=30` (idle 0.04 to 0.12 cores
lower). [#228](https://github.com/kaeawc/simee/issues/228) measured them together and each alone, against the default,
on one 4-vCPU host: three rounds of five warm boots in turn (15 boots), 2026-11-28. The fifth arm
is an AVD made by the shipped `-create-avd -profile software-lite`. The method, host and every
run are in [benchmarks.md](../benchmarks.md#software-mode-a-smaller-screen-and-30-hz);
`scripts/bench/software-profile.sh --arms "default lcd540 vsync30 lite shipped"` reproduces it. Medians:

| | default | 540x960 only | 30 Hz only | both (`config.ini`) | both (shipped) |
| --- | --- | --- | --- | --- | --- |
| Idle host CPU, 240 to 300 s after `sys.boot_completed` | 0.59 cores | 0.59 | 0.49 | 0.53 | 0.53 |
| Host CPU while swiping, warm launcher | 2.43 cores | 2.15 | 2.46 | 2.11 | 2.01 |
| Frames in 20 swipes, warm / cold launcher | 185 / 121 | 198 / 128 | 174 / 112 | 182 / 118 | 185 / 118 |
| p90 frame time, warm launcher | 300 ms | 250 ms | 350 ms | 250 ms | 200 ms |
| Warm boot to `sys.boot_completed` | 169.8 s | 156.9 s | 165.7 s | 156.4 s | 181.9 s |
| Process tree PSS | 2868 MB | 2824 | 2846 | 2804 | 2828 |

What it says:

- **The gains do not add.** 30 Hz removes callbacks the composer takes while nothing draws
  (section 5, rank 5), and is the only arm that clearly lowers the idle cost (0.59 to 0.49); the
  profile's 0.53 is between that and the default's, and the screen alone is the default's 0.59. The
  smaller screen has SwiftShader draw 44% fewer pixels (rank 4), which fits it lowering the swiping
  cost (2.43 to 2.15 cores) and the frame times; 30 Hz alone does neither (2.46 cores, p90 350 ms).
- **Frames: no gain together.** The screen alone draws 7% more frames on a warm launcher and 30 Hz
  alone 6% fewer, so the profile draws the default's count (182 and 185 against 185). Software
  mode still draws 2 to 4 frames a second under adb-driven swipes.
- **Janky frames fall from 97% to 85% and 81%, mostly because a 30 Hz deadline is 33 ms**: 30 Hz
  alone is 93%, the screen alone 96%. Read the frame times, not that percentage.
- **No boot or memory win.** The warm boots overlap (the default's 169.8 to 182.5 s, the profile's
  154.9 to 194.8 s), and PSS is within 2% in every arm: a 1.5 GB guest and QEMU's translation
  cache are the memory, not the framebuffer.
- **Noise.** The default's idle cost was 0.68, 0.59 and 0.58 on three boots of one AVD, so the
  profile's 0.51 to 0.55 is under two of them and not clearly under the third, and the 30 Hz arm's
  0.45 to 0.52 is the only one under all three.

An earlier 12-boot run of the same script (2026-11-27) found the profile at 0.46 idle against 0.68
and read as if the halves added. Its idle window was anchored to the launcher, not to
`sys.boot_completed`, so the arms did not all start at the same offset; the script was fixed and
the run repeated, and these numbers replace it. The host reported 3.8 GiB when the re-run
started and 7.8 GiB when it ended, at an unrecorded time in between, and the guests' PSS is higher
than in the first run, so do not compare memory across the runs (details in benchmarks.md).

The decision: the profile ships, opt-in (`-profile software-lite`), with the default AVD unchanged.
It buys about a tenth less idle CPU and about 17% less swiping CPU on a shared host, and it costs
a 540x960, 30 Hz display for the apps; it is not faster to boot or lighter. Default, owner may
override. What this doesn't cover: one host, one image (API 36 `default` x86_64), a 1.5 GB guest,
animations on, 3 boots per arm; launcher swipes over adb, not a real app; the stock emulator (it
has no software mode that boots, docs/android.md); and the sensors HAL's poll rate, which the
profile doesn't touch.
