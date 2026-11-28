//! `qemud:sensors`: the goldfish sensors HAL's host side.
//!
//! The HAL sends `list-sensors` (we answer with a bitmask of sensor handles), then
//! `set:<name>:<0|1>` and `set-delay:<ms>`; we stream `<name>:<values>` events for enabled
//! sensors followed by `sync:<µs>`. Values live in [`Sensors`], which the console's
//! `sensor set|get` changes (shake, rotation, hinge angle, proximity, ...).
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::qemud::{frame, take_frames};
use super::{HostMsg, PipeCtx, PipeHandler, PipeService};

/// Protocol names in handle order (handle = index), as the HAL's sensor list defines them.
pub use sim_core::goldfish::SENSORS as NAMES;

/// Shared, live sensor values.
pub struct Sensors {
    values: Mutex<HashMap<String, Vec<f32>>>,
    /// Bitmask of sensor handles the device has (`sim_core::goldfish::sensor_mask`).
    pub mask: u32,
}

impl Sensors {
    /// A phone's sensors with stock's defaults: motion, magnetic, orientation and environment
    /// (handles 0–10 and 17).
    pub fn phone() -> Sensors {
        Sensors::new(0x7ff | (1 << 17))
    }

    /// The sensors in `mask`, at rest. Every sensor has a value, so the console can set any.
    pub fn new(mask: u32) -> Sensors {
        let mut v: HashMap<String, Vec<f32>> = HashMap::new();
        v.insert("acceleration".into(), vec![0.0, 9.80665, 0.0]);
        v.insert("acceleration-uncalibrated".into(), vec![0.0, 9.80665, 0.0]);
        v.insert("gyroscope".into(), vec![0.0, 0.0, 0.0]);
        v.insert("gyroscope-uncalibrated".into(), vec![0.0, 0.0, 0.0]);
        v.insert("magnetic".into(), vec![0.0, 5.9, -48.4]);
        v.insert("magnetic-uncalibrated".into(), vec![0.0, 5.9, -48.4]);
        v.insert("orientation".into(), vec![0.0, 0.0, 0.0]);
        v.insert("temperature".into(), vec![25.0]);
        v.insert("proximity".into(), vec![1.0]);
        v.insert("light".into(), vec![400.0]);
        v.insert("pressure".into(), vec![1013.25]);
        v.insert("humidity".into(), vec![40.0]);
        v.insert("hinge-angle0".into(), vec![180.0]);
        v.insert("hinge-angle1".into(), vec![180.0]);
        v.insert("hinge-angle2".into(), vec![180.0]);
        v.insert("heart-rate".into(), vec![70.0]);
        v.insert("rgbc-light".into(), vec![0.0, 0.0, 0.0, 0.0]);
        v.insert("wrist-tilt".into(), vec![0.0]);
        Sensors { values: Mutex::new(v), mask }
    }

    /// Whether the device advertises sensor `name` (console or protocol spelling).
    pub fn advertised(&self, name: &str) -> bool {
        sim_core::goldfish::sensor_handle(Self::protocol_name(name)).is_some_and(|h| self.mask & (1 << h) != 0)
    }

    /// Console names (stock spelling) → protocol names.
    pub fn protocol_name(console: &str) -> &str {
        match console {
            "magnetic-field" => "magnetic",
            "magnetic-field-uncalibrated" => "magnetic-uncalibrated",
            other => other,
        }
    }

    pub fn set(&self, name: &str, values: Vec<f32>) -> bool {
        let name = Self::protocol_name(name);
        let mut v = self.values.lock().unwrap_or_else(|e| e.into_inner());
        match v.get_mut(name) {
            Some(slot) if slot.len() == values.len() => {
                *slot = values;
                true
            }
            _ => false,
        }
    }

    pub fn get(&self, name: &str) -> Option<Vec<f32>> {
        self.values.lock().unwrap_or_else(|e| e.into_inner()).get(Self::protocol_name(name)).cloned()
    }
}

impl Default for Sensors {
    fn default() -> Self {
        Sensors::phone()
    }
}

pub struct SensorService {
    pub sensors: Arc<Sensors>,
}

