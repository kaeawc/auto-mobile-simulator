//! ARM PL011 UART: the guest console (`ttyAMA0`).
use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::bus::{MmioDevice, le_value, set_le};
use crate::irq::IrqLine;

const DR: u64 = 0x00;
const RSR: u64 = 0x04;
const FR: u64 = 0x18;
const ILPR: u64 = 0x20;
const IBRD: u64 = 0x24;
const FBRD: u64 = 0x28;
const LCR_H: u64 = 0x2c;
const CR: u64 = 0x30;
const IFLS: u64 = 0x34;
const IMSC: u64 = 0x38;
const RIS: u64 = 0x3c;
const MIS: u64 = 0x40;
const ICR: u64 = 0x44;
const DMACR: u64 = 0x48;

const FR_RXFE: u32 = 1 << 4;
const FR_TXFE: u32 = 1 << 7;
const INT_RX: u32 = 1 << 4;
const INT_TX: u32 = 1 << 5;
const INT_RT: u32 = 1 << 6;

/// PeriphID0-3 then PCellID0-3 at 0xfe0..0x1000 (the AMBA bus matches drivers on these).
const ID: [u8; 8] = [0x11, 0x10, 0x14, 0x00, 0x0d, 0xf0, 0x05, 0xb1];

struct State {
    rx: std::collections::VecDeque<u8>,
    ris: u32,
    imsc: u32,
    regs: [u32; 8], // ilpr, ibrd, fbrd, lcr_h, cr, ifls, dmacr, rsr
    line_buf: Vec<u8>,
    /// The level last driven on the line (it starts low): the controller hears changes only.
    level: bool,
}

pub struct Pl011 {
    state: Mutex<State>,
    irq: Arc<dyn IrqLine>,
    out: Mutex<Box<dyn Write + Send>>,
}

impl Pl011 {
    pub fn new(irq: Arc<dyn IrqLine>, out: Box<dyn Write + Send>) -> Pl011 {
        Pl011 {
            state: Mutex::new(State {
                rx: Default::default(),
                ris: 0,
                imsc: 0,
                regs: [0, 0, 0, 0, 0x300, 0x12, 0, 0],
                line_buf: Vec::with_capacity(256),
                level: false,
            }),
            irq,
            out: Mutex::new(out),
        }
    }

