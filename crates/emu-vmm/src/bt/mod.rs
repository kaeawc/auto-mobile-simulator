//! Short-range radios behind virtio-console ports, as the stock board has them: a Bluetooth
//! controller on the `bluetooth` port and a UWB subsystem on the `uwb` port (docs/research/bluetooth.md).
//!
//! - Bluetooth: `init.ranchu.rc` links `vendor.qemu.vport.bluetooth` to `/dev/bluetooth0`, where
//!   `bt_vhci_forwarder` bridges it to the kernel's `/dev/vhci` (so the guest has an `hci0`);
//!   the HAL, `android.hardware.bluetooth-service.default`, opens hci0's user channel. The port
//!   carries H4 ([`h4`]); [`controller`] answers it. Stock serves it from netsim (rootcanal).
//! - UWB: `init.ranchu.rc` starts `android.hardware.uwb-service /dev/hvc2` once
//!   `vendor.qemu.vport.uwb` is set, so the port is a named console port (the guest's third,
//!   hvc2). It carries UCI ([`uwb`]). Stock serves it from netsim (pica).
//!
//! Neither radio has peers yet: discovery finds nothing and ranging is refused.

pub mod controller;
pub mod h4;
pub mod hci;
pub mod uwb;

use crate::virtio::console::PortBackend;

/// The `bluetooth` port: H4 in, the controller's events out. ACL, SCO and ISO data can't arrive
/// for a connection that never exists, and are dropped.
pub struct BluetoothPort {
    h4: h4::Parser,
    controller: controller::Controller,
    /// Opcodes already reported as unsupported (each is reported once, up to a bound).
    reported: Vec<u16>,
}

impl BluetoothPort {
    pub fn new(addr: [u8; 6]) -> BluetoothPort {
        BluetoothPort { h4: h4::Parser::default(), controller: controller::Controller::new(addr), reported: Vec::new() }
    }
}

impl PortBackend for BluetoothPort {
    fn guest_data(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in self.h4.push(data) {
            if p.kind != h4::Kind::Command {
                continue;
            }
            let (events, unknown) = self.controller.command(&p.bytes);
            if let Some(op) = unknown
                && !self.reported.contains(&op)
                && self.reported.len() < 64
            {
                self.reported.push(op);
                eprintln!("emu: bluetooth: unsupported HCI command {op:#06x} (answered Unknown HCI Command)");
            }
            for e in events {
                out.extend(h4::frame(h4::Kind::Event, &e));
            }
        }
        out
    }

    fn guest_open(&mut self, open: bool) -> Vec<u8> {
        if !open {
            self.h4.clear();
        }
        Vec::new()
    }
}

/// The `uwb` port.
#[derive(Default)]
pub struct UwbPort(uwb::Uwbs);

impl PortBackend for UwbPort {
    fn guest_data(&mut self, data: &[u8]) -> Vec<u8> {
        self.0.push(data)
    }

    fn guest_open(&mut self, open: bool) -> Vec<u8> {
        if !open {
            self.0.clear();
        }
        Vec::new()
    }

    /// The HAL opens `/dev/hvc2`, the tty, not the port's `/dev/vportNpM`.
    fn console(&self) -> bool {
        true
    }
}

/// A public Bluetooth address that is stable for `seed` (the AVD's disk paths): `BB:BB:BB` (the
/// stock emulator's prefix) and three bytes of its FNV-1a hash, so two devices on one host differ.
pub fn address_for(seed: &[u8]) -> [u8; 6] {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in seed {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let h = h.to_be_bytes();
    [0xBB, 0xBB, 0xBB, h[5], h[6], h[7]]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bluetooth_port_answers_h4_commands_with_h4_events() {
        let mut port = BluetoothPort::new([0xBB, 0xBB, 0xBB, 1, 2, 3]);
        assert!(port.guest_open(true).is_empty(), "nothing unsolicited: the forwarder drops it");
        // Reset split across writes, then Read BD_ADDR and an ACL packet in one.
        assert!(port.guest_data(&[1, 0x03]).is_empty());
        assert_eq!(port.guest_data(&[0x0c, 0x00]), [4, 0x0e, 0x04, 0x01, 0x03, 0x0c, 0x00]);
        let out = port.guest_data(&[1, 0x09, 0x10, 0x00, 2, 0x01, 0x00, 0x01, 0x00, 0xaa]);
        assert_eq!(out, [4, 0x0e, 0x0a, 0x01, 0x09, 0x10, 0x00, 3, 2, 1, 0xbb, 0xbb, 0xbb]);
        // A close drops a partial command.
        assert!(port.guest_data(&[1, 0x01]).is_empty());
        port.guest_open(false);
        assert!(port.guest_data(&[0x10, 0x00]).is_empty());
        // An unknown command is answered (and reported once).
        assert_eq!(port.guest_data(&[1, 0x22, 0x20, 0x00]), [4, 0x0e, 0x04, 0x01, 0x22, 0x20, 0x01]);
        port.guest_data(&[1, 0x22, 0x20, 0x00]);
        assert_eq!(port.reported, [0x2022]);
    }

    #[test]
    fn the_uwb_port_answers_uci() {
        let mut port = UwbPort::default();
        assert_eq!(port.guest_data(&[0x2c, 0x01, 0x00, 0x02, b'U', b'S']), [0x4c, 0x01, 0x00, 0x01, 0x00]);
    }

    #[test]
    fn addresses_are_stable_per_seed_and_differ_between_seeds() {
        let a = address_for(b"/avd/a.avd/userdata-qemu.img");
        assert_eq!(a, address_for(b"/avd/a.avd/userdata-qemu.img"));
        assert_ne!(a, address_for(b"/avd/b.avd/userdata-qemu.img"));
        assert_eq!(a[..3], [0xBB, 0xBB, 0xBB]);
    }
}