impl PipeService for SensorService {
    fn open(&self, name: &str, ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        if name != "qemud:sensors" {
            return None;
        }
        let shared = Arc::new(Stream::default());
        let (s, sensors, start) = (shared.clone(), self.sensors.clone(), Instant::now());
        let _ = std::thread::Builder::new().name(format!("sensors-{}", ctx.conn)).spawn(move || {
            crate::device_thread();
            // Every enabled sensor goes out each tick, changed or not, as stock's
            // `_hwSensorClient_tick` does (the HAL reports continuous sensors at their rate).
            // With none enabled the thread sleeps until one is. Events go out under the protocol
            // name even where the HAL enables a sensor by another one: it enables
            // `magnetic-field[-uncalibrated]` but parses `magnetic[-uncalibrated]:` events, and
            // logs "don't know how to parse" (~90 times per rotate) for the echoed spelling.
            while let Some(enabled) = s.next_tick() {
                let mut out = Vec::new();
                for name in enabled {
                    if let Some(vals) = sensors.get(&name) {
                        let joined: Vec<String> = vals.iter().map(|v| format!("{v}")).collect();
                        frame(&mut out, format!("{}:{}", Sensors::protocol_name(&name), joined.join(":")).as_bytes());
                    }
                }
                frame(&mut out, format!("sync:{}", start.elapsed().as_micros()).as_bytes());
                if !ctx.send(HostMsg::Data(out)) {
                    break;
                }
            }
        });
        Some(Box::new(SensorPipe { stream: shared, mask: self.sensors.mask, buf: Vec::new() }))
    }
}

struct Stream {
    enabled: Mutex<HashSet<String>>,
    /// Signalled when a sensor is enabled or the pipe closes.
    changed: Condvar,
    delay_ms: AtomicU64,
    stop: AtomicBool,
    /// Times the stream thread woke for a tick.
    #[cfg(test)]
    ticks: AtomicU64,
}

impl Default for Stream {
    fn default() -> Stream {
        Stream {
            enabled: Mutex::default(),
            changed: Condvar::new(),
            delay_ms: AtomicU64::new(200),
            stop: AtomicBool::new(false),
            #[cfg(test)]
            ticks: AtomicU64::new(0),
        }
    }
}

impl Stream {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.enabled.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sleeps one delay (parked meanwhile, with nothing enabled, until something is), then the
    /// sensors to send; `None` once the pipe has closed.
    fn next_tick(&self) -> Option<Vec<String>> {
        let mut en = self.lock();
        loop {
            while en.is_empty() && !self.stop.load(Ordering::SeqCst) {
                en = self.changed.wait(en).unwrap_or_else(|e| e.into_inner());
            }
            let delay = Duration::from_millis(self.delay_ms.load(Ordering::Relaxed).clamp(10, 1000));
            let stop = || self.stop.load(Ordering::SeqCst);
            en = self.changed.wait_timeout_while(en, delay, |_| !stop()).unwrap_or_else(|e| e.into_inner()).0;
            if stop() {
                return None;
            }
            #[cfg(test)]
            self.ticks.fetch_add(1, Ordering::Relaxed);
            if !en.is_empty() {
                return Some(en.iter().cloned().collect());
            }
        }
    }

    fn close(&self) {
        let _en = self.lock();
        self.stop.store(true, Ordering::SeqCst);
        self.changed.notify_all();
    }
}

struct SensorPipe {
    stream: Arc<Stream>,
    mask: u32,
    buf: Vec<u8>,
}

impl PipeHandler for SensorPipe {
    fn guest_data(&mut self, data: &[u8], to_guest: &mut Vec<u8>) -> bool {
        self.buf.extend_from_slice(data);
        for msg in take_frames(&mut self.buf) {
            let msg = String::from_utf8_lossy(&msg).into_owned();
            if msg == "list-sensors" {
                frame(to_guest, self.mask.to_string().as_bytes());
            } else if let Some(rest) = msg.strip_prefix("set-delay:") {
                if let Ok(ms) = rest.trim().parse::<u64>() {
                    self.stream.delay_ms.store(ms, Ordering::Relaxed);
                }
            } else if let Some(rest) = msg.strip_prefix("set:")
                && let Some((name, on)) = rest.rsplit_once(':')
            {
                let mut en = self.stream.lock();
                if on == "1" {
                    en.insert(name.to_string());
                    self.stream.changed.notify_all();
                } else {
                    en.remove(name);
                }
            }
            // `time:<ns>` (clock sync from the guest) needs no answer.
        }
        true
    }

