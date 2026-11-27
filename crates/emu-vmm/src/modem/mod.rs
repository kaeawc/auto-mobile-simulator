//! A GSM/LTE AT-command modem, as a pure state machine: guest bytes in, guest bytes out.
//!
//! The guest's radio HAL (`android.hardware.radio-service.ranchu`, AOSP
//! `device/generic/goldfish/hals/radio`) opens the virtio-console port named by
//! `vendor.qemu.vport.modem` and drives it with AT commands. This module answers them and
//! produces the unsolicited result codes for host events (incoming calls and SMS, registration
//! and signal changes). It has no I/O and no threads; the transport moves the bytes.
//!
//! # Protocol facts (from the HAL sources; expensive to rediscover)
//!
//! Framing (`AtChannel.cpp`, `AtResponse.cpp`, `Parser.cpp`):
//! - The HAL ends every request with a single `\r`. SMS PDUs follow a `> ` prompt and end with
//!   Ctrl-Z (0x1A), which the HAL also follows with `\r`.
//! - The HAL's reader skips any `\r`/`\n` at the start of a response, then parses one
//!   response at a time:
//!   - `+CMD: <payload>\r` for single-line commands (payload left-trimmed, up to the first `\r`);
//!   - multi-line commands (`COPS`, `CLCC`, `CCFCU`, `CGDCONT`) take everything up to the
//!     literal `\rOK\r`, so their lines must be separated by a bare `\r` and the last line
//!     must be followed directly by `OK\r`. A `\n` anywhere in between breaks the parse.
//!   - a line not starting with `+`, `%` or `^` that is not `OK`, `ERROR`, `RING` or `> ` is a
//!     raw value (IMSI, ICCID, IMEI) and runs up to the next `\rOK\r`.
//!
//!   So every line here ends in a bare `\r` (no `\r\n`), exactly like the stock emulator's
//!   modem driver, which appends `\r` to each answer.
//! - Fatal pitfalls: a `+XXX` line whose command the HAL does not know (e.g. `+CRING`, `+CUSD`,
//!   `+CGEV`, `+CMTI`, `+CSSU`, `+CLIP` as a URC) makes its reader return an error and the HAL
//!   aborts. A raw line such as `NO CARRIER` or `CONNECT` swallows the next `OK`. Echo must
//!   stay off (`ATE0` is the first init command and echo would turn into a raw value). The only
//!   unsolicited codes emitted here are `RING`, `+CMT`, `+CREG`, `+CGREG`, `+CEREG`, `+CSQ`,
//!   `+CFUN` and `%CTZV`.
//! - Every request waits (3 s) for a response of the specific type its filter accepts; a
//!   timeout makes the HAL drop and reopen the channel, which ends in an abort. So every
//!   command it sends must be answered with exactly the expected shape, even when the answer is
//!   an error (`+CME ERROR: <n>` is accepted where the filter allows `CmeError`).
//!
//! Init (`main.cpp`): `ATE0Q0V1`, `AT+CMEE=1`, `AT+CREG=2`, `AT+CGREG=2`, `AT+CEREG=2`,
//! `AT+CCWA=1`, `AT+CMOD=0`, `AT+CMUT=0`, `AT+CSSN=0,1`, `AT+COLP=0`, `AT+CSCS="HEX"`,
//! `AT+CUSD=1`, `AT+CGEREP=1,0`, `AT+CMGF=0`, `AT+CFUN?`; each must end in `OK`. Like the stock
//! modem, the end of init (`AT+CMGF=0`) is when the time zone (`%CTZV`) and the current
//! registration and signal state are pushed.
//!
//! Network (`RadioNetwork.cpp`): the HAL never queries `CREG`/`CSQ`; it keeps the last `+CREG`
//! and `+CSQ` it saw (solicited or not) and uses `+CREG` for both voice and data registration.
//! `+CREG: <stat>,"<lac hex>","<ci hex>",<AcT>` (no spaces). `+CSQ` carries 12 fields: GSM
//! rssi/ber, CDMA dbm/ecio, EVDO dbm/ecio/snr, LTE signal/rsrp/rsrq/rssnr/cqi (INT_MAX =
//! unknown). Operators come from the compound `AT+COPS=3,0;+COPS?;+COPS=3,1;+COPS?;+COPS=3,2;
//! +COPS?`, answered as `+COPS: 0,0,<long>\r+COPS: 0,1,<short>\r+COPS: 0,2,<numeric>\rOK\r`
//! with unquoted names (the HAL keeps quotes verbatim and slices MCC/MNC out of the numeric).
//! A standalone `AT+COPS?` only parses as `<mode>,2,<numeric>` or `<mode>,0,0`, so the name
//! format defaults to numeric. `AT+CTEC?` answers `<current>,<preferred hex>` where `<current>`
//! is a bit mask (`1 << tech`, LTE = 32); `AT+CTEC=?` lists technology indices (GSM 0,
//! WCDMA 1, CDMA 2, EVDO 3, TD-SCDMA 4, LTE 5, NR 6).
//!
//! Voice (`RadioVoice.cpp`): the HAL only learns of call changes from `RING`, after which the
//! framework polls `AT+CLCC` (`+CLCC: <idx>,<dir>,<stat>,<mode>,<mpty>,<number>,<toa>`, number
//! unquoted). The stock modem uses `RING` the same way for every call state change. `ATA`,
//! `ATH`, `AT+CHLD=...` are written without waiting for a reply. `getMute` sends `AT+CLCC` but
//! waits for `+CMUT` (a HAL bug), so `AT+CLCC` answers start with a `+CMUT:` line; the call
//! poll ignores it.
//!
//! SMS (`RadioMessaging.cpp`): `AT+CMGS=<len>` -> `> \r`; then `<smsc+tpdu hex><Ctrl-Z>` ->
//! `+CMGS: <mr>\rOK\r`. Incoming: `+CMT: <tpdu len>\r<smsc+tpdu hex>\r` (no `\n`: the HAL reads
//! the PDU from right after the first `\r`). The guest acks with `AT+CNMA=1|2`, which must get
//! `OK`.
//!
//! Data (`RadioData.cpp`): `AT+CGDCONT=<cid>,"<type>","<apn>",,0,0` -> `OK`, then
//! `AT+CGCONTRDP=<cid>` -> `+CGCONTRDP: <cid>,<bearer>,"<apn>",<addr>/<prefix>,<gw>,<dns>`
//! (everything after the gateway is one DNS server). The HAL configures the guest's `eth0` from
//! it; the addresses are the user-mode NAT's classic 10.0.2.15/24, gateway 10.0.2.2, DNS
//! 10.0.2.3.
//!
//! SIM (`RadioSim.cpp`): `+CPIN: READY`, raw `AT+CICCID` and `AT+CIMI` values, `AT+CGSN=2` ->
//! raw 15-digit IMEI followed by a 2-digit SVN; file access is in [`sim`].
//!
//! # The reference RIL (API 30 and older)
//!
//! Older images run `rild` with the goldfish `libreference-ril`, which opens `qemud:gsm` and
//! polls (`AT+CREG?`, `AT+CGREG?`, `AT+CTEC?`) where the HAL waits for unsolicited codes. It reads
//! the stock modem's answers, which differ from what the HAL is given here
//! ([`Modem::set_reference_ril`]; captured from a stock 37.2.13 emulator's radio log, API 30):
//! - `AT+CREG?` -> `+CREG: 2,1,"0003","005b"`: no `<AcT>`. The RIL hands a fifth field to the
//!   framework as an Android radio technology, so TS 27.007's E-UTRAN `7` read as `EVDO_0`, the
//!   phone switched to CDMA, `VOICE_RADIO_TECH` (GPRS) switched it back, and the loop kept
//!   `com.android.phone` and `system_server` at 150% of a host core (#83).
//! - `AT+CGREG?`/`AT+CEREG?` -> `+CGREG: <n>,<stat>,"<lac>","<ci>","<tech hex>"` whatever `<n>`,
//!   the last field an Android radio technology (LTE = `000e`).
//! - `AT+CTEC?` -> `+CTEC: <index>,<preferred hex>` and `AT+CTEC=<index>,"<preferred hex>"` ->
//!   `+CTEC: DONE` (or `+CTEC: <index>` when it changes): indices, not bit masks.
pub mod calls;
pub mod pdu;
pub mod sim;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fmt;

