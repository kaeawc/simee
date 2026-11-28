// The live demo's players. /demos/ marks a figure per stream (#live-emu, #live-sdk); this lays a
// <video> over each, plays the relay's WHEP stream for it, and says when the stream is offline.
// The figures are in the static page and in Rune's mirror alike (Rune's canvas can't hold a
// video), so the players follow the figure's box through layout, resize and Rune's hand-over.
const media = document.querySelector('meta[name="simee-media"]')?.content
const streams = [["live-emu", "emu"], ["live-sdk", "sdk"]]
const RETRY_MS = 5000

function layer() {
  const el = document.createElement("div")
  el.style.cssText = "position:absolute;left:0;top:0;width:0;height:0;z-index:10;pointer-events:none"
  document.body.appendChild(el)
  return el
}

function player(parent) {
  const box = document.createElement("div")
  box.style.cssText = "position:absolute;overflow:hidden;background:#111;display:flex;align-items:center;justify-content:center"
  const video = document.createElement("video")
  Object.assign(video, { muted: true, autoplay: true, playsInline: true })
  video.style.cssText = "width:100%;height:100%;object-fit:contain;display:none"
  const label = document.createElement("span")
  label.style.cssText = "position:absolute;font:13px/20px var(--rune-mono);color:#ddd"
  box.append(video, label)
  parent.appendChild(box)
  return { box, video, label }
}

// A WHEP session (RFC 9725 egress): offer receive-only video, gather candidates, POST, answer.
async function watch(url, p) {
  p.label.textContent = "connecting…"
  const pc = new RTCPeerConnection()
  pc.addTransceiver("video", { direction: "recvonly" })
  pc.ontrack = (e) => {
    p.video.srcObject = e.streams[0] ?? new MediaStream([e.track])
    p.video.style.display = "block"
    p.label.textContent = ""
  }
  const ended = new Promise((resolve) => {
    pc.onconnectionstatechange = () => ["failed", "closed", "disconnected"].includes(pc.connectionState) && resolve()
  })
  await pc.setLocalDescription(await pc.createOffer())
  await new Promise((resolve) => {
    if (pc.iceGatheringState === "complete") return resolve()
    pc.onicegatheringstatechange = () => pc.iceGatheringState === "complete" && resolve()
    setTimeout(resolve, 2000)
  })
  const res = await fetch(url, { method: "POST", headers: { "Content-Type": "application/sdp" }, body: pc.localDescription.sdp })
  if (!res.ok) {
    pc.close()
    throw new Error(`relay answered ${res.status}`)
  }
  await pc.setRemoteDescription({ type: "answer", sdp: await res.text() })
  await ended
  pc.close()
  throw new Error("offline")
}

async function keepWatching(url, p) {
  for (;;) {
    try {
      await watch(url, p)
    } catch (e) {
      // 404 (no publisher), a relay that can't be reached, a dropped session: all offline.
      p.video.style.display = "none"
      p.label.textContent = "offline"
      console.debug("live demo:", e.message)
    }
    await new Promise((r) => setTimeout(r, RETRY_MS))
  }
}

function start() {
  const slots = streams.map(([id, name]) => ({ id, name })).filter((s) => document.getElementById(s.id))
  if (!slots.length || !media || !("RTCPeerConnection" in window)) return
  const parent = layer()
  for (const s of slots) {
    s.p = player(parent)
    keepWatching(`${media.replace(/\/$/, "")}/${s.name}/whep`, s.p)
  }
  const place = () => {
    for (const s of slots) {
      const r = document.getElementById(s.id)?.getBoundingClientRect()
      if (!r) continue
      Object.assign(s.p.box.style, { left: `${r.left + scrollX}px`, top: `${r.top + scrollY}px`, width: `${r.width}px`, height: `${r.height}px` })
    }
  }
  place()
  addEventListener("resize", place)
  document.addEventListener("runepresent", () => requestAnimationFrame(place))
  new ResizeObserver(place).observe(document.body)
}

if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start)
else start()
