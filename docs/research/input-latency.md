# Input latency: tap to first changed frame

How long a tap sent through `simee serve` takes to show up in a frame that simee's own frame
clients see, on both platforms, split into the stages that can be told apart from the host
(and, on Android, from the guest's own clocks). Issue #201.

## Method

`crates/sim-api/examples/input_latency.rs` (run by `scripts/bench/input-latency.sh`) keeps one
API connection to `simee serve` and one frame client of the device, and for each tap:

1. waits until a 64x64 region around the touch point has been still for 300 ms and takes it as
   the baseline;
2. times a no-op `identity` call (the API's own round trip on the kept connection);
3. sends `touch` down at a known `Instant` (t0) and records when the reply came back;
4. records when the first frame whose region differs from the baseline arrived at the frame
   client, and how many unchanged frames came before it;
5. sends `touch` up after 60 ms.

Three warm-up taps are dropped; each round is 60 taps. The frame client is what simee's own
readers use:

- **Android**: a client of emu's display socket (`<avd>/simee/display.sock`) taking frames from
  the shared ring at full size (`sim_android::display::Frames`, as the desktop window and the
  H.264 source read them). A frame's arrival time is when its ring notice and pixels have been
  read (the `pread` of a 1080x2400 frame is part of it).
- **iOS**: the framebuffer's damage-rectangle callback (`Display::watch_frames`, which the desktop
  and the encoder follow); the callback's time is the frame's arrival, and the framebuffer is then
  copied to compare the region (a copy during which the surface's seed changed is dropped and left
  to the next callback).

What changes on touch down, and nowhere else:

- **Android**: the pointer location overlay (`settings put system pointer_location 1`), a
  system window that draws crosshairs through each touch as soon as the touch is dispatched to it,
  over the launcher's empty workspace at (700, 1500). No tap timeout delays it (list items in a
  scrolling container show their pressed state only after `ViewConfiguration.getTapTimeout`, 100
  ms).
- **iOS**: the `g` key of the Spotlight keyboard at (600, 2015) on an iPhone 17, whose preview
  balloon shows on touch down. A table row or a home screen icon would be delayed by its scroll
  view's `delaysContentTouches`, or launch an app on touch up.

Guest side (Android only): `adb shell -tt getevent -lt /dev/input/event0` (over a pty, so each line
arrives as it is printed) gives the kernel's timestamp of each touch down (the multi-touch
screen's `ABS_MT_TRACKING_ID`; emu's screen sends no `BTN_TOUCH`), and `dumpsys SurfaceFlinger
--latency 'PointerLocation - display 0#N'` every 20 taps gives the overlay layer's actual present
times. Both are the guest's `CLOCK_MONOTONIC`, so "kernel event to present" needs no clock
mapping. iOS has no cheap equivalent and is measured from the host only.

Host: Mac15,9 (M3 Max, 16 CPUs, 128 GB), macOS 26.6.2, simee 30636fb, 2026-11-27. Android:
API 36 `google_apis` arm64, 1080x2400@420, 4 vCPUs, 3 GB, a fresh `-no-golden` AVD warm from its
first boot, 60 s settle. iOS: iPhone 17, iOS 27.2 (24B8117), 30 s settle. 2 rounds of 60 taps per
arm (120 taps). Host load (`spectra load`, 1-minute): Android 38.7 at the start (other lanes'
builds) and 4.8 at the end; iOS 8.9 at the start and 58.8 at the end (Spotlight indexing on the
host, `mds_stores` and `fseventsd`, after the simulator was created). An earlier iOS run (load 5.0
to 111.7) gave 33.7 / 42.6 / 53.8 ms (median / p90 / p99): the same median, longer tails.

```bash
scripts/bench/input-latency.sh --platform android --arms "emu emu:EMU_RENDERER_GL=1" --rounds 2 --taps 60
scripts/bench/input-latency.sh --platform ios --rounds 2 --taps 60
```

## Results

