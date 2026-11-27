//! `qemud:gps`: NMEA sentences for the goldfish GNSS HAL.
//!
//! The HAL reads raw bytes (no qemud framing), collects `$...\r\n` lines and parses `$GPGGA`
//! (altitude) and `$GPRMC` (position, speed, course; validity must be `A`) with fixed `sscanf`
//! formats, so field layout matters: `DDMM.mmmm` / `DDDMM.mmmm` with the hemisphere letters,
//! a numeric magnetic variation plus its letter in RMC, and `alt,M` after an empty HDOP in GGA.
//! The console's `geo fix` sets the position; open pipes get it immediately and once a second
//! after that (apps polling for updates expect a steady stream, as on stock).
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{HostMsg, PipeCtx, PipeHandler, PipeService};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fix {
    pub latitude: f64,
    pub longitude: f64,
    pub altitude: f64,
    pub satellites: u32,
    pub speed_knots: f64,
    pub course: f64,
}

/// The current fix; pipes park on `changed` until it changes, their repeat is due or they close.
#[derive(Default)]
pub struct Gps {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    fix: Option<Fix>,
    /// Bumped by every `set_fix`, so a pipe sends a fix set twice to the same place twice.
    version: u64,
}

/// How often an open pipe repeats the current fix.
const REPEAT: Duration = Duration::from_secs(1);

impl Gps {
    pub fn set_fix(&self, fix: Fix) {
        let mut s = self.lock();
        s.fix = Some(fix);
        s.version += 1;
        drop(s);
        self.changed.notify_all();
    }

