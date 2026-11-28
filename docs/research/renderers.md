# Renderers: MoltenVK vs KosmicKrisp, ANGLE-Metal vs SwANGLE, lavapipe vs SwiftShader

emu renders every guest through gfxstream, and two host drivers sit under it: the **host
Vulkan** (MoltenVK or KosmicKrisp on the GPU; lavapipe or SwiftShader on the CPU) and the
**host GLES**, ANGLE, on Metal or on SwiftShader ("SwANGLE"). Each had been checked for
correctness, but none had been measured against the others. This page measures them per API
and recommends defaults. Issue #355.

**Summary.** The boot tables below are 3 rounds per arm (the cold-init table is 5). On API 36,
warm boots to `sys.boot_completed` took 3.7 to 4.0 s on MoltenVK + ANGLE-Metal, MoltenVK +
SwANGLE and KosmicKrisp + ANGLE-Metal, and 9.46 s on KosmicKrisp + SwANGLE (10.8, 9.5 and
4.8 s). That arm's slow boots are unexplained (footnote 1). Their host load at launch does not
account for them: the 9.5 s boot launched at load 9.7, in the 5 to 15 range of the 3.7 to 4.0 s
boots. Frame pacing, idle CPU and memory are within noise of one another on API 36 for the
MoltenVK arms. SwANGLE on API 30 drops most frames, and `-gpu swiftshader` does not boot on
macOS.

## Method

`scripts/bench/renderers.sh` (registered in `scripts/bench/run-all.sh` as `android-renderers`).

Six arms, each chosen through the environment emu leaves alone when it is already set
(`configure_environment`, `crates/emu-gfxstream/src/lib.rs`) and through `-gpu`:

| Arm | `-gpu` | Environment |
| --- | --- | --- |
| MoltenVK + ANGLE-Metal | `host` | `ANGLE_DEFAULT_PLATFORM=metal` |
| MoltenVK + SwANGLE | `host` | `ANGLE_DEFAULT_PLATFORM=swiftshader` |
| KosmicKrisp + ANGLE-Metal | `host` | `ANDROID_EMU_VK_ICD=kosmickrisp`, `VK_DRIVER_FILES`/`VK_ICD_FILENAMES` = `lib64/vulkan/libkosmickrisp_icd.json`, `ANGLE_DEFAULT_PLATFORM=metal` |
| KosmicKrisp + SwANGLE | `host` | the same with `ANGLE_DEFAULT_PLATFORM=swiftshader` |
| lavapipe | `lavapipe` | none (host GLES as emu picks it) |
| SwiftShader | `swiftshader` | none |

gfxstream itself points the Vulkan loader at the manifest that `ANDROID_EMU_VK_ICD` names:
with `VK_LOADER_DEBUG=driver` the KosmicKrisp arm loads `libvulkan_kosmickrisp.dylib`, and
the default loads `libMoltenVK.dylib`. All the ICDs come from the SDK emulator's
`lib64/vulkan` (37.2.13); nothing was downloaded.

For each API, the script creates one AVD with `emu -create-avd -no-golden` (google_apis arm64,
4 vCPUs, 2 GB, 1080x2400@420). One uncounted setup boot follows. Then, for each round and each arm
in turn (the order is reversed every other round):

- **First boot** (`-wipe-data`): the time from launch until `sys.boot_completed` (polled every
  0.25 s) and until the launcher has focus (`dumpsys window`, polled each second). It also
  records emu's `renderer initialized in` (`-verbose`), the peak RSS of the process tree, and
  the 1-minute host load at launch.
- **Warm boot** (no wipe): the same measures. Then 30 s of settle, then the idle host CPU of the
  process tree over 60 s. Then `scripts/android-jank.sh`: 10 app-drawer swipes up and down on
  the launcher, then the launcher's `dumpsys gfxinfo`. Peak memory is the larger of the boot's
  peak RSS and the RSS after the swipes.
- **Correctness**: two screenshots per warm boot, both under `renderers-shots/` in the output
  directory (`--out`). One is the home screen. The other is Settings search
  (`am start -a com.android.settings.action.SETTINGS_SEARCH`) with Gboard up. The script checks
  that Gboard is shown with `dumpsys input_method`. Each region is scored by the share of its
  dominant colour (a blank buffer is one flat colour: 1.0) and by the share of pixels that
  differ by more than 32 from the lavapipe arm's shot.
- **Cold renderer init**, with no device: `emu -warm-renderer <lib64>` is an API 36 boot's
  renderer init without a guest, timed from launch to exit. Each run uses a copy of emu whose
  embedded bundle id (`dev.simee.emu`, `apps/emu/build.rs`) is rewritten to a new id of the same
  length. Metal then gives that copy an empty shader cache of its own: the first run is cold
  and the second is warm. The script removes that cache afterwards and touches no existing cache.

