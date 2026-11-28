# Cloud agent VMs as simee hosts (no KVM, no GPU)

Research for [#454](https://github.com/kaeawc/simee/issues/454), part of the benchmark epic
[#456](https://github.com/kaeawc/simee/issues/456): agents run simee inside cloud VMs (Hive workers,
Claude cloud sessions). Such a VM has no `/dev/kvm` and no GPU, so Android runs in software mode
(QEMU's TCG with emu's devices and a software Vulkan renderer). This page says what one of those
hosts is, what simee measured there, how noisy it was, and what it is good for.

Two scripts produce everything below; nothing here was typed from memory.

- `scripts/bench/host-profile.sh` writes the host fingerprint as one JSON object. It records the
  fields of the host record in `scripts/bench/summarize.py` (`model`, `cpu`, `os`, `os_version`,
  `cpus`, `cores_physical`, `mem_gb`, `kvm`, `load1`, `thermal`, `power`, `simee_commit`, and the
  run record's `chip` and `cores`), so a profile reads like any benchmark's host, and adds the
  kernel, CPU flags, disk (size, free, filesystem, device, the kernel's rotational flag), whether
  a VM can actually be made on `/dev/kvm`, render nodes and Vulkan ICDs (`vulkaninfo --summary` when
  installed), the cgroup CPU and memory limits (the tightest of the process's cgroup and its
  parents, v2 and v1), steal from `/proc/stat` (since boot and over a window), the container
  backend's prerequisites (#210) and the CI the host is. What a host can't report is `null`. It
  reads only, and takes `thermal` and `power` from `scripts/bench/quiet.sh` when that exists
  ([#468](https://github.com/kaeawc/simee/pull/468)).
- `scripts/bench/soft-host.sh` builds, boots and measures: see its header. It waits for the
  quiet-host gate before the build and before every round of boots, and exits 4 when the host stays
  busy (`BENCH_QUIET_WAIT`, 300 s). Without #468's `lib.sh` it falls back to one check, a 1-minute
  load below 0.5 per CPU; `--no-quiet-gate` runs anyway with a warning, for numbers that are not
  published. Tested here by loading all 4 CPUs (1-minute load 7.7): it refuses with exit 4 and the
  message. The host profile is written at the start and the end of every run beside the results.

## The host measured: a Hive worker VM

One VM, 2026-11-28, simee at 9f53227. Nothing else ran on it while the benchmark did (the agent
that started the runs waited for them).

| | |
| --- | --- |
| VM | Firecracker guest on KVM (`systemd-detect-virt`: `kvm`), Ubuntu 22.04, kernel 6.1.189 |
| CPU | 4 vCPUs of an Intel Xeon at 2.4 GHz (`hypervisor`, `sse4_2`, `avx2`, `avx512f`; no `vmx`/`svm`: no nesting) |
| Memory | 3.8 GiB, no swap; the cgroup's memory limit is 3.47 GiB (an ancestor's `memory.max`; the session's own is `max`), no CPU quota |
| Disk | 10.5 GB virtio-blk `vda`, ext4, 4.2 GB free at the start; the kernel reports `rotational=1` for it, so don't read the flag as a spinning disk |
| `/dev/kvm` | absent |
| GPU | no render node, no PCI GPU, no Vulkan ICD directory, no `vulkaninfo`: software rendering only (the SDK renderer brings SwiftShader) |
| Container backend (#210) | cgroup v2, overlayfs and user namespaces present; no binderfs and no `/dev/binder*` |
| Steal | 0.01% of CPU time over the whole benchmark; 0.0% in every 2 to 5 s window |
| Network | the system image (1.7 GB on disk) and platform-tools came down in 82 s, with no credentials |

The same fingerprint of a GitHub-hosted `ubuntu-latest` runner and of a Claude cloud session is not
here: neither was reachable from this session (see Not measured).

Compared with the host `docs/research/software-mode-profile.md` profiled the day before (also a
Firecracker VM, 4 vCPUs at 2.2 GHz, but 7.8 GiB and a 9.8 GB disk): half the memory. That
matters below.

## Results

Dependencies were fetched first and are not timed. The commands and the order of the runs are in
"Method". Values are the median of 3 rounds with the rounds in parentheses, unless a row says
"one run".

| Measure | Result | Rounds |
| --- | --- | --- |
| Cold `cargo build --release -p simee-emu`, wall | 46.1 s | 38.4, 48.5, 46.1 |
| The same, CPU time | 167.5 s | 138.7, 177.1, 167.5 |
| Then `-p simee` (the CLI) on top, wall | 122.3 s | 87.4, 122.3, 138.2 |
| Rebuild of `emu` after touching `apps/emu/src/main.rs` | 1.1 s | 1.1, 1.0, 2.1 |
| QEMU 11.1.3 from its tarball (`make -j4`), wall / CPU | 596.8 s / 2090 s | one run |
| System image and platform-tools download | 82.1 s | one run |
| Cold boot (new userdata) to `sys.boot_completed` | 969 s | 907, 1178, 969 |
| Warm boot, launch until adb lists the device | 117.6 s | 100.0, 117.6, 146.3 |
| Warm boot to `sys.boot_completed` | 374.5 s | 289.0, 374.5, 717.9 |
| Peak RSS of emu and QEMU, cold / warm boot | 2310 MB / 2175 MB | 2350, 2310, 2282 / 2175, 2154, 2242 |
| Host CPU 60 to 120 s after the warm boot | 3.79 cores | 3.79, 3.85, 2.05 |
| Host CPU 240 to 300 s after the warm boot | 3.85 cores | 3.87, 3.85, 3.84 |
| RSS 300 s after the warm boot | 2351 MB | 2397, 2351, 2232 |
| `simee` tap (the API's 60 ms hold) | 60.9 ms | 60.9, 60.9, 61.0 |
| `simee` screenshot, base64 in the reply / to a file | 35.4 ms / 21.8 ms | 35.4, 44.2, 22.4 / 21.8, 33.7, 18.3 |
| `simee` tap then screenshot (one automation step) | 89.4 ms | 92.2, 89.4, 86.1 |
| `adb exec-out screencap -p` | 9.9 s | 11.7, 9.9, 9.3 |
| Steal during the warm boots, and over the whole run | 0.01% | |

The guest was a 720x1280 phone, API 36 `default` x86_64, 4 vCPUs and 1536 MB, booted with
`emu -avd -accel off -no-window -no-audio`. The 20-call medians come from
`scripts/ios-step-bench.py` over one `simee serve` connection.

### How noisy it was

- **Steal says nothing.** It was 0.01% throughout, yet the warm boot took 289 to 718 s and the cold
  boot 907 to 1178 s. A Firecracker guest sees none of the contention on the machine below it, so
  steal can show a noisy neighbour and cannot rule one out. The spread is real and its cause is
  not measured.
- **Builds vary by 26% in wall time and 28% in CPU time** (38.4 to 48.5 s wall, 138.7 to 177.1 s
  CPU). CPU time is the figure that load inside the VM doesn't inflate, so the CPU itself ran at
  different speeds from one build to the next. The `simee` CLI on top grew from 87 to 138 s over the
  three rounds, a trend a quiet host shouldn't show.
- **Boots are the noisiest row**: the slowest warm boot is 2.5x the fastest. Three rounds do not
  pin a boot time here. Treat 374 s as "between 5 and 12 minutes".
- **The gate passed with a load of 1.66** at the start of the boot rounds (the limit is 2.0 for 4
  CPUs), because the renderer's unpack had just run. The boots' own load is 4 by construction
  (QEMU's translator threads fill the CPUs), so the gate only judges the host before a round.
- **Memory was tight**: during the second cold boot, `free -m` showed 881 MB available of 3.9 GB
  with 1.5 GB of it the guest's RAM (shared memory), and no swap. The research host with twice the
  memory booted warm in 172.6 s and idled at 0.58 cores
  (`docs/research/software-mode-profile.md`, section 2); here it was 374.5 s and 3.85. The image's
  1.9 GB `system.img` is read through the page cache, so pressure on it is a candidate cause of
  both the slowdown and the spread. It was not tested, for example with a larger VM or a smaller
  guest, so it stays a candidate.
- **Idle never came.** 3.85 cores 4 to 5 minutes after boot means the guest was still busy in
  every round (the first round's 60 to 120 s window sat at 3.79 and the third's at 2.05); the
  published software-mode idle is 0.58 to 0.75 cores. A 300 s window did not reach it, so the
  idle row is "not reached within 5 minutes", not a measured idle.

## What simee on such a host is good for

- **Build, unit and integration CI: yes.** A cold release build of `emu` is 46 s, and the rebuild after a
  source change 1 s, on 4 vCPUs. The unit tests need no device (AGENTS.md). Everything that doesn't boot a
  guest, including the API, CLI and MCP servers, runs here.
- **Provisioning: yes.** Images and platform-tools download without credentials in about a minute
  and a half; QEMU 11.1 has to be built from source on Ubuntu 22.04 (10 minutes here, 4 GB of disk
  while building), because the distribution's 6.2 is too old.
- **A booted device for scripted checks: possible, slowly.** A warm boot is 5 to 12 minutes and a
  cold one 15 to 20. Once up, simee's own calls are fast (screenshot 22 to 35 ms, tap 61 ms), because
  they talk to emu's display socket and not to the slow guest; `adb screencap`, which runs inside
  the guest, takes about 10 s. So a job that boots once and drives the device through simee works;
  one that boots per test does not.
- **Interactive devices: no.** The guest keeps all 4 CPUs busy after boot, leaving nothing for the
  agent's own work on a 4-vCPU VM, and the launcher draws at about 7 frames a second on the
  4-vCPU host `docs/benchmarks.md` measured (not re-measured here). Without KVM this is a CI host, not a desktop.
- **Disk and memory are the limits of this VM.** 10.5 GB is not enough for the workspace's release
  build, QEMU's build and the image together: the run needed two invocations (below). With 3.8 GiB
  the guest competes with the page cache.

## Method

Everything ran on the one VM above, in this order, with `scripts/bench/soft-host.sh`:

1. `--build-qemu --rounds 3`: the build rounds. `--build-qemu` needed more free disk than the VM
   had after them (the target dir is 854 MB; the Rust toolchain and registry took 1.8 GB), so
   it was skipped and recorded as a note.
2. The toolchain was removed and the run repeated as `--build-qemu --skip-build --emu <the binary
   of step 1> --simee <likewise>`: QEMU built (it first failed for a missing `python3-tomli`, which
   Python 3.10 needs and the script's error now names), the image downloaded, and the boots then
   skipped for lack of the 3 GB the script then asked for.
3. The AVD's disk guard was lowered to 2 GB (the boots fit) and the
   run repeated as `--skip-build --emu ... --simee ... --rounds 3`: the boots, idle windows and
   screenshots above. QEMU and the image came from the cache of step 2.

A host with 20 GB free does all of it in one invocation. The build rounds of step 1 and the boot
rounds of step 3 therefore ran in separate processes, each behind the quiet gate, and the
build and boot numbers are not from one session. The run that produced the boots took 1 h 43 min:
`soft-host-20261128-132909.profile.json` and `-151228.json` hold its profile and samples, the
profile at the end shows the same steal and 2.8 GB of free disk.

## Not measured

- **Claude cloud sessions and GitHub-hosted runners.** Neither was reachable from this session.
  The scripts are written to run on both (they read standard `/proc` and `/sys` paths and need
  only `python3`, `perl` and `curl`; `soft-host.sh` still boots with `-accel off` on a runner with
  KVM), but they have not run on a hosted runner or in a Claude session, so no number or
  fingerprint is claimed for either. Run `scripts/bench/host-profile.sh` and
  `scripts/bench/soft-host.sh --build-qemu` there and add a column.
- **The software-lite profile** ([#385](https://github.com/kaeawc/simee/pull/385)): not merged.
- **`stream_start` through OpenH264 on this host** (#390, #263) and **the container backend**
  (#210): no binderfs here, so the backend can't run; the encoder was not exercised.
- **A larger VM**, to test whether memory explains the boot times, and **more rounds**: three do not
  pin a boot that varies 2.5x. The warm-boot row should be re-run with 5 or more rounds.
- **How long a session may run.** The runs lasted 1 h 43 min without being stopped; the limit is
  unknown.
- Nothing here is in `docs/benchmarks.md`: it is owned by another lane and the numbers are one VM's.