    /// Queues host input for the guest.
    pub fn push_input(&self, bytes: &[u8]) {
        let mut s = self.lock();
        s.rx.extend(bytes);
        s.ris |= INT_RX | INT_RT;
        self.update(&mut s);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Drives the line to the masked status, only when that changes it: every console byte
    /// is a register access, and each call into the controller is a hypervisor call (plus a
    /// vCPU wake on a rise).
    fn update(&self, s: &mut State) {
        let level = s.ris & s.imsc != 0;
        if level != s.level {
            s.level = level;
            self.irq.set(level);
        }
    }

    fn emit(&self, s: &mut State, b: u8) {
        // Line-buffer so concurrent writers to the host stream don't interleave mid-line.
        s.line_buf.push(b);
        if b == b'\n' || s.line_buf.len() >= 1024 {
            let mut out = self.out.lock().unwrap_or_else(|p| p.into_inner());
            let _ = out.write_all(&s.line_buf);
            let _ = out.flush();
            s.line_buf.clear();
        }
    }
}

impl MmioDevice for Pl011 {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let mut s = self.lock();
        let v: u32 = match offset {
            DR => {
                let b = s.rx.pop_front().unwrap_or(0);
                if s.rx.is_empty() {
                    s.ris &= !(INT_RX | INT_RT);
                }
                self.update(&mut s);
                u32::from(b)
            }
            RSR => s.regs[7],
            FR => FR_TXFE | if s.rx.is_empty() { FR_RXFE } else { 0 },
            ILPR => s.regs[0],
            IBRD => s.regs[1],
            FBRD => s.regs[2],
            LCR_H => s.regs[3],
            CR => s.regs[4],
            IFLS => s.regs[5],
            IMSC => s.imsc,
            RIS => s.ris,
            MIS => s.ris & s.imsc,
            DMACR => s.regs[6],
            0xfe0..=0xffc => u32::from(ID[((offset - 0xfe0) / 4) as usize]),
            _ => 0,
        };
        set_le(data, u64::from(v));
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let v = le_value(data) as u32;
        let mut s = self.lock();
        match offset {
            DR => {
                self.emit(&mut s, v as u8);
                // The transmitter is infinitely fast: the FIFO is always empty again.
                s.ris |= INT_TX;
            }
            RSR => s.regs[7] = 0,
            ILPR => s.regs[0] = v,
            IBRD => s.regs[1] = v,
            FBRD => s.regs[2] = v,
            LCR_H => s.regs[3] = v,
            CR => s.regs[4] = v,
            IFLS => s.regs[5] = v,
            IMSC => {
                s.imsc = v & 0x7ff;
                // Enabling the TX interrupt with an empty FIFO fires it immediately.
                if s.imsc & INT_TX != 0 {
                    s.ris |= INT_TX;
                }
            }
            ICR => s.ris &= !v,
            DMACR => s.regs[6] = v,
            _ => {}
        }
        self.update(&mut s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irq::{CountLine, TestLine};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn rd(u: &Pl011, off: u64) -> u32 {
        let mut b = [0u8; 4];
        u.read(off, &mut b);
        u32::from_le_bytes(b)
    }

    #[test]
    fn transmits_lines_and_raises_tx() {
        let line = Arc::new(TestLine::default());
        let sink = Sink::default();
        let u = Pl011::new(line.clone(), Box::new(sink.clone()));
        u.write(IMSC, &INT_TX.to_le_bytes());
        assert!(line.level(), "TX interrupt with empty FIFO");
        u.write(ICR, &INT_TX.to_le_bytes());
        assert!(!line.level());
        for b in b"hi\n" {
            u.write(DR, &[*b, 0, 0, 0]);
        }
        assert_eq!(sink.0.lock().expect("lock").as_slice(), b"hi\n");
        assert_eq!(rd(&u, 0xfe0), 0x11);
        assert_eq!(rd(&u, 0xffc), 0xb1);
    }

    #[test]
    fn receives() {
        let line = Arc::new(TestLine::default());
        let u = Pl011::new(line.clone(), Box::new(std::io::sink()));
        assert_ne!(rd(&u, FR) & FR_RXFE, 0);
        u.write(IMSC, &INT_RX.to_le_bytes());
        u.push_input(b"a");
        assert!(line.level());
        assert_eq!(rd(&u, FR) & FR_RXFE, 0);
        assert_eq!(rd(&u, DR), u32::from(b'a'));
        assert!(!line.level());
    }

    /// Every console byte is a DR write and every input byte a DR read: the controller is told
    /// only when the line changes, not on every access.
    #[test]
    fn drives_the_line_only_on_changes() {
        let line = Arc::new(CountLine::default());
        let u = Pl011::new(line.clone(), Box::new(std::io::sink()));
        u.write(IMSC, &(INT_RX | INT_TX).to_le_bytes());
        assert!(line.level(), "TX fires on enable");
        for b in b"a console line\n" {
            u.write(DR, &[*b, 0, 0, 0]);
        }
        u.push_input(b"typed");
        for _ in 0..5 {
            rd(&u, DR);
            rd(&u, FR);
        }
        assert!(line.level(), "TX still pending");
        u.write(ICR, &INT_TX.to_le_bytes());
        assert!(!line.level());
        u.write(CR, &0x301u32.to_le_bytes());
        u.push_input(b"x");
        assert!(line.level());
        rd(&u, DR);
        assert!(!line.level());
        assert_eq!((line.calls(), line.transitions()), (4, 4));
    }
}
