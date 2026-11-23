//! The modem behind the guest's `modem` virtio-console port, shared with the console's `sms` and
//! `gsm` commands. Each guest boot gets a fresh modem (the radio HAL re-initializes it); host
//! events produce unsolicited bytes that go out through the port.
use std::sync::{Arc, Mutex};

use crate::modem::{Modem, ModemError, RegState};
use crate::virtio::console::{PortBackend, PortHandle};

#[derive(Default)]
pub struct Telephony {
    modem: Mutex<Modem>,
    port: Mutex<Option<PortHandle>>,
}

impl Telephony {
    fn with<T>(&self, f: impl FnOnce(&mut Modem) -> T) -> T {
        f(&mut self.modem.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn push(&self, bytes: Vec<u8>) {
        if let Some(p) = self.port.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            p.send(bytes);
        }
    }

    /// A new guest boot: fresh modem state, new port.
    pub fn attach(&self, port: PortHandle) {
        self.with(|m| *m = Modem::new());
        *self.port.lock().unwrap_or_else(|e| e.into_inner()) = Some(port);
    }

    /// Delivers an SMS from `from` to the guest.
    pub fn sms_deliver(&self, from: &str, text: &str) -> Result<(), ModemError> {
        let b = self.with(|m| m.sms_deliver(from, text))?;
        self.push(b);
        Ok(())
    }

    /// Messages the guest sent, as (destination, text).
    pub fn sent_sms(&self) -> Vec<(String, String)> {
        self.with(|m| m.sent_sms().iter().map(|s| (s.to.clone(), s.text.clone())).collect())
    }

    pub fn call(&self, op: CallOp, number: &str) -> Result<(), ModemError> {
        let b = self.with(|m| match op {
            CallOp::Incoming => m.call_incoming(number),
            CallOp::Accept => m.call_accept(number),
            CallOp::Busy => m.call_busy(number),
            CallOp::Cancel => m.call_cancel(number),
            CallOp::Hold => m.call_hold(number),
        })?;
        self.push(b);
        Ok(())
    }

    pub fn list_calls(&self) -> Vec<String> {
        self.with(|m| m.list_calls())
    }

    pub fn set_voice(&self, s: RegState) {
        let b = self.with(|m| m.set_voice_state(s));
        self.push(b);
    }

    pub fn set_data(&self, s: RegState) {
        let b = self.with(|m| m.set_data_state(s));
        self.push(b);
    }

    pub fn states(&self) -> (RegState, RegState) {
        self.with(|m| (m.voice_state(), m.data_state()))
    }

    pub fn set_signal(&self, rssi: u8, ber: u8) {
        let b = self.with(|m| m.set_signal(rssi, ber));
        self.push(b);
    }

    pub fn signal(&self) -> (u8, u8) {
        self.with(|m| m.signal())
    }

    /// The port's host end.
    pub fn backend(self: &Arc<Self>) -> Box<dyn PortBackend> {
        Box::new(ModemPort(self.clone()))
    }
}

#[derive(Clone, Copy, Debug)]
pub enum CallOp {
    Incoming,
    Accept,
    Busy,
    Cancel,
    Hold,
}

struct ModemPort(Arc<Telephony>);

impl PortBackend for ModemPort {
    fn guest_data(&mut self, data: &[u8]) -> Vec<u8> {
        self.0.with(|m| m.input(data))
    }
}
