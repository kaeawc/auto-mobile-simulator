//! `qemud:gps`: NMEA sentences for the goldfish GNSS HAL.
//!
//! The HAL reads raw bytes (no qemud framing), collects `$...\r\n` lines and parses `$GPGGA`
//! (altitude) and `$GPRMC` (position, speed, course; validity must be `A`) with fixed `sscanf`
//! formats, so field layout matters: `DDMM.mmmm` / `DDDMM.mmmm` with the hemisphere letters,
//! a numeric magnetic variation plus its letter in RMC, and `alt,M` after an empty HDOP in GGA.
//! The console's `geo fix` sets the position; open pipes get it immediately and once a second
//! after that (apps polling for updates expect a steady stream, as on stock).
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

#[derive(Default)]
pub struct Gps {
    fix: Mutex<Option<Fix>>,
}

impl Gps {
    pub fn set_fix(&self, fix: Fix) {
        *self.fix.lock().unwrap_or_else(|e| e.into_inner()) = Some(fix);
    }

    pub fn fix(&self) -> Option<Fix> {
        *self.fix.lock().unwrap_or_else(|e| e.into_inner())
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
            let mut last: Option<Fix> = None;
            let mut since = Duration::ZERO;
            while !s.load(Ordering::Relaxed) {
                let fix = gps.fix();
                // A new fix goes out right away; the current one is repeated every second.
                if let Some(f) = fix
                    && (fix != last || since >= Duration::from_secs(1))
                {
                    if !ctx.send(HostMsg::Data(nmea(&f, now_secs()).into_bytes())) {
                        break;
                    }
                    since = Duration::ZERO;
                }
                last = fix;
                std::thread::sleep(Duration::from_millis(100));
                since += Duration::from_millis(100);
            }
        });
        Some(Box::new(GpsPipe { stop }))
    }
}

struct GpsPipe {
    stop: Arc<AtomicBool>,
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
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
