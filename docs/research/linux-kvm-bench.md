# Linux with KVM on GitHub's Ubuntu runners: the benchmark

Method and reading guide for [#453](https://github.com/kaeawc/simee/issues/453), part of the
benchmark epic [#456](https://github.com/kaeawc/simee/issues/456). `docs/benchmarks.md` has numbers
for Linux without KVM and for a Mac; none for Linux with KVM. This workflow measures them on the
runner every contributor can reach. Nothing here is a published number yet: the first run has to be
dispatched after this merges, and `docs/benchmarks.md` quotes a row only once a run has produced it.

## What runs

`.github/workflows/bench-linux.yml` ("Bench Linux KVM"), on `workflow_dispatch` (input: rounds,
default 5) and weekly (Monday 04:17 UTC). Never on a push or a pull request; a run takes about 45
minutes. On `ubuntu-latest` it:

1. opens `/dev/kvm` to the runner user (`.github/actions/enable-kvm`, the same step `ci.yml` and
   `software-mode.yml` use);
2. builds `emu` and `simee` in release, builds or restores QEMU 11.1 and the API 36 x86_64 image
   (the same pinned versions and cache keys as `software-mode.yml`), and fetches the renderer, so the
   first cold boot doesn't include a download;
3. waits up to two minutes for the 1-minute load to fall under half the CPUs (the build just loaded
   the runner), then runs `scripts/bench/linux-kvm.sh --rounds 5 --port 5700 --no-quiet-gate`;
4. appends the table and the runner's profile to the job summary and uploads `bench/results/` as the
   `bench-linux-kvm` artifact.

## What the script measures

One new AVD per round (`emu -create-avd -no-golden`: 4 vCPUs, 2 GB, 720x1280@320, console port 5700
and up), every boot with `-accel on`, so a run that couldn't get KVM fails instead of reporting a
TCG number. Each round:

| Measure | How |
| --- | --- |
| cold boot, s | the AVD's first boot: launch until adb lists the device, and until `sys.boot_completed` (`lib.sh`'s `android_boot`, polled every 0.25 s) |
| warm boot, s | a second boot of the same AVD, the same two times |
| peak RSS, MB | the highest RSS of emu and its children seen while booting, for the cold and the warm boot |
| memory, MB | after `--settle` (60 s) on the warm boot: RSS (`ps`) and PSS (`/proc/<pid>/smaps_rollup`) of emu and everything below it. PSS splits shared pages between the processes sharing them, so it is the one to compare across tools; RSS counts the guest RAM mapping and the renderer's libraries in full |
| idle host CPU, % of one core | user + system CPU of the same tree over the next `--window` (60 s), divided by the wall time from `/proc/uptime` |
| screenshot, ms | `--shots` (30) calls of `simee screenshot` (one process per call, through the display socket), p50 and p95, after one uncounted call; and the same for `adb exec-out screencap -p` |
| frame pacing | `scripts/android-jank.sh`: 10 launcher app-drawer swipes up and down, then the launcher's `dumpsys gfxinfo`: janky frames %, frame time p50/p90/p99, frames rendered |

`--quick` is a smoke run (one round, 10 s settle and window, 5 shots). `--dry-run` prints the plan
and exits before building or booting anything.

Frame pacing is there because emu exposes it on Linux the way it does on a Mac: `android-jank.sh`
only needs adb and the guest's `gfxinfo`. The frames are the guest's (software-rendered unless the
renderer finds a GPU, which a hosted runner doesn't have), so read it as the guest's own pacing on
the runner's CPU, not as the host's display path.

## The runner is a shared VM

A hosted runner is a Hyper-V guest on shared hardware, and KVM inside it is nested virtualization.
So:

- **Before measuring**, the script records the runner's profile: `lscpu`'s model, architecture,
  threads per core and hypervisor vendor, the CPU count, memory total and available, whether
  `/dev/kvm` is present, the kernel, the runner image version, the load averages and the five
  busiest processes. It is merged into the result JSON under `runner`, with the link to the run.
- **The quiet-host gate is off** (`--no-quiet-gate`): a runner is never idle, and the gate (from
  [#450](https://github.com/kaeawc/simee/pull/450), `docs/research/bench-protocol.md`) would refuse
  every run. The load is recorded instead: each sample carries the 1-minute load at its time, and
  `host_start` and `host_end` carry the load before and after. A run is published as "GitHub
  runner", never mixed into a quiet-host row, and never compared with the Mac's.
- **Five rounds, spread recorded.** Every measure has its five samples in the JSON and the table
  shows the median with the samples. `summarize.py` records p50, p95 and IQR per measure where
  #450's version is merged. Publish a median only when its spread is within the noise budget there
  (IQR over p50 of 0.10 for times, 0.25 for CPU), and say which runner CPU model it came from:
  GitHub's pool mixes CPU generations from run to run, so two runs differ for that reason alone.
- **Nested KVM costs exits.** Boot times on a runner are slower than on bare metal with the same
  CPU; compare runs of this workflow with each other, and the stock emulator only when it ran on the
  same runner in the same job (`software-mode.yml`'s KVM leg does that for boot).

## Reading the output

- **`bench-linux-kvm` artifact**: `linux-kvm-<time>.json` is the result: `medians` per measure,
  `samples` (round, measure, value, load), `host_start`/`host_end`, and `runner`. `linux-kvm-summary.md`
  is the table the job summary shows. With #450 merged, a `.host.json` sits beside the JSON.
- **A `failed` sample** is a boot that never reached `sys.boot_completed` (600 s) or a screenshot
  loop where every call failed. It stays in the table as a word and out of the median, so a median
  of fewer than five values says how many rounds survived: check the samples before quoting it.
- **The log lines**: the script prints `no 'guest CPU on KVM' line in the log` when a boot ran
  without it (the `-accel on` boot would have failed, so this means emu's log format changed).
- **Idle CPU** counts the whole tree, QEMU included. The runner's own agent isn't in it.
- **PSS** is the memory number to publish; RSS is there to compare with the rows `docs/benchmarks.md`
  already has, which are RSS.

## Choices and sources

- **Why `ubuntu-latest` x86_64.** GitHub's hosted Linux runners expose `/dev/kvm` as `root:kvm 0660`;
  a udev rule makes it usable by the runner user, which is what the Android emulator action
  documents ([android-emulator-runner](https://github.com/ReactiveCircus/android-emulator-runner),
  and GitHub's 2 April 2024 changelog extending KVM to the standard runners, which this page
  knows second-hand: check it before citing it). The arm64 runners (`ubuntu-24.04-arm`) have no
  `/dev/kvm` (probed in [#56](https://github.com/kaeawc/simee/issues/56), `docs/android.md`), so this
  is x86_64 only.
- **Why reuse `lib.sh`.** Boots, PID tracking (SIGINT first, only PIDs the script started), AVD
  creation and deletion, port checks (`bench_refuse_protected_port`) and the JSON and table are
  already there and already used by `android-boot.sh` and `android-frames.sh`.
- **Why not the stock emulator here.** The stock comparison on KVM already runs in
  `software-mode.yml`'s KVM leg (`android-boot.sh`, pinned release and SHA-256). Adding it to this
  workflow doubles the run for boot rows that exist; the rest of #453's list (a `bench-data` branch
  with history, a regression issue, `docs/benchmarks.md`'s section, a nightly schedule) is left for
  after the first runs show how much the runner's spread allows.
- **Cold means a new AVD.** `-no-golden` keeps `emu -create-avd` from starting the AVD with a settled
  userdata, so the cold boot formats and encrypts `/data` as a user's first boot does.

## Verification

On a host without `/dev/kvm`: `shellcheck` and `shfmt` over `scripts/bench/linux-kvm.sh`,
`scripts/bench/linux-kvm.sh --dry-run` (the plan, the port refusals), the runner-profile and summary
steps run on their own, and the percentile helper against `seq`. The boots, memory, CPU, screenshot
and frame-pacing steps run only in the workflow, and have not run yet.
