//! `qemud:gsm`: the modem of images whose radio is the legacy `rild` with the goldfish
//! reference RIL (API 30 and older), which opens this pipe instead of a `modem` virtio-console
//! port. The stream is raw AT commands and answers, no qemud framing, the same modem the console
//! port serves ([`crate::telephony::Telephony`]).
use std::sync::Arc;

use super::{HostMsg, PipeCtx, PipeHandler, PipeService};
use crate::telephony::Telephony;

pub struct GsmService {
    pub telephony: Arc<Telephony>,
}

impl PipeService for GsmService {
    fn open(&self, name: &str, ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        (name == "qemud:gsm").then(|| {
            let conn = ctx.conn;
            self.telephony.attach_pipe(ctx);
            Box::new(GsmPipe { telephony: self.telephony.clone(), conn }) as _
        })
    }
}

struct GsmPipe {
    telephony: Arc<Telephony>,
    conn: u64,
}

impl PipeHandler for GsmPipe {
    fn guest_data(&mut self, data: &[u8], to_guest: &mut Vec<u8>) -> bool {
        to_guest.extend(self.telephony.guest_input(data));
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
        self.telephony.detach_pipe(self.conn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipe::WorkerEvent;
    use crate::telephony::CallOp;

    fn reply(p: &mut Box<dyn PipeHandler>, at: &str) -> String {
        let mut out = Vec::new();
        assert!(p.guest_data(at.as_bytes(), &mut out));
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn the_reference_ril_talks_to_the_modem_and_hears_its_events() {
        let t = Arc::new(Telephony::default());
        let svc = GsmService { telephony: t.clone() };
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(svc.open("qemud:gsmx", PipeCtx { conn: 7, events: tx.clone() }).is_none());
        let mut p = svc.open("qemud:gsm", PipeCtx { conn: 7, events: tx }).expect("open");
        // Raw AT, no qemud framing: the reference RIL's init.
        assert_eq!(reply(&mut p, "ATE0Q0V1\r"), "OK\r");
        assert!(reply(&mut p, "AT+CFUN=1\r").contains("OK"));
        // A host event goes out on this pipe, as the worker's message for its connection.
        t.call(CallOp::Incoming, "5551234").expect("ring");
        let ring = rx.try_iter().find_map(|e| match e {
            WorkerEvent::Host { conn: 7, msg: HostMsg::Data(d) } => Some(d),
            _ => None,
        });
        let ring = ring.expect("unsolicited bytes on the pipe");
        assert!(String::from_utf8_lossy(&ring).contains("RING"), "{ring:?}");
        let mut out = Vec::new();
        assert!(p.host_msg(HostMsg::Data(ring.clone()), &mut out));
        assert_eq!(out, ring, "the handler passes them to the guest");
        assert!(reply(&mut p, "AT+CLCC\r").contains("5551234"));
        // Closed: events no longer go anywhere.
        p.closed();
        t.call(CallOp::Cancel, "5551234").expect("hang up");
        assert!(rx.try_iter().next().is_none(), "nothing sent after the close");
    }

    #[test]
    fn a_stale_close_keeps_the_newer_pipe() {
        let t = Arc::new(Telephony::default());
        let svc = GsmService { telephony: t.clone() };
        let (tx, rx) = std::sync::mpsc::channel();
        let mut old = svc.open("qemud:gsm", PipeCtx { conn: 1, events: tx.clone() }).expect("open");
        let mut new = svc.open("qemud:gsm", PipeCtx { conn: 2, events: tx }).expect("reopen");
        old.closed();
        assert!(reply(&mut new, "AT+CFUN=1\r").contains("OK"));
        t.set_signal(10, 0);
        assert!(
            rx.try_iter().any(|e| matches!(e, WorkerEvent::Host { conn: 2, msg: HostMsg::Data(_) })),
            "the reopened pipe still gets events"
        );
    }
}
