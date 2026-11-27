//! The modem behind the guest's `modem` virtio-console port (or, on API 30 and older images, its
//! `qemud:gsm` pipe), shared with the console's `sms` and `gsm` commands. Each guest boot gets a
//! fresh modem (the radio HAL re-initializes it); host events produce unsolicited bytes that go
//! out through whichever of the two the guest's radio opened last.
use std::sync::{Arc, Mutex};

use crate::modem::{Modem, ModemError, RegState};
use crate::pipe::{HostMsg, PipeCtx};
use crate::virtio::console::{PortBackend, PortHandle};

#[derive(Default)]
pub struct Telephony {
    modem: Mutex<Modem>,
    port: Mutex<Option<Out>>,
}

/// Where unsolicited modem bytes go.
enum Out {
    Console(PortHandle),
    Pipe(PipeCtx),
}

impl Telephony {
    fn with<T>(&self, f: impl FnOnce(&mut Modem) -> T) -> T {
        f(&mut self.modem.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn push(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        match self.port.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            Some(Out::Console(p)) => p.send(bytes),
            Some(Out::Pipe(ctx)) => {
                ctx.send(HostMsg::Data(bytes));
            }
            None => {}
        }
    }

    /// A new guest boot: fresh modem state, new port.
    pub fn attach(&self, port: PortHandle) {
        self.with(|m| *m = Modem::new());
        *self.port.lock().unwrap_or_else(|e| e.into_inner()) = Some(Out::Console(port));
    }

    /// The guest's radio opened `qemud:gsm` (API 30 and older): unsolicited bytes go there from
    /// now on. The modem keeps its state, as it does when the console port is reopened.
    pub fn attach_pipe(&self, ctx: PipeCtx) {
        // Only the legacy reference RIL opens the pipe; it reads the stock modem's answers.
        self.with(|m| m.set_reference_ril(true));
        *self.port.lock().unwrap_or_else(|e| e.into_inner()) = Some(Out::Pipe(ctx));
    }

    /// The `qemud:gsm` pipe `conn` closed: nothing to send unsolicited bytes to until it's reopened.
    pub fn detach_pipe(&self, conn: u64) {
        let mut port = self.port.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(port.as_ref(), Some(Out::Pipe(ctx)) if ctx.conn == conn) {
            *port = None;
        }
    }

    /// AT commands from the guest; returns the modem's answers.
    pub fn guest_input(&self, data: &[u8]) -> Vec<u8> {
        self.with(|m| m.input(data))
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
        self.0.guest_input(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::console::{Console, PortSpec, Sink};

    #[test]
    fn console_commands_change_the_modem_the_guest_talks_to() {
        let t = Arc::new(Telephony::default());
        let (_console, handles) = Console::new(vec![PortSpec { name: "modem".into(), backend: Box::new(Sink) }]);
        // Before a guest boots there's no port: events still change the modem.
        t.set_signal(20, 3);
        assert_eq!(t.signal(), (20, 3));
        t.attach(handles[0].clone());
        assert_eq!(t.signal(), Modem::new().signal(), "a new boot gets a fresh modem");

        let mut port = t.backend();
        let reply = |port: &mut Box<dyn PortBackend>, at: &str| String::from_utf8_lossy(&port.guest_data(at.as_bytes())).into_owned();
        assert!(reply(&mut port, "AT\r").contains("OK"), "the guest's radio HAL gets answers");
        t.call(CallOp::Incoming, "5551234").expect_err("the radio is off until the HAL turns it on");
        assert!(reply(&mut port, "AT+CFUN=1\r").contains("OK"));

        t.set_voice(RegState::Roaming);
        t.set_data(RegState::Searching);
        assert_eq!(t.states(), (RegState::Roaming, RegState::Searching));

        t.call(CallOp::Incoming, "5551234").expect("ring");
        assert_eq!(t.list_calls().len(), 1);
        assert!(reply(&mut port, "AT+CLCC\r").contains("5551234"), "the HAL's call poll sees it");
        t.call(CallOp::Busy, "5551234").expect_err("busy is for outgoing calls");
        t.call(CallOp::Accept, "5551234").expect("accept");
        t.call(CallOp::Hold, "5551234").expect("hold");
        t.call(CallOp::Cancel, "5551234").expect("hang up");
        assert!(t.list_calls().is_empty());
        t.call(CallOp::Busy, "5550000").expect_err("no such call");

        t.sms_deliver("5551234", "hello").expect("sms");
        assert!(t.sent_sms().is_empty(), "nothing sent by the guest yet");
    }
}
