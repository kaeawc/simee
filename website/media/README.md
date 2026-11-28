# Live demo relay

MediaMTX on Fly (app `simee-media`, https://simee-media.fly.dev): the demo Mac publishes each
device over WHIP and the site's `/demos/` page plays them over WHEP (`website/public/live.js`).

```text
emu device   ─ screenrecord ─ ffmpeg WHIP ─┐                        ┌─ WHEP ─ browser
SDK emulator ─ screenrecord ─ ffmpeg WHIP ─┴─ simee-media (MediaMTX) ┴─ WHEP ─ browser
```

- Signaling (WHIP POSTs, WHEP POSTs) is HTTPS through Fly's shared edge.
- Media is WebRTC, DTLS-SRTP, on port 8189. UDP needs the app's dedicated IPv4, bound to
  `fly-global-services` (`fly.toml`); TCP on the same port serves viewers whose network blocks UDP.
- Anyone may watch. Only the `publisher` user may publish, and only to `emu` and `sdk`.
  Publishing anywhere else, or with a wrong password, gets 401. Watching a stream nobody
  publishes gets 404, which the page shows as offline.
- `mediamtx.yml` runs as is on a laptop; `fly.toml` overrides what differs on Fly through
  MediaMTX's `MTX_*` environment variables.

## Set up once

By someone with access to the Fly organization and the repository:

```bash
cd website/media
fly apps create simee-media
fly ips allocate-v4 -a simee-media          # dedicated IPv4: UDP needs it ($2/month)
fly secrets set -a simee-media \
  MTX_WEBRTCADDITIONALHOSTS=<that IPv4> \
  MTX_AUTHINTERNALUSERS_1_PASS=$(openssl rand -hex 24)   # keep it for the demo Mac
fly tokens create deploy -a simee-media -x 8760h | gh secret set FLY_MEDIA_API_TOKEN --env website -R kaeawc/simee
fly deploy --remote-only
```

`.github/workflows/website-media.yml` then deploys pushes to main that touch this directory.

## Go live

On the demo Mac, with the same AVD booted under emu and under the SDK's emulator:

```bash
SIMEE_MEDIA_PASS=<the password above> scripts/live-demo.sh --emu emulator-5554 --sdk emulator-5556
```

It streams each screen (`adb exec-out screenrecord --output-format=h264`, so both devices are
encoded the same way) through ffmpeg's `whip` muxer (ffmpeg 9.0 or newer), and drives both with the
same input every few seconds (`--no-drive` to leave them alone). Ctrl-C stops it.

## Try it locally

```bash
docker build -t simee-media website/media
docker run --rm -p 8889:8889 -p 8189:8189/udp -p 8189:8189/tcp \
  -e MTX_AUTHINTERNALUSERS_1_PASS=test -e MTX_WEBRTCADDITIONALHOSTS=127.0.0.1 simee-media
SIMEE_WEB_MEDIA_ORIGIN=http://127.0.0.1:8889 website/scripts/build.sh --dev   # the page plays from it
SIMEE_MEDIA_PASS=test scripts/live-demo.sh --emu emulator-5554 --media http://127.0.0.1:8889
```

Map the ports one to one: the relay's ICE candidates name 127.0.0.1:8189.

## Checked, and not yet

Checked locally on 2026-11-27, with MediaMTX 1.21.4 in Docker and ffmpeg 9.1:
- the config loads;
- the publisher's password comes from the env override, and wrong or missing passwords and
  unnamed paths get 401;
- ffmpeg publishes over WHIP with basic auth;
- both streams play in the page, over the Rune figures;
- an absent stream shows as offline.

Not yet:
- the relay on Fly (UDP through `fly-global-services`, the dedicated IPv4);
- `scripts/live-demo.sh` against real devices (screenrecord's restarts into one session);
- viewers behind networks that block UDP and need ICE over TCP 8189, or a TURN server.
