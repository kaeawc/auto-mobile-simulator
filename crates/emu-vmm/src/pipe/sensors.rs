//! `qemud:sensors`: the goldfish sensors HAL's host side.
//!
//! The HAL sends `list-sensors` (we answer with a bitmask of sensor handles), then
//! `set:<name>:<0|1>` and `set-delay:<ms>`; we stream `<name>:<values>` events for enabled
//! sensors followed by `sync:<µs>`. Values live in [`Sensors`], which the console's
//! `sensor set|get` changes (shake, rotation, hinge angle, proximity, ...).
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::qemud::{frame, take_frames};
use super::{HostMsg, PipeCtx, PipeHandler, PipeService};

/// Protocol names in handle order (handle = index), as the HAL's sensor list defines them.
pub const NAMES: [&str; 18] = [
    "acceleration",
    "gyroscope",
    "magnetic",
    "orientation",
    "temperature",
    "proximity",
    "light",
    "pressure",
    "humidity",
    "magnetic-uncalibrated",
    "gyroscope-uncalibrated",
    "hinge-angle0",
    "hinge-angle1",
    "hinge-angle2",
    "heart-rate",
    "",
    "wrist-tilt",
    "acceleration-uncalibrated",
];

/// Shared, live sensor values.
pub struct Sensors {
    values: Mutex<HashMap<String, Vec<f32>>>,
    /// Bitmask of sensor handles the device has (phones: motion + environment).
    pub mask: u32,
}

impl Sensors {
    pub fn phone() -> Sensors {
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
        v.insert("heart-rate".into(), vec![70.0]);
        // Handles 0–10 and 17: motion, magnetic, orientation and environment sensors.
        let mask = 0x7ff | (1 << 17);
        Sensors { values: Mutex::new(v), mask }
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
        let shared = Arc::new(Stream { enabled: Mutex::new(HashSet::new()), delay_ms: AtomicU64::new(200), stop: AtomicBool::new(false) });
        let (s, sensors, start) = (shared.clone(), self.sensors.clone(), Instant::now());
        let _ = std::thread::Builder::new().name(format!("sensors-{}", ctx.conn)).spawn(move || {
            while !s.stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(s.delay_ms.load(Ordering::Relaxed).clamp(10, 1000)));
                let enabled: Vec<String> = s.enabled.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect();
                if enabled.is_empty() {
                    continue;
                }
                let mut out = Vec::new();
                for name in enabled {
                    if let Some(vals) = sensors.get(&name) {
                        let joined: Vec<String> = vals.iter().map(|v| format!("{v}")).collect();
                        frame(&mut out, format!("{name}:{}", joined.join(":")).as_bytes());
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
    delay_ms: AtomicU64,
    stop: AtomicBool,
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
                let mut en = self.stream.enabled.lock().unwrap_or_else(|e| e.into_inner());
                if on == "1" {
                    en.insert(name.to_string());
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
        self.stream.stop.store(true, Ordering::Relaxed);
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
}