pub use calls::{Call, CallState};
pub use pdu::SmsEncoding;

/// Network registration state (TS 27.007 `<stat>`), as set by the console's `gsm voice` and
/// `gsm data`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegState {
    Unregistered = 0,
    Home = 1,
    Searching = 2,
    Denied = 3,
    Unknown = 4,
    Roaming = 5,
}

impl RegState {
    /// Parses the stock console's state names (`off` = `unregistered`, `on` = `home`).
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "unregistered" | "off" => RegState::Unregistered,
            "home" | "on" => RegState::Home,
            "roaming" => RegState::Roaming,
            "searching" => RegState::Searching,
            "denied" => RegState::Denied,
            _ => return None,
        })
    }

    /// The name the stock console's `gsm status` prints.
    pub fn name(self) -> &'static str {
        match self {
            RegState::Unregistered => "unregistered",
            RegState::Home => "home",
            RegState::Roaming => "roaming",
            RegState::Searching => "searching",
            RegState::Denied => "denied",
            RegState::Unknown => "<unknown>",
        }
    }

    fn registered(self) -> bool {
        matches!(self, RegState::Home | RegState::Roaming)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModemError {
    /// No call to or from this number.
    NoSuchCall(String),
    /// `call_busy` needs an outgoing call.
    NotOutbound(String),
    /// The guest has the radio off.
    RadioOff,
    TooManyCalls,
    InvalidNumber(String),
}

impl fmt::Display for ModemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModemError::NoSuchCall(n) => write!(f, "no current call to/from number '{n}'"),
            ModemError::NotOutbound(n) => write!(f, "no current outbound call to number '{n}'"),
            ModemError::RadioOff => f.write_str("radio is off"),
            ModemError::TooManyCalls => f.write_str("too many calls"),
            ModemError::InvalidNumber(n) => {
                write!(f, "bad phone number format '{n}', use digits, # and + only")
            }
        }
    }
}

impl std::error::Error for ModemError {}

/// An SMS the guest sent (concatenated parts are reassembled into one entry).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentSms {
    /// Destination address; empty if the PDU could not be decoded.
    pub to: String,
    /// Message text; for 8-bit data or an undecodable PDU, the bytes in hex.
    pub text: String,
    pub encoding: SmsEncoding,
    pub parts: u8,
}

// TS 27.007 +CME ERROR codes.
const CME_NOT_ALLOWED: u16 = 3;
const CME_NOT_SUPPORTED: u16 = 4;
const CME_INVALID_INDEX: u16 = 21;
const CME_NOT_FOUND: u16 = 22;
const CME_NO_NETWORK: u16 = 30;
const CME_EMERGENCY_ONLY: u16 = 32;
const CME_BAD_PARAMS: u16 = 50;

/// Technology indices of `AT+CTEC` (the HAL's `ratUtils::ModemTechnology`).
const TECH_GSM: u8 = 0;
const TECH_WCDMA: u8 = 1;
const TECH_LTE: u8 = 5;
const TECH_NR: u8 = 6;
const SUPPORTED_TECHS: &str = "0,1,5";

const HOME: (&str, &str, &str) = ("Android", "Android", "310260");
const ROAMING: (&str, &str, &str) = ("TelKila", "TelKila", "310295");

/// Data-call addressing of the user-mode NAT (`10.0.2.0/24`).
const PDP_ADDRESS: &str = "10.0.2.15";
const PDP_PREFIX: u8 = 24;
const PDP_GATEWAY: &str = "10.0.2.2";
const PDP_DNS: &str = "10.0.2.3";

const UNKNOWN: i32 = i32::MAX;
const MAX_LINE: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PduTarget {
    Send,
    Write,
}

enum Reply {
    Lines(Vec<String>),
    Prompt(PduTarget),
}

enum Fail {
    Error,
    Cme(u16),
}

type Res = Result<Reply, Fail>;

fn ok() -> Res {
    Ok(Reply::Lines(Vec::new()))
}

fn line(s: impl Into<String>) -> Res {
    Ok(Reply::Lines(vec![s.into()]))
}

struct Pdp {
    kind: String,
    apn: String,
    active: bool,
}

struct Forward {
    number: String,
    toa: u16,
    active: bool,
}

struct PendingSms {
    to: String,
    reference: u16,
    total: u8,
    encoding: SmsEncoding,
    parts: BTreeMap<u8, String>,
}

pub struct Modem {
    line: Vec<u8>,
    pdu_mode: Option<PduTarget>,
    /// Unsolicited codes produced while executing a command line, sent after its final result.
    urcs: Vec<u8>,
    initialized: bool,

    echo: bool,
    cmee: u8,
    radio_on: bool,
    creg_n: u8,
    cgreg_n: u8,
    cereg_n: u8,
    voice: RegState,
    data: RegState,
    rssi: u8,
    ber: u8,
    tech: u8,
    preferred_techs: u32,
    cops_mode: u8,
    cops_format: u8,
    lac: u16,
    cell_id: u32,
    roaming_pref: u8,
    subscription_source: u8,

    calls: calls::Calls,
    clip: u8,
    clir: u8,
    muted: bool,
    call_waiting: bool,
    forwards: BTreeMap<u8, Forward>,

    smsc: String,
    smsc_toa: u16,
    cscb: (u8, String, String),
    next_mr: u8,
    next_sim_index: u16,
    concat_ref: u8,
    sent: Vec<SentSms>,
    pending_sms: Vec<PendingSms>,