    fn host_msg(&mut self, msg: HostMsg, to_guest: &mut Vec<u8>) -> bool {
        match msg {
            HostMsg::Data(d) => {
                to_guest.extend_from_slice(&d);
                true
            }
            HostMsg::Close | HostMsg::Attach(_) => false,
        }
    }

    fn closed(&mut self) {
        self.stream.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_enable_and_console_values() {
        let sensors = Arc::new(Sensors::phone());
        let (tx, rx) = std::sync::mpsc::channel();
        let svc = SensorService { sensors: sensors.clone() };
        let mut pipe = svc.open("qemud:sensors", PipeCtx { conn: 1, events: tx }).expect("open");
        let mut req = Vec::new();
        frame(&mut req, b"list-sensors");
        frame(&mut req, b"set-delay:10");
        frame(&mut req, b"set:acceleration:1");
        let mut out = Vec::new();
        assert!(pipe.guest_data(&req, &mut out));
        let mut out_buf = out.clone();
        assert_eq!(take_frames(&mut out_buf), vec![(0x7ff | (1 << 17)).to_string().into_bytes()]);
        assert!(sensors.set("acceleration", vec![1.0, 2.0, 3.0]));
        assert!(!sensors.set("acceleration", vec![1.0]), "wrong arity");
        assert!(sensors.set("magnetic-field", vec![1.0, 1.0, 1.0]), "console alias");
        // The stream thread delivers an event batch for the enabled sensor.
        let ev = rx.recv_timeout(Duration::from_secs(2)).expect("event");
        let crate::pipe::WorkerEvent::Host { msg: HostMsg::Data(mut d), .. } = ev else { panic!("data") };
        let frames = take_frames(&mut d);
        assert_eq!(frames[0], b"acceleration:1:2:3");
        assert!(frames[1].starts_with(b"sync:"));
        pipe.closed();
    }

    #[test]
    fn the_stream_sleeps_while_nothing_is_enabled_and_keeps_its_rate_when_something_is() {
        let s = Arc::new(Stream::default());
        s.delay_ms.store(10, Ordering::Relaxed);
        let t = s.clone();
        let sent = std::thread::spawn(move || {
            let mut batches = 0;
            while t.next_tick().is_some() {
                batches += 1;
            }
            batches
        });
        std::thread::sleep(Duration::from_millis(200));
        // Before, a 10 ms delay woke the thread 100 times a second with nothing to send.
        assert_eq!(s.ticks.load(Ordering::Relaxed), 0, "parked");
        let mut pipe = SensorPipe { stream: s.clone(), mask: 1, buf: Vec::new() };
        let mut req = Vec::new();
        frame(&mut req, b"set:acceleration:1");
        let enabled_at = Instant::now();
        assert!(pipe.guest_data(&req, &mut Vec::new()));
        // Wait for the event, not for a fixed time: a starved host stretches every sleep, so the
        // deadline only bounds a hang. It ticks, and a few ticks are enough to check the rate.
        let deadline = Instant::now() + Duration::from_secs(5);
        while s.ticks.load(Ordering::Relaxed) < 5 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        let ticks = s.ticks.load(Ordering::Relaxed);
        // Read the clock after the count: a late read only loosens the bound.
        let elapsed = enabled_at.elapsed();
        assert!(ticks >= 5, "only {ticks} ticks in {elapsed:?}");
        // Never faster than the requested 10 ms, by the time that really passed (unchanged
        // values included).
        let most = (elapsed.as_millis() / 10) as u64 + 1;
        assert!(ticks <= most, "{ticks} ticks in {elapsed:?} (at most {most})");
        // Disabled again: parked again (at most the tick in flight).
        let mut req = Vec::new();
        frame(&mut req, b"set:acceleration:0");
        assert!(pipe.guest_data(&req, &mut Vec::new()));
        std::thread::sleep(Duration::from_millis(30));
        let parked = s.ticks.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(200));
        assert!(s.ticks.load(Ordering::Relaxed) <= parked + 1);
        let closed = Instant::now();
        pipe.closed();
        assert!(sent.join().expect("stream") as u64 >= ticks);
        assert!(closed.elapsed() < Duration::from_secs(2), "a close wakes it: {:?}", closed.elapsed());
    }

