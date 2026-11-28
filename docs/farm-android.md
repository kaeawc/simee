# Running many Android devices (simee farms)

How to start many simee devices on one Mac quickly, settle them, and keep their idle cost low,
with the measurements behind each choice. Host for every number: Apple M3 Max (12 P + 4 E cores,
128 GB), shared with CI runners (1-minute load averages from 20 to 500 during these runs, so
wall times are noisy; exit counts, console bytes and disk sizes are not). Image: API 36
`google_apis` arm64, 4 vCPU, 2 GB, 720x1280.

## Tools

- `emu -clone-avd NAME -from GOLDEN` / `sim_android::Android::clone_avd`: a new AVD whose simee
  disks (`simee/userdata.img`, `cache.img`, `encryptionkey.img`, the metadata partition that holds
  user data's key) are APFS clones (`std::fs::copy` uses `fclonefileat` on macOS) of a stopped
  golden AVD's. The clone boots warm, persistent, with its own state from then on. The golden's
  stock emulator files are never read or written; its instance lock is held while copying, so a
  running golden is refused. `emu -delete-avd NAME` (`Env::delete_avd`) validates the name as
  `-create-avd` does, finds the directory through `<name>.ini`'s `path=` as booting does, and
  removes only that directory (it must hold a `config.ini`) and the pointer. It refuses while any
  emulator runs the AVD: one advertised for it in the running directory (stock writes the same
  file), or a held lock file in its directory (simee's `simee/instance.lock`, stock's
  `multiinstance.lock`, or `hardware-qemu.ini.lock` naming a live PID).
- Through the API and CLI: `clone {device: <golden>, name}` (`simee clone <golden> <name>`) and
  `boot_many {devices, concurrency?, timeout_s?, first_port?}` (`simee boot-many <avd>… -j K
  --first-port P`), the same methods as iOS's (docs/farm.md, docs/api.md).
- `sim_android::Android::boot_many(avds, concurrency, timeout, first_port)`: boots up to K at a
  time, each on a console port handed out by one allocator (`allocate_ports`: even ports whose
  console and adb ports no running emulator advertises, whose claim no starting simee holds
  (`port_usable`: the lock file below) and that bind now), and returns one `BootOutcome` per AVD
  (port, launch-to-booted time or the reason it failed). A failure doesn't stop the others; the
  next device starts when any finishes. Another allocator (a second farm, a bare `emu` from
  AutoMobile or Studio) can still claim a pair between the probe and emu's own claim; emu then
  exits with "console port N is taken by another simee instance" and that device is retried on
  the next free pair (up to 3 times) instead of being reported failed. A device that doesn't boot
  within the timeout is stopped (`kill -INT`, 10 s, then SIGKILL) and reaped by `boot_with`
  (`Device::boot` too): a failed outcome leaves no emulator running, no port held, no zombie.
- emu claims its console port with a lock file (`~/Library/Caches/simee/ports/<port>.lock`, held
  for the process's life) before probing it, so instances started at the same moment can't pick
  the same pair (they used to: both probed a pair free, one then failed to bind). `-port N` is
  claimed too and fails at once if another simee holds it.
- `scripts/farm-bench-android.sh <label> <golden> <N>`: the benchmark below.

## Golden user data per system image

A fresh AVD's first boot is the slow one: init waits for apexd to decompress the image's compressed
APEX files one at a time, vold formats and encrypts user data, and `system_server` does its
first-boot work (docs/research/software-mode-profile.md, section 4). `emu -create-avd` and
`emu -provision ... -create-avd` therefore keep one settled user data per system image and start
every later AVD of that image from a clone of it (#226).

- **First AVD of an image.** `-create-avd NAME` makes the AVD, then boots it once in a child
  `emu -avd NAME` (no window, no audio) and waits for `emu: boot completed`. With the SDK's adb it
  then waits until the home app (the one `cmd package resolve-activity` names for the HOME
  category) is the resumed activity and the guest's CPUs are under 25% busy in two samples 5 s
  apart, for at most 10 minutes; a guest that doesn't settle in that time, or an adb that never
  answers, fails the build (no golden is kept from an unsettled guest). Without adb in the SDK
  the guest gets a fixed 120 s. It is then stopped: with adb, `adb shell reboot -p` (the guest
  syncs, unmounts user data and powers off, emu exits when it does; if it hasn't within 2 minutes
  the console stops it instead), without adb emu's console `kill` (the stock text protocol with
  `~/.emulator_console_auth_token`: the VM is stopped as `adb emu kill` does, with no guest
  shutdown). The build says which ran. Only then are its simee disks (`userdata.img`,
  `cache.img`, `encryptionkey.img`: the set `-clone-avd` copies) kept. NAME keeps its own settled
  disks, so it never pays a second boot of its own.
- **Later AVDs.** `-create-avd NAME2` finds the golden, clones its disks into `NAME2/simee/` (a
  reflink where the file system has them, else a copy that keeps the image's holes; APFS clones the
  files) and returns: the first start of NAME2 is a warm boot.
- **Where, and the key.** `<avd home>/.simee-golden/<api>_<tag>_<abi>_<id>/` holds the disks and
  `golden.ini`; it is on the AVDs' file system so reflinks work, and `emu -list-avds` doesn't see
  it. `<id>` is the BLAKE3 of the image's identity, computed as #170's bundles compute it (API
  level, tag, ABI, revision, the hash of `system.img`, `vendor.img`, `kernel-ranchu`, `ramdisk.img`,
  and the build fingerprint), and of the AVD's *shape*: data partition size, screen size and density,
  `hw.mainKeys`/`dPad`/`keyboard`/`sdCard`, `tag.id`, `abi.type` and `boot.prop`. Memory, vCPU
  count, GPU mode and names are not in it. AVDs of one image with another shape (a bigger screen,
  a larger data partition) get their own golden, built when the first of them is created.
- **Invalidation.** A new image revision, or the same revision with other contents (it is the
  files' hashes that count, not the version string), has another `<id>`, so the old golden is not
  used; when the new one is built the goldens of the image it replaced (same package path under the
  SDK, other identity) are removed. Hashing the image's files costs a few seconds per `-create-avd`.
  Nothing else removes a golden: delete `.simee-golden/` to start over.
- **Concurrency.** A golden is a directory that only appears by `rename` once `golden.ini`
  (written last) lists its disks and their lengths; a build works in `.<name>.partial-<pid>/`. The
  build holds an `flock` on `<name>.lock` from its check for an existing golden to the rename, so
  two `-create-avd` runs for one image build once: the second waits (up to 45 minutes, saying so),
  then clones the golden the first made. A killed build leaves only a `.partial-*` directory,
  which the next build removes, and a directory without a valid `golden.ini` is never used.
- **Failure.** If the build can't boot the AVD, doesn't reach `boot completed` in 30 minutes, the
  guest doesn't settle, or emu can't be stopped, `-create-avd` still creates the AVD, removes its half-settled disks (it first-boots
  fresh) and warns; the log of the build is `<avd>/simee/golden-build.log`. Hosts that can't boot
  the image (an arm64 image on x86_64 Linux, any image on a host with no emulator backend, no QEMU
  in software mode) skip the golden and say so.
- **Opt out.** `-no-golden` neither builds nor uses a golden: a pristine first boot, as stock's
  AVDs have. `scripts/farm-bench-android.sh`'s `MODE=fresh` passes it.
- **Guest identity is duplicated**, as with `-clone-avd` and #170's bundle imports
  (`emu_avd::bundle::DUPLICATED_IDENTITY`): `android_id` and the per-app SSAIDs live in the
  encrypted user data, which emu can't edit offline, so every AVD made from one golden is the same
  device to its guest's apps and services. `ro.serialno` is emu's constant
  (`EMULATOR0EMURS0`) in every AVD anyway, and the adb serial `emulator-<port>` is per launch. An
  AVD that must look like a new device to an app (a test that registers accounts, say) uses
  `-no-golden`, or `-wipe-data` on a clone.

### What it saves (software mode, 2026-11-27)

One host for all of it: a 4-vCPU Firecracker VM (Intel Xeon 2.2 GHz, no `/dev/kvm`, no GPU, 3.9 GB
of RAM, ext4 without reflinks), Ubuntu 22.04, release `emu`, QEMU 11.1.3 built from the release
tarball, API 36 `default` x86_64 r2, AVDs from `emu -create-avd` with `-memory 1536 -cores 4`
(720x1280 at 320 dpi, 4 GB data). Nothing else ran on the VM; the 1-minute load average was
0.6 before the first run (each guest then holds 2 to 3 of the 4 cores while it boots). Times are
wall clock from `emu`'s start (or the start of `-create-avd`, where said) to `emu: boot completed`
and to the first time `dumpsys activity activities` reports a launcher as the resumed activity,
polled every 2 s. `scripts/golden-bench.sh golden|fresh <name> <sysdir>` is each row: three runs,
median shown.

| Second AVD of the image | `-create-avd` | boot completed | launcher | create + launcher |
| --- | --- | --- | --- | --- |
| with the golden, this version (guest powered off by `adb shell reboot -p`) | 1.4 s (1.2, 1.6, 1.4) | 155.8 s (155.8, 147.8, 165.9) | 178.5 s (178.5, 175.1, 194.9) | 179.7 s |
| with the golden, first version (stopped through the console, no hash cache) | 24.4 s (23.2, 24.4, 24.9) | 186.3 s (186.1, 188.3, 186.3) | 213.9 s (211.3, 213.9, 217.3) | 238.3 s |
| `-no-golden` | 0.0 s | 380.2 s (396.5, 380.2, 378.1) | 421.0 s (423.1, 413.8, 421.0) | 421.0 s |
| saved by this version | | 224 s | 243 s | 241 s |

The `-no-golden` row was measured earlier the same day, interleaved with the first version's
runs, and not repeated for this version (it doesn't touch that path). The golden's guest is shut
down cleanly now, so the AVDs cloned from it boot about 30 s sooner than from the power-cut one.

- **The golden's one-time cost** is the first AVD's `-create-avd`: 562 s with this version (one
  run: 375 s to `boot completed` on a fresh AVD, the rest waiting for the launcher and an idle
  guest, the power-off and the copy of the 553 MB of data in the 4 GB sparse image); 597 s and 566 s
  with the first version (one run each). That AVD keeps its settled disks, so it boots like the
  second ones (154 s / 187 s, one run).
- **Hashing the image.** The first version's 24 s `-create-avd` was 21 s of hashing `system.img`
  (1.9 GB) for the golden's key, with none of it in the page cache. The hashes are remembered under
  `.simee-golden/.hashes/` by file path, size, inode, modification and change time, so only the
  first AVD of an installed image pays it (21.3 s once with the cache removed and the page cache
  dropped); the next takes 0.5 to 1.6 s with the clone of the 553 MB of data included.
- **Concurrency, real processes** (first version): `-create-avd c1` and, 5 s later,
  `-create-avd c2` with no golden: one build (566 s); c2 said it was waiting, cloned after 561 s,
  and the two `userdata.img` were byte-identical.
- **Not measured:** Hypervisor.framework (no Mac here; `reboot -p` ending emu with status 0, which
  the stop relies on, is seen only in software mode), KVM (no `/dev/kvm`), a second image, a
  filesystem with reflinks (this one reports `sparse copy`), a host with more than 4 cores. The
  issue's "faster on HVF/KVM too" is expected from the same first-boot work (formatting,
  encryption, apexd, dexopt) but no number is claimed.

## Settled, and the benchmark

Settled = `sys.boot_completed=1`, then the emu process using under 60% of one host core over 5 s
(host-side, so a busy guest isn't loaded further by adb). Every boot shows a long tail after
`boot_completed`: 20–90 s of post-boot jobs at 100–200% CPU before the device quiets. That tail,
not the boot, dominates time-to-settled.

The script reports per run: time to all settled, median boot and settle (each from the device's
own launch, which is also where its `BOOT_TIMEOUT` starts: with K < N, later devices start
later; the runs below used K = N), failures, memory two
ways (sum of RSS, and the system-wide change in anonymous + wired + compressor pages from
`vm_stat`, at settle and 60 s later), host CPU % and vCPU exits/s per device at idle (from
`EMU_EXIT_PROFILE`), and `df`/`du` for the AVDs it made. RSS counts guest pages that host and
guest both touched twice under Hypervisor.framework (about 4.0 GB for a 2 GB guest); the
system-wide figure doesn't, but on this shared host CI moves it by gigabytes between samples, so
neither is a clean per-device number yet (see Results).

### The `settle` and `profile` methods

The API's `settle` (#420) judges an Android device as iOS's judges a simulator, with the same
parameters and result: the guest's processes (and kernel threads) under `cpu_pct` (10) % of a
core over `window_s` (10) s, read from `/proc/<pid>/stat` in one adb call a second and timed on
the guest's `/proc/uptime`, with `sys.boot_completed=1` and `system_server` running in place of
SpringBoard. It measures the guest, not the emu process around it (the script above), so the
figure doesn't include the emulator's own idle work; the probe's shell is left out, adbd is not.
There is no QoS split to read, so all of it counts as foreground. The golden build's settle
(`apps/emu/src/golden.rs`: the launcher resumed and the whole guest under 25% busy, twice 5 s
apart) stays as it is: it runs in emu before the API exists.

`profile {name: "slim"}` turns off, for user 0 (`pm disable-user`, kept in
`package-restrictions.xml`, so across reboots and into clones and exports), Google packages that
run their own scheduled work and that nothing AutoMobile or an app under test reaches: Device
Health Services, Digital Wellbeing, the Data Restore Tool and Google Partner Setup
(`crates/sim-android/src/profile.rs` says why each, and what is kept). One an image doesn't have
is reported in `not_loaded`. What each saves isn't measured yet (benchmarks.md, "Not yet
measured"); an entry that saves nothing measurable comes out.

## The console (88% of all vCPU exits)

The PL011 traps twice per byte (FR read, DR write), synchronously in the writer. With
`console=ttyAMA0` at the default console level, a boot printed ~230 KB (kernel, init, ueventd,
SELinux) and idle printed ~2 KB/s (the IUwb lookups and goldfish-logcat restarts below).

What stock does (`SetupParameters.cpp` `setupVirtualSerialPorts`, `main-kernel-parameters.cpp`),
by mode, on QEMU2 for ARM:

| | stock without `-show-kernel` | stock with `-show-kernel` | simee default | simee `-show-kernel` |
|---|---|---|---|---|
| console | `console=0` (no kernel console), serial backed by `null` | `console=ttyAMA0,38400` | `console=ttyAMA0,38400 loglevel=3` | `console=ttyAMA0,38400` |
| always | `keep_bootcon earlyprintk=ttyAMA0` | same | same | same |
| `printk.devkmsg=on` | only with `-shell` | yes | yes | yes |

The cmdline in docs/reference/stock-board.md is the `-show-kernel` one.

simee now: without `-show-kernel`, `console=ttyAMA0` stays with `loglevel=3`: only emergencies,
alerts and criticals print, which includes init's FATAL lines and panics (`panic()` raises the
console level), so `console.log` is empty for a normal boot; dmesg keeps everything
(`printk.devkmsg=on` stays, a better debugging story than stock's rate-limited kmsg).
`-show-kernel` prints everything, as before; `EMU_CONSOLE=full` logs everything to console.log
without printing it. console.log is created once per emu process and appended to across guest
resets, so a panic's text survives the reboot it causes (each reset used to recreate it, which
is why one sysrq-c test found it empty; not yet re-tested on a device). Considered and not
taken: `console=hvc0` on the virtio-console port 0 (whole buffers per kick, full logs kept),
because it changes `/dev/console` for every image, and quieting already removes ~all of the cost.

The desktop's Android panel (LOG) shows the last 200 lines of the device's log
(`sim_android::device_log_tail`): console.log's critical lines (at most a quarter), then the
logcat stream. sim-android starts every emu (`Device::boot`, `boot_many`) with
`-logcat '*:I' -logcat-output <avd>/simee/logcat.txt`, so devices booted from the desktop, the
API or the CLI have that stream; one started some other way shows only console.log's lines, and
an empty panel says how to get the kernel log.

## goldfish-logcat and /dev/hvc1

`init.ranchu.rc` runs `service goldfish-logcat /system/bin/logcat -f /dev/hvc1 ${ro.boot.logcat}`
(class main, always). simee's virtio-console had one console port, so `/dev/hvc1` had no
backend: logcat exited every ~5 s, each restart forking `flags_health_check` and writing ~8
console lines. Stock backs hvc1 with a virtconsole (null unless `-logcat`).

Now the virtio-console's third port (after port 0 and `modem`) is an unnamed console port, which
the guest makes `/dev/hvc1` (unnamed ports are console ports; hvc numbers follow port order; no
board change: same device, same slot). Its output is discarded unless `-logcat TAGS` (stdout,
filter TAGS, stock's flag) or `-logcat-output FILE` (all of it unless `-logcat` gives a filter;
FILE rolls over to `FILE.1` at 4 MiB, so at most 8 MiB on disk). When it's discarded the filter
is `*:S` (`androidboot.logcat`; was `*:V`), so logcat formats and writes nothing. The
destination is opened once per process (a reset appends) and fed through a bounded queue
(1024 guest buffers) on its own thread: the virtio-console worker, which also serves the radio
HAL's `modem` port, only queues. When the destination falls behind (a stdout pipe nobody reads)
output is dropped, counted (`-verbose` prints the count at exit) and noted in the stream as
`[emu: N bytes dropped ...]`. hvc1 hands over a whole buffer per kick, so a logcat stream costs
the guest logcat's formatting, not vCPU exits per byte; sim-android launches with `*:I`.

## UWB (system_server asks for IUwb every second)

`init.ranchu.rc` starts the UWB HAL only `on property:vendor.qemu.vport.uwb=*`, and the HAL opens
`/dev/hvc2` (stock serves it from netsim). simee had no `uwb` port, so the HAL never started and
system_server's UWB service retried the lazy AIDL service every second (servicemanager → init
`ctl.interface_start` → "Could not find"), ~3 kmsg lines/s. Now the virtio-console has stock's
`uwb` port, a named console port (the guest's hvc2), answered by a UCI responder with no peers
(`crates/emu-vmm/src/bt/uwb.rs`): the HAL starts, UWB reports READY, and `IUwb` is looked up once
per boot, as on stock (docs/research/bluetooth.md).

## Port and adb scaling

adb servers scan adb ports 5555–5585 (16 emulators) unless started with
`ADB_LOCAL_TRANSPORT_MAX_PORT`; instances above that are known only because simee tells the
server (`host:emulator:<adb port>`) at boot. A server started later (after `adb kill-server`)
never heard of them. emu now repeats that announcement every 5 s (a no-op for a server that has
the device: one loopback connect and write). Tested with a private adb server
(`adb -P 5137 nodaemon server`, emu started with `ANDROID_ADB_SERVER_PORT=5137`, port 5600): the
device was listed, gone right after the server restarted, and listed again within 8 s.

## Not done here: balloon and snapshots

- virtio-balloon with free-page reporting would return idle guests' free pages (the API 36 kernel
  has page reporting built in, `virtio_balloon.ko` ships in system_dlkm). It needs a virtio-mmio
  slot the stock board doesn't define for it, so a board change with evidence from stock first,
  and on macOS returning a range means `hv_vm_unmap` + remap of 16 KiB-aligned runs.
- Snapshots (save RAM + vCPU + GIC + devices, restore as `MAP_PRIVATE` of the RAM file, which
  would also share clean guest pages between instances) need the renderer's state: the SDK's
  gfxstream refuses `stream_renderer_snapshot` (`docs/roadmap-android.md`). Golden userdata clones
  are the snapshot-free half of that: no first boot, but each device still boots and settles.

## Results

`scripts/farm-bench-android.sh`, clones of one settled golden, 1-minute load 20 at each start and
40–760 by the end (CI), so times and CPU % are noisy; base = emu before these changes.

| run | N | boot med s | settle med s | idle CPU % med | idle exits/s med | PL011 share |
|---|---|---|---|---|---|---|
| base | 1 | 7.6 | 74.5 | 88 | 6895 | – |
| new | 1 | 4.1 | 96.7 | 137 | 867 | 0% |
| base | 2 | 5.4 | 26.7 | 39 | 4274 | 94% |
| new | 2 | 5.1 | 125.3 | 38 | 144 | 0% |
| base | 4 | 5.0 | 49.9 | 144 | 6623 | 76% |
| new | 4 | 6.2 | 42.2 | 37 | 154 | 0% |
| new, fresh AVD (first boot) | 1 | 9.3 | 50.9 | 62 | 284 | 0% |

- Boot: 516k vCPU exits before (88% PL011) → 77–109k after (64 PL011 accesses); console.log
  1.4 MB → 0 bytes for a normal boot.
- Idle, quiet window on one device: 4.5k → 122 exits/s; host CPU 35–42% → 29%; guest busy
  7–8% → 5.7% of 4 vCPUs, tasks 2.7 → 2.3/s. What's left is the guest's own work (vCPU threads
  ~12.5% each in `hv_trap`; system_server ~11%), not exits.
- Clones: `df` grew 0–3 MB per clone while `du` counts 1.7 GB each.
- Memory: RSS ~4.0 GB per device (2 GB guest); the system-wide figure moved by gigabytes with CI
  between samples, so it isn't a per-device number here.
- N=8 and the K sweep weren't run: the host stayed over its load limit (1-min load 145–160).