Tap to first changed frame, 120 taps each:

| Device | Median | p90 | p99 |
| --- | --- | --- | --- |
| Android, emu (default: Vulkan-only renderer) | 39.1 ms | 51.9 ms | 58.2 ms |
| Android, emu with `EMU_RENDERER_GL=1` | 30.7 ms | 39.1 ms | 74.7 ms |
| iOS, iPhone 17 | 33.2 ms | 36.8 ms | 44.3 ms |

No tap went without a changed frame within 3 s. Round medians: Android 41.2 and 36.9 ms (GL arm
29.9 and 31.1), iOS 33.0 and 33.2 ms.

### Android stages (emu, default)

| Stage | Median | p90 | p99 | What it is |
| --- | --- | --- | --- | --- |
| API round trip (`identity`) | 0.04 ms | 0.07 ms | 0.10 ms | the kept connection's own cost |
| `touch` down reply | 0.36 ms | 0.39 ms | 0.45 ms | API receive, device lookup, the `touch` line written to the display socket, reply |
| tap to the guest kernel's event | ≤ 0.90 ms | ≤ 1.25 ms | ≤ 1.37 ms | upper bound: when getevent's line reached the host (adb's pty adds its own delay) |
| guest: kernel event to SurfaceFlinger present | 23.5 ms | 31.1 ms | 33.1 ms | input dispatch, the overlay's draw on the next vsync, composition on the one after |
| host: tap to kernel event + present to seen | 11.9 ms | 27.5 ms | 33.8 ms | total minus the guest's part |
| **total** | 39.1 ms | 51.9 ms | 58.2 ms | |

The two host legs can be separated because each boot has one fixed offset between the guest's
clock and the host's: it is at most `guest_down - t0` for every tap and at least `guest_present -
seen` for every tap. In both rounds that window is 5.1 ms wide, and the guest's touch timestamp
trails t0 by the same amount within 0.06 ms (median; p90 0.09 ms) on every tap. So the tap reaches
the guest kernel after a constant delay, and all of the variation in the host's 11.9 ms is between
SurfaceFlinger's present and simee seeing the frame: from 5.1 ms to 30 ms, median 11.6 ms with the
whole window charged to that side.

Two things say where that variation comes from. Every tap's changed frame was preceded by 1 (72
taps) or 2 (48 taps) frames whose region had not changed: frame notices that came before the new
frame was posted. And emu's own stream statistics (`EMU_FRAME_STATS=1`) put reading a frame back
and copying it into the ring at 0.98 ms median (3.35 ms max), so the copy isn't it.

The guest renders on ANGLE/Vulkan, so emu runs the renderer without GL
(`crates/emu-vmm/src/machine.rs:182-193`), and a renderer without GL has no post callback: frame
notices come from `PostTicker` (`crates/emu-vmm/src/display.rs:568-713`), poked by every renderer
fence (`crates/emu-vmm/src/machine.rs:213-224`). After an idle period the first fence gives a
notice at once, before host composition has posted the new frame (it posts on the renderer's post
worker after the composer's fence), so readers read back the old frame; the thread then sleeps the
whole 16 ms interval (`display.rs:710`, `POST_NOTICE_INTERVAL` at `machine.rs:54`) before the next
notice, and the new frame waits for it.

### The same with the renderer's post callback (`EMU_RENDERER_GL=1`)

`EMU_RENDERER_GL=1` (still read at 30636fb: `gl_requested`, `crates/emu-vmm/src/machine.rs:163-170`)
keeps GL in the renderer, and with it gfxstream's post callback, so a notice follows each post:

| Stage | Median | p90 | p99 |
| --- | --- | --- | --- |
| `touch` down reply | 0.38 ms | 0.53 ms | 1.38 ms |
| guest: kernel event to present | 24.6 ms | 31.9 ms | 74.8 ms |
| host: tap to kernel event + present to seen | 5.8 ms | 8.7 ms | 11.6 ms |
| **total** | 30.7 ms | 39.1 ms | 74.7 ms |

118 of 120 changed frames were the first frame after the tap. The guest's part is unchanged
(23.5 against 24.6 ms median); the host's falls from 11.9 to 5.8 ms median and from 27.5 to 8.7 ms
at p90. One tap's guest stage ran to 75 ms (the p99s). The GL renderer is not the fix: it copies
every frame twice on the CPU and costs about twice the host CPU and more janky frames
(docs/benchmarks.md "Frame pacing": phone 63% against 33% of a core). It shows what a notice per
post is worth.

### iOS stages

| Stage | Median | p90 | p99 |
| --- | --- | --- | --- |
| API round trip (`identity`) | 0.04 ms | 0.06 ms | 0.07 ms |
| `touch` down reply (API receive, the HID message sent) | 0.08 ms | 0.12 ms | 0.17 ms |
| HID message sent to the damage callback of the changed frame | 33.0 ms | 36.7 ms | 44.2 ms |
| **total** | 33.2 ms | 36.8 ms | 44.3 ms |

118 of 120 changed frames were the first callback after the tap. The third stage is the device's
own: backboardd's event delivery, the keyboard drawing the balloon on the next frame, the render
server presenting it into the framebuffer at 60 Hz (two frame intervals are 33.3 ms). The
`touch` reply comes back once the HID message is handed to the HID client (a server sends without
a completion handler: `sim_coresim::track_sends`, `crates/sim-coresim/src/hid.rs:118-121`), so its
delivery is inside that stage; nothing in simee's path up to the send is above 0.2 ms at p99.

## The largest controllable stage

On iOS every stage simee controls is under 0.2 ms at p99: there is nothing in simee's path to fix.

On Android the guest's 23.5 ms (dispatch and two vsyncs) is Android's own pipeline. The largest
stage simee controls is present to seen, 11.9 ms median and 27.5 ms p90 in a 39.1 ms tap: frame
notices paced by renderer fences instead of posts. Proposed fix (crate source, not changed here):

- give `PostTicker` (`crates/emu-vmm/src/display.rs:599`) a probe for "has the renderer posted a
  new frame": `Renderer::last_posted_color_buffer` (`crates/emu-gfxstream/src/lib.rs:1050`, one
  call through gfxstream's `FrameBuffer`, no readback), passed in where the ticker is made
  (`crates/emu-vmm/src/machine.rs:213-214`);
- in `PostTicker::run` (`display.rs:661-712`), while pokes are recent, poll that probe every 1 ms
  instead of sleeping 16 ms after each notice, and give a notice only when the posted color buffer
  changes; keep the 50 ms tail. Host composition is expected to cycle through its swapchain's color buffers, so
  each post changes the handle; a renderer that posts the same buffer again would need a post
  counter, and the current fence-paced notice can stay as the fallback when the probe errs.

Expected, from the GL arm: present to seen from 11.9 / 27.5 ms (median / p90) to about 5.8 /
8.7 ms, so tap to frame from about 39 to about 31 ms median and 52 to 39 ms p90, with no unchanged
frame read back before each changed one (today 1 to 2 per tap, a full 1080x2400 readback each), and
without the GL renderer's CPU cost. Whether the color buffer handle changes on every post has to be
checked on a device first (`last_posted_color_buffer` logged per notice).

## Raw data

The CSVs (one row per tap: `t0_s`, `noop_ms`, `touch_reply_ms`, `frame_ms`, `unchanged_frames`,
`frame`, and on Android `guest_down_s`, `guest_present_s`, `guest_ms`, `getevent_line_ms`), the
script's JSON summaries and the `spectra load` snapshots are in the lane's state directory,
`~/.claude/orchestrate/kaeawc__simee/lat201/csv/` (`input-latency-20261127-142357-*` for Android,
`input-latency-20261127-143702-*` for iOS; `input-latency-20261127-142957-*` is the earlier, more
loaded iOS run).
