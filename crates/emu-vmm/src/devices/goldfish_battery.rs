//! Goldfish battery ("google,goldfish-battery", stock `0x0a04_0000`, SPI 17): the guest's
//! `battery` and `ac` power supplies. Registers are 32-bit; reading INT_STATUS returns and
//! clears the change bits (battery 1 << 0, AC 1 << 1), and the line stays up while unread
//! enabled bits remain. The console's `power` command changes values through [`Battery`].
use std::sync::{Arc, Mutex};

use crate::bus::{MmioDevice, set_le};
use crate::irq::IrqLine;

const INT_STATUS: u64 = 0x00;
const INT_ENABLE: u64 = 0x04;
const AC_ONLINE: u64 = 0x08;
const STATUS: u64 = 0x0c;
const HEALTH: u64 = 0x10;
const PRESENT: u64 = 0x14;
const CAPACITY: u64 = 0x18;
const VOLTAGE: u64 = 0x1c;
const TEMP: u64 = 0x20;
const CHARGE_COUNTER: u64 = 0x24;
const VOLTAGE_MAX: u64 = 0x28;
const CURRENT_MAX: u64 = 0x2c;
const CURRENT_NOW: u64 = 0x30;
const CURRENT_AVG: u64 = 0x34;
const CHARGE_FULL_UAH: u64 = 0x38;
const CYCLE_COUNT: u64 = 0x40;

const BATTERY_CHANGED: u32 = 1 << 0;
const AC_CHANGED: u32 = 1 << 1;

/// `POWER_SUPPLY_STATUS_*`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Status {
    Unknown = 0,
    Charging = 1,
    Discharging = 2,
    NotCharging = 3,
    Full = 4,
}

/// `POWER_SUPPLY_HEALTH_*`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Health {
    Unknown = 0,
    Good = 1,
    Overheat = 2,
    Dead = 3,
    Overvoltage = 4,
    Failure = 5,
    Cold = 6,
}

#[derive(Clone, Copy, Debug)]
pub struct State {
    pub ac_online: bool,
    pub status: Status,
    pub health: Health,
    pub present: bool,
    /// Percent, 0–100.
    pub capacity: u32,
}

impl Default for State {
    /// Stock defaults: on AC, charging, healthy, full.
    fn default() -> Self {
        State { ac_online: true, status: Status::Charging, health: Health::Good, present: true, capacity: 100 }
    }
}

struct Inner {
    state: State,
    int_status: u32,
    int_enable: u32,
    irq: Option<Arc<dyn IrqLine>>,
}

impl Inner {
    fn update_line(&self) {
        if let Some(irq) = &self.irq {
            irq.set(self.int_status & self.int_enable != 0);
        }
    }
}

/// Shared battery state: the device reads it, the console changes it.
pub struct Battery {
    inner: Mutex<Inner>,
}

impl Default for Battery {
    fn default() -> Self {
        Battery { inner: Mutex::new(Inner { state: State::default(), int_status: 0, int_enable: 0, irq: None }) }
    }
}

impl Battery {
    pub fn state(&self) -> State {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).state
    }

    /// Applies `f` and raises the change interrupts for what it changed.
    pub fn update(&self, f: impl FnOnce(&mut State)) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let before = g.state;
        f(&mut g.state);
        let s = g.state;
        if s.ac_online != before.ac_online {
            g.int_status |= AC_CHANGED;
        }
        if s.status != before.status || s.health != before.health || s.present != before.present || s.capacity != before.capacity {
            g.int_status |= BATTERY_CHANGED;
        }
        g.update_line();
    }
}

pub struct GoldfishBattery {
    battery: Arc<Battery>,
}

impl GoldfishBattery {
    /// Attaches `battery` to this run's interrupt line (a guest reboot builds a new device).
    pub fn new(battery: Arc<Battery>, irq: Arc<dyn IrqLine>) -> GoldfishBattery {
        {
            let mut g = battery.inner.lock().unwrap_or_else(|e| e.into_inner());
            g.irq = Some(irq);
            g.int_status = 0;
            g.int_enable = 0;
            g.update_line();
        }
        GoldfishBattery { battery }
    }
}

impl MmioDevice for GoldfishBattery {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let mut g = self.battery.inner.lock().unwrap_or_else(|e| e.into_inner());
        let s = g.state;
        let v: u32 = match offset {
            INT_STATUS => {
                let v = g.int_status & (BATTERY_CHANGED | AC_CHANGED);
                g.int_status = 0;
                g.update_line();
                v
            }
            INT_ENABLE => g.int_enable,
            AC_ONLINE => u32::from(s.ac_online),
            STATUS => s.status as u32,
            HEALTH => s.health as u32,
            PRESENT => u32::from(s.present),
            CAPACITY => s.capacity.min(100),
            VOLTAGE => 3_800_000,
            TEMP => 250,
            CHARGE_COUNTER => 10_000,
            VOLTAGE_MAX => 5_000_000,
            CURRENT_MAX => 2_000_000,
            CURRENT_NOW => 900_000,
            CURRENT_AVG => 900_000,
            CHARGE_FULL_UAH => 4_000_000,
            CYCLE_COUNT => 10,
            _ => 0,
        };
        set_le(data, u64::from(v));
    }

    fn write(&self, offset: u64, data: &[u8]) {
        if offset == INT_ENABLE {
            let mut g = self.battery.inner.lock().unwrap_or_else(|e| e.into_inner());
            g.int_enable = crate::bus::le_value(data) as u32;
            g.update_line();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct Line(AtomicBool);
    impl IrqLine for Line {
        fn set(&self, level: bool) {
            self.0.store(level, Ordering::SeqCst);
        }
    }

    fn rd(d: &GoldfishBattery, off: u64) -> u32 {
        let mut b = [0u8; 4];
        d.read(off, &mut b);
        u32::from_le_bytes(b)
    }

    #[test]
    fn values_and_change_interrupts() {
        let battery = Arc::new(Battery::default());
        let line = Arc::new(Line::default());
        let d = GoldfishBattery::new(battery.clone(), line.clone());
        assert_eq!((rd(&d, AC_ONLINE), rd(&d, STATUS), rd(&d, CAPACITY), rd(&d, PRESENT)), (1, 1, 100, 1));
        battery.update(|s| s.capacity = 42);
        assert!(!line.0.load(Ordering::SeqCst), "masked until the driver enables it");
        d.write(INT_ENABLE, &(BATTERY_CHANGED | AC_CHANGED).to_le_bytes());
        assert!(line.0.load(Ordering::SeqCst));
        assert_eq!(rd(&d, INT_STATUS), BATTERY_CHANGED);
        assert!(!line.0.load(Ordering::SeqCst), "reading clears it");
        assert_eq!(rd(&d, CAPACITY), 42);
        battery.update(|s| s.ac_online = false);
        assert_eq!(rd(&d, INT_STATUS), AC_CHANGED);
        battery.update(|s| s.capacity = 42);
        assert_eq!(rd(&d, INT_STATUS), 0, "no change, no interrupt");
    }
}