    pdp: BTreeMap<u8, Pdp>,

    sim: sim::SimFiles,
    imsi: String,
    iccid: String,
    imei: String,
    svn: String,

    tz_quarters: i32,
    tz_dst: bool,
    tz_name: String,
    clock: fn() -> u64,

    /// The guest is the legacy reference RIL (see the module docs), not the radio HAL.
    reference_ril: bool,
}

fn system_clock() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Default for Modem {
    fn default() -> Self {
        Self::new()
    }
}

impl Modem {
    /// A modem with a ready SIM (IMSI 310260000000000, number +15555215554) on the "Android"
    /// LTE network, radio off until the guest turns it on (like the stock modem).
    pub fn new() -> Self {
        let iccid = "89014103211118510720".to_string();
        let number = "+15555215554";
        Modem {
            line: Vec::new(),
            pdu_mode: None,
            urcs: Vec::new(),
            initialized: false,
            echo: false,
            cmee: 1,
            radio_on: false,
            creg_n: 0,
            cgreg_n: 0,
            cereg_n: 0,
            voice: RegState::Home,
            data: RegState::Home,
            rssi: 20,
            ber: 99,
            tech: TECH_LTE,
            preferred_techs: (1 << TECH_GSM) | (1 << TECH_WCDMA) | (1 << TECH_LTE),
            cops_mode: 0,
            cops_format: 2,
            lac: 3,
            cell_id: 91,
            roaming_pref: 0,
            subscription_source: 0,
            calls: calls::Calls::default(),
            clip: 1,
            clir: 0,
            muted: false,
            call_waiting: true,
            forwards: BTreeMap::new(),
            smsc: "+15555000000".to_string(),
            smsc_toa: 145,
            cscb: (0, String::new(), String::new()),
            next_mr: 0,
            next_sim_index: 1,
            concat_ref: 0,
            sent: Vec::new(),
            pending_sms: Vec::new(),
            pdp: BTreeMap::new(),
            sim: sim::SimFiles::new(&iccid, number),
            imsi: "310260000000000".to_string(),
            iccid,
            imei: "358240051111110".to_string(),
            svn: "01".to_string(),
            tz_quarters: 0,
            tz_dst: false,
            tz_name: String::new(),
            clock: system_clock,
            reference_ril: false,
        }
    }

    /// Bytes from the guest; returns bytes for the guest (responses, and unsolicited codes the
    /// commands caused). Partial lines are buffered until their `\r`.
    pub fn input(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for &b in data {
            if let Some(target) = self.pdu_mode {
                match b {
                    0x1a => {
                        let pdu = std::mem::take(&mut self.line);
                        self.pdu_mode = None;
                        out.extend(self.finish_pdu(target, &pdu));
                    }
                    // ESC abandons the message (TS 27.005 3.5.1).
                    0x1b => {
                        self.line.clear();
                        self.pdu_mode = None;
                        out.extend_from_slice(b"OK\r");
                    }
                    b'\r' | b'\n' => {}
                    _ => self.push_byte(b),
                }
                continue;
            }
            match b {
                b'\r' | b'\n' => {
                    if !self.line.is_empty() {
                        let l = std::mem::take(&mut self.line);
                        out.extend(self.command_line(&String::from_utf8_lossy(&l)));
                    }
                }
                _ => self.push_byte(b),
            }
        }
        out
    }

    fn push_byte(&mut self, b: u8) {
        if self.line.len() < MAX_LINE {
            self.line.push(b);
        } else {
            // Runaway line: drop it rather than grow without bound.
            self.line.clear();
        }
    }

    /// Whether the HAL has finished its init sequence (seen `AT+CMGF=0`).
    pub fn initialized(&self) -> bool {
        self.initialized
    }

    pub fn radio_on(&self) -> bool {
        self.radio_on
    }

    /// Answers queries the way the stock modem answers the legacy reference RIL (`qemud:gsm`,
    /// API 30 and older) rather than the radio HAL; see the module docs.
    pub fn set_reference_ril(&mut self, on: bool) {
        self.reference_ril = on;
    }

    // ---- host controls -------------------------------------------------------------------

    /// Delivers an SMS from `from` (digits, optionally `+`; or an alphanumeric sender) and
    /// returns the `+CMT` codes to push (several for a concatenated message).
    pub fn sms_deliver(&mut self, from: &str, text: &str) -> Result<Vec<u8>, ModemError> {
        if from.is_empty() || from.chars().count() > 20 {
            return Err(ModemError::InvalidNumber(from.to_string()));
        }
        let stamp = pdu::scts((self.clock)(), self.tz_quarters);
        self.concat_ref = self.concat_ref.wrapping_add(1);
        let mut out = Vec::new();
        for tpdu in pdu::build_deliver(from, text, stamp, self.concat_ref) {
            // No SMSC address (length 0) before the TPDU.
            let mut full = vec![0u8];
            full.extend_from_slice(&tpdu);
            out.extend(format!("+CMT: {}\r{}\r", tpdu.len(), pdu::hex(&full)).bytes());
        }
        Ok(out)
    }

    /// Messages the guest has sent, oldest first.
    pub fn sent_sms(&self) -> &[SentSms] {
        &self.sent
    }

    /// The far end `number` calls the device.
    pub fn call_incoming(&mut self, number: &str) -> Result<Vec<u8>, ModemError> {
        if !valid_number(number) {
            return Err(ModemError::InvalidNumber(number.to_string()));
        }
        if !self.radio_on {
            return Err(ModemError::RadioOff);
        }
        self.calls.add(true, number).ok_or(ModemError::TooManyCalls)?;
        Ok(b"RING\r".to_vec())
    }

    /// The far end answers (or resumes) the call: it becomes active.
    pub fn call_accept(&mut self, number: &str) -> Result<Vec<u8>, ModemError> {
        self.set_call_state(number, CallState::Active)
    }

    /// The far end of an outgoing call is busy: the call ends.
    pub fn call_busy(&mut self, number: &str) -> Result<Vec<u8>, ModemError> {
        let call = self.calls.find_mut(number).ok_or_else(|| ModemError::NotOutbound(number.to_string()))?;
        if call.inbound {
            return Err(ModemError::NotOutbound(number.to_string()));
        }
        self.calls.remove_number(number);
        Ok(b"RING\r".to_vec())
    }

    /// The far end hangs up.
    pub fn call_cancel(&mut self, number: &str) -> Result<Vec<u8>, ModemError> {
        if !self.calls.remove_number(number) {
            return Err(ModemError::NoSuchCall(number.to_string()));
        }
        Ok(b"RING\r".to_vec())
    }

    /// The far end puts the call on hold.
    pub fn call_hold(&mut self, number: &str) -> Result<Vec<u8>, ModemError> {
        self.set_call_state(number, CallState::Held)
    }

