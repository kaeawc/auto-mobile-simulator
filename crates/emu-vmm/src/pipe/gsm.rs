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

    /// `parseRegistrationState` in the reference RIL: the fields after the prefix, by comma
    /// count, with quotes stripped; a fifth field (`<n>,<stat>,<lac>,<ci>,<x>`) is what the RIL
    /// reports to the framework as the Android radio technology.
    fn ril_registration(reply: &str, prefix: &str) -> (Vec<String>, Option<u32>) {
        let l = reply.split('\r').find(|l| l.starts_with(prefix)).unwrap_or_else(|| panic!("no {prefix} in {reply:?}"));
        let fields: Vec<String> = l[prefix.len()..].split(',').map(|f| f.trim().trim_matches('"').to_string()).collect();
        assert!((1..=5).contains(&fields.len()), "the RIL rejects {l:?}");
        let rat = (fields.len() == 5).then(|| u32::from_str_radix(&fields[4], 16).expect("hex technology"));
        (fields, rat)
    }

    /// The reference RIL's polls, as a stock modem answers them (#83: emu's `+CREG` `<AcT>` 7
    /// read as `EVDO_0` and flipped the phone between GSM and CDMA several times a second).
    #[test]
    fn the_reference_ril_gets_the_stock_modems_answers() {
        let t = Arc::new(Telephony::default());
        let svc = GsmService { telephony: t };
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut p = svc.open("qemud:gsm", PipeCtx { conn: 1, events: tx }).expect("open");
        // The reference RIL's init (stock radio log, API 30).
        for at in ["ATE0Q0V1", "AT+CMEE=1", "AT+CREG=2", "AT+CGREG=1", "AT+CMGF=0", "AT+CFUN=1"] {
            assert!(reply(&mut p, &format!("{at}\r")).contains("OK"), "{at}");
        }
        // Voice: no technology field, so the RIL never reports a CDMA one.
        let (voice, rat) = ril_registration(&reply(&mut p, "AT+CREG?\r"), "+CREG: ");
        assert_eq!(&voice[..2], ["2", "1"], "registered home");
        assert_eq!(rat, None, "{voice:?}");
        // Data: always the full form, with the Android radio technology (LTE = 14).
        for (at, prefix) in [("AT+CGREG?\r", "+CGREG: "), ("AT+CEREG?\r", "+CEREG: ")] {
            let (data, rat) = ril_registration(&reply(&mut p, at), prefix);
            assert_eq!(data[1], "1", "registered home: {data:?}");
            assert_eq!(rat, Some(14), "{data:?}");
        }
        // Technology: an index (LTE = 5) and a hex preferred mask; setting the current one is DONE.
        assert!(reply(&mut p, "AT+CTEC?\r").starts_with("+CTEC: 5,"));
        assert!(reply(&mut p, "AT+CTEC=5,\"23\"\r").starts_with("+CTEC: DONE"));
        assert!(reply(&mut p, "AT+CTEC=0,\"201\"\r").starts_with("+CTEC: 0\r"));
        assert!(reply(&mut p, "AT+CTEC?\r").starts_with("+CTEC: 0,201"));
        let (_, rat) = ril_registration(&reply(&mut p, "AT+CGREG?\r"), "+CGREG: ");
        assert_eq!(rat, Some(1), "GSM data is GPRS");
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
