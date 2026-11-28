# Streaming

Device video over WebRTC: simee publishes each device's screen as H.264 over WHIP to a media
server (MediaMTX), and browsers play it over WHEP. The plan is epic #262; this page describes what
exists and what comes next.

## `sim-rtc`: the WHIP publisher

`crates/sim-rtc` (published as `simee-rtc`) takes an H.264 elementary stream (Annex B access
units with presentation times) and publishes it to a WHIP endpoint. It has no async runtime: the
WHIP requests block the caller, and the media path is one thread per publisher that sleeps on its
UDP socket.

```rust
let config = PublisherConfig::new("http://127.0.0.1:8889/device-1/whip");
let mut publisher = Publisher::connect(config, |event| match event {
    PublisherEvent::KeyframeRequested(_) => { /* encode an IDR next */ }
    PublisherEvent::BitrateEstimate { bps, .. } => { /* retarget the encoder */ }
    _ => {}
})?;
publisher.send(&access_unit, pts)?; // Annex B, one picture
// drop(publisher) or publisher.close(): DELETE the WHIP resource
```

| Module | What it does |
| --- | --- |
| `whip` | RFC 9725: POST the SDP offer (`application/sdp`), `201 Created` with `Location` (resolved against the endpoint) and `ETag`, up to 3 redirects, PATCH trickle ICE (`application/trickle-ice-sdpfrag`, `If-Match`; `405`/`501` means the server doesn't trickle), DELETE to end (`404` counts as done), `Authorization: Bearer` |
| `http`, `url` | A blocking HTTP/1.1 client on `std::net`: one deadline for the whole exchange, `Content-Length`, chunked and read-to-close bodies, size limits, no header injection |
| `h264` | Annex B splitting, access units, the SPS `profile-level-id`, the check that a stream may go out on the negotiated one, and the cached SPS/PPS put in front of every IDR |
| `rtp` | RFC 6184 packetization mode 1: single NAL unit packets and FU-A fragments at the MTU (1200 by default), the marker on each access unit's last packet, 90 kHz timestamps from the PTS; a depacketizer (also STAP-A) for tests and receivers |
| `sdp` | The `profile-level-id` the answer settled on; trickle ICE fragments |
| `publisher` | `Publisher`: str0m in RTP mode with one send-only H.264 format, one host candidate, the media thread, the keyframe-request throttle and the encoder callback |
| `datachannel` | The `simee-input` data channel protocol: viewer input decoded and validated, rate limited, moves coalesced, handed to an `InputSink`; acks that echo the viewer's timestamp (see "Input over a data channel") |

Rules it enforces:

- **Plaintext WHIP only to loopback.** `http://` to any other host is refused unless
  `allow_insecure_http` is set (a trusted LAN): the bearer token and the DTLS fingerprints that
  authenticate the media would cross the network in the clear. The `Location` and redirects are
  held to the same rule. `https://` is refused for now (see below).
- **The SPS must match what was negotiated.** The offer carries the configured
  `profile-level-id` (Constrained Baseline `42e01f` by default; Main `4d001f` and any other value
  work); the answer's is the session's. Every SPS sent must have the same `profile_idc` and a level
  no higher, or `send` refuses the access unit (`Error::ProfileMismatch`). A Baseline session
  takes Baseline with or without `constraint_set1`, as AutoMobile learned from Android encoders
  that emit `42 80 xx`.
- **Every IDR goes with its SPS and PPS**, from the access unit or the last ones seen, so a viewer
  that joins late or recovers from loss can decode it. Access units before the first IDR are
  dropped (`Sent::WaitingForKeyframe`); an IDR before any SPS/PPS is refused.
- **Keyframe requests reach the encoder, throttled.** PLI and FIR become
  `PublisherEvent::KeyframeRequested`, at most one per `keyframe_interval` (500 ms); requests
  inside it are coalesced into one at its end, never dropped.
- **Bandwidth estimates reach the encoder.** TWCC estimation (str0m's, on by default from 1 Mbit/s,
  probing up to `max_bitrate_bps`) and REMB from the server become
  `PublisherEvent::BitrateEstimate`. NACKs are answered from str0m's RTX cache.
- **The resource is deleted** when the publisher is dropped or closed, and when `connect` fails
  after the POST.

### Why str0m

[str0m](https://github.com/algesten/str0m) is sans-IO WebRTC: ICE, DTLS-SRTP, RTP/RTCP, NACK/RTX,
TWCC and the pacer as a state machine that the caller feeds with datagrams and time. That fits
simee, which has no async runtime (the MCP server avoided tokio for the same reason,
[benchmarks.md](benchmarks.md#mcp-server)). webrtc-rs needs tokio. Dependency trees, from a
scratch package each (`cargo tree -e normal --prefix none | sort -u | wc -l`, 2026-11-27):

| Stack | Crates in the normal tree |
| --- | --- |
| `simee-rtc` on str0m 0.26, macOS (CryptoKit backend) | 70 (80 with build dependencies) |
| `simee-rtc` on str0m 0.26, Linux, default features | 52 |
| `simee-rtc` on str0m 0.26, Linux, `--features rust-crypto` | 109 |
| `webrtc` 0.21 (webrtc-rs), default features | 219, tokio among them |

The publisher uses str0m's RTP mode and packetizes itself, so the MTU, the marker bit and the
SPS/PPS-before-IDR rule are ours and tested here; str0m does SRTP, the RTX cache and the pacer.
The counts include the root package.

### DTLS backends

str0m needs a crypto backend for DTLS. sim-rtc uses CryptoKit (`str0m-apple-crypto`) on Apple
targets, which builds no C. On Linux the pure-Rust backend (`--features rust-crypto`) still
generates its certificate with `rcgen`, which builds aws-lc's C; that would break
`cargo clippy --target x86_64-unknown-linux-gnu` from macOS (no C cross toolchain), so it's
opt-in. Without it, `Publisher::connect` is `Error::Unsupported` and the publisher's end-to-end
tests print SKIP; everything else (WHIP, HTTP, H.264, RTP) builds and tests everywhere.

### Tests

`cargo test -p simee-rtc`. The end-to-end tests (`tests/publish.rs`) run a fake WHIP server on
`std::net` whose answer comes from a str0m receiver on loopback UDP: offer and answer, ICE and
DTLS, every access unit back intact (FU-A included) with SPS/PPS before each IDR, PLI to the
callback, a bandwidth estimate, DELETE on drop, and refusals (profile mismatch, IDR without
parameter sets, a rejected answer). The fixtures are ten 96x64 frames of ffmpeg's `testsrc`
encoded by libx264 (Constrained Baseline `42c01e` and Main `4d401f`, two GOPs, about 3 KB each):

```bash
ffmpeg -f lavfi -i testsrc=size=96x64:rate=10 -frames:v 10 -c:v libx264 -profile:v baseline \
  -level 3.0 -g 5 -keyint_min 5 -sc_threshold 0 -bf 0 -pix_fmt yuv420p -x264-params slices=1 \
  -f h264 testsrc-baseline.h264   # -profile:v main -level 3.1 for testsrc-main.h264
```

`publishes_to_a_local_mediamtx` (ignored) publishes the fixture to a running MediaMTX
(`SIMEE_WHIP_URL`, default `http://127.0.0.1:8889/simee-test/whip`).

### The MediaMTX harness (#267)

`tests/mediamtx/mediamtx.yml` is the test config: WHIP and WHEP on loopback, the control API on,
`webrtcTrackGatherTimeout: 30s` (AutoMobile measured first frames up to 15 s on a device; the 2 s
default closes the publisher first), host candidates only (no STUN), RTSP, RTMP, HLS, SRT, MoQ,
metrics and pprof off, and anonymous publish, read and API from loopback only. It is valid on its
own (ports 8889, 9997 and 8189); the harness and the script start it on free ports through
MediaMTX's `MTX_*` environment overrides instead.

- **Finding MediaMTX.** `$MEDIAMTX_BIN`, else `mediamtx` on PATH. Nothing is downloaded by the
  tests or `scripts/mediamtx.sh` (`scripts/mediamtx-fetch.sh` fetches the pinned release, "In CI"
  below); with no binary the MediaMTX tests print `SKIP: no MediaMTX ...` and pass, unless
  `SIMEE_REQUIRE_MEDIAMTX` is set.
- **Rust.** `crates/sim-rtc/tests/common/mod.rs` has `MediaMtx`: it writes the config to a temp dir,
  starts the server on free ports, waits for `GET /v3/config/global/get` (failing with the log when
  the process dies or stalls), exposes `api_get` and `ready_path`, and on drop kills the child by
  the PID it spawned, removes the temp dir and, if the test is panicking, prints the server log.
  `tests/mediamtx.rs` publishes both fixtures (Constrained Baseline and Main) with the publisher
  and checks through the API that the path is `ready` with an `H264` track of the fixture's profile.
- **By hand.** `scripts/mediamtx.sh start` prints `WHIP_URL`, `WHEP_URL`, `API_URL`, the PID and
  the log path; `stop` kills the recorded PID; `status` reports it. `MEDIAMTX_STATE` moves its
  state dir. Like the Rust tests, the script exits 3 with a message when there is no binary.

Run it: `MEDIAMTX_BIN=/path/to/mediamtx cargo test -p simee-rtc --test mediamtx` (on Linux add
`--features rust-crypto`, or the publisher tests skip for lack of DTLS; the harness test still runs).

#### In CI

CI's `mediamtx` job (`.github/workflows/ci.yml`, and the macOS `lint-test` job for the same MediaMTX tests) runs all of `simee-rtc`'s tests with
`--features rust-crypto` against MediaMTX v1.19.2, so the publisher's end-to-end tests (WHIP to
the fake server, the input channel, MediaMTX) run on Linux; the `linux` job builds without a DTLS
backend, where they skip. `scripts/mediamtx-fetch.sh` is the one place with the pin: the version
and the SHA-256 of each release archive, taken from the release's `checksums.sha256`.

| Archive | SHA-256 |
| --- | --- |
| `mediamtx_v1.19.2_linux_amd64.tar.gz` | `f9c601cc303ceca8fad2883917b022882672c5bc56311e92dbceb16e5f20c60c` |
| `mediamtx_v1.19.2_linux_arm64.tar.gz` | `562f419912a8668c18216a9e8c95359ec82fbb754e4a44e2953ef62b98eec688` |
| `mediamtx_v1.19.2_darwin_arm64.tar.gz` | `c225e46ab65295f95ee9fcda75703129c07add8852483abda09299a78524391f` |
| `mediamtx_v1.19.2_darwin_amd64.tar.gz` | `b8851bf53d1e0d6d078240d54e78517a3a239b40040b22cddddf5a0ef2712c17` |

The script checks the archive against its pin on every run and unpacks the binary only from a
verified archive; a mismatch deletes it and fails. By hand: `export
MEDIAMTX_BIN=$(scripts/mediamtx-fetch.sh)` (cached under `~/.cache/simee`). To bump MediaMTX,
change the version and the four hashes there, nothing else. The job sets `SIMEE_REQUIRE_MEDIAMTX`,
which turns a skipped MediaMTX test (no binary, no DTLS backend) into a failure, so a broken
download can't go green; a `MEDIAMTX_BIN` that names no file always fails.

The archive is not cached with `actions/cache`: the download, check and unpack take about 1.4 s,
a cache restore costs about that, and the repository's Actions cache is full. The job's build
(`rust-crypto` compiles aws-lc) is cached by `rust-cache` like the others, saved from `main`.
The macOS `lint-test` job fetches the darwin arm64 pin the same way and runs the same MediaMTX tests (simee-rtc is one of the crates it tests), with `SIMEE_REQUIRE_MEDIAMTX` set; no extra macOS job. The `mediamtx` job's Rust build restores the `linux` job's cache and saves none of its own (the Actions cache is over its limit).

What the first real run (v1.19.2, Linux amd64) settled:

- The harness's guesses held: the `MTX_WEBRTCADDRESS`, `MTX_APIADDRESS`,
  `MTX_WEBRTCLOCALUDPADDRESS` and `MTX_WEBRTCLOCALTCPADDRESS` overrides move the listeners (the
  log says so), `GET /v3/config/global/get` answers when the API is up, an unknown path is `404`
  with `{"status":"error","error":"path not found"}`, and a published path reads
  `{"name":...,"ready":true,"source":{"type":"webRTCSession",...},"tracks":["H264"],"tracks2":[{"codec":"H264",
  "codecProps":{"width":96,"height":64,"profile":"Main","level":"3.1"}}],"inboundBytes":1684,...}`.
- The test now asserts more of that: a WebRTC source, a single H264 track, MediaMTX's own reading
  of the SPS profile (`Baseline` for the Constrained Baseline fixture, `Main` for the Main one: the
  profiles `Publisher` negotiated reach the server intact) and bytes received.
- The start waits for the WebRTC listener as well as the API, and retries on new ports when one
  was taken between choosing it and MediaMTX binding it.
- 30 runs of the MediaMTX tests, four at a time, passed with no process or temp dir left behind.

### Input over a data channel (#271)

`Publisher::connect_with_input(config, InputSession::new(sink, InputConfig::default()), on_event)`
adds a reliable, ordered data channel labelled `simee-input` to the offer (an `m=application`
section beside the video); a peer that opens a channel with that label itself is accepted too.
Plain `connect` offers no channel. The channel's messages go to an `InputSession` on the
publisher's thread, which hands validated events to an `InputSink` (`fn input(&mut self, &InputEvent)
-> Result<(), String>`, also implemented for closures) and writes its replies back. Keep the sink
short: it runs where the media is sent. `stream_start {input: true}` wires one to a simee device
("Streams from the API" below).

The channel only reaches a viewer whose peer connection ends at the publisher. MediaMTX doesn't
relay it, and doesn't ignore it either: v1.19.2 refuses an offer with an `m=application` section
(`406`, `{"status":"error","error":"unsupported media 'application'"}`). So `connect_with_input`
fails against a MediaMTX endpoint, and `Publisher::connect_with_input_or_without` (what
`stream_start {input: true}` uses) answers that refusal, and only that one (a 406 to the POST whose
body names `application`), by offering the stream again without the channel, once; it returns why
the channel was dropped. Any other error, including the retry's own, is returned as it is
(`mediamtx_takes_the_stream_without_the_input_channel` runs this against MediaMTX; the
`input_fallback` tests pin the retry rules). Input for a MediaMTX viewer needs a direct peer
connection or a relay of its own, which isn't built: the stream there runs with `input: false`.

**Messages** are one compact JSON object per text frame. Every message has `"v":1` (the version)
and a `kind`; `seq` (an integer) and `t` (the viewer's time in milliseconds, any clock, e.g.
`performance.now()`) are optional and echoed back. Unknown fields are ignored, so a minor
addition stays readable by this version.

| `kind` | Fields | Notes |
| --- | --- | --- |
| `touch` | `phase` (`down`, `move`, `up`, `cancel`), `id` (pointer id), `x`, `y`, `w`, `h` | Multi-touch by `id` (up to 10 down). `x`/`y` in `0..=1` of the frame, `w`/`h` the frame size the viewer saw; `up`/`cancel` may omit the point and lift where the pointer last was |
| `key` | `phase` (`down`, `up`), `usage` or `key` | `usage`: a USB HID keyboard-page usage (`0x04..=0xe7`). `key`: a browser name, a `KeyboardEvent.code` (`KeyA`, `Digit1`, `ArrowLeft`, `ShiftLeft`, `F5`) or a layout-free `KeyboardEvent.key` (`Enter`, `Escape`, `Backspace`, `Tab`, `" "`, `Shift`); turned into the usage |
| `text` | `text` | Typed as it is (an IME commit, a paste) |
| `button` | `button` (`home`, `power`, `volume_up`, `volume_down`, `back`, `app_switch`, `menu`, `assistant`), `phase` (`down`, `up`, `press`; default `press`) | The sink refuses buttons its device lacks |
| `rotate` | `orientation` (`portrait`, `landscape_left`, `landscape_right`, `portrait_upside_down`) | |
| `fold` | `posture` (`closed`, `half_open`, `open`, `flipped`, `tent`) | The Android emulator's postures |
| `wheel` | `x`, `y`, `w`, `h`, `dx`, `dy` | Deltas in pixels of the frame the viewer saw (a `WheelEvent` in pixel mode), positive right and down |
| `ping` | | Answered with `pong` at once |

```json
{"v":1,"seq":7,"t":5123.4,"kind":"touch","phase":"down","id":0,"x":0.5,"y":0.25,"w":590,"h":1280}
```

**Replies**, the same shape:

| `kind` | Fields | When |
| --- | --- | --- |
| `hello` | `protocol` (`simee-input`), `versions` (`[1]`) | Once, when the channel opens |
| `ack` | `seq`, `t`, `rx`, `done` | An input message that carried `t`, once the sink took it: `rx` is its arrival and `done` the sink's return, Unix milliseconds on the server's clock |
| `pong` | `seq`, `t`, `rx` | Every `ping` |
| `error` | `seq` (when readable), `code`, `message` | A refused message; the session carries on |

Error codes: `encoding` (a binary frame or not UTF-8), `json`, `size` (over 4096 bytes), `version`
(no `v`, or not 1), `kind`, `field` (missing, wrong type or out of range: a coordinate outside
`0..=1`, a frame side of 0 or over 16384, a usage outside the keyboard page), `pointer` (a move or
lift for a pointer that isn't down, a second down, an eleventh pointer), `rate`, `sink` (the sink's
own refusal: a button the device lacks, a stale frame). Nothing a viewer sends panics the session:
the JSON reader is the crate's own (sim-rtc has no serde), bounded in depth and size.

**Rate limits and coalescing.** Messages pass a token bucket (1000 per second, bursts of 200);
beyond it they are dropped with one `rate` error per second (carrying the dropped message's `seq`).
An `up`/`cancel` for a pointer that is down, and a key or button `up` for one that is held, are
never dropped, whatever the budget. At most one move per pointer per
`min_move_interval` (8 ms) reaches the sink; moves inside it are replaced by the latest, which is
delivered when the interval ends or before any other event, so order is kept: a lift always
follows the last position. A move replaced before delivery is not acked; the one that replaced it
is. The pointer is freed on `up`/`cancel` even when the sink refuses it, so a finger never sticks. When the channel closes, is replaced by a new one, or the publisher
stops (or the session is dropped), `InputSession::release_all` sends `cancel` for every pointer
still down and `up` for every held key and button, so a viewer that vanishes mid-gesture leaves
nothing stuck. `text` reaches the sink raw: a sink must never pass it to a shell.

**Coordinates.** A point is normalised to the stream frame and carries the size the viewer saw, so
the encoder's downscale (the longest side capped, e.g. at 1280: an iPhone's 1179x2556 is streamed
as 590x1280) and the size the page shows the video at don't matter. `DisplayMap::new(w, h)` (the
device's framebuffer size) maps it: `to_device(&point)` is the device pixel, clamped inside the
display, and refuses a point from a frame of another shape (`StaleFrame`: the device rotated while
the input was in flight) rather than stretching it, allowing the encoder's rounding (even sides,
16-pixel minimum: 4000x20 streams as 100x16). A square display looks the same rotated, so its
rotation can't be detected from the frame's shape; `delta_to_device` scales wheel deltas;
`stream_size(Some(1280))` predicts the encoder's frame (the rule of `simee-encode`'s
`output_size`).

**Latency.** `t` echoed with `rx` and `done` gives the viewer the channel's round trip (`pong`) and
when its input reached the device's input path (`ack`), on the server's clock. Input-to-photon is
that plus the time to the first frame showing the change, which needs the video side: #268 and
#270.

**Tests.** `src/datachannel.rs` unit tests: every kind round-trips through JSON, browser-written
JSON decodes (named keys, surrogate pairs, exponents), every refusal has its code and `seq`, the
mapping through the downscale and the stale-frame refusal, coalescing per pointer with order kept,
acks and pongs, the rate limit, and a refused lift still freeing its pointer. `tests/datachannel.rs`
runs the publisher (the offerer) against a str0m answerer behind the fake WHIP server over loopback
UDP: the answerer sees the channel open and the `hello`, sends a script of touches, keys, text,
buttons, rotation, fold and wheel plus refused messages and a `ping`, and the fake sink gets the
events in order (moves coalesced) while each `t` comes back in an `ack` or the `pong` (a few
milliseconds round trip on loopback), with video flowing beside the channel. A plain `connect`
offers no channel. The Chromium test (a real browser tapping a button through the channel and
seeing the result in the video, with input-to-photon measured) belongs with #268.

## `sim-encode`: the encoder

`crates/sim-encode` (published as `simee-encode`) turns a device's frames into what the publisher
sends: Annex B access units with presentation times, an IDR with its SPS and PPS in front
whenever one is due or asked for. On macOS the encoder is VideoToolbox; on Linux it is OpenH264
([below](#linux-openh264)). It is safe Rust apart from the FFI: `sim-coresim`'s `videotoolbox.rs`
(behind that crate's `videotoolbox` feature, so the CLI doesn't load VideoToolbox) and one
`SetOption` call in `src/linux.rs` (the runtime bitrate change OpenH264's Rust crate lacks).

```rust
let context = Context::new()?;
let mut source = IosSource::new(context.device(udid)?.display()?)?; // or AndroidSource::connect(avd_dir, 0)?
let mut encoder = VtEncoder::new(EncoderConfig { fps: 30, max_size: Some(1280), ..Default::default() })?;
let controls = Arc::new(Controls::default()); // request_keyframe(), set_bitrate(), stop() from any thread
stream::run(&mut source, &mut encoder, Pacing::new(30, Some(Duration::from_millis(500))), &controls, |unit| {
    publisher.send(&unit.data, unit.pts)?; // simee-rtc
    Ok(Flow::Continue)
})?;
```

`examples/publish_device.rs` does exactly that against a WHIP URL, wiring the publisher's
`KeyframeRequested` to `Controls::request_keyframe` and `BitrateEstimate` to
`Controls::set_bitrate`, and offering the first IDR's own `profile-level-id` (VideoToolbox picks
the level from the frame size: a full-size iPhone frame is level 5.x, which the default `42e01f`
offer would refuse).

**The encoder.** A `VTCompressionSession` with the hardware encoder required, preferred (the
default; `VtEncoder::hardware()` says whether it fell back to software) or avoided; Constrained
Baseline (`kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel`, looked up at run time: macOS 12)
or Main; `RealTime`, no frame reordering, `MaxKeyFrameInterval` and `MaxKeyFrameIntervalDuration`
from `keyframe_interval` (2 s), `AverageBitRate` (4 Mbit/s) and a `DataRateLimits` ceiling over
one second (8 Mbit/s), `ExpectedFrameRate`. An IDR on demand is
`kVTEncodeFrameOptionKey_ForceKeyFrame`. Each picture is submitted and then completed
(`VTCompressionSessionCompleteFrames` up to its timestamp), so one picture is in flight at a time
and the latency below is the whole of it. VideoToolbox keeps the SPS and PPS in the format
description and stores NAL units behind 4-byte lengths; each keyframe's parameter sets are cached
and every IDR goes out as `SPS PPS [SEI] IDR` with start codes. A new input size (a rotation)
makes a new session, which starts with an IDR.

**Inputs, with no colour conversion on the CPU.** iOS: the framebuffer `IOSurface` itself,
wrapped as a `CVPixelBuffer` (`CVPixelBufferCreateWithIOSurface`, rewrapped only when the surface
changes); VideoToolbox converts BGRA to YUV on the GPU/media engine. Android: emu's frames come
through the shared ring (`shm`), a reader thread keeps only the newest, and the encoder takes it
with one copy into an IOSurface-backed `CVPixelBufferPool`. CoreVideo allocates no IOSurface
`RGBA` (or `ARGB`) buffers on this host (`kCVReturnInvalidPixelFormat`), so that copy swaps red
and blue as it goes into a BGRA buffer (`sim_core::pixels::swap_red_blue_into`, 0.26 ms for a
1080x2400 frame). Frames are only copied when one is encoded.

**Pacing.** The source says when the device presented a frame (iOS: the framebuffer's damage
callback, `Display::watch_frames`; Android: emu's ring announcements) and the capture instant is
stamped then, before any copy. The pacer encodes the newest frame no sooner than one interval
after the previous encode (the fps cap); a frame held back is encoded when its slot comes, so the
last picture of an animation is never lost. On a still screen the last frame is re-encoded every
heartbeat (`Pacing::new(fps, Some(500 ms))`), and a keyframe request on a still screen re-encodes
it at the next slot. Timestamps count from the start of the run and are kept strictly increasing
by at least one 90 kHz tick. `max_size` caps the longest edge (aspect kept, even sides);
VideoToolbox scales on the way in. On Android, `AndroidSource::connect(dir, max_width)` can also
have emu shrink frames by a whole factor before they're sent (`hello <max_width>`).

### Linux: OpenH264

`OpenH264Encoder` implements the same `Encoder` trait, so `stream::run`, the pacer, the keyframe
re-request after a dropped IDR and `Controls::set_bitrate` work unchanged. It is behind
`simee-encode`'s `openh264` feature (off by default, Linux only); without the feature
`simee_encode::available()` is false there, as before, and everything else builds and tests on
every host. Building with the feature compiles OpenH264's C++ (`g++`); no system library is
needed at run time.

**Streaming from a Linux `simee`.** `stream_start` uses it in a build with the feature forwarded
through `simee-api` and the CLI:

```bash
sudo apt-get install -y nasm g++        # g++ builds OpenH264's C++; nasm is the x86_64 speed-up (below)
cargo install simee --features openh264 # or: cargo build --release -p simee --features openh264
simee serve &                           # streams live in the serving process
simee stream start emulator-5554 --whip http://127.0.0.1:8889/phone/whip
```

On Linux the device is an Android one (emu's display socket and frame ring, `AndroidSource`;
iOS simulators run only on macOS, and asking for one fails `stream_start` with that reason). The
settings are the macOS ones: real-time `ScreenContentRealTime`, Constrained Baseline unless the
Main profile is asked for, no B-frames, an IDR at least every `keyframe_interval`, one on every
`KeyframeRequested`, the target retuned on every `BitrateEstimate`, the SPS/PPS cached and put in
front of every IDR, and the heartbeat re-encode on a static screen. A build without the feature
fails `stream_start` with `unsupported: this build of simee has no H.264 encoder; rebuild with
... --features openh264`, leaving nothing running.

**Why `simee` doesn't enable it by default.** The owner approved the dependency on 2026-11-27
behind an off-by-default feature. Three things keep the feature opt-in rather than on for Linux
builds of `simee`: the patent point below (Cisco's coverage is for its own binary, not for source
a product compiles), the C++ toolchain every Linux build and CI job would then need, and
`scripts/check.sh`'s Linux cross-clippy from macOS, which stops in `openh264-sys2`'s build script
with the feature on. Turning it on for `simee`'s Linux release binaries is one line in the CLI's
`[features]` (`default = ["openh264"]`) and a decision on the patent point, not a code change;
`simee-api` and `simee` forward the feature (`simee-api/openh264`, `simee/openh264`) so a
packager can do it either way.

**Install `nasm` on x86_64.** `openh264-sys2` assembles OpenH264's SIMD with `nasm`, and without it
silently builds the plain C++: the encoder is 2.4 times slower (61.8 ms against 26.1 ms per
full-size picture, [Encode cost on Linux](#encode-cost-on-linux)). `apt-get install nasm` (CI's
`linux` job does) before the first build; `nasm` appearing later doesn't rebuild anything, so
`cargo clean -p openh264-sys2` after installing it. `simee-encode`'s build script prints a cargo
warning on x86 Linux with the feature when `nasm` isn't found, or when `OPENH264_NO_ASM` is set.
aarch64 needs nothing: its NEON code is assembled by `cc`.

```bash
cargo test -p simee-encode -p simee-api --features openh264
cargo run --release -p simee-encode --features openh264 --example linux_bench
```

**What it matches.** Constrained Baseline (`42c0xx`) or Main (`4d4cxx`, CAVLC, no B-frames); real-time, no frame reordering, one access unit per
picture; an IDR on request (`force_intra_frame`) and every `keyframe_interval`; SPS and PPS in
front of every IDR (the encoder's own are cached, and `ParameterSets::annex_b_access_unit` puts
the cached pair in front of an IDR that came without); Annex B out; a new size (a rotation)
makes a new session that starts with an IDR; `max_size` scales down with even sides and a floor
of 16; `Hardware::Require` is `Error::Unsupported` and `Prefer` runs the software encoder
(`hardware()` is `Some(false)`). A picture the rate control skips comes back as no access unit,
which `stream::run` already treats as a dropped one: a requested IDR that was skipped is asked
for again.

**Where it differs from VideoToolbox.**

- *Screen content mode.* `UsageType::ScreenContentRealTime`, frame skipping on. In a one-off run
  with a temporary switch (not in the tree) on the same synthetic screens at 400x800 and
  1 Mbit/s it gave 41.3 dB luma PSNR with 2 pictures skipped in 4 s; camera mode gave 39.7 dB and
  skipped half of them (at 500 kbit/s: 39.9 dB against 29.4 dB). The cost is that OpenH264 starts an IDR by itself when most of the screen changes at once (an
  app launch): such a unit has `keyframe: true`, SPS and PPS in front, and is counted in
  `Stats::keyframes`. The mode forces scene-change detection on and adaptive quantization and
  background detection off, so the config asks for exactly that (anything else prints a warning
  to stderr from inside OpenH264).
- *Bitrate.* `set_bitrate` is clamped like VideoToolbox's (100 kbit/s to `max_bitrate_bps`) and
  takes effect on the next picture through `SetOption`: the ceiling on spatial layer 0 first (a
  target over the layer's ceiling is refused; `SPATIAL_LAYER_ALL` would set another limit), then
  the target. OpenH264 has no equivalent of `DataRateLimits` (a limit over one second):
  `max_bitrate_bps` is the ceiling a retarget may reach and the encoder's per-layer maximum. With
  `max_bitrate_bps: None` the ceiling is 240 Mbit/s (under level 5.2's maximum, above which
  OpenH264 pulls its ceiling back under the target and refuses it), which is also the most any
  target can be, so an absurd request never fails the stream. The encoder starts with its
  ceiling equal to the starting target (the Rust wrapper can't set it earlier), so the first
  picture is held to it; the real ceiling is set right after.
- *Colour.* The frames arrive as 32-bit RGBA or BGRA (emu's ring is RGBA) and are converted to
  I420 in `i420.rs`: BT.709, limited range, each chroma sample the mean of its 2x2 block, and the
  SPS says so (VUI colour description), which is what a browser assumes for HD video anyway.
  `max_size` is a box-filter downscale (up to 4x4 taps per output pixel) done on the RGB first; a
  frame at most one pixel larger than the even output size is cropped instead. The buffers are
  reused between pictures, so a steady stream allocates nothing per frame.
  On Linux the stream first asks emu for frames at that size: `hello` with the width whose whole
  shrink factor leaves the longest side at or over `max_size`, as close to it as a whole factor
  allows (never under it: a picture can't be scaled back up). The box filter then only finishes
  the exact size, or runs in full when emu ignores the request. The request follows a rotation:
  frames still in the old shape's size are held back (up to 15) so the encoder restarts once, and
  emu sends its newest frame again at a new `hello` width, so a static screen still shows the
  turned picture.
- *Size.* OpenH264 stops at 3840x2160 (either way up); a larger picture without `max_size` is
  `Error::Unsupported`. Scaling up isn't supported.
- *No hardware.* VA-API, NVENC and V4L2 are not wired (see "Not yet").

**Frame source.** `simee_encode::AndroidSource` exists on Linux too: the same display-socket
reader as macOS (`android_frames.rs`, shared: a thread keeps the newest frame of emu's ring and
the capture instant), with the picture being that frame itself, RGBA, no copy. It works against
a running AVD, including software mode on Linux.

**Choosing it.** Options, from the licence and build requirements (checked 2026-11-27) and, where
noted, from running them here:

| Option | Licence | Build and run | Verdict |
| --- | --- | --- | --- |
| `openh264` crate 0.9.10, `source` feature (OpenH264 as `openh264-sys2` 0.9.10 vendors it) | BSD-2-Clause | `cc` compiles the C++, `nasm` optional; no run-time dependency; builds on x86_64 and aarch64 Linux | **Chosen.** The wrapper has no runtime bitrate API (one `unsafe` call), and building from source means Cisco's patent-licence coverage of its own binaries doesn't apply: see the note below. |
| `openh264` crate, `libloading` feature | BSD-2-Clause | Loads Cisco's prebuilt `libopenh264.so`, which has to be downloaded at install time; pure Rust build | Keeps Cisco's patent coverage and needs no C++ compiler, but every user and every CI run needs the library. A follow-up, as another feature with the same `OpenH264Encoder`. |
| `rusty_h264-encoder` 0.17.4 (pure Rust, `forbid(unsafe)`) | BSD-2-Clause | Cargo only | Five months old, rate control is average bitrate with no runtime retarget in its API. One run here, same converted frames through `encode_planes`, 4 Mbit/s, 30 fps: 83 ms per full-size frame on its `Fast` preset and 151 ms on `Balanced`, against 21 ms for OpenH264 (576x1280: 31 and 61 ms against 15). Worth re-checking as it matures; it would remove the C++ build and the `unsafe`. |
| x264 (`x264` crate or a `dlopen` of `libx264`) | GPL-2.0 | System library | A GPL encoder in an MIT workspace; and it is a system package. Not chosen. |
| FFmpeg, GStreamer | LGPL/GPL by build | System libraries | System packages again, and a large surface for one encoder. |
| VA-API, NVENC, V4L2 stateful encoders | n/a | Hardware | Nothing to test on a CI runner or this VM. A follow-up. |

*Patents.* OpenH264's [FAQ](https://www.openh264.org/faq.html) says Cisco covers the AVC/H.264
royalties only for the binary module it ships, which a product has to download when it is
installed; a product that builds the source itself is outside that. The `openh264` feature is
off by default, no release binary of `simee` or `emu` enables it, and nothing links the encoder
unless a build asks for it. Whether to ship an encoder built from source, or to take the
`libloading` route, is the owner's call (default: ship neither).

*Why a feature, off by default.* The OpenH264 build needs a C++ compiler for the target, and
`scripts/check.sh` lints the Linux target from macOS with no C++ cross toolchain: with the
feature on, `cargo clippy --target x86_64-unknown-linux-gnu` from a Mac stops in `openh264-sys2`'s
build script (the same reason `simee-rtc`'s `rust-crypto` is opt-in). Checked here by linting
`aarch64-unknown-linux-gnu` from an x86_64 host: it passes without the feature and fails with
`failed to find tool "aarch64-linux-gnu-g++"` with it. So the pre-push hook doesn't build the
Linux encoder; CI's `linux` job does, with its own steps after the workspace's: `nasm` installed,
`cargo clippy -p simee-encode --all-targets --features openh264` and
`cargo nextest run -p simee-encode --features openh264`. Run the same before pushing a change
to `src/linux.rs`, `tests/openh264.rs` or the bench.

### Encode cost

`scripts/bench/encode.sh` (the `encode_bench` example): 10 s per run with a swipe every 0.7 s
(home screen pages on iOS, the app drawer on Android), medians of 3 rounds, the encoder's
defaults (Constrained Baseline, 4 Mbit/s average, 8 Mbit/s ceiling, IDR every 2 s), 2026-11-27,
Apple M3 Max (Mac15,9), macOS 26.6.2, hardware encoder in every run. Encode latency is from
handing the picture to VideoToolbox until its access unit is out; the picture step is wrapping
the surface (iOS) or the one copy into the pool (Android); CPU is the encoding process (the
capture, the copy, the frame reader) and, apart, the host's `VTEncoderXPCService`. The encoded
rate is bounded by the frames the device presented while swiping, about 48 a second on both, so
the 60 fps cap encodes about 46.

| iPhone 17 Pro, iOS 27.2 | 1206x2622, 30 fps | 1206x2622, 60 fps | 588x1280, 30 fps | 588x1280, 60 fps |
| --- | --- | --- | --- | --- |
| Encoded, fps | 26.8 | 46.4 | 26.9 | 46.8 |
| Encode latency p50 / p95, ms | 17.28 / 17.45 | 17.20 / 17.35 | 6.07 / 6.28 | 6.06 / 6.19 |
| Surface wrap p50, ms | 0.44 | 0.40 | 0.45 | 0.40 |
| Encoding process CPU, % of a core | 2.7 | 4.0 | 2.8 | 4.2 |
| `VTEncoderXPCService` CPU, % of a core | 0.9 | 1.5 | 0.9 | 1.5 |
| Output, kbit/s | 4153 | 4178 | 4142 | 4184 |
| First IDR (start, still screen) / median IDR (moving), KB | 34.2 / 64.6 | 26.2 / 58.1 | 22.2 / 46.8 | 17.3 / 38.3 |

| Android phone, API 36 google_apis, 1080x2400@420, emu | 1080x2400, 30 fps | 1080x2400, 60 fps | 576x1280, 30 fps | 576x1280, 60 fps |
| --- | --- | --- | --- | --- |
| Encoded, fps | 26.9 | 45.7 | 22.1 | 37.3 |
| Encode latency p50 / p95, ms | 14.51 / 14.62 | 14.45 / 14.58 | 5.80 / 5.91 | 5.77 / 5.89 |
| Copy into the pool p50, ms | 0.26 | 0.26 | 0.26 | 0.26 |
| Encoding process CPU, % of a core | 8.5 | 9.8 | 6.9 | 8.1 |
| `VTEncoderXPCService` CPU, % of a core | 0.9 | 1.5 | 0.7 | 1.2 |
| Output, kbit/s | 2508 | 2650 | 3461 | 4100 |
| First IDR / median IDR, KB | 23.3 / 54.9 | 19.1 / 43.4 | 21.5 / 54.7 | 23.8 / 54.7 |

```bash
scripts/bench/encode.sh --udid <booted iPhone>            # or no --udid: it makes its own
scripts/bench/encode.sh --udid <booted iPhone> --max-size 1280
scripts/bench/encode.sh --avd <running AVD>               # or --platform android: its own AVD
```

What the numbers say: at full size a moving iPhone frame takes the hardware encoder about 17 ms
(9.6 ms on a still screen, one 5 s `--still` run), which, with one picture in flight, caps a full-size stream just under
60 fps; capped at 1280 it's 6 ms. The CPU cost is small because nothing is converted on the CPU:
2.7-4.2% of a core on iOS. Android's is higher because the encoding process also reads every
frame emu presents from the ring (about 10 MB each at full size), not because of the encoder.
The tablet and iPad rows are not measured yet.

### Encode cost on Linux

`scripts/bench/encode-linux.sh` (the `linux_bench` example): no device and no emulator, so it
runs on any Linux host. A synthetic app drawer (wallpaper gradient, a grid of coloured icons with
text-like labels, scrolled 24 px per picture and back) is rendered up front at 1080x2400 and
presented at 60 fps; the real encode loop encodes it at the cap for 10 s; medians of 3 rounds;
the encoder's defaults (Constrained Baseline, 4 Mbit/s average, 8 Mbit/s ceiling, IDR every
2 s), OpenH264 built with `nasm`, 2026-11-27. Host: a Linux VM, Intel Xeon @ 2.20 GHz, 4 vCPUs,
3.8 GB, Ubuntu 22.04, kernel 6.1.189, rustc 1.99.1, load about 0.8 (the encoder runs on one
thread; its process had 3 other vCPUs to itself). Encode latency is from handing the picture to
the encoder until the access unit is out, and includes the colour conversion and scaling; PSNR is
of the luma of the decoded stream against the frame the encoder was given. The screens are
synthetic: a real device's frames will differ in how much they change, and the bitrate and PSNR
follow that, though the time per picture mostly doesn't.

| Synthetic phone screen, 1080x2400 | full size, 30 fps | full size, 60 fps | capped at 1280 (576x1280), 30 fps | capped at 1280, 60 fps |
| --- | --- | --- | --- | --- |
| Encoded, fps | 28.8 | 28.4 | 29.8 | 58.7 |
| Encode latency p50 / p95, ms | 26.1 / 40.9 | 36.5 / 41.8 | 22.4 / 25.9 | 14.2 / 15.5 |
| Colour conversion (and scaling) alone p50, ms | 5.1 | 5.1 | 6.6 | 6.6 |
| Process CPU, % of a core | 82 | 99 | 68 | 86 |
| Luma PSNR, dB | 45.2 | 44.9 | 42.6 | 39.5 |
| Output, kbit/s | 2190 | 1638 | 3830 | 3901 |
| First IDR / median IDR, KB | 46.1 / 54.1 | 43.0 / 41.9 | 39.2 / 44.5 | 35.6 / 35.6 |

Phone and tablet on one machine, same method (`scripts/bench/encode-linux.sh --rounds 3`), 2026-11-28.
Host: a Linux VM (a different one from the table above, so compare rows within this table, not with
it), Intel Xeon @ 2.20 GHz, 4 vCPUs, 7.8 GB, Ubuntu 22.04, kernel 6.1.189, OpenH264 built with
`nasm`, load about 1.0. Medians of 3 rounds:

| Synthetic screen | phone 1080x2400, 30 fps | phone, 60 fps | phone capped at 1280, 30 fps | phone capped at 1280, 60 fps | tablet 2560x1600, 30 fps | tablet, 60 fps | tablet capped at 1280, 30 fps | tablet capped at 1280, 60 fps |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Encoded, fps | 27.3 | 24.2 | 29.7 | 58.0 | 23.4 | 23.4 | 29.9 | 50.7 |
| Encode latency p50 / p95, ms | 32.0 / 51.8 | 42.9 / 48.6 | 24.2 / 34.0 | 16.3 / 18.2 | 40.2 / 52.2 | 40.7 / 52.3 | 21.7 / 29.7 | 18.6 / 25.1 |
| Colour conversion (and scaling) alone p50, ms | 6.0 | 6.1 | 6.7 | 6.7 | 9.2 | 9.1 | 9.6 | 9.8 |
| Process CPU, % of a core | 96 | 99 | 76 | 97 | 100 | 100 | 68 | 100 |
| Luma PSNR, dB | 45.2 | 44.9 | 42.6 | 39.5 | 48.5 | 48.3 | 47.6 | 47.5 |
| Output, kbit/s | 2006 | 1417 | 3749 | 3883 | 1021 | 995 | 818 | 929 |

A full-size tablet picture costs about 40 ms, so one encoder thread holds it to 23 fps; the cap
(`max_size`, 1280 by default in `stream_start`) is what makes 30 fps. The output is under the
4 Mbit/s target in every column.

Built without `nasm` (`OPENH264_NO_ASM=1`, what a host without it gets, 30 fps, same method):
61.8 ms p50 and 118 ms p95 at full size (13.6 fps, 100% of a core) and 35.0 / 57.9 ms capped
(24.6 fps). With `nasm` the encoder itself is about 2.4 times faster.

```bash
scripts/bench/encode-linux.sh                                # the table above
scripts/bench/encode-linux.sh --fps 30 --max-size 0          # full size only
OPENH264_NO_ASM=1 scripts/bench/encode-linux.sh --fps 30     # without assembly
```

What the numbers say: one thread encodes a full 1080x2400 screen at about 26 ms, so a full-size
stream tops out near 30 fps on this CPU and 60 fps needs the 1280 cap (14 ms, 59 fps, 86% of a
core). Of a full-size picture's 26 ms, 5 ms is the conversion; scaling to 1280 costs 1.6 ms more
and halves the encode. VideoToolbox's hardware encoder takes 14-17 ms per full-size frame on an
M3 Max with almost no CPU (above); the software encoder on this VM costs most of a core. The
process stayed at or under one core in every run (the Rust wrapper always asks for one slice per
picture; using more threads would mean slicing the picture, which this change doesn't try), so
the rest of the host's cores stay free for the guest. Not measured: aarch64 speed (the tests ran there under `qemu-user`, which
says nothing about time), an emu software-mode guest running beside the encoder, and any
hardware encoder.

### Tests

`cargo test -p simee-encode`: Annex B and length-prefixed conversion, the SPS/PPS cache and its
repeat before every IDR, the pacer (fps cap, newest frame kept, heartbeat, keyframe refresh) and
the loop on a fake source and encoder with a fake clock, timestamp monotonicity; on macOS
(`tests/videotoolbox.rs`, synthetic frames, SKIP on a host with no H.264 encoder) Constrained
Baseline and Main, forced and periodic IDRs with their parameter sets, scaling, RGBA input, a
size change, and every access unit decoded with `VTDecompressionSession`. `tests/live.rs` does the
same on a real device's frames (`SIMEE_E2E_UDID=<udid>` or `SIMEE_E2E_AVD=<avd>`), with the SPS
read by `simee-rtc`'s parser.

On Linux, `cargo test -p simee-encode` runs the colour conversion and scaling (`src/i420.rs`:
BT.709 primaries, 2x2 chroma means, odd and padded frames, a downscale, flat colours through
every tap count, reused buffers), the display-socket reader against a fake display (a frame taken
once, the newest of several, the count of those it skipped, a closed connection), and the
Annex B parameter-set cache. With `--features openh264`, `tests/openh264.rs` encodes synthetic
screens and decodes every access unit with OpenH264's decoder: Constrained Baseline and Main
(`profile-level-id` read by `simee-rtc`'s parser), forced and periodic IDRs each with SPS and PPS
in front and none on P frames, `max_size` and odd sizes, a rotation (new session, new SPS, IDR),
BGRA and RGBA giving identical streams, BT.709 colours after a lossy round trip, `set_bitrate`
up and down (bytes per picture 1235, 13627 and 413 at 200 kbit/s, 3 Mbit/s and 200 kbit/s again
on noise; with the `SetOption` call removed the second figure is 822 and the test fails), a
picture the rate control skips, the refusals (hardware required, over 3840x2160, a short buffer,
fps 0), a retarget with no ceiling (not held to the starting target, an absurd request held to
240 Mbit/s, the SPS unchanged), and the whole loop on a fake source and clock with the real encoder (the 30 fps cap, an
IDR requested mid-run answered by the next picture, a retarget), and a scrolling screen encoded at 30 and at 60 fps and decoded again, each picture at least 35 dB of luma PSNR from what the encoder was given (the worst of 40 is 40.5 dB; no timing is asserted). The same tests passed on
`aarch64-unknown-linux-gnu` under `qemu-user` (OpenH264 built from source with its NEON code)
and on x86_64 with `OPENH264_NO_ASM=1`.

## Streams from the API (#266)

`crates/sim-api/src/stream.rs` puts the encoder and the publisher behind four methods
(docs/api.md, "Streams"), which the MCP server serves as the `stream` toolset:

- `stream_start` (`device, whip_url, token?, fps?(30), max_size?(1280), bitrate?(2500000),
  audio?(false), input?(false)`) → `{stream_id, whip_resource, lease_ttl_s, device}`, plus `input`
  (boolean) and, when the server refused the channel, `input_note`, when `input` was asked for.
- `stream_status` (`stream_id`): the state, the stage timeline and the stats; renews the lease.
- `stream_stop` (`stream_id`): ends the stream and sends the WHIP DELETE.
- `streams` (`device?`): every stream of the process, without renewing anything.

**What a stream is.** A worker thread runs `simee-encode`'s loop (`VtEncoder` on `IosSource` or
`AndroidSource` on macOS, `OpenH264Encoder` on `AndroidSource` on Linux, a 500 ms heartbeat on a still screen) and connects the publisher on the first
access unit, offering that IDR's own `profile-level-id`, as `examples/publish_device.rs` does.
`stream_start` waits (at most 40 s) for that connection and fails with the reason when it
doesn't come: the URL refused before any request (as `WhipClient::new` checks it: `https`,
plaintext to a host that isn't loopback, another scheme), a 401/403 from the WHIP server ("pass a
valid `token`"), a device that isn't booted, `audio: true` (the Opus track is #265), or a host with
no encoder (macOS has VideoToolbox; Linux has OpenH264 in a build with `--features openh264`, and says how to get it). A failed start leaves nothing behind.
The publisher's `KeyframeRequested` forces an IDR (counted as `pli` or `fir`), `BitrateEstimate`
retargets the encoder, and `Disconnected` ends the stream.

**Stages and stats.** `stages` lists what the stream reached, in order, each in milliseconds
since the request: `requested`, `first_encoded_frame`, `whip_connected` (the WHIP answer
applied, ICE and DTLS up), `ended`. The first frame comes before the connection because the
offer carries that frame's profile and level. `stats` has `encoded` access units (heartbeats
included) and `sent` ones, `keyframes`, `bytes`, `fps` and `bitrate_bps` (averaged from the first
frame to now, or to the end), the publisher's latest `bitrate_estimate_bps`, the viewers' `pli`
and `fir` requests, and with input the `input_events` delivered and `input_errors`. What a viewer
sees (time to first picture over WHEP) is #268's.

**Leases and ownership.** Streams outlive the call and the connection that started them: one
registry per process (a `simee serve`, which the CLI forwards to, or the MCP server). A stream's
lease is 30 s; `stream_status` renews it, and a thread stops any stream whose lease lapsed
(encode loop first, then the WHIP DELETE), so a crashed client's stream ends within 30 s. A
stream that ended on its own stays listed as `ended`, with its `reason`, until its lease lapses,
and a new stream to the same destination replaces it. There is one stream per device and WHIP
URL; a second `stream_start` for the pair is refused with the running one's id.

**Input.** With `input: true` the publisher is opened with `connect_with_input_or_without`: a
server that refuses the data channel (MediaMTX: `406 unsupported media 'application'`) is offered
the stream again without it, and the stream runs with `input: false` and an `input_note` (in the
`stream_start` result and in `stream_status`, whose `input` is the live state), instead of failing.
Where the channel is carried, the
session's sink maps each event through a `DisplayMap` of the device's `screen` size (the space
`touch` and `swipe` take) and queues one device call to a thread of the stream's own, which runs
it through the method table (so the publisher's thread never waits on a device):

| Event | Device call |
| --- | --- |
| `touch` down/move/up | `touch` at the mapped pixel; `cancel` is an `up` where the finger was |
| `key` down | `key {usage}` (a press; the `up` adds nothing) |
| `text` | `type {text}` |
| `button` press | `button` (`home`, `power` → `lock`, `volume_up`/`volume_down`, `back`, `app_switch` → `recents`, `menu`, `assistant` → `siri`, iOS) |
| `button` down … up | `button` with `hold_ms` the time it was held (10 ms to 10 s), on the `up` |
| `rotate` | `rotate {orientation}` (iOS); the screen size is read again so later points map onto the new shape |
| `fold` | `fold {posture}` (`half_open` → `half-opened`, `open` → `opened`) |
| `wheel` | a 100 ms `swipe` from the point, by the deltas scaled to device pixels, the other way |

A point from a frame of another shape (the device rotated while it was in flight) is refused
with a `sink` error, except a lift, which lifts where the finger was. One touch at a time: a
second pointer is refused (the device methods are single-pointer). Android refuses `rotate` to an
orientation (its `rotate` turns 90° per call) and the assistant button. A device call that fails
counts in `input_errors` (with `last_input_error`); the viewer's `ack` means the event was queued.
Stopping the stream lifts anything still down.

**CLI.** `simee stream start|status|stop|list|watch` (docs/cli.md) calls these methods on a
running `simee serve`. It never runs in-process, even with `--in-process`: a stream dies with its
process without sending its DELETE, so with no server it refuses and says so. `watch` renews the
lease (a `stream_status`) every `lease_ttl_s / 3`, prints one line per renewal, stops the stream
on Enter, and exits with an error when the stream ended on its own. Ctrl-C ends `watch` without a
stop (the CLI has no safe signal hook; the server's lease reaper deletes the resource within
30 s). The token comes from `--token-file` or `SIMEE_STREAM_TOKEN` and is never printed.
`stream_cli.rs` has the tests: argument parsing, the no-server refusal, and the watch loop's
renew and stop against a fake client.

**Tests.** `cargo test -p simee-api stream::` runs every method on fakes, with no network and no
device: `simee-encode`'s own loop over a fake source and encoder, a fake WHIP publisher that logs
each POST and DELETE (and can refuse with a status), a fake device input, and a fake clock for
the leases. They check the start (the offered profile and token, the encoder settings), the
stage order, a lapsed lease stopping the stream and deleting its resource, the duplicate refusal,
stop sending the DELETE, the refusals leaving nothing behind, keyframe requests and estimates
reaching the encoder and the stats, a peer disconnect, the input mapping (touch through the
downscale, keys, text, a held button, a scroll, a rotation re-reading the screen, the second
finger refused, the lift on stop), and that docs/api.md has each method's row as the table writes
it. With `--features openh264` on Linux, `cargo test -p simee-api --features openh264 linux_media`
runs the real media path on a fake emulator display socket: an Android target's frames through
OpenH264, a requested IDR with its SPS/PPS in front, heartbeats on the static screen, every access
unit decoded; an iOS target and a missing display are refused with a reason. A real stream end to
end (Chromium over WHEP) is #268's.

## Not yet

- **TLS.** `https://` WHIP endpoints are refused: a TLS client (rustls) needs a crypto provider
  of its own, which is the same C-or-pure-Rust question as DTLS. Until then, publish to a loopback
  MediaMTX (or a LAN one with `allow_insecure_http`).
- **STUN and TURN.** The offer has one host candidate (the address the host routes to the
  endpoint from, or `local_ip`); no server-reflexive or relay candidates, and the `Link` headers
  of RFC 9725 §4.6 (ICE servers) are not read. Trickle ICE is in the client but the publisher
  doesn't need it with host candidates gathered up front.
- **Linux in CI.** The `linux` job tests without `rust-crypto`, so the publisher's end-to-end
  tests skip there; the `mediamtx` job runs them with it, and macOS's `lint-test` runs them with Apple's DTLS and MediaMTX.
- **Reconnects.** A publisher reports `Disconnected`; publishing again is a new publisher with a
  new WHIP resource. A stream whose peer disconnects ends (`state: "ended"`); starting it again is
  the client's `stream_start`.
- **Stream input gaps.** Multi-touch, key chords (a modifier held while another key is pressed:
  `key` presses one key), and a target orientation on Android aren't wired to devices yet.
- **The CLI's `simee stream` and the desktop's Share control** (the rest of #266) call these
  methods; they aren't built yet.
- **Linux hardware encoders** (VA-API, NVENC, V4L2) and a faster software path: OpenH264 runs one
  thread (one slice) per encoder, a full-size stream tops out near 30 fps on the VM measured, and
  the colour conversion is single-threaded. The Linux encoder is behind the `openh264` feature
  (`scripts/check.sh` doesn't build it; CI's `linux` job does), and `libloading` (Cisco's binary,
  with its patent coverage) isn't offered.
- **A Linux `simee` that streams out of the box.** `stream_start` works on Linux only in a build
  with `--features openh264` ([why it's opt-in](#linux-openh264)); the release binaries don't have
  it. Not yet run: a stream from a real emulator's frames on Linux (the tests use a fake display
  socket, and the bench synthetic screens), and `simee-api`'s `linux_media` tests in CI (the
  `linux` job runs `cargo nextest run -p simee-encode --features openh264`; adding
  `-p simee-api` to it covers them).
- **Pipelined encoding.** One picture is in flight at a time, so a full-size iPhone stream can't
  quite reach 60 fps (17 ms per moving frame); submitting the next frame before the previous one
  is out would lift the rate, not the latency.
- **iOS runtimes without frame callbacks.** `IosSource` follows `Display::watch_frames`; a runtime
  whose callback never fires gets only heartbeats (the desktop's capture falls back to polling the
  surface seed; the encoder doesn't yet).

## The plan (#262)

| Issue | What |
| --- | --- |
| #263 | `sim-encode`: VideoToolbox H.264 straight from the IOSurface on macOS, OpenH264 on Linux behind a feature (hardware encoders on Linux still to come); it answers `KeyframeRequested` and `BitrateEstimate` |
| #264 | `sim-rtc` |
| #265 | An Opus audio track |
| #266 | Start, watch and stop streams from the API, MCP, CLI and desktop |
| #267 | A MediaMTX harness for tests and CI |
| #268 | Chromium WHEP playback tests: first frame, pixel change, late join, stage timings |
| #269 | WHEP tests on every form factor |
| #270 | Latency, encode cost and bitrate per form factor, against AutoMobile |
| #271 | Input from the browser over a data channel: the `simee-input` protocol and `InputSink` in `sim-rtc` (wiring to devices is #266, the browser test #268) |
