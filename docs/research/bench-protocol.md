# The quiet-host benchmark protocol

Research and reference for [#450](https://github.com/kaeawc/simee/issues/450), part of the
benchmark-rigor epic [#456](https://github.com/kaeawc/simee/issues/456). Many published numbers were
taken at a 1-minute load of 13 to 146 on a shared Mac while other builds and devices ran: the iOS
idle CPU and settle rows are marked noisy, one renderer arm ran at load 90 to 146, and the
`-gpu metal` latency tail came from one run while the load was falling. This is the protocol that
stops that: a benchmark refuses to run on a busy host, records the host it ran on, and marks a run
whose rounds disagree.

It lives here, not in `benchmarks.md`, because [#385](https://github.com/kaeawc/simee/pull/385)
holds that file; the protocol belongs in its "Reproducing a number" section once that merges, with
this page as the reference behind it.

## What it does

Every script under `scripts/bench/` (the shell ones through `lib.sh`'s `bench_start`, the two
Python ones through `summarize.quiet_gate`) passes the gate before it measures:

```text
scripts/bench/android-boot.sh: refused: this host is not quiet for android-boot.sh (looked for 300 s, BENCH_QUIET_WAIT=300):
  load: the 1-minute load is 13.40, not below 4.00 (0.5 x 8 CPUs; BENCH_QUIET_LOAD_FACTOR or BENCH_QUIET_MAX_LOAD)
  process: compiler cargo (pid 4121) is running
```

The exit status is 4. `run-all.sh` stops at the first refusal (every later benchmark would wait for
the same host) and exits 4.

| Condition | Default | Read from |
| --- | --- | --- |
| 1-minute load | below 0.5 x CPUs (`BENCH_QUIET_LOAD_FACTOR`, or `BENCH_QUIET_MAX_LOAD` for an absolute limit) | `/proc/loadavg`; `sysctl vm.loadavg` |
| Foreign processes | none: emulators (`emu`, `emulator`, `qemu-*`, `crosvm`), simulators (`Simulator`, `simee`, `launchd_sim`, CoreSimulator's service) or compilers (`cargo`, `rustc`, `clang`, `gcc`, `cc1`, `ld`, `swiftc`, `xcodebuild`, `ninja`) | `ps -axo pid=,ppid=,comm=` |
| Thermal pressure | none | macOS: `pmset -g therm` (`CPU_Speed_Limit` or `CPU_Scheduler_Limit` under 100), and `powermetrics --samplers thermal` (a level other than Nominal) when it can run without a prompt; Linux: `/sys/class/thermal` (a zone at or over `BENCH_QUIET_MAX_TEMP_C`, default 85, or a processor cooling device above state 0) |
| Power | not on battery | macOS: `pmset -g batt`; Linux: `/sys/class/power_supply` |

- **The bench's own processes are never foreign.** Own is the gate's shell, its ancestors (the
  `cargo run` or the `run-all.sh` that started it), everything below it, and
  `BENCH_QUIET_ALLOW_PIDS` plus their descendants. A sibling of an ancestor is foreign. Processes
  are matched by name to *detect* them; the gate never signals anything, and the exact basename is
  matched (`clangd`, `simeectl`, `cargo-watch` are not foreign).
- **A host that cannot report is not refused for it.** A VM or container with no thermal sensors
  records `thermal: unavailable`; a desktop with no battery records `power: unknown`. Without root
  or passwordless `sudo`, `powermetrics` is not read, and the record says so; `pmset` alone decides
  (reports of `pmset -g therm` on Apple silicon show no `CPU_Speed_Limit`, so a Mac benchmarked
  without root may have only the load, process and power checks to rely on; run the publishing pass
  as root or with passwordless `sudo` for `powermetrics`).
- **The wait is bounded.** The gate looks every `BENCH_QUIET_POLL` seconds (5) for up to
  `BENCH_QUIET_WAIT` seconds (300). The 1-minute load decays with a 60 s time constant, so a
  benchmark that follows a build waits about 80 s for the build's own load to fall below 4 on an
  8-CPU host; that wait is part of the protocol, not an error.
- **`--no-quiet-gate`** skips the gate, for CI smoke runs and a developer's check that a script
  works. The record says `quiet_gate: disabled (--no-quiet-gate)` and the run is not publishable.
  `software-mode.yml`'s KVM leg passes it: a hosted runner has just built the workspace, and
  [#453](https://github.com/kaeawc/simee/issues/453) decides how runner data is gated.
- **`run-all.sh --quiet`** is the run whose numbers are published. It looks once before starting
  (a refusal costs no build), refuses `--quick`, `--rounds` under 3 and `--no-quiet-gate`, and
  reports a benchmark whose result is not publishable as `NOISY`, exiting 1.

## The host record

Each result `<name>-<time>.json` has a sibling `<name>-<time>.host.json` (the same object is the
`record` key inside the result):

| Field | Meaning |
| --- | --- |
| `bench`, `bench_version` | the benchmark, and `file@sha256[:12]` of its script: a changed method is a changed version |
| `rounds`, `args` | rounds that ran, the command line |
| `commit` | simee's commit, `-dirty` with uncommitted changes |
| `model`, `chip`, `cores`, `cores_physical`, `mem_gb` | the machine (`Mac15,8`, `Apple M3 Max`, 16 logical cores, ...) |
| `os`, `os_version`, `kvm` | the OS and its version; whether `/dev/kvm` exists (Linux) |
| `load1_start`, `load1_end`, `load_source` | the 1-minute load at the start and the end, and where it came from |
| `thermal_start`, `thermal_end`, `thermal_detail` | `nominal`, `pressure` or `unavailable`, and what was read |
| `power_start`, `power_end`, `laptop` | `ac`, `battery`, `ups` or `unknown`; whether an internal battery exists |
| `quiet_gate` | `passed`, `passed after 45 s`, or `disabled (--no-quiet-gate)` |
| `publishable`, `not_publishable`, `noisy` | the verdict and its reasons |

The load at the end of a run includes the benchmark's own work, so it is recorded, not judged.

## Spread and noise budgets

For every measure of every arm, `finish` records `n`, p50, p95, IQR, standard deviation and
`rel_iqr` (IQR over p50) under `spread` in the result. Percentiles interpolate linearly between the
closest ranks (the default of NumPy and most spreadsheets), so with three rounds p95 is an
interpolation between the two largest.

A measure is **noisy** when it has three or more samples and `rel_iqr` is over its budget. The
budgets are in `scripts/bench/noise-budgets.tsv`: a glob over the measure's lowercased name, first
match wins, `-` for a count or verdict with no spread to budget. They are first
estimates of what a quiet host shows, not fitted to data (there is none yet: the published runs were
not quiet), and are meant to be tightened with the quiet-host re-runs of
[#451](https://github.com/kaeawc/simee/issues/451):

| Measures | Budget (IQR / p50) |
| --- | --- |
| times in seconds and milliseconds | 0.10 |
| p90, p95, p99 tails | 0.20, 0.25, 0.30 |
| CPU, `% of a core`, cores | 0.25 |
| RSS and MB | 0.10; MB/s 0.15 |
| frame rate | 0.10 |
| janky frames | 0.50 (a small share) |
| colours, pixels differing, failures, exit status | none |
| anything else | 0.15 |

`BENCH_NOISE_BUDGET=0.2` replaces every budget, for a run where the host cannot be made quieter; the
record still says what the gate did.

## What is publishable

A result is publishable when it passed the gate (not `--no-quiet-gate`), ran three or more rounds
(not `--quick`), has no noisy measure, and the host was neither under thermal pressure nor on battery
at the end. `python3 -I scripts/bench/summarize.py check bench/results/*.host.json` prints each
result's verdict and exits 1 if any is not. A noisy or unpublishable run is kept (its JSON says why)
and stays out of `docs/benchmarks.md`'s tables and the README; re-run it on a quieter host.

## Choices and sources

- **Load threshold.** The issue's 0.5 x CPUs is strict on purpose. The load average is the number
  of jobs in the run queue or waiting for disk I/O, averaged over the minute
  ([`proc_loadavg(5)`](https://man7.org/linux/man-pages/man5/proc_loadavg.5.html)), so half a task
  per CPU leaves the benchmark's own work most of the machine. It is a smoothed number, so the
  process check, which sees a build that has just started, catches a spike the average has not yet
  registered.
- **Thermal on macOS.** `pmset -g therm` reports `CPU_Speed_Limit` and `CPU_Scheduler_Limit` as
  percentages (100 is unlimited) and "No thermal warning level has been recorded" when there is none;
  `powermetrics --samplers thermal` prints `Current pressure level` (Nominal, Moderate, Heavy,
  Trapping, Sleeping) and is run with `sudo` in its documentation
  ([`pmset`](https://ss64.com/mac/pmset.html), [`powermetrics`](https://ss64.com/mac/powermetrics.html);
  the `pmset -g therm` field names and the pressure levels are from published user reports, not
  Apple's reference, and are the first thing to check on a Mac). The gate uses both and treats
  anything other than Nominal or 100 as pressure.
- **Thermal on Linux.** `/sys/class/thermal/thermal_zone*/temp` is millidegrees Celsius in practice, and a
  cooling device's `cur_state` above 0 means that device is limiting ([kernel thermal sysfs
  API](https://www.kernel.org/doc/html/latest/driver-api/thermal/sysfs-api.html); the page shows
  `37000` for a zone and does not state the unit, so millidegrees is the convention the gate reads,
  and a value under 1000 is read as whole degrees). Only processor-class cooling
  devices count; a fan spinning is cooling, not throttling.
- **Power on Linux.** `power_supply/*/type` is `Mains` or `Battery` (and others), `online` is 0 or
  non-zero, and a battery's `status` is `Discharging`, `Charging`, `Full` or `Not charging`
  ([sysfs-class-power](https://www.kernel.org/doc/Documentation/ABI/testing/sysfs-class-power)). Discharging is battery;
  any online non-battery supply, or a charging or full battery, is AC.
- **Spread statistic.** IQR over the median, not the standard deviation: three or four rounds is too
  few for a stable standard deviation, and one outlier (a boot that waited on a disk flush) dominates
  it; the IQR ignores a single tail value. The standard deviation is still recorded.
- **Detection by name, never by kill.** Foreign processes are found by name because there is no
  other handle on a process this run did not start; every other part of `scripts/bench/` stops only
  PIDs it started.

## Verification

`scripts/bench/test-quiet.sh` covers the gate and the record against a fake `/proc` and `/sys` and
stubs for `ps`, `sysctl`, `pmset`, `powermetrics`, `sudo`, `id` and `getconf`: a loaded host is
refused, a quiet one passes, foreign and own processes, thermal and power on both OSes, the bounded
wait, well-formed records, the spread helpers and the budgets, and `run-all.sh --quiet`. It needs no
device and no particular load. The macOS paths are exercised through the stubs and the output formats
above, not on a Mac: the first run on the owner's Mac is the check that `pmset` and `powermetrics`
print what the parsers expect on Apple silicon.
