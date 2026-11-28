# API 34 and 35 on MoltenVK: the pipelines Metal refuses

An API 34 phone on emu's default renderer (MoltenVK, with ANGLE on SwiftShader as the host GLES)
logged 15,303 `[mvk-error] Render pipeline compile failed` lines in one boot and session. On that
boot the #351 touch-to-pixel probe saw no changed frame for any of its 60 taps, and #355 saw
Gboard blank on API 34 and 35. API 36 logs none of these errors. Issue #412.

**Summary.** Every one of the failures is the same: the guest's ANGLE builds a pipeline whose
vertex input format is a float format, while the shader reads that input as `uvec2`. Vulkan
allows this; the values are just undefined. Metal refuses to compile it, so MoltenVK fails the
pipeline and the draw is lost. No MoltenVK setting and no gfxstream feature changes that.
KosmicKrisp, Mesa's Vulkan on Metal, which the SDK ships beside MoltenVK, compiles the same
pipeline. So emu now runs these guests on KosmicKrisp. API 36 and newer, and API 30 to 33, stay
on MoltenVK.

## The log

The boot log comes from the 2026-11-28 device batch (M3 Max, origin/main `04b667a`, SDK
emulator renderer, `-verbose`). It has 30,830 lines. All 15,303 `mvk-error` lines carry the same
detail:

```text
[mvk-error] VK_ERROR_INITIALIZATION_FAILED: Render pipeline compile failed (Error code 2):
Vertex attribute m_14(2) of type uint2 cannot be read using MTLAttributeFormatFloat4.
```

The first one comes right after `com.android.systemui` creates its `VkDevice` (`engine:'ANGLE'`).
They continue for the rest of the session. No other MoltenVK error, and no shader-conversion or
SPIR-V capability error, appears. The renderer ran with `GuestVulkanOnly` (without GL), so every
guest draw reached MoltenVK as a guest Vulkan pipeline. The host's own GLES draws nothing on
this path.

## Why only API 34 and 35