    fn set_call_state(&mut self, number: &str, state: CallState) -> Result<Vec<u8>, ModemError> {
        let call = self.calls.find_mut(number).ok_or_else(|| ModemError::NoSuchCall(number.to_string()))?;
        call.state = state;
        Ok(b"RING\r".to_vec())
    }

    /// The current calls.
    pub fn calls(&self) -> &[Call] {
        self.calls.list()
    }

    /// Lines in the stock console's `gsm list` format, e.g. `inbound from 5551234    : active`.
    pub fn list_calls(&self) -> Vec<String> {
        self.calls
            .list()
            .iter()
            .map(|c| {
                let dir = if c.inbound { "inbound from" } else { "outbound to " };
                format!("{dir} {:<10} : {}", c.number, c.state.console_name())
            })
            .collect()
    }

    pub fn voice_state(&self) -> RegState {
        self.voice
    }

    pub fn data_state(&self) -> RegState {
        self.data
    }

    /// Sets the voice registration state; returns the `+CREG` update. Roaming switches the
    /// operator to the stock roaming network ("TelKila", 310295).
    pub fn set_voice_state(&mut self, state: RegState) -> Vec<u8> {
        self.voice = state;
        let mut out = Vec::new();
        if self.radio_on && self.creg_n > 0 {
            push_line(&mut out, &self.reg_line("CREG", self.creg_n, self.voice, false));
        }
        out
    }

    /// Sets the data registration state; returns the `+CGREG`/`+CEREG` updates. While data is
    /// not registered, `AT+CGCONTRDP` fails, so data calls cannot come up. (The HAL derives
    /// the registration it reports from `+CREG` alone.)
    pub fn set_data_state(&mut self, state: RegState) -> Vec<u8> {
        self.data = state;
        let mut out = Vec::new();
        if self.radio_on {
            if self.cgreg_n > 0 {
                push_line(&mut out, &self.reg_line("CGREG", self.cgreg_n, self.data, false));
            }
            if self.cereg_n > 0 {
                push_line(&mut out, &self.reg_line("CEREG", self.cereg_n, self.data, false));
            }
        }
        out
    }

    /// `(rssi, ber)` as set by [`Modem::set_signal`].
    pub fn signal(&self) -> (u8, u8) {
        (self.rssi, self.ber)
    }

    /// Sets the signal strength (TS 27.007 `+CSQ`: rssi 0..=31 or 99, ber 0..=7 or 99; out of
    /// range values become 99, so callers validate) and returns the `+CSQ` update. LTE
    /// measurements are derived from rssi.
    pub fn set_signal(&mut self, rssi: u8, ber: u8) -> Vec<u8> {
        self.rssi = if rssi <= 31 { rssi } else { 99 };
        self.ber = if ber <= 7 { ber } else { 99 };
        let mut out = Vec::new();
        push_line(&mut out, &self.csq_line());
        out
    }

    /// Sets the zone reported by NITZ (`%CTZV`): offset east of UTC in minutes (rounded to
    /// quarter hours), daylight saving, and an Olson name such as `America/Los_Angeles`
    /// (empty: let the guest pick from the offset). Returns the `%CTZV` update once the HAL is
    /// up. The default is UTC with no name.
    pub fn set_timezone(&mut self, offset_minutes: i32, dst: bool, name: &str) -> Vec<u8> {
        self.tz_quarters = offset_minutes / 15;
        self.tz_dst = dst;
        self.tz_name = name.to_string();
        let mut out = Vec::new();
        if self.initialized {
            push_line(&mut out, &self.ctzv_line());
        }
        out
    }

    /// The device's own number, as stored in the SIM's EF_MSISDN.
    pub fn set_phone_number(&mut self, number: &str) {
        self.sim.set_msisdn(number);
    }

    /// Replaces the wall clock (seconds since the Unix epoch) used for NITZ and SMS time stamps.
    pub fn set_clock(&mut self, clock: fn() -> u64) {
        self.clock = clock;
    }

