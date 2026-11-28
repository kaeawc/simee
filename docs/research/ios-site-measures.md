# iOS measures for the README's Table 1

The iOS half of the README's first performance table: simee against Simulator.app on the
measures the Android half has. These rows belong in [benchmarks.md](../benchmarks.md); they are
here because open pull requests held that file when they were written. Move them there, as a row
of its summary table and a section, once it is free.

## Results

One host, 2026-11-27: M3 Max, 16 CPUs, 128 GB, macOS 26.6, an iPhone 17 on iOS 27.2, median of 5
runs of each arm. Host load was high throughout (1-minute load 13-24 when each run started,
28-60 during the idle windows), so the idle CPU and settle rows are indicative only.

| Measure | simee | `simctl`, headless (a) | `simctl` with Simulator.app open (b) |
| --- | --- | --- | --- |
| Cold boot, a new device | 18.6 s | 17.5 s | 18.5 s |
| Warm boot | 6.6 s | **2.6 s** | 6.6 s |
| Peak memory, warm (`footprint`) | 2985 MB | 3201 MB | 3114 MB |
| Idle host CPU, of a core (noisy) | 13.6% | 14.0% | 12.5% |
| Screenshot, median / p95 | **23 / 47 ms** through `simee serve`; 148 / 251 ms in its own process | 547 / 599 ms | 565 / 585 ms |
| Settle to quiet, 5 s windows under 10% of a core (noisy) | 131 s | 116 s | 125 s |

The README quotes column (b) as Simulator.app.

- **Warm boot.** Headless, `simctl` boots in 2.6 s against 6.6 s with a window. simee matched
  (b), because Simulator.app was already running and opens a window for any device of the default
  set. Arm (a) used `simctl --set` on a device set of its own, and simee can't target a custom
  device set yet, so there is no headless simee number to set against 2.6 s.
- **Everything else.** Arms (a) and (b) are within the noise of each other.

## Method

Each run creates a new iPhone 17 under each arm (the arms' order alternating from run to run),
times its first boot (cold), shuts it down and times a second boot (warm), waits for it to settle,
then measures idle CPU over a 60 s window and times 10 screenshots after one unmeasured one, and
deletes the device. A boot is `simee boot <udid>` for simee and `simctl bootstatus <udid> -b` for
the other arms. Peak memory is sampled from the warm boot to the end of the idle window, as
`footprint` summed over the device's process tree; RSS was not used, since it counts shared pages
twice. Idle CPU is the CPU time of the same tree over the window. Simulator.app was running and
could not be quit, which is why arm (a) needed a device set of its own.

No script: the measurement was a one-off harness outside the repository. A `scripts/bench/`
script for it is a follow-up, along with the two below.

## Not measured

- Frame pacing: `simctl` has no frame statistics, and `xctrace` wasn't tried.

## Follow-ups

- `--set` support in simee, so it can be measured headless the same way as arm (a).
- A warm-boot comparison with no window on either side.
