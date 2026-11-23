//! ARM PL031 real-time clock: seconds since the epoch, from the host clock.
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::bus::{MmioDevice, le_value, set_le};

const DR: u64 = 0x00;
const MR: u64 = 0x04;
const LR: u64 = 0x08;
const CR: u64 = 0x0c;
const IMSC: u64 = 0x10;
const RIS: u64 = 0x14;
const MIS: u64 = 0x18;
const ICR: u64 = 0x1c;
const ID: [u8; 8] = [0x31, 0x10, 0x04, 0x00, 0x0d, 0xf0, 0x05, 0xb1];

#[derive(Default)]
struct State {
    /// Guest time = host time + offset (set by writing LR).
    offset: i64,
    mr: u32,
    imsc: u32,
}

#[derive(Default)]
pub struct Pl031 {
    state: Mutex<State>,
}

fn host_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl MmioDevice for Pl031 {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let v: u32 = match offset {
            DR => (host_secs() + s.offset) as u32,
            MR => s.mr,
            LR => (host_secs() + s.offset) as u32,
            CR => 1,
            IMSC => s.imsc,
            RIS | MIS => 0,
            0xfe0..=0xffc => u32::from(ID[((offset - 0xfe0) / 4) as usize]),
            _ => 0,
        };
        set_le(data, u64::from(v));
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let v = le_value(data) as u32;
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        match offset {
            LR => s.offset = i64::from(v) - host_secs(),
            MR => s.mr = v,
            IMSC => s.imsc = v & 1,
            ICR | CR => {}
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_host_time_and_honours_load() {
        let rtc = Pl031::default();
        let mut b = [0u8; 4];
        rtc.read(DR, &mut b);
        let now = u32::from_le_bytes(b) as i64;
        assert!((now - host_secs()).abs() <= 1);
        rtc.write(LR, &1000u32.to_le_bytes());
        rtc.read(DR, &mut b);
        assert!((u32::from_le_bytes(b) as i64 - 1000).abs() <= 1);
    }
}
