// simee host audio helper (built on demand by sim-audio; public Core Audio only). `devices` runs on
// any macOS; `tap` needs 14.2 and answers an error naming it before then.
//
//   simaudio devices
//       JSON: every host audio device (id, uid, name, input/output channels).
//   simaudio tap --launchd <pid> [--out file.wav] [--mute]
//       Taps the audio output of every process descending from the given launchd_sim (one simulated
//       device), following clients as they start and stop. --mute keeps the device silent on the
//       host while capturing (a per-device null sink). Runs until stdin closes or SIGTERM, then
//       writes a 16-bit stereo WAV (when --out is given) and prints JSON stats.
import CoreAudio
import Foundation

let sys = AudioObjectID(kAudioObjectSystemObject)

func addr(_ sel: AudioObjectPropertySelector, _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress(mSelector: sel, mScope: scope, mElement: kAudioObjectPropertyElementMain)
}
func prop<T: Numeric>(_ obj: AudioObjectID, _ sel: AudioObjectPropertySelector, _ def: T, _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal) -> T {
    var a = addr(sel, scope)
    var v = def
    var size = UInt32(MemoryLayout<T>.size)
    let st = withUnsafeMutablePointer(to: &v) { AudioObjectGetPropertyData(obj, &a, 0, nil, &size, $0) }
    return st == 0 ? v : def
}
func ids(_ obj: AudioObjectID, _ sel: AudioObjectPropertySelector) -> [AudioObjectID] {
    var a = addr(sel)
    var size: UInt32 = 0
    guard AudioObjectGetPropertyDataSize(obj, &a, 0, nil, &size) == 0, size > 0 else { return [] }
    var arr = [AudioObjectID](repeating: 0, count: Int(size) / 4)
    guard AudioObjectGetPropertyData(obj, &a, 0, nil, &size, &arr) == 0 else { return [] }
    return arr
}
func str(_ obj: AudioObjectID, _ sel: AudioObjectPropertySelector) -> String {
    var a = addr(sel)
    var cf: Unmanaged<CFString>?
    var size = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
    guard AudioObjectGetPropertyData(obj, &a, 0, nil, &size, &cf) == 0, let s = cf else { return "" }
    return s.takeRetainedValue() as String
}
func channels(_ dev: AudioObjectID, _ scope: AudioObjectPropertyScope) -> Int {
    var a = addr(kAudioDevicePropertyStreamConfiguration, scope)
    var size: UInt32 = 0
    guard AudioObjectGetPropertyDataSize(dev, &a, 0, nil, &size) == 0, size > 0 else { return 0 }
    let buf = UnsafeMutableRawPointer.allocate(byteCount: Int(size), alignment: 16)
    defer { buf.deallocate() }
    guard AudioObjectGetPropertyData(dev, &a, 0, nil, &size, buf) == 0 else { return 0 }
    return UnsafeMutableAudioBufferListPointer(buf.assumingMemoryBound(to: AudioBufferList.self)).reduce(0) { $0 + Int($1.mNumberChannels) }
}
func json(_ v: Any) {
    if let d = try? JSONSerialization.data(withJSONObject: v, options: [.sortedKeys]) { print(String(decoding: d, as: UTF8.self)) }
}
func fail(_ msg: String) -> Never {
    json(["error": msg])
    exit(1)
}

func parent(_ pid: pid_t) -> pid_t {
    var info = proc_bsdinfo()
    let n = proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &info, Int32(MemoryLayout<proc_bsdinfo>.size))
    return n == Int32(MemoryLayout<proc_bsdinfo>.size) ? pid_t(info.pbi_ppid) : 0
}
/// Core Audio process objects whose process descends from `root`.
func deviceClients(root: pid_t) -> [AudioObjectID] {
    ids(sys, kAudioHardwarePropertyProcessObjectList).filter { obj in
        var pid: pid_t = prop(obj, kAudioProcessPropertyPID, -1)
        for _ in 0 ..< 64 where pid > 1 {
            if pid == root { return true }
            pid = parent(pid)
        }
        return false
    }
}