Each boot held a host boot slot (`--boot-gate` with waaagh's `boot-slot.sh`). Every sample in
the result JSON carries the 1-minute host load.

A run that gives no usable numbers is a failure, not a sample. A boot that never completed, got
no slot, or crashed after `sys.boot_completed` shows its word (`never`, `no-slot`, `crashed`) in
its boot-time row. A crashed run's other numbers are dropped, so a dead process's 0 CPU-seconds
never enters a median, and the note under each table counts the failures per arm.

The script drives one device on `--port` (default 5700, adb 5701). It refuses an odd port, and a
port whose serial or adb port's serial is on the protected list in `scripts/bench/lib.sh`, whether
or not that device is up. It re-checks that the ports are free before each boot, after the wait for
a boot slot, right before the launch. It sends the console quit only to the emulator it started and
only while that process listens on the port. It signals a PID only while the PID still has the
start time and command it had at launch, recorded when the process started (`lib.sh` does the same
for every PID a bench tracks). Its AVDs live in a private `ANDROID_AVD_HOME` under the work
directory, never in `~/.android/avd`. `scripts/bench/test-renderers.sh` and
`scripts/bench/test-lib.sh` test these guards without a device.

```bash
scripts/bench/renderers.sh --cold-only --rounds 5
scripts/bench/renderers.sh --api "36 30" --rounds 3 --settle 30 --window 60 --no-cold-init \
  --boot-gate <waaagh>/skills/orchestrate/scripts/boot-slot.sh
```

Host: an M3 Max (16 CPUs, 128 GB) on macOS 26.6.2, emu at `da98c13`, SDK emulator 37.2.13
renderer, 2026-11-27. Each boot table's value is the median of 3 rounds, with the runs in
parentheses. The cold-init table is the median of 5.

The tables were compiled from the raw runs before `renderers.sh` left crashed runs out of the
medians itself. The one cell that counted a crash, API 30's MoltenVK + ANGLE-Metal idle CPU,
is recomputed below from the runs that finished.

## Cold and warm renderer init

The renderer init of an API 36 boot, without a guest. Median of 5 interleaved rounds; host load
5.1 to 5.3.

| Arm | Metal cache cold | Metal cache warm |
| --- | --- | --- |
| MoltenVK + ANGLE-Metal | 4.46 s (4.45 to 4.48) | 0.34 s |
| MoltenVK + SwANGLE (emu's default for API 34+) | 1.49 s (1.49 to 1.51) | 0.35 s |
| KosmicKrisp + ANGLE-Metal | 5.59 s (5.56 to 5.68) | 0.49 s |
| KosmicKrisp + SwANGLE | 2.60 s (2.58 to 2.61) | 0.47 s |
| lavapipe | 0.45 s | 0.45 s |
| SwiftShader | fails every run (see below) | fails |

KosmicKrisp compiles more Metal than MoltenVK does on a cold cache: about 1.1 s more on top of
ANGLE's own shader library. Warm, it is about 0.13 s slower. The same gap shows in booted runs:
the first KosmicKrisp boot of each AVD initialized in 2.8 s (API 36) and 3.7 s (API 30), and the
later ones in 0.46 to 0.68 s. MoltenVK took 0.32 to 0.49 s.

## API 36 (guest UI on Skia Vulkan, renderer without GL)

Host load 5.1 to 23.8 over the run (most boots launched at 5 to 15).

| Measure | MoltenVK + ANGLE-Metal | MoltenVK + SwANGLE | KosmicKrisp + ANGLE-Metal | KosmicKrisp + SwANGLE | lavapipe | SwiftShader |
| --- | --- | --- | --- | --- | --- | --- |
| First boot, to `sys.boot_completed` | 10.8 s (10.3, 10.8, 12.3) | 11.1 s (11.4, 11.1, 11.1) | 11.0 s (16.0, 11.0, 11.0) | 10.8 s (11.1, 10.8, 10.7) | 12.0 s (16.1, 11.1, 12.0) | never (aborts) |
| First boot, to the launcher | 10.9 s | 11.2 s | 11.1 s | 11.0 s | 12.1 s | — |
| Warm boot, to `sys.boot_completed` | 3.73 s (3.73, 3.73, 4.05) | 4.02 s (4.02, 3.73, 4.02) | 3.74 s (3.74, 3.74, 4.06) | 9.46 s (10.8, 9.5, 4.8)¹ | 4.37 s (4.37, 4.40, 4.30) | never |
| Warm boot, to the launcher | 3.8 s | 4.1 s | 3.8 s | 9.6 s¹ | 4.5 s | — |
| Renderer init in the boot, warm | 321 ms | 327 ms | 465 ms | 497 ms | 431 ms | — |
| Idle host CPU, % of a core | 14.5 | 15.3 | 14.4 | 16.0 | 18.4 | — |
| Swipes: janky frames | 2.1% (2.1, 3.0, 0.5) | 0.5% (3.9, 0.2, 0.5) | 2.2% (2.2, 0, 3.6) | 4.4% (4.4, 10.0, 0.4) | 2.1% (5.7, 2.1, 1.8) | — |
| Swipes: frame time p90 | 5 ms | 5 ms | 6 ms | 6 ms | 12 ms | — |
| Swipes: host CPU, % of a core | 41 | 34 | 37 | 50 | 281 | — |
| Peak RSS, process tree | 3971 MB | 3981 MB | 4031 MB | 4029 MB | 4681 MB | — |

¹ KosmicKrisp + SwANGLE's three warm boots launched at host loads of 20.6 (10.8 s), 9.7 (9.5 s)
and 15 (4.77 s). The other GPU arms' warm boots launched at 5 to 15 and took 3.7 to 4.1 s, so the
load at launch doesn't explain the 9.5 s boot: it launched at 9.7, in the same range. The cause
is unexplained. Not rerun.

## API 30 (GLES over the goldfish pipe, guest GLES on the host's ANGLE)

The host was loaded by other work for most of this run: load 5 to 16 in round 1 (lavapipe 29 to
55, and one KosmicKrisp + SwANGLE sample at 235), then 11 to 146 in rounds 2 and 3. Boot times
held up. Read the frame pacing for its direction only: the SwANGLE arms were janky in every
round at every load.

| Measure | MoltenVK + ANGLE-Metal | MoltenVK + SwANGLE | KosmicKrisp + ANGLE-Metal | KosmicKrisp + SwANGLE | lavapipe | SwiftShader |
| --- | --- | --- | --- | --- | --- | --- |
| First boot, to `sys.boot_completed` | 17.9 s (21.9, 17.5, 17.9) | 17.8 s (18.0, 17.8, 17.7) | 17.7 s (25.8, 17.7, 17.6) | 18.0 s (17.3, 23.2, 18.0) | 18.2 s (18.2, 17.7, 21.7) | never (aborts) |
| First boot, to the launcher | 19.0 s | 19.0 s | 18.9 s | 19.1 s | 19.5 s | — |
| Warm boot, to `sys.boot_completed` | 17.0 s | 17.0 s | 17.0 s | 16.8 s | 17.9 s | never |
| Renderer init in the boot, warm | 447 ms | 372 ms | 521 ms | 512 ms | 582 ms | — |
| Idle host CPU, % of a core | 5.35 (4.0, crashed, 6.7)² | 5.8 | 5.1 | 6.1 | 6.3 | — |
| Swipes: janky frames, round 1 (load 5 to 16) | 11.1% | 93.7% | 17.2% | 97.0% | 36.5% (load 29 to 55) | — |
| Swipes: janky frames, median of the runs that finished | 26.9% (11.1, crashed, 42.8)² | 92.3% | 33.8% | 92.7% | 72.3% | — |
| Swipes: frame time p90 | 25.5 ms | 53 ms | 26 ms | 85 ms | 53 ms | — |
| Swipes: host CPU, % of a core | 50 | 153 | 49 | 148 | 151 | — |
| Peak RSS, process tree | 3907 MB | 4275 MB | 3936 MB | 4294 MB | 4091 MB | — |

² In round 2, MoltenVK + ANGLE-Metal's warm boot crashed with SIGSEGV after
`sys.boot_completed`. It is a failed run, 1 of 3 warm boots for that arm, and is left out of
the medians: the idle CPU cell is the median of 4.0 and 6.7. The crash came at host load 90 and
followed a stream of `Failed to find ColorBuffer: 36` and
`Failed to bind to texture: invalid color buffer` from `emulation_gl.cpp`. That was 1 of 6
ANGLE-Metal warm boots on API 30; KosmicKrisp + ANGLE-Metal had none in 3.

## Correctness

| | MoltenVK + ANGLE-Metal | MoltenVK + SwANGLE | KosmicKrisp + ANGLE-Metal | KosmicKrisp + SwANGLE | lavapipe | SwiftShader |
| --- | --- | --- | --- | --- | --- | --- |
| API 36: launcher icons | draw 3/3 | draw 3/3 | draw 3/3 | draw 3/3 | draw 3/3 | no boot |
| API 36: Gboard keyboard | draws 3/3 | draws 3/3 | draws 3/3 | draws 3/3 | draws 3/3 | no boot |
| API 36: Gboard against lavapipe's | 0% of pixels differ | 0% | 0% | 0% | (reference) | — |
| API 30: launcher icons | draw 2/2 (the round-2 boot crashed) | draw 3/3 | draw 3/3 | draw 3/3 | draw 2/3 (an ANR dialog over the third) | no boot |
| API 30: Gboard keyboard | draws 2/2 | draws 3/3 | draws 3/3 | draws 3/3 | draws 2/3 (the same ANR) | no boot |

On API 36, the dominant colour covers 0.486 of every arm's Gboard region, against 1.0 for a
blank buffer. MoltenVK draws Gboard on API 36: the blank keyboard of the Gboard note in
[android.md](../android.md#performance-notes) is gone now that the UI draws with Skia on Vulkan.
The home screens differ from lavapipe's in 8% of pixels, all in the wallpaper's gradient; the
icons, dock and search bar match.

On API 30, Gboard took a dark theme on some boots and a light one on others, whatever the arm.
That flipped its colour count (34 or 84 to 88), so its pixel difference from lavapipe's shot
doesn't measure correctness there. None of its shots is blank: the dominant colour covers at most
0.70 of the region. The lavapipe shot that had an ANR over it ("Pixel Launcher isn't
responding") was taken at host load 84.

Not checked: a GL app and a Vulkan app of their own. No such APK is in these images, and none
was installed. On API 36 every app's UI is Skia on Vulkan, and on API 30 it is Skia GL over the
pipe, so the launcher and Gboard exercise each stack's main path.

**SwiftShader (`-gpu swiftshader`) doesn't boot on macOS.** On API 36 and API 30, 6 of 6 boots
fail at the renderer's init. gfxstream logs `Failed to find memory type for ColorBuffers`
(`vk_common_operations.cpp`), and emu aborts with SIGABRT before the guest starts. The
device-free init fails the same way. The SDK's SwiftShader, as this renderer sets it up, offers
no memory type gfxstream accepts for color buffers. The Linux numbers in
[benchmarks](../benchmarks.md) ran it under another renderer setup.

## Recommendation: defaults per API

| Guest | Host Vulkan | Host GLES | Change from today |
| --- | --- | --- | --- |
| API 36 and newer (renderer without GL) | MoltenVK | SwANGLE | none |
| API 31 to 33 (GLES encoder on the host's GL) | MoltenVK | ANGLE-Metal | none (not measured here; same GLES path as API 30) |
| API 30 (GLES over the pipe) | MoltenVK | ANGLE-Metal | none |
| API 29, first boot | MoltenVK | SwANGLE (#94) | none (not rerun) |
| No GPU / CI / headless | lavapipe | — | none |

- **API 36: keep MoltenVK + SwANGLE.** Its warm boot, frame pacing, idle CPU and memory match
  ANGLE-Metal's within noise. A cold Metal cache costs it 1.5 s instead of 4.5 s.
- **API 30 to 33: keep ANGLE-Metal.** Their guest GLES runs on the host's ANGLE, and on SwANGLE
  the launcher drops 90% or more of its frames during swipes, at three times the host CPU (150%
  of a core against 50%). Boot times are the same either way.
- **KosmicKrisp is not a better default.** It is equal or slower on every measure here: about
  0.13 s more renderer init warm, about 1.1 s more cold, and about 60 MB more RSS. Its only
  advantage was Gboard on API 36, and MoltenVK draws Gboard there now. The case still open is
  API 34 and 35 phones, where the UI is Skia GL on ANGLE on the guest's Vulkan and Gboard was
  blank on MoltenVK. Measure that with `scripts/bench/renderers.sh --api 34` before switching
  those images to KosmicKrisp.
- **lavapipe stays the fallback.** It boots within 0.7 s of the GPU arms on API 36, but its
  launcher swipes take 281% of a core and it uses about 700 MB more RSS.
- **`-gpu swiftshader` is broken here.** It should fail with a message, or fall back to lavapipe,
  until the color-buffer memory type is fixed. The mode maps to `Icd::SwiftShader` in
  `apps/emu/src/instance.rs:219`, via `crates/emu-avd/src/gpu.rs:43`. The
  [rendering modes table](../android.md#rendering-modes) still offers it "for comparing against
  lavapipe".

## Not measured

- API 33 and 34. Their images are installed (`--api "33 34"`); not run this time.
- API 30's frame pacing on a quiet host, and whether the ANGLE-Metal crash in footnote 2
  recurs without load.
- Why KosmicKrisp + SwANGLE's warm boot on API 36 took 9.5 s once at load 9.7 and 10.8 s at
  load 20.6 (footnote 1). It is unexplained, and a quiet host is the first thing to try.
