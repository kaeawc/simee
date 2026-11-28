# Running a device farm

How to start many devices on one Mac, get them quiet fast, and keep them cheap while they idle.
Measured with `scripts/farm-bench-ios.sh` (iOS) on a 16-core (12 P + 4 E) Apple-silicon MacBook
with 128 GB, iOS 27.2 runtime, iPhone 17 devices, while CI jobs shared the host (1-minute load
averages of 10-500 during the runs: treat single numbers as ±30 %).

## iOS simulators

### Quick start

```bash
# Once: a template that has done its first boot (data migration, indexing) and is shut down.
T=$(simee create farm-template "iPhone 17" com.apple.CoreSimulator.SimRuntime.iOS-27-2)
simee boot "$T" --settle --timeout 600 && simee shutdown "$T"

# Per farm: clones (APFS: ~15 MB of disk each), booted four at a time, each waited for until quiet,
# optionally with the slim profile (per device: a clone doesn't inherit it; written before the
# boot, so the clone never starts those daemons).
for i in 1 2 3 4 5 6 7 8; do simee clone "$T" farm-$i; done
simee boot-many farm-1 farm-2 farm-3 farm-4 farm-5 farm-6 farm-7 farm-8 --profile slim
```

`boot-many` waits for each device to settle by default (`--no-settle` doesn't); `--settle` is
still accepted. It exits non-zero only when a device failed to boot: one that booted but didn't
settle in time is printed as `settled=false` (with a `booted, not settled after N s` note on
stderr) and counted in `unsettled=`. `boot --settle` likewise exits 0 for a device that booted
but didn't settle, and prints the note.

Name runtimes by identifier (`com.apple.CoreSimulator.SimRuntime.iOS-27-2`): a host with an
Xcode beta can have two runtimes called "iOS 27.2".

### What "settled" means

`boot` returns when the service reports the device booted (terminal boot status) and its display
has a surface. That is 2-20 s after the call, while the device is still at 300-2400 % of a core:
a first boot migrates data and indexes for one to two minutes, every boot starts 190-300 daemons,
and poster, widget and Siri extensions keep starting for minutes after.

`boot --settle` (and `boot_many {settle}`, and `settle` for a device already booted) also waits
until the device's whole process tree (its `launchd_sim` and every descendant) uses under 10 % of
one core in the foreground over 10 s with SpringBoard running (`settle_cpu_pct`,
`settle_window_s`, `settle_foreground` change the rule; foreground below).
CPU is each process's accumulated CPU time, so it is exact over the window and a process
that exits doesn't make the tree look quieter. The threshold comes from the measurements: an idle
device runs at 1-6 % (no client attached), so 10 % separates "storm over" from "idle" with room
for one busy daemon; 5 % (the research brief's guess) never fired on some idle devices.

The wait reads the tree through libproc (`sim_coresim::procs`) once a second: the host's process
table (pid, parent, name), then the CPU time of the device's own processes. It used to spawn a
host-wide `ps -Ao pid=,ppid=,time=,args=` instead, 6.5-8.4 % of a core during a `boot_many` on a
host with ~2000 processes. Measured on this host with ~1200 processes and no device booted
(`cargo test -p simee-coresim --release poll_cost -- --ignored --nocapture --test-threads=1`, median
of 3): the `ps` listing cost 38.2 ms of CPU per poll; the table costs 0.24 ms, and reading CPU
costs 1.1 µs per process, so a poll of a 100-300 process device costs about 0.35-0.6 ms. Each wait
reads the table itself, so 8 waits at once cost about 4 ms a second, where one shared `ps` listing
cost 38 ms.

Each process's CPU is split by QoS: the `background_cpu_pct` of a result is what ran at the
maintenance or background QoS (launchd jobs of `ProcessType` Background, darwinbg tasks:
priority 4, on the efficiency cores), `foreground_cpu_pct` the rest, `cpu_pct` all of it. Once a
device is past its first-boot storm most of what it still runs is background work on the 4 E-cores
while the P-cores idle. So the rule judges the tree on its foreground CPU only (the default
since 2026-11-26): a device with only background work left counts as settled, and the next
device's boot storm runs on the P-cores beside it. `settle_foreground: false` (`foreground:
false` for `settle`, `--settle-cpu all` on the CLI) judges all of it, as before. The QoS split
itself was checked on the host (`taskpolicy -b yes` lands 99.9 % in background CPU, plain `yes`
0 %: `procs::tests::background_cpu_is_attributed_to_background_qos`). Measured on fresh clones of
a booted-once template, `boot-many --profile slim`, 3 interleaved runs of each (2026-11-26, this
host with other work: 1-minute load 25-550, so read the ranges):

| N, K | judged on | each device settled | all settled | CPU-s per device to settle |
| --- | --- | --- | --- | --- |
| 1, 1 | all CPU | 46.8 s (46.4-79.5) | | 131 (128-138) |
| 1, 1 | foreground | 33.3 s (28.3-44.2) | | 123 (117-128) |
| 4, 4 | all CPU | 107 s (87-134) | 109 s (90-134) | 132 (128-135) |
| 4, 4 | foreground | 41 s (34-71) | 43 s (35-71) | 76 (74-114) |

At N = 4 the devices judged on all CPU wait out each other's background work on the 4 E-cores
(28-35 background CPU-s each); judged on foreground they settle before most of it starts, at
92-120 processes instead of 121-124. CPU-s to settle is lower partly because the wait ends
sooner: the background work still runs after it.

The settle wait has its own budget, `settle_timeout_s` (`--settle-timeout`), counted from the end
of the boot, separate from `timeout_s` (the boot call). Its default is 600 s, and in `boot_many`
150 s per device in flight above K = 4 (K = 8: 1200 s): settle times measured 250-537 s per device
under load, and K storms at once make each one longer. A wait that runs out is not a failed boot:
the result says `settled: false` with `settle_error` (the CPU over the last window), `waited_s`,
`procs`, `cpu_pct`, `background_cpu_pct` and `foreground_cpu_pct`, and the device stays booted.
The standalone `settle` method still fails when it runs out of time (waiting is all it does), with
the same 600 s default.

Settled is not forever. Deferred work starts two to four minutes after boot (process count
190 → 240 on a clone, with 100-700 % bursts), so a device that settled at 60 s can be busy again
at 200 s. For a farm that is fine: settle says when the boot storm is over and the device can take
work; the later bursts are background work the device does anyway.

### Settle times

| Boot | N | `boot` returned (all) | settled (all, from outside) | simee `--settle` |
| --- | --- | --- | --- | --- |
| first boot, fresh device | 1 | 19 s | 97-107 s | — |
| first boot, fresh device | 2 | 30 s | 68-80 s | — |
| first boot, fresh device | 4 | 47 s | 421-460 s | — |
| first boot, fresh device | 8 | 373 s | 448-508 s | — |
| clone of a booted-once template | 1 | 1.5-20 s | 50-84 s | 50-79 s |
| clones, `boot-many -j 4` | 8 | 125 s | 65-245 s | 45-79 s each, all 124 s |
| clones, `boot-many -j 4`, quieter host | 8 | — | — | 44-85 s each, all 128 s |
| clones, second boot of the same devices | 4 | 16-25 s each | 251-263 s | 250-263 s (round 1: 152-291 s) |

Later boots were not measurably faster than a clone's first boot here (the host was at load 60+
during that run); what removes most of the work is the clone of a booted-once template.

"From outside" is the benchmark's own judge (same rule, sampled once a second from a separate
process); under load its samples are seconds apart, so it lags simee's own wait.

### Booting many at once: `boot_many` and K

`boot_many` boots a list with at most K boots (and settle waits) in flight; each worker thread has
its own service connection, so nothing in simee serialises the boots (the service's `boot` call
itself returns in ~1-20 s). K sweep, 8 clones, time until all settled:

| K | all settled (outside) | simee's own `all_s` |
| --- | --- | --- |
| 2 | 537 s | 537 s |
| 4 | 245 s | 124 s |
| 8 | 528 s | 297 s |

K = 8 starts eight storms at once and every device takes longer than four would in sequence; K = 2
leaves the host idle. The default is a quarter of the logical CPUs, at least 2 (4 here).

A slot is held through the profile and the settle wait, not just the boot call: what K bounds is
the number of boot storms in flight, which is what the sweep measured. Without the settle wait a
slot would be freed when the service's `boot` returns (1-20 s), long before the storm is over, so
K would bound only those calls and the storms of all N devices would overlap. That is why
`boot_many` settles by default (`settle: false`, `--no-settle`, opts out for a caller that only
wants the service's "booted"); `boot` still settles only when asked, since a single boot has no
slot to hold.

The shaper (`shaped`) is resolved once per `boot_many` call, on the caller's thread, before any
worker starts: K workers building it at once would race on the same cache files.

### Clones: no first boot per device

`clone` (CoreSimulator's `-[SimDeviceSet cloneDevice:name:error:]`, what `simctl clone` does)
copies a shut-down device: its data, settings and apps, as APFS clones. 4 s per clone; `du` says
2.2 GB per device but the volume's used space grows ~10-30 MB per clone. A clone of a template
that has done its first boot skips the data migration and indexing storm.

### The `slim` profile

`boot --profile slim` (or `simee profile <device> slim` on a booted one) disables (in the
device's own launchd database) and stops 74 daemons no AutoMobile tool and no simee method needs: PosterBoard and the
~15 poster extensions it keeps alive (~30-45 MB footprint each), widgets (chronod), Siri and
suggestions, photo and media analysis, Health, Home, fitness, the Store, iCloud sync, Mail, News,
Weather, Wallet, Game Center, Screen Time, watch and Find My companions, experiments and ML hosts,
App Intents (linkd), account sync, Focus status, Memoji and Apple Intelligence's model catalog.
The list, each job's rationale and safety note, and what was kept on purpose are a table, `SLIM`
and `KEPT` in `crates/sim-core/src/launchd_profiles.rs` ([ios.md](ios.md#launchd-job-profiles)).

| per device | fresh | clone | slim clone |
| --- | --- | --- | --- |
| processes | 253-297 | 217-244 | 113-153 |
| footprint (dirty memory, shared once) | 2.6-2.8 GB | 2.0-2.2 GB | 1.07-1.2 GB |
| CPU of 8 devices together, 60 s after settle | 869 % | 25 % | 7 % |

Where the disables live: `launchd_sim` keeps its `launchctl disable`/`enable` overrides on the
host, in `/private/var/tmp/com.apple.CoreSimulator.SimDevice.<UDID>/disabled.plist` (an XML
plist of label → bool, one file for all of the device's domains, read when it starts), not in
the device's data directory. That is why a clone, a new UDID, starts with none. `boot --profile`
of a shut-down device writes the profile's labels into that file before the boot
(`sim_core::launchd`, keeping the file's other overrides; the result's `profile.preset` counts the
labels it added), so the boot never starts those jobs. It also passes them as CoreSimulator's
`disabled_jobs` boot option (`simctl boot --disabledJob=`), which CoreSimulator writes to
`/private/tmp/com.apple.CoreSimulator.SimDevice.<UDID>/disabled.plist` instead (and deletes on a
boot without it). Runtimes differ in which file their launchd_sim reads: iOS 18.6 to 27.2 the
`/private/var/tmp` one, iOS 18.0 the `/private/tmp` one (measured, CoreSimulator 1174.21), so one
of the two works on each. After the boot simee reads back the file in the directory where
launchd_sim wrote its `launchd.log` during this boot; when it holds every label disabled, no
`launchctl` runs at all (the result's `profile.verified` is `file`). Otherwise (a device booted
already, a runtime that logs in neither directory, a label enabled since) the profile applies the
old way: `launchctl list` and `print-disabled`, then a disable and a bootout per job still on.
`com.apple.siri.context.service` is left alone when it is disabled and not running: launchd_sim
loads it whatever the overrides say and re-creates it 0.25 s after a bootout. `delete` also
removes both directories (the service leaves them behind, ~1-3 MB of `launchd.log` per device).

Applied after the boot instead (a device booted without it, or `simee profile` on a booted one),
it takes ~20 s on a quiet host, 24-66 s under load (the runtime has no shell, so `launchctl
disable` and `bootout` are a spawn each per job), counted in `settle_s`. A clone's first boot,
`boot-many --profile slim`, fresh clones of a booted-once template, the old way against the
disables written before the boot, both judged on all CPU (2026-11-26, this host with other
work: 1-minute load 25-550; median of 3, interleaved; at N = 4 the median device of each run):

| N, K | | profile | each device settled | all settled | CPU-s to settle | processes at settle |
| --- | --- | --- | --- | --- | --- | --- |
| 1, 1 | after the boot | 32.1 s (27.2-39.1) | 95.0 s (52.7-97.2) | | 159 (133-163) | 139 |
| 1, 1 | written before | 1.6 s (1.5-3.7) | 46.8 s (46.4-79.5) | | 131 (128-138) | 123 |
| 4, 4 | after the boot | 42.9 s (39.0-45.1) | 227 s (167-238) | 229 s (170-238) | 167 (158-211) | 139-158 |
| 4, 4 | written before | 5.4 s (3.7-8.5) | 107 s (87-134) | 109 s (90-134) | 132 (128-135) | 121-124 |

With both changes (written before, judged on foreground) four clones were all settled in 43 s
(35-71) against 229 s.

CPU-s is what the device's process tree used from the boot until it settled (`cpu_s` in the
result; `background_cpu_s` its background-QoS part), sampled once a second: a daemon the profile
stopped before the first sample isn't counted, so the "after the boot" rows read low. The result
reports what launchd did, not what was asked: `disabled` counts the labels `launchctl
print-disabled system` lists as disabled afterwards, `failed` names the others (and any whose
`disable` printed an error; the "Please switch to user/foreground/…" warning a running job's
`disable` prints is not one), `not_loaded` the labels `launchctl list` didn't show before (already
stopped, or not a job in this runtime: `launchctl disable` accepts any label, so a mistyped one
shows up here).

Most disables survive a plain reboot (measured, iOS 27.2: a slimmed device shut down and booted
without `--profile` came up with 133 processes, its sampled jobs still listed disabled and none
loaded). Three used not to: companionappd, NPKCompanionAgent and brookcompaniond came back listed
`enabled` and running after every reboot. They are paired-watch daemons (the runtime's
`System/Library/NanoLaunchDaemons`, `Disabled` in their own plists), and `nanoregistrylaunchd`
enables all 17 of them each time it runs, one to three minutes after a boot (its `Setting service
… to enabled (initiated by nanoregistrylau…)` lines in `launchd.log`, iOS 27.2), which also
started addressbooksyncd, bulletindistributord and pairedsyncd: the ~30 CPU-s burst 50-70 s after
boot. The profile now disables nanoregistrylaunchd itself and those three as well; nanoregistryd,
which asks for it, doesn't spin without it (the devices below settle and idle as before). A clone
doesn't inherit any of the disables (`clone` resets them: a clone of a slimmed template came up
with 221 processes). So pass
`--profile slim` on every boot, not only a clone's first. Applying it again is cheap: it reads
`launchctl list` and `print-disabled` first and leaves alone each job that is still disabled and
not loaded (the result's `already` counts them, and they aren't listed in `not_loaded`), so a
reboot costs those two reads, a disable and a bootout per job that came back, and the final
`print-disabled`. `simee profile <device> slim --undo` (`profile {name, undo: true}`) enables the
same jobs again; they run from the next boot (measured: 59 enabled in 13 s; after a reboot the
sampled jobs were loaded again and none listed disabled, 209 processes at settle against 133 slim).

`com.apple.sleepd` is kept. With it disabled, every later boot of a slimmed device spins:
SpringBoard and mobiletimerd retry the `com.apple.sleepd.sleepserver` mach service in a tight loop
(138k log lines in 3 s, about 350 % of a core), and the device never settles. Measured on iOS 27.2,
3 warm reboots each, judged from outside by a 2 s sampler (same 10 % / 10 s rule), load 4-60:

| warm reboot | settled | CPU-s to settle | processes | footprint |
| --- | --- | --- | --- | --- |
| stock | 48-110 s | 347-383 | 198-208 | 1.79-1.92 GB |
| slim without sleepd | 29-83 s | 69-142 | 94-113 | 733-922 MB |
| slim with sleepd disabled | never | ~350 % of a core, spinning | | |

`com.apple.carkitd` is not in the profile: `carplay` needs it. The kept jobs are pinned by a test
per simee method that reaches into the device (`carplay`, `location`, `privacy`, `push`,
`appearance`, the keyboard, `open_url`, `biometric`, `status_bar`, `notify`, `reset_keychain`,
`defaults_*`, install and launch, `logs`, network shaping, audio).

Kept: SpringBoard, backboardd, InputUI (keyboard), the accessibility server and daemons
(AutoMobile reads the screen through accessibility), installd, lsd, runningboardd, tccd,
usernotificationsd, locationd, sharingd, webbookmarksd (Safari), assetsd, sleepd, logging, configd_sim,
nsurlsessiond. AutoMobile's tools pass on a slim device (getApple, observe, pressButton, listApps,
launchApp, tapOn, swipeOn, terminateApp, openLink, homeScreen, getLogs).

#### What a slim clone still spent its boot on

With the first 62 jobs off, a fresh slim clone still used 115 CPU-s (iOS 27.2) and 54 CPU-s (iOS
18.6) in its first 120 s, spread over ~110 processes, none above 10 CPU-s. Measured per process
(libproc task times, sampled once a second from the boot, each process mapped to its launchd label
by `launchctl list` and the runtime's launchd plists), median of 3 clones of a booted-once
template, 2026-11-27, this host at 1-minute load 5-16. The top of iOS 27.2's list (CPU-s; — : not seen running on 18.6):

| job | 27.2 | 18.6 | |
| --- | --- | --- | --- |
| SpringBoard | 9.4 | 4.3 | kept |
| diagnosticd | 5.6 | 6.1 | kept: the log stream (`logs`) |
| nanotimekitcompaniond | 5.0 | 1.6 | **off**: watch faces, no watch |
| dataaccessd | 4.7 | 0.4 | **off**: Exchange/CalDAV/CardDAV sync, no accounts |
| AuthenticationServicesAgent | 4.5 | — | kept: password AutoFill, passkeys |
| sharingd | 3.1 | 0.8 | kept |
| Calendar (app) | 2.9 | 0.7 | gone with linkd (below) |
| Spotlight (app) | 2.9 | 0.7 | SpringBoard starts it; not a launchd job |
| mobileassetd | 2.7 | 1.4 | kept: keyboard and font assets |
| callservicesd | 2.6 | 0.5 | kept: CallKit; SpringBoard starts it |
| WidgetRenderer_Default (app) | 2.4 | 0.4 | SpringBoard starts it; not a launchd job |
| MTLCompilerService | 2.4 | — | kept |
| InputUI | 2.2 | — | kept: the keyboard |
| MercuryPosterExtension | 2.2 | 0.3 | an extension SpringBoard hosts; not a launchd job |
| donotdisturbd | 2.2 | 0.4 | kept: notification delivery and Focus |

Most of it is start-up: on iOS 27.2 ~106 of the 115 CPU-s fall in the first 15 s. The rest was one
burst 13-38 s after the boot (a different moment each boot): donotdisturbd's Focus-filter
first-launch task asks linkd (App Intents) to run each app's default Focus action, which starts
the Calendar, Safari and Messages intents extensions and Calendar itself, ~8 CPU-s, and lands
inside or just before the 10 s settle window. With linkd off the task finds no extensions and
nothing starts. The profile also turns off nanotimekitcompaniond, nanoprefsyncd (watch),
dataaccessd, Safari's passwordbreachd, activitysharingd, activityawardsd and fitcore (Fitness;
healthd was off already), StatusKitAgent (Focus status sharing: 2.8 CPU-s on 18.6, most of it
after 20 s), avatarsd (Memoji), modelcatalogd and familynotification: 74 jobs in all. In the
first 120 s a clone then used 86 CPU-s on iOS 27.2 (96 processes) and 40 on 18.6, with no CPU after
15 s beyond trustd and the kept daemons' trickle (N=1 each).

Kept though heavy, because app flows reach them: donotdisturbd and liveactivitiesd (notification
delivery, Live Activities), AuthenticationServicesAgent, callservicesd, calaccessd (EventKit),
mobileassetd, identityservicesd and diagnosticd. Spotlight, WidgetRenderer and the poster
extension are started by SpringBoard as apps, not by launchd label, so a profile can't stop them.

`boot --profile slim --settle`, fresh clones of a booted-once template, the 62-job profile against
the 74-job one, 3 interleaved rounds each (2026-11-27, this host at 1-minute load 8-21), median
(runs):

| runtime, settle judged on | | settled | CPU-s to settle | background CPU-s | processes |
| --- | --- | --- | --- | --- | --- |
| iOS 27.2, foreground (default) | 62 jobs | 33.2 s (40.3, 32.2, 33.2) | 112.0 | 25.1 | 114 |
| | 74 jobs | 20.1 s (20.1, 20.1, 22.3) | 82.3 | 8.6 | 95-96 |
| iOS 27.2, all CPU | 62 jobs | 38.9 s (38.9, 26.2, 39.5) | 114.6 | 26.2 | 108-113 |
| | 74 jobs | 30.2 s (20.0, 30.3, 30.2) | 82.0 | 10.3 | 93-94 |
| iOS 18.6, foreground (default) | 62 jobs | 25.5 s (20.8, 30.6, 25.5) | 42.1 | 13.3 | 91-97 |
| | 74 jobs | 19.7 s (29.3, 19.7, 17.5) | 36.6 | 9.4 | 79-84 |
| iOS 18.6, all CPU | 62 jobs | 31.5 s (30.7, 31.5, 31.6) | 44.0 | 14.5 | 97-102 |
| | 74 jobs | 19.4 s (19.4, 19.6, 18.5) | 33.2 | 8.2 | 79 |

On a lean clone of both runtimes an app installed with `install` launched, took typed text from the
keyboard, got location from `location` (37.3349, -122.0090), received a `simctl push` while in the
foreground (banner and delegate), and picked an image added with `simctl addmedia` through
the photos picker (PHPicker), after the notification and location prompts were allowed. The
AutoMobile harness (`scripts/automobile-harness.sh`: getApple, observe, pressButton) passed on
both, and a reboot of the iOS 27.2 device with `--profile slim` settled in 20 s on all CPU (no
spinning client, unlike sleepd).

### Forwarding to `simee serve`

A CLI call that runs in its own process costs several times more than one forwarded to a running
`simee serve` (measured: screenshot 164 vs 37 ms, tap 321 vs 83 ms; 38 ms with 8 devices
driven through one server). The server itself is cheap: 7 MB idle, 73 MB, 16 threads and 32 file
descriptors with one device driven, 124 MB, 14 threads and 32 descriptors with eight (each
device's input worker lets go after five idle minutes). On a farm run every caller with the
same `DEVELOPER_DIR`, `HOME`, `TMPDIR` and `ANDROID_*`/`EMU_*`/`SIMEE_*` settings as the server
(`PATH` doesn't matter), and set `SIMEE_REQUIRE_SERVER=1` so a caller that can't use the server
fails with the reason instead of silently running slower; `--verbose` prints where each command
ran. See docs/api.md.

### Benchmark

```bash
cargo build --release -p simee
scripts/farm-bench-ios.sh 1 2 4 8                                  # fresh devices, one boot each
MODE=clone TEMPLATE=<udid> BOOT=many K=4 BOOT_ARGS=--settle scripts/farm-bench-ios.sh 8
MODE=clone TEMPLATE=<slim udid> BOOT=many BOOT_ARGS=--settle ROUNDS=2 scripts/farm-bench-ios.sh 4
```

One `key=value` line per N and round in `bench/`: wall time until `boot` returned and until all
were settled, each device's settle time, memory (system-wide `vm_stat` delta and `footprint` of the
devices' processes), processes per device, the devices' CPU at settle and 60 s later, disk per
device, failures. Devices are named `perffarm-<label>-<i>`, created for the run and deleted after.

## Android emulators

The same methods drive an AVD farm, with the same names and result shapes:

```bash
# Once: a golden AVD booted under simee and stopped (its settled disks are what clones copy).
simee boot golden && simee shutdown golden
for i in 1 2 3 4; do simee clone golden farm-$i; done   # copy-on-write disks, no first boot
simee boot-many farm-1 farm-2 farm-3 farm-4 -j 2 --first-port 5554
```

`boot_many` on AVDs (`sim_android::Android::boot_many`) starts at most `concurrency` emulators at
once, each on its own console/adb port pair handed out from `first_port` (default 5554) by one
allocator, and waits for each to report `sys.boot_completed`. Each device's result is `device`,
`boot_s`, `console_port` and `serial` (`emulator-<port>`), or `error`; `failed` counts errors and
`unsettled` is always 0. It runs in the calling process (`IN_CALLER`): the emulators are its
children. One call boots one platform's devices.

There is no Android `settle`: sim-android has no settle rule in Rust (the Android bench script,
`scripts/farm-bench-android.sh`, judges it from the host: `sys.boot_completed`, then the emu
process under 60 % of a core for 5 s). `boot` and `boot_many` refuse
the iOS-only `settle`, `settle_*`, `profile`, `shaped` and `env` on an AVD with the reason,
instead of ignoring them.

Per settled device at idle the bench also records `footprint_mb` (`footprint` of the emu process:
its dirty memory, the guest pages it has touched included) and `idle_wakeups_s` (`top -l 2 -s
IDLE_S -stats pid,idlew`: how often the process woke a core out of idle), with their medians, beside
the host CPU and vCPU exits. On a loaded host few cores are idle, so the wakeup count reads low.