    /// The full registration and signal state as unsolicited codes, e.g. to resynchronize a
    /// guest that reopened the port.
    pub fn network_status(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, n, state) in [("CREG", self.creg_n, self.voice), ("CGREG", self.cgreg_n, self.data), ("CEREG", self.cereg_n, self.data)]
        {
            if n > 0 {
                push_line(&mut out, &self.reg_line(name, n, state, false));
            }
        }
        push_line(&mut out, &self.csq_line());
        out
    }

    // ---- response builders ---------------------------------------------------------------

    fn operator(&self) -> (&'static str, &'static str, &'static str) {
        if self.voice == RegState::Roaming { ROAMING } else { HOME }
    }

    fn has_network(&self) -> bool {
        self.radio_on && self.cops_mode != 2
    }

    /// TS 27.007 `<AcT>` for the current technology.
    fn act(&self) -> u8 {
        match self.tech {
            TECH_GSM => 0,
            TECH_WCDMA => 2,
            TECH_NR => 11,
            _ => 7,
        }
    }

    fn reg_line(&self, name: &str, n: u8, state: RegState, query: bool) -> String {
        let stat = if self.radio_on { state as u8 } else { 0 };
        let loc = format!("\"{:04X}\",\"{:08X}\",{}", self.lac, self.cell_id, self.act());
        match (query, n) {
            (true, 0 | 1) => format!("+{name}: {n},{stat}"),
            (true, _) => format!("+{name}: {n},{stat},{loc}"),
            (false, 1) => format!("+{name}: {stat}"),
            (false, _) => format!("+{name}: {stat},{loc}"),
        }
    }

    /// A registration query's answer in the stock modem's format, for the reference RIL.
    fn ril_reg_line(&self, name: &str, n: u8, state: RegState) -> String {
        let stat = if self.radio_on { state as u8 } else { 0 };
        let loc = format!("\"{:04X}\",\"{:08X}\"", self.lac, self.cell_id);
        match name {
            "CREG" if n == 2 => format!("+CREG: {n},{stat},{loc}"),
            "CREG" => format!("+CREG: {n},{stat}"),
            _ => format!("+{name}: {n},{stat},{loc},\"{:04x}\"", self.ril_radio_tech()),
        }
    }

    /// The Android radio technology (`RIL_RadioTechnology`) of the current technology.
    fn ril_radio_tech(&self) -> u8 {
        match self.tech {
            TECH_GSM => 1,   // GPRS
            TECH_WCDMA => 3, // UMTS
            TECH_NR => 20,
            _ => 14, // LTE
        }
    }

    fn csq_line(&self) -> String {
        let known = self.radio_on && self.rssi != 99;
        let (rssi, ber) = if self.radio_on { (self.rssi, self.ber) } else { (99, 99) };
        let lte: [i32; 5] = if known {
            let r = i32::from(self.rssi);
            // rsrp/rsrq are positive (-dBm, -dB); rssnr in 0.1 dB.
            [r, 140 - r * 55 / 31, 10, (r * 10).min(300), r / 2]
        } else {
            [99, UNKNOWN, UNKNOWN, UNKNOWN, UNKNOWN]
        };
        format!("+CSQ: {rssi},{ber},{u},{u},{u},{u},{u},{},{},{},{},{}", lte[0], lte[1], lte[2], lte[3], lte[4], u = UNKNOWN)
    }

    fn ctzv_line(&self) -> String {
        let (y, mo, d, h, mi, s) = pdu::civil((self.clock)() as i64);
        let sign = if self.tz_quarters < 0 { '-' } else { '+' };
        format!(
            "%CTZV: {:02}/{mo:02}/{d:02}:{h:02}:{mi:02}:{s:02}{sign}{}:{}:{}",
            y % 100,
            self.tz_quarters.unsigned_abs(),
            u8::from(self.tz_dst),
            // The HAL treats '/' as a separator; the stock modem sends '!' instead.
            self.tz_name.replace('/', "!")
        )
    }

    // ---- command dispatch ----------------------------------------------------------------

    fn command_line(&mut self, raw: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let text = raw.trim();
        if self.echo {
            out.extend_from_slice(text.as_bytes());
            out.push(b'\r');
        }
        // Anything that is not an AT command is ignored, like the stock modem does.
        if text.len() < 2 || !text.is_char_boundary(2) || !text[..2].eq_ignore_ascii_case("AT") {
            return out;
        }
        self.urcs.clear();
        let mut lines = Vec::new();
        let mut result = Ok(());
        let mut prompt = None;
        match split_commands(&text[2..]) {
            Some(cmds) => {
                for cmd in cmds {
                    match self.execute(cmd) {
                        Ok(Reply::Lines(l)) => lines.extend(l),
                        Ok(Reply::Prompt(t)) => {
                            prompt = Some(t);
                            break;
                        }
                        Err(f) => {
                            result = Err(f);
                            break;
                        }
                    }
                }
            }
            None => result = Err(Fail::Error),
        }
        for l in &lines {
            push_line(&mut out, l);
        }
        if let Some(t) = prompt {
            self.pdu_mode = Some(t);
            self.line.clear();
            out.extend_from_slice(b"> \r");
        } else {
            match result {
                Ok(()) => out.extend_from_slice(b"OK\r"),
                Err(Fail::Error) => out.extend_from_slice(b"ERROR\r"),
                Err(Fail::Cme(_)) if self.cmee == 0 => out.extend_from_slice(b"ERROR\r"),
                Err(Fail::Cme(n)) => out.extend(format!("+CME ERROR: {n}\r").bytes()),
            }
        }
        out.append(&mut self.urcs);
        out
    }

    fn execute(&mut self, cmd: &str) -> Res {
        let first = cmd.as_bytes().first().copied().unwrap_or(0);
        if matches!(first, b'+' | b'%' | b'^' | b'$' | b'*' | b'!') {
            // Name up to '=', '?' or ':' (the HAL sends "AT+CLIR: <n>").
            let split = cmd.find(['=', '?', ':']).unwrap_or(cmd.len());
            let name = cmd[..split].to_ascii_uppercase();
            return self.extended(&name, &cmd[split..]);
        }
        let arg: u32 = cmd.get(1..).unwrap_or("").trim_start_matches(|c: char| !c.is_ascii_digit()).parse().unwrap_or(0);
        match first.to_ascii_uppercase() {
            b'D' => self.dial(&cmd[1..]),
            b'A' => {
                self.calls.answer();
                ok()
            }
            b'H' => {
                self.calls.hangup();
                ok()
            }
            b'E' => {
                self.echo = arg == 1;
                ok()
            }
            // Result code and register settings that do not change behavior here.
            b'Q' | b'V' | b'Z' | b'S' | b'X' | b'L' | b'M' | b'&' | b'I' => ok(),
            _ => Err(Fail::Error),
        }
    }

    fn extended(&mut self, name: &str, rest: &str) -> Res {
        let query = rest == "?";
        let test = rest == "=?";
        let args = rest.strip_prefix('=').or_else(|| rest.strip_prefix(':')).map(str::trim).filter(|_| !test);
        let ints: Vec<Option<i64>> = args.map(|a| a.split(',').map(|f| f.trim().parse().ok()).collect()).unwrap_or_default();
        let int = |i: usize| ints.get(i).copied().flatten();

        match name {
            "+CMEE" => {
                if query {
                    return line(format!("+CMEE: {}", self.cmee));
                }
                self.cmee = int(0).unwrap_or(0).clamp(0, 2) as u8;
                ok()
            }
            "+CREG" | "+CGREG" | "+CEREG" => {
                let short = &name[1..];
                let (n, state) = match name {
                    "+CREG" => (&mut self.creg_n, self.voice),
                    "+CGREG" => (&mut self.cgreg_n, self.data),
                    _ => (&mut self.cereg_n, self.data),
                };
                if test {
                    return line(format!("+{short}: (0-2)"));
                }
                if query {
                    let n = *n;
                    if self.reference_ril {
                        return line(self.ril_reg_line(short, n, state));
                    }
                    return line(self.reg_line(short, n, state, true));
                }
                *n = int(0).unwrap_or(0).clamp(0, 2) as u8;
                ok()
            }
            "+CFUN" => {
                if query {
                    return line(format!("+CFUN: {}", u8::from(self.radio_on)));
                }
                if test {
                    return line("+CFUN: (0,1,4),(0,1)");
                }
                let on = match int(0) {
                    Some(1) => true,
                    Some(0 | 4) => false,
                    _ => return Err(Fail::Cme(CME_BAD_PARAMS)),
                };
                self.set_radio(on);
                ok()
            }
            "+CTEC" => self.ctec(query, test, args),
            "+COPS" => self.cops(query, test, args),
            "+CSQ" => line(self.csq_line()),

            "+CPIN" => {
                if query {
                    line("+CPIN: READY")
                } else {
                    ok()
                }
            }
            "+CPINR" => {
                let code = args.unwrap_or("SIM PIN").trim_matches('"');
                let n = if code.contains("PUK") { 10 } else { 3 };
                line(format!("+CPINR: {code},{n},{n}"))
            }
            "+CICCID" => line(self.iccid.clone()),
            "+CIMI" => line(self.imsi.clone()),
            "+CGSN" => line(match int(0) {
                Some(2) => format!("{}{}", self.imei, self.svn),
                Some(3) => self.svn.clone(),
                _ => self.imei.clone(),
            }),
            "+CGMI" => line("Android"),
            "+CGMM" => line("Emulator"),
            "+CGMR" => line("1.0"),
            "+CRSM" => match args.and_then(|a| self.sim.crsm(a)) {
                Some(l) => line(l),
                None => Err(Fail::Cme(CME_BAD_PARAMS)),
            },
            // No logical channels or UICC authentication.
            "+CSIM" | "^MBAU" => Err(Fail::Cme(CME_NOT_SUPPORTED)),
            "+CCHO" => Err(Fail::Cme(CME_NOT_FOUND)),
            "+CCHC" | "+CGLA" => Err(Fail::Cme(CME_INVALID_INDEX)),
            "+CLCK" => {
                if int(1) == Some(2) {
                    line("+CLCK: 0")
                } else {
                    ok()
                }
            }
            "+CUSATD" => {
                if query {
                    line("+CUSATD: 0,1")
                } else {
                    ok()
                }
            }
            // No SIM toolkit: empty envelope response, terminal response accepted.
            "+CUSATE" => line("+CUSATE:"),
            "+CUSATT" => line("+CUSATT: 0"),
            "+CCSS" => {
                if query {
                    return line(format!("+CCSS: {}", self.subscription_source));
                }
                self.subscription_source = int(0).unwrap_or(0).clamp(0, 1) as u8;
                ok()
            }
            "+WRMP" => {
                if query {
                    return line(format!("+WRMP: {}", self.roaming_pref));
                }
                self.roaming_pref = int(0).unwrap_or(0).clamp(0, 2) as u8;
                ok()
            }
            "+WSOS" => {
                if query {
                    line("+WSOS: 0")
                } else {
                    ok()
                }
            }

            "+CLCC" => Ok(Reply::Lines(self.clcc())),
            "+CHLD" => match args {
                Some(a) if self.calls.chld(a) => ok(),
                _ => Err(Fail::Cme(CME_BAD_PARAMS)),
            },
            "+CMUT" => {
                if query {
                    return line(format!("+CMUT: {}", u8::from(self.muted)));
                }
                self.muted = int(0) == Some(1);
                ok()
            }
            "+CLIP" => {
                if query {
                    return line(format!("+CLIP: {},1", self.clip));
                }
                self.clip = u8::from(int(0) == Some(1));
                ok()
            }
            "+CLIR" => {
                if query {
                    // m = 4: temporary mode, presentation allowed.
                    return line(format!("+CLIR: {},4", self.clir));
                }
                self.clir = int(0).unwrap_or(0).clamp(0, 2) as u8;
                ok()
            }
            "+CCWA" => {
                // <n>[,<mode>[,<class>]]; mode 0 disable, 1 enable, 2 query.
                match int(1) {
                    Some(2) => {
                        let class = int(2).filter(|c| *c > 0).unwrap_or(1);
                        line(format!("+CCWA: {},{class}", u8::from(self.call_waiting)))
                    }
                    Some(m) => {
                        self.call_waiting = m == 1;
                        ok()
                    }
                    None => ok(),
                }
            }
            "+CCFCU" => self.ccfcu(args),

            "+CSCA" => {
                if query {
                    return line(format!("+CSCA: {},{}", self.smsc, self.smsc_toa));
                }
                let a = args.unwrap_or("");
                let (addr, toa) = match a.rsplit_once(',') {
                    Some((addr, toa)) => (addr, toa.trim().parse().ok()),
                    None => (a, None),
                };
                self.smsc = addr.trim().to_string();
                self.smsc_toa = match toa {
                    Some(t) if t != 0 => t,
                    _ if self.smsc.contains('+') => 145,
                    _ => 129,
                };
                ok()
            }
            "+CSCB" => {
                if query {
                    let (mode, ids, dcss) = &self.cscb;
                    return line(format!("+CSCB: {mode},\"{ids}\",\"{dcss}\""));
                }
                let a = args.unwrap_or("0");
                let mut quoted = a.split('"').skip(1).step_by(2);
                self.cscb = (
                    int(0).unwrap_or(0).clamp(0, 1) as u8,
                    quoted.next().unwrap_or("").to_string(),
                    quoted.next().unwrap_or("").to_string(),
                );
                ok()
            }
            "+CMGS" if !test => Ok(Reply::Prompt(PduTarget::Send)),
            "+CMGW" if !test => Ok(Reply::Prompt(PduTarget::Write)),
            "+CMGF" => {
                if query {
                    return line("+CMGF: 0");
                }
                if int(0).unwrap_or(0) != 0 {
                    // Text mode is not supported; the HAL uses PDU mode.
                    return Err(Fail::Cme(CME_NOT_SUPPORTED));
                }
                if !self.initialized {
                    // End of the HAL's init sequence: push time and network state.
                    self.initialized = true;
                    let mut urcs = Vec::new();
                    push_line(&mut urcs, &self.ctzv_line());
                    urcs.extend(self.network_status());
                    self.urcs.extend(urcs);
                }
                ok()
            }

            "+CGDCONT" => self.cgdcont(query, test, args),
            "+CGACT" => {
                if query {
                    return Ok(Reply::Lines(self.pdp.iter().map(|(cid, p)| format!("+CGACT: {cid},{}", u8::from(p.active))).collect()));
                }
                let state = int(0) == Some(1);
                let cids: Vec<u8> = ints.iter().skip(1).filter_map(|c| c.and_then(|c| u8::try_from(c).ok())).collect();
                if state && !self.data_attached() {
                    return Err(Fail::Cme(CME_NO_NETWORK));
                }
                for (cid, p) in &mut self.pdp {
                    if cids.is_empty() || cids.contains(cid) {
                        p.active = state;
                    }
                }
                ok()
            }
            "+CGCONTRDP" => {
                if !self.data_attached() {
                    return Err(Fail::Cme(CME_NO_NETWORK));
                }
                let want = int(0).and_then(|c| u8::try_from(c).ok());
                if want.is_some_and(|c| !self.pdp.contains_key(&c)) {
                    return Err(Fail::Cme(CME_BAD_PARAMS));
                }
                let mut lines = Vec::new();
                for (cid, p) in &mut self.pdp {
                    if want.is_none_or(|w| w == *cid) {
                        p.active = true;
                        lines.push(format!("+CGCONTRDP: {cid},5,\"{}\",{PDP_ADDRESS}/{PDP_PREFIX},{PDP_GATEWAY},{PDP_DNS}", p.apn));
                    }
                }
                Ok(Reply::Lines(lines))
            }
            "+CGPADDR" => Ok(Reply::Lines(
                self.pdp.iter().filter(|(_, p)| p.active).map(|(cid, _)| format!("+CGPADDR: {cid},\"{PDP_ADDRESS}\"")).collect(),
            )),

            // Accepted settings that change nothing observable here. CNMA must always get OK
            // (the HAL waits for it after every incoming SMS); CGEREP must never produce +CGEV
            // and CUSD never +CUSD, since the HAL cannot parse them.
            "+CMOD" | "+CSSN" | "+COLP" | "+CSCS" | "+CUSD" | "+CGEREP" | "+CNMA" | "+CMGD" | "+CNMI" | "+CSMS" | "+VTS" | "+CPWD"
            | "+CGQREQ" | "+CGQMIN" | "+CGATT" | "+CMGS" | "+CMGW" => ok(),
            _ => Err(Fail::Cme(CME_NOT_SUPPORTED)),
        }
    }

    fn data_attached(&self) -> bool {
        self.radio_on && self.data.registered()
    }

    fn set_radio(&mut self, on: bool) {
        if on == self.radio_on {
            return;
        }
        self.radio_on = on;
        if !on {
            self.calls = calls::Calls::default();
            for p in self.pdp.values_mut() {
                p.active = false;
            }
        }
        // Announce the new power state before the registration that depends on it, so the
        // HAL reports signal strength for a radio it already knows is on.
        let mut urcs = Vec::new();
        push_line(&mut urcs, &format!("+CFUN: {}", u8::from(on)));
        urcs.extend(self.network_status());
        self.urcs.extend(urcs);
    }

    fn ctec(&mut self, query: bool, test: bool, args: Option<&str>) -> Res {
        if test {
            return line(format!("+CTEC: {SUPPORTED_TECHS}"));
        }
        if self.reference_ril {
            return self.ril_ctec(query, args);
        }
        if query {
            return line(format!("+CTEC: {},{:X}", 1u32 << self.tech, self.preferred_techs));
        }
        let a = args.unwrap_or("");
        let mut fields = a.split(',');
        let current: u32 = fields.next().and_then(|f| f.trim().parse().ok()).ok_or(Fail::Cme(CME_BAD_PARAMS))?;
        if current == 0 || current >= 1 << 7 {
            return Err(Fail::Cme(CME_BAD_PARAMS));
        }
        // The HAL takes the lowest set bit as the current technology.
        let tech = current.trailing_zeros() as u8;
        if let Some(mask) = fields.next().and_then(|f| u32::from_str_radix(f.trim().trim_matches('"'), 16).ok()) {
            self.preferred_techs = mask;
        }
        if tech == self.tech {
            return line("+CTEC: DONE");
        }
        self.tech = tech;
        line(format!("+CTEC: {}", 1u32 << tech))
    }

    /// `AT+CTEC` as the reference RIL speaks it: technology indices, not bit masks.
    fn ril_ctec(&mut self, query: bool, args: Option<&str>) -> Res {
        if query {
            return line(format!("+CTEC: {},{:x}", self.tech, self.preferred_techs));
        }
        let mut fields = args.unwrap_or("").split(',');
        let tech: u8 = fields.next().and_then(|f| f.trim().parse().ok()).ok_or(Fail::Cme(CME_BAD_PARAMS))?;
        if !SUPPORTED_TECHS.split(',').any(|t| t == tech.to_string()) {
            return Err(Fail::Cme(CME_BAD_PARAMS));
        }
        if let Some(mask) = fields.next().and_then(|f| u32::from_str_radix(f.trim().trim_matches('"'), 16).ok()) {
            self.preferred_techs = mask;
        }
        if tech == self.tech {
            return line("+CTEC: DONE");
        }
        self.tech = tech;
        line(format!("+CTEC: {tech}"))
    }

    fn cops(&mut self, query: bool, test: bool, args: Option<&str>) -> Res {
        if test {
            let (hl, hs, hn) = HOME;
            let (rl, rs, rn) = ROAMING;
            return line(format!("+COPS: (2,{hl},{hs},{hn}),(1,{rl},{rs},{rn})"));
        }
        if query {
            if !self.has_network() {
                return Err(Fail::Cme(CME_NO_NETWORK));
            }
            let (long, short, numeric) = self.operator();
            let name = match self.cops_format {
                0 => long,
                1 => short,
                _ => numeric,
            };
            return line(format!("+COPS: {},{},{name}", self.cops_mode, self.cops_format));
        }
        let a = args.unwrap_or("0");
        let fields: Vec<&str> = a.split(',').map(|f| f.trim().trim_matches('"')).collect();
        let num = |i: usize| fields.get(i).and_then(|f| f.parse::<u8>().ok());
        match num(0) {
            Some(0) => self.cops_mode = 0,
            Some(1 | 4) => {
                let Some(oper) = fields.get(2) else {
                    return Err(Fail::Cme(CME_BAD_PARAMS));
                };
                let matches = |o: (&str, &str, &str)| [o.0, o.1, o.2].contains(oper);
                if matches(HOME) {
                    self.voice = RegState::Home;
                    self.data = RegState::Home;
                } else if matches(ROAMING) {
                    self.voice = RegState::Roaming;
                    self.data = RegState::Roaming;
                } else {
                    return Err(Fail::Cme(CME_EMERGENCY_ONLY));
                }
                self.cops_mode = 1;
            }
            Some(2) => self.cops_mode = 2,
            Some(3) => match num(1) {
                Some(f @ 0..=2) => self.cops_format = f,
                _ => return Err(Fail::Cme(CME_BAD_PARAMS)),
            },
            _ => return Err(Fail::Cme(CME_BAD_PARAMS)),
        }
        ok()
    }

    fn dial(&mut self, s: &str) -> Res {
        let s = s.trim();
        if !s.ends_with(';') {
            // Circuit-switched data is not supported.
            return Err(Fail::Cme(CME_NOT_SUPPORTED));
        }
        // The number ends at the CLIR/CUG modifiers or the emergency suffix ("@<cat>,#").
        let number: String = s
            .chars()
            .take_while(|c| !matches!(c, '@' | ';' | 'I' | 'i' | 'G' | 'g'))
            .filter(|c| c.is_ascii_digit() || matches!(c, '+' | '*' | '#'))
            .collect();
        if number.is_empty() {
            return Err(Fail::Cme(CME_BAD_PARAMS));
        }
        if !self.radio_on {
            return Err(Fail::Cme(CME_NO_NETWORK));
        }
        if self.calls.add(false, &number).is_none() {
            return Err(Fail::Cme(CME_NOT_ALLOWED));
        }
        ok()
    }

    fn clcc(&mut self) -> Vec<String> {
        // The +CMUT line serves the HAL's getMute, which polls AT+CLCC for it.
        let mut lines = vec![format!("+CMUT: {}", u8::from(self.muted))];
        for c in self.calls.list() {
            let toa = if c.number.starts_with('+') { 145 } else { 129 };
            lines.push(format!(
                "+CLCC: {},{},{},0,{},{},{toa}",
                c.index,
                u8::from(c.inbound),
                c.state as u8,
                u8::from(c.multiparty),
                c.number
            ));
        }
        // The far end starts ringing once the guest has seen the call dialing; RING makes
        // the guest poll again and see it alerting.
        if self.calls.advance_dialing() {
            self.urcs.extend_from_slice(b"RING\r");
        }
        lines
    }

    fn ccfcu(&mut self, args: Option<&str>) -> Res {
        // <reason>,<mode>,<numbertype>,<toa>,"<number>",<class>[,...]
        let a = args.ok_or(Fail::Cme(CME_BAD_PARAMS))?;
        let fields: Vec<&str> = a.split(',').map(str::trim).collect();
        let num = |i: usize| fields.get(i).and_then(|f| f.parse::<u16>().ok());
        let reason = num(0).ok_or(Fail::Cme(CME_BAD_PARAMS))? as u8;
        let mode = num(1).ok_or(Fail::Cme(CME_BAD_PARAMS))?;
        let class = num(5).filter(|c| *c > 0).unwrap_or(1);
        match mode {
            2 => {
                let l = match self.forwards.get(&reason) {
                    Some(f) if f.active => {
                        format!("+CCFCU: 1,{class},2,{},\"{}\"", f.toa, f.number)
                    }
                    _ => format!("+CCFCU: 0,{class},2,129,\"\""),
                };
                line(l)
            }
            // disable, enable, register, erase
            0 | 1 | 3 | 4 => {
                let number = fields.get(4).map(|n| n.trim_matches('"')).unwrap_or("");
                let toa = num(3).unwrap_or(129);
                let entry = self.forwards.entry(reason).or_insert(Forward { number: String::new(), toa: 129, active: false });
                match mode {
                    0 => entry.active = false,
                    1 => entry.active = !entry.number.is_empty() || !number.is_empty(),
                    3 => entry.active = true,
                    _ => {
                        self.forwards.remove(&reason);
                        return ok();
                    }
                }
                if !number.is_empty() {
                    entry.number = number.to_string();
                    entry.toa = toa;
                }
                ok()
            }
            _ => Err(Fail::Cme(CME_BAD_PARAMS)),
        }
    }

    fn cgdcont(&mut self, query: bool, test: bool, args: Option<&str>) -> Res {
        if test {
            return line("+CGDCONT: (1-15),\"IP\",,,(0-2),(0-4)");
        }
        if query {
            return Ok(Reply::Lines(
                self.pdp.iter().map(|(cid, p)| format!("+CGDCONT: {cid},\"{}\",\"{}\",,0,0", p.kind, p.apn)).collect(),
            ));
        }
        let a = args.unwrap_or("");
        let mut fields = a.split(',').map(|f| f.trim().trim_matches('"'));
        let cid: u8 = fields.next().and_then(|c| c.parse().ok()).filter(|c| (1..=15).contains(c)).ok_or(Fail::Cme(CME_BAD_PARAMS))?;
        match fields.next() {
            None => {
                self.pdp.remove(&cid);
            }
            Some(kind) => {
                let apn = fields.next().unwrap_or("").to_string();
                self.pdp.insert(cid, Pdp { kind: kind.to_string(), apn, active: false });
            }
        }
        ok()
    }

    // ---- SMS -----------------------------------------------------------------------------

    fn finish_pdu(&mut self, target: PduTarget, raw: &[u8]) -> Vec<u8> {
        let hex: String = String::from_utf8_lossy(raw).chars().filter(|c| !c.is_whitespace()).collect();
        match target {
            PduTarget::Send => {
                self.log_submit(&hex);
                let mr = self.next_mr;
                self.next_mr = self.next_mr.wrapping_add(1);
                format!("+CMGS: {mr}\rOK\r").into_bytes()
            }
            PduTarget::Write => {
                let index = self.next_sim_index;
                self.next_sim_index = self.next_sim_index % 255 + 1;
                format!("+CMGW: {index}\rOK\r").into_bytes()
            }
        }
    }

    fn log_submit(&mut self, hex: &str) {
        let submit = pdu::unhex(hex).and_then(|b| pdu::decode_submit(&b).ok());
        let Some(s) = submit else {
            self.sent.push(SentSms { to: String::new(), text: hex.to_string(), encoding: SmsEncoding::EightBit, parts: 1 });
            return;
        };
        let Some(c) = s.concat.filter(|c| c.total > 1) else {
            self.sent.push(SentSms { to: s.to, text: s.text, encoding: s.encoding, parts: 1 });
            return;
        };
        let pos = self.pending_sms.iter().position(|p| p.to == s.to && p.reference == c.reference && p.total == c.total);
        let pos = pos.unwrap_or_else(|| {
            if self.pending_sms.len() >= 16 {
                self.pending_sms.remove(0);
            }
            self.pending_sms.push(PendingSms {
                to: s.to.clone(),
                reference: c.reference,
                total: c.total,
                encoding: s.encoding,
                parts: BTreeMap::new(),
            });
            self.pending_sms.len() - 1
        });
        let pending = &mut self.pending_sms[pos];
        pending.parts.insert(c.seq, s.text);
        if pending.parts.len() == usize::from(pending.total) {
            let p = self.pending_sms.remove(pos);
            self.sent.push(SentSms { to: p.to, text: p.parts.into_values().collect(), encoding: p.encoding, parts: p.total });
        }
    }
}