func devices() {
    json(ids(sys, kAudioHardwarePropertyDevices).map { d in
        ["id": Int(d), "uid": str(d, kAudioDevicePropertyDeviceUID), "name": str(d, kAudioObjectPropertyName),
         "input_channels": channels(d, kAudioObjectPropertyScopeInput), "output_channels": channels(d, kAudioObjectPropertyScopeOutput)] as [String: Any]
    })
}

final class Capture: @unchecked Sendable {
    let lock = NSLock()
    var pcm = Data()
    var frames = 0, sumSq = 0.0, peak: Float = 0
    let keep: Bool
    init(keep: Bool) { self.keep = keep }
    func add(_ list: UnsafeMutableAudioBufferListPointer) {
        lock.lock(); defer { lock.unlock() }
        // The tap aggregate delivers interleaved stereo float32 in one buffer.
        for b in list {
            guard let d = b.mData else { continue }
            let f = d.assumingMemoryBound(to: Float.self)
            let count = Int(b.mDataByteSize) / 4
            frames += count / max(1, Int(b.mNumberChannels))
            for i in 0 ..< count {
                let v = max(-1, min(1, f[i]))
                sumSq += Double(v * v)
                peak = max(peak, abs(v))
                if keep {
                    var s = Int16(v * 32767).littleEndian
                    withUnsafeBytes(of: &s) { pcm.append(contentsOf: $0) }
                }
            }
        }
    }
}

func wav(_ pcm: Data, rate: Int, channels: Int) -> Data {
    var d = Data()
    func u32(_ v: UInt32) { var x = v.littleEndian; withUnsafeBytes(of: &x) { d.append(contentsOf: $0) } }
    func u16(_ v: UInt16) { var x = v.littleEndian; withUnsafeBytes(of: &x) { d.append(contentsOf: $0) } }
    d.append(contentsOf: Array("RIFF".utf8)); u32(UInt32(36 + pcm.count)); d.append(contentsOf: Array("WAVE".utf8))
    d.append(contentsOf: Array("fmt ".utf8)); u32(16); u16(1); u16(UInt16(channels)); u32(UInt32(rate))
    u32(UInt32(rate * channels * 2)); u16(UInt16(channels * 2)); u16(16)
    d.append(contentsOf: Array("data".utf8)); u32(UInt32(pcm.count)); d.append(pcm)
    return d
}

