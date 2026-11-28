# Debugging

What simee exposes that the stock tools do not, and how to reach it. The short version is in
[comparison.md](comparison.md).

## iOS debug surface

Because a simulated iOS device's processes run on the host under their own `launchd_sim`, and its
filesystem is a host directory, simee can expose far more than the stock simulator:

| Area | What you get |
| --- | --- |
| Processes | every device process with CPU, memory and uptime; signal any of them |
| Network | open sockets per process (TCP/UDP, peer, state) and bytes in/out per process |
| Storage | every app's data and bundle container with sizes; list, read, write, delete files; empty an app's data; reveal in Finder |
| Preferences | read and write any `defaults` domain inside the device |
| Logs | the device's unified log (`log show`, with predicates) |
| In-device exec | run the runtime's own tools inside the device: `defaults`, `launchctl`, `notifyutil`, `log`, `heap`, `leaks`, `vmmap`, `sample`, ... |
| Rendering | Core Animation overlays: flash updated regions, blended layers, off-screen rendering, copied and misaligned images, cache hits |
| Device state | status bar overrides, Dynamic Type size, Increase Contrast, hardware keyboard, keyboard language, Dynamic Island, backlight, keychain reset, memory warning |
| Events | push notifications, Darwin notifications (with state), Face ID / Touch ID enroll, match and no-match, location |
| Launch | launch apps with arguments and environment (e.g. `CFNETWORK_DIAGNOSTICS=3`) |

All of it is available three ways: the CLI, the desktop panel, and a JSON-lines unix socket API.

```bash
simee call processes '{"device":"booted"}'
simee call push '{"device":"booted","bundle_id":"com.example.app","payload":{"aps":{"alert":"hi"}}}'
simee exec booted launchctl list              # run a program inside the device
```

Android devices' equivalents go through adb (`adb shell`, logcat) and the emulator console
(`simee console`); the desktop shows their guest console.

## iOS network shaping (no root)

simee shapes an iOS device's network without root, MDM or a network extension:

```bash
simee boot <device> --shaped            # or: simee call network_shape '{"device":"…","inject":"boot"}'
simee call network_shape '{"device":"…","preset":"3g"}'
simee call network_shape '{"device":"…","latency_ms":300,"down_kbps":1500,"loss_pct":5,
  "rules":[{"pattern":"api.example.com","offline":true},{"pattern":"10.0.0.0/8","latency_ms":20}]}'
simee call network_clear '{"device":"…"}'
```

- **Socket shaper (recommended):** a small dylib injected into every device process through the
  device's boot environment. It interposes socket and DNS calls and applies offline, latency,
  bandwidth and loss live from a profile file. Covers URLSession (HTTP/1.1, HTTP/2, HTTP/3/QUIC),
  WebSockets, WebKit, raw TCP/UDP and Network.framework, with per-host rules by hostname or CIDR.
- **Shaping proxy:** an HTTP/SOCKS proxy set in the device's own `configd_sim`; shapes proxy-aware
  HTTPS without decrypting it and logs every host (`network_flows`).
- **Kernel shaping:** `scripts/pf-shape.sh` (needs sudo) adds pf/dummynet pipes for the shaper's
  source-port range, for exact queueing and packet loss.

Measured coverage and limits: [docs/research/network-shaping.md](research/network-shaping.md).

## iOS audio (no driver)

An iOS device's processes are ordinary clients of the host's Core Audio server, so simee captures or
silences exactly one device with a Core Audio process tap (macOS 14.2+), no loopback driver needed:

```bash
simee call audio_capture_start '{"device":"…","path":"/tmp/device.wav","mute":true}'   # → capture_id
simee call audio_capture_stop  '{"capture_id":"cap-1"}'                                 # → rms, peak, duration
simee call audio_mute  '{"device":"…","on":true}'                                       # per-device null sink
simee call audio_route '{"device":"…","scope":"output","uid":"BlackHole2ch_UID"}'       # any host device
```

Microphone injection still needs a loopback device. Details: [docs/research/audio-camera.md](research/audio-camera.md).

On Android, `audio_state`, `audio_volume` and `audio_mute` act on the guest's media stream over adb
(`dumpsys audio`, `cmd media_session volume`, and `cmd audio adj-mute` from API 35). The device is
silent on the host unless it was booted with `audio`; capture and routing are iOS-only for now.


## Android

- `emu -verbose` prints per-vCPU exit counts (MMIO reads and writes, sysreg traps, WFI) at
  shutdown, so a slow guest shows where its time goes.
- `EMU_FRAME_STATS=1` prints per-stream frame timings; `EMU_GPU_TRACE=1` prints fence timing.
- `simee console <device> <command>` speaks the whole emulator console, plus emu's own
  `touch`, `clipboard`, `frames`, `sms list`, `xr` and `car`.
- Per-device state is plain files in `<avd>/simee/`: `console.log` (the guest console),
  `emu.log`, `display.sock`.
- The desktop window shows the live guest console beside the screen.
- `adb`, logcat and `adb emu` work unchanged.