What the guest's UI draws with depends on the image (`guest_graphics`, `default_egl` in
`apps/emu/src/instance.rs`; the Gboard note in [android.md](../android.md#performance-notes)):

| Guest | UI toolkit (HWUI) | What reaches the host |
| --- | --- | --- |
| API 30 to 33 | Skia GL on the GLES encoder | GLES calls, on the host's ANGLE |
| API 34 and 35 phones, Automotive 34/35, google-xr 34 | Skia GL on the guest's ANGLE | Vulkan pipelines that the guest's ANGLE builds |
| API 36 and newer (`VulkanVirtualQueue`) | Skia on Vulkan (`skiavk`) | Vulkan pipelines that Skia builds |

Only the middle row puts ANGLE's translated pipelines on the host's Vulkan, and only that row
fails. The `MTLAttributeFormatFloat4` in the message is MoltenVK's mapping of
`VK_FORMAT_R32G32B32A32_SFLOAT`. The likeliest source is ANGLE's current-value attribute. ANGLE
feeds a vertex attribute whose array is disabled from a four-float buffer, and it keeps that
float format when the program declares the attribute as an unsigned integer vector. That source
is a hypothesis: the guest program and its shader were not captured (see "Device check").

## Reproduced on the host, without a device

[`api34-moltenvk-probe.c`](api34-moltenvk-probe.c) builds two pipelines on the host's Vulkan.
Location 0 is a `vec2` and location 1 a `vec4`, as in the guest's shader, and location 2 is
`layout(location=2) in uvec2 tex;` (compiled with the SDK's `glslangValidator`). Location 2 is
fed first with `VK_FORMAT_R32G32B32A32_SFLOAT`, then with `VK_FORMAT_R16G16_UINT` as a control.
The drivers are the ones in the SDK emulator's `lib64/vulkan`, on an M3 Max with macOS 26.6.2.

| Driver | float format for `uvec2` | matching `R16G16_UINT` |
| --- | --- | --- |
| MoltenVK (emu's `MVK_CONFIG_*`) | `VK_ERROR_INITIALIZATION_FAILED`: "Vertex attribute tex(2) of type uint2 cannot be read using MTLAttributeFormatFloat4." | ok |
| KosmicKrisp | ok | ok |
| lavapipe | ok | ok |

MoltenVK's message is the guest's message word for word, apart from the variable name.

**No MoltenVK setting avoids it.** The same pipeline also fails with
`MVK_CONFIG_USE_METAL_ARGUMENT_BUFFERS=1`, `MVK_CONFIG_USE_METAL_PRIVATE_API=1` and
`MVK_CONFIG_FAST_MATH_ENABLED=0`. None of the 43 `MVK_CONFIG_*` names in the shipped
`libMoltenVK.dylib` covers vertex formats. emu sets three of them (`configure_environment`,
`crates/emu-gfxstream/src/lib.rs`): argument buffers off, semaphore style 0 and `MTLHeap` on.
None of them is involved here.

**No gfxstream feature avoids it.** The renderer's feature list in the boot log has no
feature that rewrites the vertex input of a guest pipeline. gfxstream passes
`vkCreateGraphicsPipelines` through to the driver.

## Options considered

- **A MoltenVK setting or a gfxstream feature.** There isn't one (above).
- **ANGLE on Metal as the host GLES for API 34 and 35** (what API 30 to 33 use). The host's
  GLES is not on this path. These guests run the renderer without GL, and the failing
  pipelines are the guest's Vulkan pipelines on the host's Vulkan driver, so changing the
  host's ANGLE backend leaves them as they are.
- **The GLES encoder for API 34 and 35**, as stock gives these images without `GuestAngle`. Its
  GLES rides the virtio-gpu pipe stream. There, API 34 and newer boots took 14 to 28 s with
  ANRs, against 5 to 8 s on ANGLE (the comment above `guest_graphics`' call in `instance.rs`).
- **`skiavk` for API 34 and 35**, as API 36 gets. HWUI on Vulkan asserts a graphics queue family
  with two queues (`VulkanVirtualQueue`), which these images don't advertise. API 35 also
  hard-codes RenderEngine to `skiaglthreaded`. And an app's own GLES would still go through the
  guest's ANGLE.
- **KosmicKrisp for these guests.** It compiles the pipeline, ships in the same `lib64/vulkan`,
  and gfxstream already loads it by name (`ANDROID_EMU_VK_ICD=kosmickrisp`, the #355 arms).
  The [renderers](renderers.md) runs measured its cost on API 36 and 30: about 0.13 s more
  renderer init with a warm Metal cache, about 1.1 s more with a cold one, and about 60 MB more
  RSS. KosmicKrisp also drew Gboard on the same Skia-GL-on-ANGLE stack when API 36 still ran
  it ([android.md](../android.md#performance-notes), "Gboard's blank keyboard").

## The change

`-gpu host` (and `auto`, the default) picks the host-GPU driver per guest stack. The pure choice
is `emu_gfxstream::gpu_icd`, and `host_gpu_icd` in `apps/emu/src/instance.rs` feeds it the AVD:

- an API 34 or 35 guest on ANGLE (bootconfig image, guest GLES `angle`) whose image doesn't
  advertise `VulkanVirtualQueue` (so HWUI is Skia GL) runs on KosmicKrisp. These are API 34 and
  35 phones, Google TV 34, Automotive 34/35 and google-xr 34. The renderer's `lib64/vulkan` must
  ship `libkosmickrisp_icd.json`, and the `emu -probe-kosmickrisp` child must bring Vulkan up.
  `-verbose` says so once.
- everything else stays on MoltenVK: API 30 to 33, API 36 and newer (Wear OS 36 too, although
  its image has no `VulkanVirtualQueue`: unchecked on a device), `EMU_GUEST_EGL=emulation`, and a
  renderer without KosmicKrisp. If KosmicKrisp fails its probe, the boot uses MoltenVK and
  prints a notice.
- `ANDROID_EMU_VK_ICD=moltenvk` or `=kosmickrisp` picks the driver outright; KosmicKrisp is used
  only where it ships.

Whatever the environment said, gfxstream then gets the driver emu chose:
`configure_environment` always sets `ANDROID_EMU_VK_ICD`, `VK_DRIVER_FILES` and
`VK_ICD_FILENAMES` to it, and prints `ANDROID_EMU_VK_ICD=<name> honoured` or `... overridden`
when the user had set one. So `=kosmickrisp` on a host where KosmicKrisp failed its probe, or a
software or unknown name under `-gpu host`, no longer reaches gfxstream (software Vulkan is
`-gpu lavapipe` or `-gpu swiftshader`). Every other variable (`ANGLE_DEFAULT_PLATFORM`,
`MVK_CONFIG_*`) is still set only when unset.

`Icd::KosmicKrisp` gets the loader's manifest and `ANDROID_EMU_VK_ICD`, and none of MoltenVK's
`MVK_CONFIG_*`. The host GLES choice (`host_angle`) is unchanged: ANGLE on SwiftShader for a
renderer without GL. `-gpu lavapipe`, `software` and `off` are unchanged.

The probes (`apps/emu/src/vkprobe.rs`):

- each child has 60 s, so a slow probe on a loaded host still gets its verdict. At the deadline it is killed by its PID and reaped, and the boot goes
  on: MoltenVK in place of KosmicKrisp, lavapipe in place of MoltenVK, with a notice.
- only a clean verdict is cached: exit 0 (Vulkan came up) or 3 (it didn't, `PROBE_FAILED`). A
  timeout, a signal (a crash, jetsam, a process group's SIGTERM) or any other status (a panic's
  101) is not cached, and the next boot probes again.
- a child gets none of the inherited `VK_*`, `MVK_*`, `MESA_*` and `ANDROID_EMU_VK_*`
  variables nor `ANDROID_EMULATOR_LAUNCHER_DIR`, only its own driver's manifest and name, so a
  user's or a bench arm's driver can't become the cached verdict of another.
- the verdict is keyed by the backend and by the driver's own library
  (`libvulkan_kosmickrisp.dylib`, `libMoltenVK.dylib`), and written beside its file and renamed
  over it: a boot reading it mid-write sees no verdict rather than `fail`.
- `emu -provision-renderer` probes KosmicKrisp too, where the renderer ships it.

Tests: `gpu_icd`, `Icd::from_name`, the environment per driver, and the environment gfxstream
sees for each driver over a user's `ANDROID_EMU_VK_ICD` in `emu-gfxstream`. In `apps/emu`: the
choice per API (30, 33, 34, 35, 36, TV 34, Wear 36, Automotive 34, google-xr 34, the encoder,
the override, a renderer without KosmicKrisp), the fallback when KosmicKrisp's probe fails or is
unclear, and the probe against fake children that hang, crash, are killed or exit cleanly, the
environment a probe child sees, and the cache key and file.

## Caveats

- `emu -provision-renderer` warms MoltenVK's Metal shaders and runs KosmicKrisp's probe, not
  an API 34 boot's whole init. The first API 34 or 35 boot on a cold cache may still compile
  KosmicKrisp's shaders during the renderer's init. #355 measured 2.6 s cold for that init with
  ANGLE on SwiftShader, against 1.5 s on MoltenVK.
- Concurrent first boots on one renderer build each run their own probe (no lock); each writes
  the same verdict.
- A KosmicKrisp probe that hangs on every boot costs each API 34 or 35 boot its 60 s before
  MoltenVK; one that always crashes costs about 1 s. Neither is cached, by design.
- #355 saw two slow warm boots on KosmicKrisp with ANGLE on SwiftShader on API 36 (9.5 and
  10.8 s, against 3.7 to 4.1 s). The cause is unexplained (its footnote 1). Watch API 34's warm
  boot time.
- `scripts/bench/renderers.sh`'s MoltenVK arms set `ANDROID_EMU_VK_ICD=moltenvk`, so on API 34
  and 35 they measure MoltenVK.
- API 36's apps that draw with their own GLES still go through the guest's ANGLE on MoltenVK.
  Its launcher, Gboard and Settings draw with Skia on Vulkan and were clean.

## Device check (on main, after merge)

On an API 34 `google_apis` arm64 phone (1080x2400@420), and then the same on API 35:

1. Boot it on the default renderer: `emu -avd <avd> -verbose -port <port> 2>&1 | tee boot.log`.
   Expect the line `host Vulkan is KosmicKrisp` once, and
   `Graphics Adapter Vendor` not `MoltenVK`. `VK_LOADER_DEBUG=driver` shows
   `libvulkan_kosmickrisp.dylib`.
2. `grep -c mvk-error boot.log` gives 0 after boot, Settings and Gboard. Grep for KosmicKrisp's
   own errors too (`grep -iE 'kk_|mesa|VK_ERROR' boot.log`): expect none.
3. Run the #351 probe on API 34 with an A/B arm:
   `scripts/bench/input-latency.sh --platform android --image system-images/android-34/google_apis/arm64-v8a --arms "emu emu:ANDROID_EMU_VK_ICD=moltenvk" --rounds 1 --taps 60`.
   Expect `tap -> first changed frame seen n=60` on the `emu` arm, and `n=0` on the MoltenVK arm,
   which reproduces #412.
4. Gboard: `adb shell am start -a com.android.settings.action.SETTINGS_SEARCH`, then
   `adb shell dumpsys input_method | grep mInputShown=true`, then `adb exec-out screencap -p`.
   Expect keys drawn: the dominant colour of the keyboard region is under 1.0, scored the way
   `renderers.sh` scores it.
5. An API 36 phone: no `host Vulkan is KosmicKrisp` line, `Graphics Adapter Vendor MoltenVK`,
   0 `mvk-error`, warm boot within its 3.7 to 4.1 s.
6. Note API 34's warm boot time and renderer init (`renderer initialized in`) against MoltenVK's.
7. A Google TV 34 AVD (`system-images/android-34/android-tv/arm64-v8a`): as steps 1 and 2 (the
   `host Vulkan is KosmicKrisp` line, 0 `mvk-error`), and the home screen drawn
   (`adb exec-out screencap -p`, more than one colour).
8. A Wear OS 36 AVD (`system-images/android-36/android-wear-signed/arm64-v8a`): as step 5 (no
   KosmicKrisp line, MoltenVK). Note whether its UI draws and its `mvk-error` count: if it
   freezes like #412, it is a candidate for KosmicKrisp in a follow-up.
9. `ANDROID_EMU_VK_ICD=lavapipe emu -avd <api34> -verbose ...`: expect
   `ANDROID_EMU_VK_ICD=lavapipe overridden: gfxstream gets kosmickrisp` and
   `VK_LOADER_DEBUG=driver` showing `libvulkan_kosmickrisp.dylib`.