fn push_line(out: &mut Vec<u8>, l: &str) {
    out.extend_from_slice(l.as_bytes());
    out.push(b'\r');
}

/// Digits, `#`, `*` and an optional leading `+` (the stock console's rule, plus `*`).
fn valid_number(n: &str) -> bool {
    let digits = n.strip_prefix('+').unwrap_or(n);
    !digits.is_empty() && digits.len() <= 40 && digits.bytes().all(|b| b.is_ascii_digit() || b == b'#' || b == b'*')
}

/// Splits the part of a command line after `AT` into commands (V.250 5.2): extended commands
/// (`+`, `%`, `^`, ...) run to the next `;` outside quotes, `D` takes the rest of the line, and
/// basic commands are a letter (or `&` + letter) with an optional number (`S<n>=<v>`/`S<n>?`).
/// `None` for bytes that fit none of these.
fn split_commands(body: &str) -> Option<Vec<&str>> {
    let b = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let start = i;
        match c {
            b';' | b' ' => {
                i += 1;
                continue;
            }
            b'+' | b'%' | b'^' | b'$' | b'*' | b'!' => {
                let mut quoted = false;
                while i < b.len() && (quoted || b[i] != b';') {
                    if b[i] == b'"' {
                        quoted = !quoted;
                    }
                    i += 1;
                }
            }
            b'D' | b'd' => {
                out.push(&body[i..]);
                break;
            }
            b'&' => {
                i += 2;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                i = i.min(b.len());
            }
            c if c.is_ascii_alphabetic() => {
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if c.eq_ignore_ascii_case(&b'S') && i < b.len() {
                    if b[i] == b'?' {
                        i += 1;
                    } else if b[i] == b'=' {
                        i += 1;
                        while i < b.len() && b[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                }
            }
            _ => return None,
        }
        if !body.is_char_boundary(i) {
            return None;
        }
        out.push(&body[start..i]);
    }
    Some(out)
}