/// The Core Audio process tap (`AudioHardwareCreateProcessTap`) is macOS 14.2+.
@available(macOS 14.2, *)
func tap(root: pid_t, out: String?, mute: Bool) {
    // A tap needs at least one process; wait until the device has an audio client.
    var clients = deviceClients(root: root)
    let started = Date()
    while clients.isEmpty {
        if Date().timeIntervalSince(started) > 30 { fail("no audio clients in the device within 30s (is it booted?)") }
        Thread.sleep(forTimeInterval: 0.2)
        clients = deviceClients(root: root)
    }
    let capture = Capture(keep: out != nil)
    /// One tap + private aggregate + IO proc over `procs`. A running tap's process list can't be
    /// changed (kAudioDevicePermissionsError), so a change of clients rebuilds all three.
    final class Session {
        var tap = AudioObjectID(0), agg = AudioObjectID(0)
        var proc: AudioDeviceIOProcID?
        var rate = 48000
        func stop() {
            if agg != 0 { AudioDeviceStop(agg, proc) }
            if let proc, agg != 0 { AudioDeviceDestroyIOProcID(agg, proc) }
            if agg != 0 { AudioHardwareDestroyAggregateDevice(agg) }
            if tap != 0 { AudioHardwareDestroyProcessTap(tap) }
            tap = 0; agg = 0; proc = nil
        }
    }
    func start(_ procs: [AudioObjectID]) -> Session? {
        let desc = CATapDescription(stereoMixdownOfProcesses: procs)
        desc.name = "simee device tap"
        desc.isPrivate = true
        desc.muteBehavior = mute ? .muted : .unmuted
        let s = Session()
        guard AudioHardwareCreateProcessTap(desc, &s.tap) == 0 else { return nil }
        let agg: [String: Any] = [
            kAudioAggregateDeviceNameKey: "simee device tap", kAudioAggregateDeviceUIDKey: "simee-tap-\(UUID().uuidString)",
            kAudioAggregateDeviceIsPrivateKey: true, kAudioAggregateDeviceTapAutoStartKey: true,
            kAudioAggregateDeviceTapListKey: [[kAudioSubTapUIDKey: str(s.tap, kAudioTapPropertyUID), kAudioSubTapDriftCompensationKey: true]],
        ]
        guard AudioHardwareCreateAggregateDevice(agg as CFDictionary, &s.agg) == 0 else { s.stop(); return nil }
        s.rate = Int(prop(s.agg, kAudioDevicePropertyNominalSampleRate, Float64(48000)))
        AudioDeviceCreateIOProcIDWithBlock(&s.proc, s.agg, nil) { _, input, _, _, _ in
            capture.add(UnsafeMutableAudioBufferListPointer(UnsafeMutablePointer(mutating: input)))
        }
        AudioDeviceStart(s.agg, s.proc)
        return s
    }
    guard var session = start(clients) else { fail("could not create a process tap (System Audio Recording permission?)") }
    let rate = session.rate
    json(["started": true, "sample_rate": rate, "clients": clients.count])
    fflush(stdout)

    // Stop on stdin EOF or SIGTERM; meanwhile follow the device's audio clients.
    let stop = DispatchSemaphore(value: 0)
    signal(SIGTERM, SIG_IGN)
    let term = DispatchSource.makeSignalSource(signal: SIGTERM, queue: .global())
    term.setEventHandler { stop.signal() }
    term.resume()
    Thread.detachNewThread {
        while FileHandle.standardInput.availableData.count > 0 {}
        stop.signal()
    }
    var current = Set(clients)
    while stop.wait(timeout: .now() + 0.5) == .timedOut {
        let now = Set(deviceClients(root: root))
        guard !now.isEmpty, now != current else { continue }
        current = now
        session.stop()
        if let next = start(Array(now)) { session = next }
        if ProcessInfo.processInfo.environment["SIMAUDIO_DEBUG"] != nil { json(["retap": now.count]) }
    }
    session.stop()
    capture.lock.lock()
    let samples = capture.frames * 2
    let rms = samples > 0 ? sqrt(capture.sumSq / Double(samples)) : 0
    var result: [String: Any] = ["frames": capture.frames, "sample_rate": rate, "duration_ms": capture.frames * 1000 / max(1, rate),
                                 "rms": rms, "peak": Double(capture.peak), "dbfs": rms > 0 ? 20 * log10(rms) : -160]
    if let out {
        do {
            try wav(capture.pcm, rate: rate, channels: 2).write(to: URL(fileURLWithPath: out))
            result["path"] = out
        } catch {
            result["error"] = "write \(out): \(error.localizedDescription)"
        }
    }
    capture.lock.unlock()
    json(result)
}

setlinebuf(stdout)
let a = CommandLine.arguments
switch a.count > 1 ? a[1] : "" {
case "devices": devices()
case "tap":
    var root: pid_t = 0, out: String?, mute = false
    var i = 2
    while i < a.count {
        switch a[i] {
        case "--launchd": i += 1; root = pid_t(a[i]) ?? 0
        case "--out": i += 1; out = a[i]
        case "--mute": mute = true
        default: fail("unknown argument \(a[i])")
        }
        i += 1
    }
    guard root > 1 else { fail("--launchd <pid> is required") }
    if #available(macOS 14.2, *) {
        tap(root: root, out: out, mute: mute)
    } else {
        let v = ProcessInfo.processInfo.operatingSystemVersion
        fail("audio capture needs macOS 14.2 or later (a Core Audio process tap, AudioHardwareCreateProcessTap); this Mac runs macOS \(v.majorVersion).\(v.minorVersion).\(v.patchVersion)")
    }
default:
    fail("usage: simaudio devices | simaudio tap --launchd <pid> [--out file.wav] [--mute]")
}