    pub fn fix(&self) -> Option<Fix> {
        self.lock().fix
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Waits until a fix newer than `seen` is set, `until` passes or `stop` is set; then the
    /// fix and its version.
    fn wait(&self, seen: u64, until: Instant, stop: &AtomicBool) -> (Option<Fix>, u64) {
        let mut s = self.lock();
        while s.version == seen && !stop.load(Ordering::SeqCst) {
            let Some(left) = until.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else { break };
            s = self.changed.wait_timeout(s, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        (s.fix, s.version)
    }

    /// Wakes the pipes so a closed one sees its `stop` (set before this).
    fn wake(&self) {
        drop(self.lock());
        self.changed.notify_all();
    }
}

fn checksum(body: &str) -> u8 {
    body.bytes().fold(0, |a, b| a ^ b)
}

fn sentence(body: &str) -> String {
    format!("${body}*{:02X}\r\n", checksum(body))
}

/// `DDMM.mmmm` (or `DDDMM.mmmm` for longitude) and the hemisphere letter.
fn dmm(value: f64, lon: bool, pos: char, neg: char) -> (String, char) {
    let hemi = if value < 0.0 { neg } else { pos };
    let v = value.abs();
    let mut deg = v.trunc() as u32;
    let mut minutes = ((v - f64::from(deg)) * 60.0 * 10_000.0).round() / 10_000.0;
    if minutes >= 60.0 {
        deg += 1;
        minutes -= 60.0;
    }
    let s = if lon { format!("{deg:03}{minutes:07.4}") } else { format!("{deg:02}{minutes:07.4}") };
    (s, hemi)
}

/// Civil (UTC) date from days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `$GPGGA` then `$GPRMC` for `fix` at `unix_secs`.
pub fn nmea(fix: &Fix, unix_secs: u64) -> String {
    let secs = unix_secs as i64;
    let (y, mo, d) = civil(secs.div_euclid(86_400));
    let tod = secs.rem_euclid(86_400);
    let hms = format!("{:02}{:02}{:02}", tod / 3600, tod / 60 % 60, tod % 60);
    let date = format!("{d:02}{mo:02}{:02}", y.rem_euclid(100));
    let (lat, ns) = dmm(fix.latitude, false, 'N', 'S');
    let (lon, ew) = dmm(fix.longitude, true, 'E', 'W');
    let gga = format!("GPGGA,{hms},{lat},{ns},{lon},{ew},1,{},,{:.1},M,0.,M,,,", fix.satellites, fix.altitude);
    let rmc = format!("GPRMC,{hms},A,{lat},{ns},{lon},{ew},{:.1},{:.1},{date},0.0,E", fix.speed_knots, fix.course);
    sentence(&gga) + &sentence(&rmc)
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub struct GpsService {
    pub gps: Arc<Gps>,
}

impl PipeService for GpsService {
    fn open(&self, name: &str, ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        if name != "qemud:gps" {
            return None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (gps, s) = (self.gps.clone(), stop.clone());
        let _ = std::thread::Builder::new().name(format!("gps-{}", ctx.conn)).spawn(move || {
            crate::device_thread();
            // A new fix goes out right away; the current one is repeated every second. In
            // between the thread sleeps on the fix (no polling: one wakeup per sentence).
            let (mut seen, mut due) = (u64::MAX, Instant::now());
            loop {
                let (fix, version) = gps.wait(seen, due, &s);
                if s.load(Ordering::SeqCst) {
                    break;
                }
                if let Some(f) = fix
                    && (version != seen || Instant::now() >= due)
                    && !ctx.send(HostMsg::Data(nmea(&f, now_secs()).into_bytes()))
                {
                    break;
                }
                seen = version;
                // No fix yet: nothing to repeat, so sleep until one is set.
                due = Instant::now() + if fix.is_some() { REPEAT } else { Duration::from_secs(3600) };
            }
        });
        Some(Box::new(GpsPipe { stop, gps: self.gps.clone() }))
    }
}

struct GpsPipe {
    stop: Arc<AtomicBool>,
    gps: Arc<Gps>,
}

impl PipeHandler for GpsPipe {
    fn guest_data(&mut self, _data: &[u8], _to_guest: &mut Vec<u8>) -> bool {
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
        self.stop.store(true, Ordering::SeqCst);
        self.gps.wake();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::pipe::WorkerEvent;

    fn fix(latitude: f64) -> Fix {
        Fix { latitude, longitude: 2.0, altitude: 0.0, satellites: 6, speed_knots: 0.0, course: 0.0 }
    }

    /// `geo fix` to the pipe's sentences: how long a new fix takes to go out, and how many
    /// sentences an unchanged fix sends in `idle`.
    fn fix_latency(runs: usize, idle: Duration) -> (f64, usize) {
        let gps = Arc::new(Gps::default());
        let (tx, rx) = std::sync::mpsc::channel();
        let mut pipe = GpsService { gps: gps.clone() }.open("qemud:gps", PipeCtx { conn: 1, events: tx }).expect("open");
        let mut lat = Vec::new();
        for i in 0..runs {
            // Off the poll grid, as a console command lands.
            std::thread::sleep(Duration::from_millis(37));
            let t = Instant::now();
            let latitude = 10.0 + i as f64;
            gps.set_fix(fix(latitude));
            let want = format!(",{},N,", dmm(latitude, false, 'N', 'S').0);
            loop {
                let Ok(WorkerEvent::Host { msg: HostMsg::Data(d), .. }) = rx.recv_timeout(Duration::from_secs(3)) else {
                    panic!("no sentence")
                };
                // Skip a repeat of the previous fix that crossed the update.
                if String::from_utf8_lossy(&d).contains(&want) {
                    break;
                }
            }
            lat.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        while rx.try_recv().is_ok() {}
        std::thread::sleep(idle);
        let repeats = rx.try_iter().count();
        pipe.closed();
        lat.sort_by(f64::total_cmp);
        (lat[lat.len() / 2], repeats)
    }

    #[test]
    fn a_new_fix_goes_out_and_the_current_one_repeats() {
        let (latency, repeats) = fix_latency(2, Duration::from_millis(2300));
        assert!((1..=3).contains(&repeats), "about one a second: {repeats}");
        assert!(latency < 50.0, "a new fix doesn't wait for a poll: {latency:.1} ms");
    }

    #[test]
    fn a_closed_pipe_stops_at_once_and_spares_the_others() {
        let gps = Arc::new(Gps::default());
        let svc = GpsService { gps: gps.clone() };
        let (tx, rx) = std::sync::mpsc::channel();
        let mut a = svc.open("qemud:gps", PipeCtx { conn: 1, events: tx }).expect("open");
        let (tx, other) = std::sync::mpsc::channel();
        let mut b = svc.open("qemud:gps", PipeCtx { conn: 2, events: tx }).expect("open");
        // No fix: nothing is sent, however long.
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        let t = Instant::now();
        a.closed();
        // Its thread is gone once its sender is.
        assert!(matches!(rx.recv_timeout(Duration::from_secs(2)), Err(std::sync::mpsc::RecvTimeoutError::Disconnected)));
        assert!(t.elapsed() < Duration::from_millis(50), "{:?}", t.elapsed());
        gps.set_fix(fix(1.0));
        assert!(matches!(other.recv_timeout(Duration::from_secs(2)), Ok(WorkerEvent::Host { msg: HostMsg::Data(_), .. })));
        b.closed();
    }

    /// `geo fix` -> sentences on the pipe, median of 21.
    ///     cargo test --release -p simee-emu-vmm gps_fix_latency -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn gps_fix_latency() {
        let (median, repeats) = fix_latency(21, Duration::from_secs(5));
        println!("gps: new fix -> sentences {median:.2} ms (median of 21); unchanged fix: {repeats} sends in 5 s");
    }

    #[test]
    fn sentences_match_the_hal_formats() {
        // 2020-04-01 19:52:06 UTC, the HAL source's own RMC example time.
        let t = 1_585_770_726;
        let fix = Fix { latitude: 48.117303, longitude: -122.082999, altitude: 4.2, satellites: 6, speed_knots: 0.0, course: 0.0 };
        let out = nmea(&fix, t);
        let lines: Vec<&str> = out.split_inclusive("\r\n").collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("$GPGGA,195206,4807.0382,N,12204.9799,W,1,6,,4.2,M,0.,M,,,*"), "{}", lines[0]);
        assert!(lines[1].starts_with("$GPRMC,195206,A,4807.0382,N,12204.9799,W,0.0,0.0,010420,0.0,E*"), "{}", lines[1]);
        for l in lines {
            let body = &l[1..l.find('*').expect("star")];
            let cs = &l[l.find('*').expect("star") + 1..l.len() - 2];
            assert_eq!(u8::from_str_radix(cs, 16).expect("hex"), checksum(body));
        }
        // Minutes that round up to 60 carry into the degrees.
        assert_eq!(dmm(10.999_999_9, false, 'N', 'S').0, "1100.0000");
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(19_358), (2023, 1, 1));
    }
}