    /// One batch of event lines after the guest enables `enable` (a name as the HAL spells it).
    fn lines_after_enabling(sensors: Arc<Sensors>, enable: &[&str]) -> Vec<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut pipe = SensorService { sensors }.open("qemud:sensors", PipeCtx { conn: 1, events: tx }).expect("open");
        let mut req = Vec::new();
        frame(&mut req, b"set-delay:10");
        for name in enable {
            frame(&mut req, format!("set:{name}:1").as_bytes());
        }
        assert!(pipe.guest_data(&req, &mut Vec::new()));
        let ev = rx.recv_timeout(Duration::from_secs(2)).expect("event");
        let crate::pipe::WorkerEvent::Host { msg: HostMsg::Data(mut d), .. } = ev else { panic!("data") };
        pipe.closed();
        let mut lines: Vec<String> = take_frames(&mut d).into_iter().map(|l| String::from_utf8(l).expect("utf8")).collect();
        assert!(lines.pop().is_some_and(|l| l.starts_with("sync:")));
        lines.sort();
        lines
    }

    #[test]
    fn the_hal_enables_magnetic_field_and_gets_events_named_magnetic() {
        // The HAL's `set:` names (traced on API 36) differ from the names its parser takes for
        // events: it logged "don't know how to parse 'magnetic-field:0:5.9:-48.4'".
        let lines = lines_after_enabling(Arc::new(Sensors::phone()), &["magnetic-field", "magnetic-field-uncalibrated"]);
        assert_eq!(lines, ["magnetic-uncalibrated:0:5.9:-48.4", "magnetic:0:5.9:-48.4"]);
    }

    #[test]
    fn every_sensor_event_line_is_its_protocol_name_and_values() {
        // Every sensor, enabled by the name the HAL sends (stock's console spelling for the two
        // magnetic ones): a line per sensor, `<protocol name>:<v>[:<v>...]`, one value per axis.
        let sensors = Arc::new(Sensors::new((1 << NAMES.len()) - 1));
        let enable: Vec<&str> = NAMES
            .iter()
            .map(|n| match *n {
                "magnetic" => "magnetic-field",
                "magnetic-uncalibrated" => "magnetic-field-uncalibrated",
                other => other,
            })
            .collect();
        let lines = lines_after_enabling(sensors.clone(), &enable);
        assert_eq!(lines.len(), NAMES.len());
        for name in NAMES {
            let want = format!("{name}:{}", sensors.get(name).expect("value").iter().map(|v| v.to_string()).collect::<Vec<_>>().join(":"));
            assert!(lines.contains(&want), "{want} in {lines:?}");
        }
        assert!(lines.iter().all(|l| !l.contains("field")), "no `-field` spelling on the wire: {lines:?}");
    }

    #[test]
    fn every_sensor_takes_as_many_values_as_sim_core_says() {
        // The API checks a `sensor` call's value count against sim_core's table before the console.
        let sensors = Sensors::new((1 << NAMES.len()) - 1);
        for name in NAMES {
            let width = sim_core::goldfish::sensor_values(name).expect(name);
            assert_eq!(sensors.get(name).map(|v| v.len()), Some(width), "{name}");
            assert!(sensors.set(name, vec![1.0; width]) && !sensors.set(name, vec![1.0; width + 1]), "{name}");
        }
    }

    #[test]
    fn the_mask_decides_what_the_hal_lists() {
        let watch = (0x7ff | (1 << 17)) | (1 << 14) | (1 << 16);
        let sensors = Arc::new(Sensors::new(watch));
        assert!(sensors.advertised("heart-rate") && sensors.advertised("wrist-tilt") && sensors.advertised("magnetic-field"));
        assert!(!sensors.advertised("hinge-angle0") && !sensors.advertised("nope"));
        assert!(sensors.set("wrist-tilt", vec![1.0]) && sensors.set("hinge-angle1", vec![90.0]), "every sensor has a value");
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut pipe = SensorService { sensors }.open("qemud:sensors", PipeCtx { conn: 1, events: tx }).expect("open");
        let mut req = Vec::new();
        frame(&mut req, b"list-sensors");
        let mut out = Vec::new();
        assert!(pipe.guest_data(&req, &mut out));
        assert_eq!(take_frames(&mut out), vec![watch.to_string().into_bytes()]);
        pipe.closed();
    }
}
