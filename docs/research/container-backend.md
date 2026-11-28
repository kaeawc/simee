# A container backend for Android on Linux (Redroid-style)

Research for [#210](https://github.com/kaeawc/simee/issues/210): run Android's userspace in a
container on the host's kernel instead of translating a whole guest under QEMU's TCG, on Linux
hosts that have binder and no usable KVM. This is a research and feasibility pass with no product
code. Every claim carries one tag:

- **[verified]**: run or read on this Hive VM today (2026-11-27), command given.
- **[cited]**: measured by someone else on another host. Quoted from a comment on #210 or from
  the linked source, not re-run here. A number tagged this way is never comparable one-to-one with a
  number measured elsewhere; the text says so where it matters.
- **[inferred]**: reasoned from the above and from reading the code, not run. These are the
  spike's questions (section 7).

## Status after the research (2026-11-28)

Owner decisions of 2026-11-27 settle what the recommendation below left open. The container backend
is pursued, and automatic selection is **KVM first, then the container (only on a host without
KVM that has binder), then TCG**, with `-backend kvm|container|tcg` to override; that replaces the
"behind an explicit opt-in" in section 8 and the caution about silent swaps in section 7. The host
side is built in `crates/emu-container` (probe, selection, pinned image, launcher; see
[android.md](../android.md#container-backend-linux-with-binder-and-no-kvm)). Facts it settled, from
reading the sources on 2026-11-28:

- **What Redroid's init expects from a runtime** ([vendor_redroid `redroid.common.rc`](https://github.com/remote-android/vendor_redroid/blob/master/redroid.common.rc)):
  `/dev/null`, `zero`, `full`, `random`, `urandom`, `tty`, `ptmx` and `pts/ptmx` already exist (it
  only `chmod`s them), and so may `/dev/ashmem`, `/dev/ion` and `/dev/dma_heap/system`; it mounts
  debugfs and binds over `/proc/vmallocinfo` and `/sys/power/state` itself. The launcher builds
  that `/dev`.
- **What the image runs**: the pinned image's config says `Entrypoint: ["/init", "qemu=1",
  "androidboot.hardware=redroid"]`, and arguments after the image name are `/init`'s, which
  [redroid-doc](https://github.com/remote-android/redroid-doc) turns into properties
  (`androidboot.redroid_width|height|dpi|fps|gpu_mode`, `androidboot.use_memfd`, `ro.xxx` overrides).
- **The pinned image**: one layer per architecture, 730 MB (amd64) and 694 MB (arm64) compressed;
  the manifest and layer digests are in `crates/emu-container/src/image.rs`.
- **Rootless works as far as a host without binder allows**: on this VM, as an unprivileged user
  with `newuidmap` and a 65,536-id range, a user namespace mounts the overlay, a `/dev` tmpfs,
  devpts, `/proc` and the host's `/sys` (read-only) and pivots into the result; as root, with the
  container mapped to a range of its own, likewise. Only binderfs fails, as section 1 predicts. Whether Redroid's `/init` then boots, and in
  how long, is still the spike.
- **Open**: the adb port (no network namespace yet, so the container's adbd takes the host's 5555),
  display, input and console, cgroup limits, and the numbers for `docs/benchmarks.md`.

## Summary

1. **This Hive VM can't run Redroid, and can't be made to without a different kernel.** The kernel
   has `# CONFIG_ANDROID_BINDER_IPC is not set`, `# CONFIG_MODULES is not set` (so no
   `binder_linux` to load), no DMA-BUF heaps and no `/dev/kvm`. Namespaces, overlayfs, memfd, IPv6
   and cgroup v2 are all there; binder is the single hard blocker. [verified]
2. **A binder host exists and it boots Android in seconds.** On a Lima VM on an M3 Max (Ubuntu
   24.04 arm64, 6 vCPU, nested virtualization), Redroid 15 booted to `sys.boot_completed` in 7.7 s
   warm (median of 3), settled at 0.004 container cores and 0.83 GiB. [cited] That is a different
   machine and architecture from every software-mode number below; the order of magnitude (seconds
   against minutes) is the finding, not the ratio.
3. **The win is real only where binder is present and KVM is not.** With KVM, emu's QEMU backend
   already boots in about 17 s warm on GitHub's runner with the full SDK image and console
   ([benchmarks](../benchmarks.md#android-software-mode-on-kvm-githubs-x86_64-runner)); a container
   saves seconds there and gives up fidelity. Without binder it can't run at all. The hosts in
   between (arm64 Linux with no KVM, cloud VMs without nested virtualization, Kubernetes nodes) are
   the ones that gain; Hive's workers gain only if their operator rebuilds the guest kernel.
4. **Recommendation: build, as a third backend behind an opt-in, in the order KVM, container,
   TCG, after a spike on a binder host.** The spike's first deliverable needs no emu code
   (section 8).

## 1. This host

All commands run as root on the VM that executes this session.

| | |
| --- | --- |
| Host | Firecracker microVM, Ubuntu 22.04, kernel 6.1.189 (`#1 SMP PREEMPT_DYNAMIC`, built on `firecracker-host`) [verified: `uname -a`, `/proc/version`] |
| CPU / memory | 4 vCPUs of an Intel Xeon at 2.2 GHz with the `hypervisor` flag and no `vmx`; 3.9 GiB RAM, no swap [verified: `/proc/cpuinfo`, `free -m`] |
| Kernel command line | `console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw … virtio_mmio.device=…` (Firecracker's) [verified: `dmesg`] |
| Container runtime | none: `docker`, `podman`, `runc`, `crun`, `nerdctl`, `ctr`, `lxc` are all absent [verified: `which`] |

### Kernel features Redroid asks for

The list is Redroid's: binderfs, ashmem or memfd, IPv6, ION or DMA-BUF heaps, 4 KB pages
([redroid-doc, deploy](https://github.com/remote-android/redroid-doc/blob/master/deploy/README.md))
[cited].

| Feature | Here | How checked |
| --- | --- | --- |
| `CONFIG_ANDROID_BINDER_IPC` / `BINDERFS` | **off** (`# CONFIG_ANDROID_BINDER_IPC is not set`, the Android menu holds nothing else) | `zcat /proc/config.gz` [verified] |
| binderfs mountable | **no**: `mount -t binder binder /tmp/bfs` fails with `unknown filesystem type 'binder'`; `binder` is not in `/proc/filesystems`; no `/dev/binder*` | run [verified] |
| Loading binder as a module | **impossible**: `# CONFIG_MODULES is not set`, no `/lib/modules`, no `/proc/modules`, no `modprobe` | `/proc/config.gz`, `ls` [verified] |
| ashmem | absent (no `/dev/ashmem`, not in `/proc/misc`); fine, it left the kernel in 5.18 and Redroid uses memfd | [verified] |
| memfd | works: `os.memfd_create` returns a descriptor; `CONFIG_MEMFD_CREATE=y` | run [verified] |
| DMA-BUF heaps / ION | **off** (`# CONFIG_DMABUF_HEAPS is not set`; no `/dev/dma_heap`, no `/dev/ion`) | [verified] |
| `/dev/uinput` | **off** (`# CONFIG_INPUT_UINPUT is not set`); whether Redroid needs it is a spike question | [verified] that it is off |
| IPv6, overlayfs, cgroup v2, seccomp, SysV IPC | on (`CONFIG_IPV6=y`, `overlay` in `/proc/filesystems`, `cgroup2` at `/sys/fs/cgroup`) | [verified] |
| User, PID, net namespaces | work unprivileged and as root: `unshare -Urm` mounts an overlayfs, `unshare -Urmpf --mount-proc` gives pid 1, `unshare -Urn` gives a loopback-only netns | run [verified] |
| `/dev/kvm` | absent; `vmx` flag absent, no `kvm` module in `/sys/module` | [verified] |
| GPU / render node | no `/dev/dri` | [verified] |

**So the answer to "can binderfs be mounted here" is no, and not for a permissions reason.** The
driver is not in the kernel image and the kernel can't load modules; being root with user
namespaces changes nothing. Docker or podman would not help either, since the container shares
this kernel. Getting binder here means a different `vmlinux` from whoever operates the Firecracker
host. The kernel config lines an operator needs are in the environment matrix comment on #210
(binder IPC, binderfs and the three device names, DMA-BUF heaps and its system heap, plus memfd,
cgroups and namespaces, which are on); from section 1 this host also lacks the heaps and uinput.

### What can be fetched

Public images pull without credentials: an anonymous token from `auth.docker.io` for
`repository:redroid/redroid:pull` gets the tag list and manifests [verified: `curl`]. The tags that
matter, with the compressed size of the single layer of each architecture [verified]:

| Tag | amd64 | arm64 |
| --- | --- | --- |
| `14.0.0-240527` | 873 MB | 800 MB |
| `15.0.0-250627` | 883 MB | 821 MB |
| `16.0.0-250627` | 955 MB | 880 MB |

(`_64only` variants and `-latest` aliases exist for each.) The images were last pushed June 2025;
there is no Android 16 QPR image and none newer. A pull is a one-time 0.9 GB download, comparable
to the 1.9 GB system image software mode already provisions.

## 2. Boot, idle CPU and memory: not run here

Section 1 closes this off: no binder, so no Redroid, so no boot to time on this VM. The brief's
second question therefore has no first-hand numbers. What exists:

**Redroid, [cited]** from the Lima spike on #210 (Apple M3 Max host, Lima VZ guest with nested
virtualization, Ubuntu 24.04 arm64, kernel 6.8.0-147, 6 vCPU, 8 GiB; shared, busy host; Docker 29.1.3;
`--privileged`, 720x1280, `redroid_gpu_mode=guest`, `use_memfd=true`):

| Measure | Redroid 15 arm64 `_64only-250627` |
| --- | --- |
| `docker run` to `sys.boot_completed=1`, warm `/data` | 7.67 s median (7.67, 7.44, 7.70) |
| same, fresh `/data` | 15.4 s median (15.38, 16.74, 7.95; the runs cluster at about 8 s and about 16 s, cause not found) |
| Memory at boot / after Settings and swipes | `memory.peak` 750-840 MiB / 969 MiB; `memory.current` 833 MiB two minutes in (484 MiB anon) |
| Settled CPU, 120 s after boot | 0.004 container cores; the VZ process 0.022 cores |
| `screencap -p` over adb | 0.08 s from the host; 328 ms for the first capture in the guest |
| Frame time while swiping Settings (20 swipes, `gfxinfo`) | p50 65 ms, p90 73, p99 200 (about 15 fps), RenderEngine on SwiftShader |
| Redroid 16 arm64 | does **not** boot in `gpu_mode=guest`: SurfaceFlinger aborts every 5 s with `output buffer not gpu writeable` (37 tombstones in 160 s) |

A second report on #210 says Redroid 12 on GitHub's x86_64 runner (6.17-azure) showed adb about 9 s
after start [cited]. The matrix also says 13.x never finished booting and 14.x locked up on some hosts
in one CI action's reports [cited]; 15 is the version with a recorded clean boot.

**Software mode, [verified in the repository, same VM class]**: `docs/research/software-mode-profile.md`
measured this VM class (Firecracker, kernel 6.1.189, 4 vCPU Xeon at 2.2 GHz, no KVM) on 2026-11-27:
warm boot to `sys.boot_completed` 172.6 s median (179.3, 161.0, 172.6), idle 0.58 cores 180-300 s
after boot, swipe frame time p50 150 ms, 1.5 GiB guest; QEMU's peak RSS is about 3 GB for a 2 GB guest
([android.md](../android.md#software-mode-linux-without-kvm)). **On KVM**, GitHub's x86_64 runner (4
vCPU Xeon Platinum 8573C): warm 16.6 s, 1858 MB peak RSS
([benchmarks](../benchmarks.md#android-software-mode-on-kvm-githubs-x86_64-runner)).

**What the three give side by side.** They are three machines (an M3 Max VM, a 2.2 GHz Xeon VM, a
cloud Xeon), two instruction sets, and different Android builds (AOSP 15 userdebug against the SDK's
API 36 `default` image). Read them as magnitudes: a container boots in seconds, KVM in tens of seconds,
TCG in minutes; a container's idle cost is a few hundredths of a core, TCG's about 0.6; resident memory
is about 0.8-1.0 GiB against 2-3 GB. None of these may go in `docs/benchmarks.md` or the README until
section 8's first deliverable measures them on one host.

## 3. How a container device would present itself to simee

Everything a front end uses is already a socket or a port, which is the reason this can be a
backend and not a rewrite ([architecture](../architecture.md)):

| Surface front ends use | Who serves it today | In a container |
| --- | --- | --- |
| `<avd>/simee/pid_<pid>.ini` advertisement (console port, adb port) | `apps/emu` (`console::Advertisement`) | same, from a new `container.rs` next to `software.rs`. [inferred] |
| adb serial `emulator-<console port>`, adb on console port + 1 | emu's adb bridge to the guest's vsock `adbd` (`AdbService::announce_port`) | Redroid's `adbd` listens on TCP 5555 in the container ([redroid-doc](https://github.com/remote-android/redroid-doc): `adb connect localhost:5555`); publishing that on the host's console + 1 should make the adb server register it as `emulator-<port>`, since that is how it finds stock emulators. [inferred; spike question 1] |
| Console on `console_port` (stock text protocol, auth token) | `apps/emu/src/console.rs` `command()`: `event`, `sensor`, `geo`, `power`, `sms`, `gsm`, `network`, `rotation`, `touch`, `screenshot`, `avd` … | the same server; each command is backed by `adb shell` or answers honestly (table below). [inferred] |
| `display.sock` (frames, ring, touch, key, wheel) | `display_server.rs` over `emu_vmm::display::FrameStore` and `emu_vmm::Inputs` | the same server and protocol; a different producer of frames and consumer of input. `FrameStore` and `sim_core::stream` build on Linux already; `ConsoleInfo.inputs` and `.sensors` are VMM types, so the spike has to put a trait there. [inferred, from reading `console.rs` and `display_server.rs`] |
| An AVD directory (`config.ini`, userdata) | `emu-avd` | an AVD whose machine is `container`: `config.ini` still gives screen, density, RAM and cores (mapped to `redroid_width/height/dpi`, `--memory`, `--cpus`); `/data` is a bind-mounted directory in `<avd>/simee/`, not `userdata-qemu.img`; the "system image" is an OCI image pulled by a new `emu -provision-container`. [inferred] |

### Display

`FrameStore` wants RGBA frames and a per-frame signal. Four ways to get them out of Redroid, cheapest
to build first:

| Option | Rate / cost | Needs | Notes |
| --- | --- | --- | --- |
| A. `adb exec-out screencap` (raw, not `-p`) polled | 0.08 s a capture over a forward [cited]: about 10 fps at best, and a SurfaceFlinger readback each time | nothing in the guest | Enough for screenshots (the API's `screenshot` is one capture) and the AutoMobile harness, whose tools take screenshots rather than stream. First thing to build. [inferred] |
| B. scrcpy's server (a jar `adb push`ed and run; H.264 over an adb forward, control over a second socket) | the encoder's rate (Redroid has no hardware encoder, so software `c2.android.avc.encoder`, CPU in the container) | a decoder on the host (`sim-encode` has an OpenH264 *encoder* behind a feature and a VideoToolbox decoder on macOS only; Linux needs a decode dependency, `default-features = false`) | Redroid's own docs use scrcpy as the viewer ([redroid-doc](https://github.com/remote-android/redroid-doc/blob/master/deploy/README.md)), so the path is known to work. The H.264 could go to `simee stream`'s WHIP publisher without a decode, since it publishes H.264 already. [inferred] |
| C. `screenrecord --output-format=h264 -` | same encoder as B, no control channel | a decoder as B | Simpler than B, weaker: older Android caps a run at three minutes. [inferred] |
| D. A virtual display the container renders into a shared buffer (what Cuttlefish does with its own composer) | no encode, no readback: the lowest cost | a patched Redroid image or composer HAL | Later; it means maintaining our own image. [inferred] |

### Input

`input tap` starts an `app_process` per call: `am start -W` for Settings took 296 ms on the Lima host
[cited], so a swipe built from `input swipe` is a single slow gesture and a drag (`touch down/move/up`
from the desktop) can't be built from it. B's control socket injects events without a process per
event and fits `touch`/`key`/`wheel` on `display.sock`; keys and text map to `input keyevent` and
`input text`. Writing to a uinput device directly would be cheaper still, but `/dev/uinput` is host
kernel state: it is off here, and whether Redroid's input stack uses it is unknown. [inferred]

### What a container does not have

Redroid's own HAL set is mock sensors, default GNSS, `rild` with no modem, remote-submix audio and
software keymaster, with no camera HAL and no emulator console (from the survey on #210, redroid-doc
README silent) [cited]. Honest results for each simee Android method, so none of them pretends:

| Capability (API method) | In a container |
| --- | --- |
| Boot, shutdown, `exec`, `install`, `launch`, `terminate`, `apps`, `push`, `logs`, `screenshot`, `tap`, `swipe`, `key`, `type`, `open_url`, `button` | **Works** through adb (and the display socket for touch). [inferred] |
| `rotate`, `appearance`, `content_size`, `backlight`, `status_bar` | **Works** through `settings put`, `wm` and `cmd` (the same commands the SDK emulator's guest runs). [inferred] |
| `network_shape` / `network speed|delay` | **Partly**: `tc` on the container's veth gives latency and rate; per-app or DNS shaping is not what the console's presets do. [inferred] |
| Battery (`power`), `location` | **Partly**: `dumpsys battery set …` and `cmd location providers add-test-provider` cover levels and a test fix, not charger events or GNSS satellites. [inferred] |
| Sensors (`sensor`, rotation by accelerometer, fold, hinge) | **No**: the sensors HAL is a mock with no injection channel; a rotation request is a `wm` call, not gravity. Say `Unsupported`. [inferred from the survey] |
| Telephony (`sms`, `gsm`, calls) | **No**: `rild` has no modem. `Unsupported`. [inferred from the survey] |
| Camera (`camera_*`) | **No**: no camera HAL. [cited] |
| Biometrics (`biometric`, `finger`) | **Unknown**: depends on the image's fingerprint HAL; spike. [inferred] |
| Bluetooth, UWB, Wi-Fi scan, audio capture and injection | **No** (as on emu's software mode today for Bluetooth peers and audio). [inferred] |
| Snapshots (`avd snapshot`), `clone`, `export` | **Snapshots no** (emu has none either); **clone yes and cheap**: `/data` is a directory, so a copy-on-write clone (reflink or an overlayfs upper) takes no copy of the image. CRIU checkpoint is not promised: binder, SurfaceFlinger's descriptors and memfd state are beyond what CRIU restores. [inferred] |
| Frame-pacing (`scripts/android-jank.sh`) | **Not representative**: SwiftShader on the container's cores, about 15 fps in the one measurement [cited]. |

AutoMobile's harness runs on adb, so most of its tools should run; the console-bound ones (sensors,
SMS, calls, biometrics, network conditions, video recording over a software encoder) are the gaps to
record, tool by tool, in the spike. [inferred]

## 4. GPU

- **`guest` mode (SwiftShader for GLES and Vulkan)** is the only mode on hosts without a render node:
  this VM has no `/dev/dri`, nor do Firecracker workers or most CI VMs. It is also the mode with the
  known crash: Redroid 16 arm64 aborts in `SkiaRenderEngine` [cited]. The x86_64 image has not been
  tried; a Redroid 15 baseline is the safe one. [inferred]
- **`host` mode** uses Mesa on the host's DRM render node; it needs the node and DMA-BUF heaps passed
  into the container. Useful on a GPU workstation, irrelevant to the "no KVM, no GPU" motivation.
- **virgl and gfxstream do not apply as they do for a VM.** Both are virtio-gpu and goldfish-pipe
  paths: a guest kernel driver talks to a host renderer. A container has the host's kernel, which
  has neither driver. emu's renderer (`emu-gfxstream`) can't be reused unless the host kernel gains
  `goldfish_pipe`. [inferred]
- **The frame-rate comparison** that matters (container SwiftShader, 15 fps [cited], against
  software mode's 7 fps and p50 150 ms [verified in the repository]) is across different machines and
  must be re-measured on one.

## 5. Security

- Every official Redroid example uses `docker run --privileged`, and the docs say to disable SELinux
  on the host if present ([redroid-doc](https://github.com/remote-android/redroid-doc/blob/master/deploy/README.md)).
  There is no published minimal capability set; Redroid's init mounts binderfs, debugfs and overlayfs,
  so at least `CAP_SYS_ADMIN`, an unconfined seccomp profile and the device nodes are needed
  [cited; the rest inferred]. A privileged container is root on the host's kernel: acceptable on an
  ephemeral CI VM that exists for one job, **not** acceptable on a shared workstation or a multi-tenant
  worker running someone else's APKs.
- Redroid also warns not to expose the adb port on a public network
  ([redroid-doc](https://github.com/remote-android/redroid-doc)); emu would publish adb and the
  console on `127.0.0.1` only, as it does for VMs.
- Mitigations to test, in order: a minimal `--cap-add` set with `--device` nodes in place of
  `--privileged`; rootless podman with binderfs mounted in the user namespace (the matrix comment
  says binderfs mounts in a user namespace; whether Redroid's init works unprivileged is untested);
  one container per VM for anything untrusted. The backend has to say in `-accel-check` and `-verbose`
  that the device is a privileged container. [inferred]
- The kernel is shared: a misbehaving Android process can exhaust host memory or PIDs, so the
  container needs cgroup limits (`--memory`, `--cpus`, `--pids-limit`) from the AVD's `hw.ramSize`
  and `hw.cpu.ncore`. [inferred]
- Linux 7.4 removes the C binder driver and keeps the Rust one, which needs `CONFIG_RUST=y`, so a
  custom kernel for a future host has to be built on 6.1 or 6.18, or with a Rust toolchain; the
  userspace ABI is unchanged ([Phoronix](https://www.phoronix.com/news/Linux-7.4-Char-Misc)) [cited].

## 6. Which hosts can run it

From the environment matrix on #210 (not repeated in full here), with this VM added:

| Host | Binder | KVM | Backend this research implies |
| --- | --- | --- | --- |
| **This Hive VM** (Firecracker 6.1.189) | **No, and no module loading** [verified] | No [verified] | TCG, until the operator builds a kernel with binder, DMA-BUF heaps and (optionally) modules; that one change takes it from minutes to seconds [inferred] |
| GitHub `ubuntu-24.04` x86_64 | Yes after `apt install linux-modules-extra-$(uname -r)` and `modprobe binder_linux` (6.17-azure) [cited] | Yes | KVM; container is testable there |
| GitHub `ubuntu-latest` (26.04, kernel 7.0) | Probably; `linux-modules-extra` no longer exists for the 7.0 azure kernel, so `modprobe binder_linux` has to work from the base package, which nobody has confirmed [cited, [runner-images #15026](https://github.com/actions/runner-images/issues/15026)] | Yes | KVM; probe `modinfo binder_linux` first |
| GitHub `ubuntu-24.04-arm` | Probably (arm64 modules-extra ships `binder_linux`) [cited] | **No** | **Container is the only fast path**; software mode is x86 only |
| Ubuntu cloud VMs without nested virtualization (AWS non-metal, GCP E2/Arm, most Kubernetes nodes) | Yes with modules-extra [cited] | No | Container |
| Lima / Colima / UTM, Ubuntu arm64 guest on Apple silicon | Yes, one-time [cited] | Yes on M3+ | macOS already has emu's Hypervisor.framework backend; no need |
| Docker Desktop, OrbStack | No [cited] | No | n/a on a Mac (native HVF) |
| GitLab SaaS, Lambda, Fargate | No / no privileged containers [cited] | No | TCG or nothing |

The intersection the issue is about, **binder and no KVM**, is mostly arm64 cloud runners and
non-nested cloud VMs. A host with both binder and KVM is better served by emu's KVM backend unless a
user wants a seconds-long boot over fidelity (section 7).

## 7. Backend design questions for the spike

1. **Own namespaces or a runtime.** Driving `docker`/`podman` keeps the spike to a day and gets
   cgroups, overlay and networking for free; emu doing `unshare`, `pivot_root` and the binderfs mount
   itself matches "no stock tools" but means a new crate with `unsafe` (AGENTS.md lists the allowed
   ones) or a safe wrapper crate, plus Redroid's own init expectations. Recommend the runtime first;
   revisit after the numbers say the boot is worth owning.
2. **Does `emulator-<port>` registration work over a published TCP adbd?** If not, emu runs
   `adb connect` and the serial is `127.0.0.1:<port>`, which breaks the `emulator-<console port>`
   convention (`Running::serial`) and every script that builds it.
3. **Which Android version.** 15 boots in the cited run; 16 aborts in guest GPU mode on arm64;
   14 locks up on some hosts; 13 never finished. Start with 15 and test 16 on x86_64.
4. **Fidelity.** The device is an AOSP userdebug build on the host's kernel with no Play services,
   not the SDK's `default` image. A suite that passes on a container may fail on emu KVM and the other
   way round. This is why automatic selection should not silently swap one for the other (below).
5. **Selection.** Order KVM, container, TCG for `auto` once the container passes the harness with
   its gaps documented; until then the container is `-accel container` only, and `-accel-check`
   reports what each backend would need (`emu_kvm::open`'s message for KVM; binder, memfd, a runtime
   and an image for the container). The owner may prefer container-before-KVM for speed; that choice
   is the owner's.
6. **Images.** Redroid's official images stop at June 2025 and have no update cadence; simee would
   depend on a project it doesn't control. Building from AOSP with Redroid's patches is possible
   and enormous; not a first-stage option.

## 8. Recommendation and first spike

**Build, behind an explicit opt-in, after a measured spike. Don't build a second path to the
kernel features: if binder is absent, report it and fall back to TCG.**

Why: the cited boot (7.7 s) and idle cost (0.004 cores) are one to two orders of magnitude better
than TCG's (172.6 s, 0.58 cores) on any host that has binder, and some hosts (arm64 Linux, non-nested
cloud VMs) have no other fast path. Against: it can serve only the hosts with binder, gives up the
console, sensors, telephony and camera, runs a different Android build, needs a privileged container,
and depends on images that haven't been refreshed since June 2025. That makes it a CI and farm
backend for the AutoMobile-style harness, not a replacement for emu KVM or a developer's phone.

**Spike 0, no emu code (about two days), on a binder host:**

1. `scripts/bench/redroid-boot.sh`: the host check (`binder` in `/proc/filesystems`, `memfd`, a
   runtime, `modprobe` with the `apt-cache show linux-modules-extra-$(uname -r)` guard), `docker run
   --privileged … redroid/redroid:15.0.0_64only-250627 androidboot.redroid_width=720
   androidboot.redroid_height=1280 androidboot.redroid_gpu_mode=guest androidboot.use_memfd=true`,
   timing launch to `sys.boot_completed`, peak RSS from the container cgroup, idle CPU at 60-120 s
   and 180-300 s, and the `android-jank.sh` swipes; the same measures for the emu arms through
   `scripts/bench/android-boot.sh` and `software-mode.sh`, cold and warm, median of 3, in one run.
2. A manual `workflow_dispatch` job on a GitHub x86_64 runner (binder available, KVM available, so it
   measures all three arms on one machine; `-accel off` for TCG), and the same script on an
   `ubuntu-24.04-arm` runner (binder probable, no KVM, so it is the real target). Results into
   `docs/benchmarks.md` with date, machine and command, and the "Not yet measured" line for what's
   left.
3. AutoMobile's harness over `adb connect`, tool by tool, to turn section 3's table from inferred
   into measured.

**Spike 1, an `emu -accel container` that does the minimum (about a week):** `-provision-container
15`, `container.rs` driving the runtime, adb published on console + 1, the console server with the
honest `Unsupported` answers, display option A (polled `screencap`) feeding `FrameStore`, touch and
keys over adb; `-accel-check` reporting the container's requirements. Then B (scrcpy), the trait that
takes `ConsoleInfo`'s VMM-typed input and sensor fields off the display server, and automatic
selection.

**What would change this:** if the arm64 runner has no binder (the matrix says "probably"), the
case rests on non-nested cloud VMs and an operator-built Hive kernel, which narrows it to
"build later". If Redroid 15 x86_64 doesn't boot on the x86_64 runner, the images are the blocker,
not the kernel.

## Not done here

- No Redroid boot, so no time, memory or CPU of a container on this VM, and no same-host comparison
  with software mode. The brief allowed the boot only "if it runs"; it can't (section 1).
- No arm64 or GitHub-runner probe: the runner rows are the matrix's, unverified by this session.
- Nothing about Redroid's input, display or HAL behavior was observed; section 3's table is
  inferred from its documentation and the cited spike.

## Sources

- #210 and its comments: the environment matrix (2026-11-27), the Docker Desktop probe, the Lima
  spike (2026-11-27).
- [redroid-doc](https://github.com/remote-android/redroid-doc) and its
  [deploy guide](https://github.com/remote-android/redroid-doc/blob/master/deploy/README.md):
  features, `--privileged`, GPU modes, `androidboot.redroid_*`, scrcpy.
- [redroid-modules](https://github.com/remote-android/redroid-modules): binder and ashmem modules
  for kernels without them.
- [actions/runner-images #15026](https://github.com/actions/runner-images/issues/15026): the 7.0
  azure kernel has no `linux-modules-extra`.
- [Phoronix: Linux 7.4 removes the C Binder driver](https://www.phoronix.com/news/Linux-7.4-Char-Misc).
- `docs/research/software-mode-profile.md`, `docs/research/tcg-context-switch.md`,
  `docs/android.md` "Software mode", `docs/benchmarks.md`: every software-mode and KVM figure.
- `apps/emu/src/console.rs`, `apps/emu/src/display_server.rs`, `apps/emu/src/software.rs`,
  `crates/sim-core/src/stream.rs`, `crates/emu-avd/src/lib.rs` (`Running`): the surfaces in section 3.
