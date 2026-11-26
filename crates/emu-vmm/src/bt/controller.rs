//! A dual-mode (BR/EDR + LE) Bluetooth controller with no radio: it answers the commands the
//! guest's two HCI hosts send — the Linux kernel's own init when `/dev/vhci` registers `hci0`,
//! then Android's stack over the HAL's user channel — and finds no peers. Inquiry and LE scans
//! report nothing, pages time out, and an LE connection attempt waits until it is cancelled.
//!
//! What it claims is deliberately small, because both hosts send only what the controller says
//! it supports: Bluetooth 4.2, legacy advertising and scanning, Secure Simple Pairing, no SCO, no
//! LE encryption, privacy, data length extension or extended advertising. The Supported Commands
//! bitmap is built from [`COMMANDS`], the table of what [`Controller::command`] answers, so the
//! two can't disagree; any other command gets Unknown HCI Command.

use super::hci::{self, ev, op, status};

/// Every advertised command and its Supported Commands bit (octet × 10 + bit, as the spec's
/// table and AOSP's `OpCodeIndex` number them).
pub const COMMANDS: &[(u16, u16)] = &[
    (op::INQUIRY, 0),
    (op::INQUIRY_CANCEL, 1),
    (op::CREATE_CONNECTION, 4),
    (op::DISCONNECT, 5),
    (op::CREATE_CONNECTION_CANCEL, 7),
    (op::ACCEPT_CONNECTION_REQUEST, 10),
    (op::REJECT_CONNECTION_REQUEST, 11),
    (op::LINK_KEY_REQUEST_REPLY, 12),
    (op::LINK_KEY_REQUEST_NEGATIVE_REPLY, 13),
    (op::PIN_CODE_REQUEST_REPLY, 14),
    (op::PIN_CODE_REQUEST_NEGATIVE_REPLY, 15),
    (op::REMOTE_NAME_REQUEST, 23),
    (op::REMOTE_NAME_REQUEST_CANCEL, 24),
    (op::READ_DEFAULT_LINK_POLICY_SETTINGS, 53),
    (op::WRITE_DEFAULT_LINK_POLICY_SETTINGS, 54),
    (op::SET_EVENT_MASK, 56),
    (op::RESET, 57),
    (op::SET_EVENT_FILTER, 60),
    (op::READ_STORED_LINK_KEY, 65),
    (op::DELETE_STORED_LINK_KEY, 67),
    (op::WRITE_LOCAL_NAME, 70),
    (op::READ_LOCAL_NAME, 71),
    (op::READ_CONNECTION_ACCEPT_TIMEOUT, 72),
    (op::WRITE_CONNECTION_ACCEPT_TIMEOUT, 73),
    (op::READ_PAGE_TIMEOUT, 74),
    (op::WRITE_PAGE_TIMEOUT, 75),
    (op::READ_SCAN_ENABLE, 76),
    (op::WRITE_SCAN_ENABLE, 77),
    (op::READ_PAGE_SCAN_ACTIVITY, 80),
    (op::WRITE_PAGE_SCAN_ACTIVITY, 81),
    (op::READ_INQUIRY_SCAN_ACTIVITY, 82),
    (op::WRITE_INQUIRY_SCAN_ACTIVITY, 83),
    (op::READ_AUTHENTICATION_ENABLE, 84),
    (op::WRITE_AUTHENTICATION_ENABLE, 85),
    (op::READ_CLASS_OF_DEVICE, 90),
    (op::WRITE_CLASS_OF_DEVICE, 91),
    (op::READ_VOICE_SETTING, 92),
    (op::WRITE_VOICE_SETTING, 93),
    (op::READ_NUMBER_OF_SUPPORTED_IAC, 112),
    (op::READ_CURRENT_IAC_LAP, 113),
    (op::WRITE_CURRENT_IAC_LAP, 114),
    (op::READ_INQUIRY_SCAN_TYPE, 124),
    (op::WRITE_INQUIRY_SCAN_TYPE, 125),
    (op::READ_INQUIRY_MODE, 126),
    (op::WRITE_INQUIRY_MODE, 127),
    (op::READ_PAGE_SCAN_TYPE, 130),
    (op::WRITE_PAGE_SCAN_TYPE, 131),
    (op::READ_LOCAL_VERSION_INFORMATION, 143),
    (op::READ_LOCAL_SUPPORTED_FEATURES, 145),
    (op::READ_LOCAL_EXTENDED_FEATURES, 146),
    (op::READ_BUFFER_SIZE, 147),
    (op::READ_BD_ADDR, 151),
    (op::READ_EXTENDED_INQUIRY_RESPONSE, 170),
    (op::WRITE_EXTENDED_INQUIRY_RESPONSE, 171),
    (op::READ_SIMPLE_PAIRING_MODE, 175),
    (op::WRITE_SIMPLE_PAIRING_MODE, 176),
    (op::READ_INQUIRY_RESPONSE_TRANSMIT_POWER_LEVEL, 180),
    (op::READ_DEFAULT_ERRONEOUS_DATA_REPORTING, 182),
    (op::WRITE_DEFAULT_ERRONEOUS_DATA_REPORTING, 183),
    (op::SET_EVENT_MASK_PAGE_2, 222),
    (op::READ_ENCRYPTION_KEY_SIZE, 204),
    (op::READ_LE_HOST_SUPPORT, 245),
    (op::WRITE_LE_HOST_SUPPORT, 246),
    (op::LE_SET_EVENT_MASK, 250),
    (op::LE_READ_BUFFER_SIZE, 251),
    (op::LE_READ_LOCAL_SUPPORTED_FEATURES, 252),
    (op::LE_SET_RANDOM_ADDRESS, 254),
    (op::LE_SET_ADVERTISING_PARAMETERS, 255),
    (op::LE_READ_ADVERTISING_PHYSICAL_CHANNEL_TX_POWER, 256),
    (op::LE_SET_ADVERTISING_DATA, 257),
    (op::LE_SET_SCAN_RESPONSE_DATA, 260),
    (op::LE_SET_ADVERTISING_ENABLE, 261),
    (op::LE_SET_SCAN_PARAMETERS, 262),
    (op::LE_SET_SCAN_ENABLE, 263),
    (op::LE_CREATE_CONNECTION, 264),
    (op::LE_CREATE_CONNECTION_CANCEL, 265),
    (op::LE_READ_FILTER_ACCEPT_LIST_SIZE, 266),
    (op::LE_CLEAR_FILTER_ACCEPT_LIST, 267),
    (op::LE_ADD_DEVICE_TO_FILTER_ACCEPT_LIST, 270),
    (op::LE_REMOVE_DEVICE_FROM_FILTER_ACCEPT_LIST, 271),
    (op::LE_RAND, 277),
    (op::LE_READ_SUPPORTED_STATES, 283),
];

/// HCI and LMP version 4.2.
pub const VERSION: u8 = 0x08;
/// The company ID the spec reserves for tests (no vendor-specific behaviour keys off it).
pub const MANUFACTURER: u16 = 0xFFFF;

/// LMP features page 0: 3- and 5-slot packets, encryption, role switch, sniff (byte 0); EDR 2 Mb/s
/// ACL (byte 3); LE supported (bit 38); extended inquiry response and Secure Simple Pairing (bits
/// 48, 51); extended features (bit 63).
const FEATURES_PAGE0: u64 = 0xA7 | 1 << 25 | 1 << 38 | 1 << 48 | 1 << 51 | 1 << 63;
/// The highest extended features page (page 1 is the host's features, as the host wrote them).
const MAX_PAGE: u8 = 1;
/// LE states (legacy advertising and scanning in every combination), as the stock
/// emulator's controller reports them.
const LE_STATES: u64 = 0x0000_03ff_ffff_ffff;
const ACCEPT_LIST_SIZE: usize = 16;
/// ACL buffers: data length, count; LE: data length, count; SCO length and count (no SCO is
/// claimed, but the field is mandatory).
const ACL: (u16, u16) = (1021, 8);
const LE_ACL: (u16, u8) = (27, 15);
const SCO: (u8, u16) = (64, 8);
/// The General Inquiry Access Code, 0x9E8B33.
const GIAC: [u8; 3] = [0x33, 0x8b, 0x9e];

/// What the host can set; [`Controller::command`] with Reset restores the defaults (the spec's).
#[derive(Clone)]
struct State {
    event_mask: u64,
    le_event_mask: u64,
    name: [u8; 248],
    class_of_device: [u8; 3],
    scan_enable: u8,
    page_timeout: u16,
    accept_timeout: u16,
    page_scan_activity: [u8; 4],
    inquiry_scan_activity: [u8; 4],
    inquiry_scan_type: u8,
    page_scan_type: u8,
    inquiry_mode: u8,
    voice_setting: u16,
    auth_enable: u8,
    ssp_mode: u8,
    le_host: [u8; 2],
    default_link_policy: u16,
    eir: Vec<u8>,
    erroneous_data_reporting: u8,
    iac_lap: [u8; 3],
    random_address: [u8; 6],
    accept_list: Vec<[u8; 7]>,
    /// An LE Create Connection is outstanding (no peer will ever answer it).
    le_connecting: bool,
}

impl Default for State {
    fn default() -> State {
        State {
            event_mask: 0x0000_1fff_ffff_ffff,
            le_event_mask: 0x1f,
            name: [0; 248],
            class_of_device: [0; 3],
            scan_enable: 0,
            page_timeout: 0x2000,
            accept_timeout: 0x1f40,
            page_scan_activity: [0x00, 0x08, 0x12, 0x00],
            inquiry_scan_activity: [0x00, 0x10, 0x12, 0x00],
            inquiry_scan_type: 0,
            page_scan_type: 0,
            inquiry_mode: 0,
            voice_setting: 0x0060,
            auth_enable: 0,
            ssp_mode: 0,
            le_host: [0, 0],
            default_link_policy: 0,
            eir: vec![0; 241],
            erroneous_data_reporting: 0,
            iac_lap: GIAC,
            random_address: [0; 6],
            accept_list: Vec::new(),
            le_connecting: false,
        }
    }
}

pub struct Controller {
    /// The public address, least significant byte first (as HCI carries it).
    addr: [u8; 6],
    state: State,
    rng: u64,
    supported: [u8; 64],
}

impl Controller {
    /// `addr` is the public address in display order (`addr[0]` is the most significant byte).
    pub fn new(addr: [u8; 6]) -> Controller {
        let mut wire = addr;
        wire.reverse();
        let mut supported = [0u8; 64];
        for &(_, bit) in COMMANDS {
            supported[usize::from(bit / 10)] |= 1 << (bit % 10);
        }
        let seed = u64::from_le_bytes([wire[0], wire[1], wire[2], wire[3], wire[4], wire[5], 0x5e, 0xe1]);
        Controller { addr: wire, state: State::default(), rng: seed | 1, supported }
    }

    /// The Supported Commands bitmap (Read Local Supported Commands).
    pub fn supported_commands(&self) -> &[u8; 64] {
        &self.supported
    }

    /// Answers one command packet (opcode, length, parameters; no H4 type byte) with the events to
    /// send, in order. `None` when the opcode is one the controller doesn't know (it answered
    /// Unknown HCI Command), so the caller can say so once.
    pub fn command(&mut self, packet: &[u8]) -> (Vec<Vec<u8>>, Option<u16>) {
        let [lo, hi, len, ref rest @ ..] = *packet else {
            return (Vec::new(), None);
        };
        let opcode = u16::from_le_bytes([lo, hi]);
        let p = &rest[..rest.len().min(usize::from(len))];
        match self.answer(opcode, p) {
            Some(events) => (events, None),
            // Android's stack probes vendor commands and accepts Command Status for them; for
            // the rest it expects Command Complete.
            None if op::is_vendor(opcode) => (vec![hci::command_status(opcode, status::UNKNOWN_COMMAND)], Some(opcode)),
            None => (vec![hci::command_complete(opcode, &[status::UNKNOWN_COMMAND])], Some(opcode)),
        }
    }

    fn next_random(&mut self) -> u64 {
        // xorshift64*: LE Rand needs unpredictable-enough bytes for address generation, not
        // cryptographic ones (there is no radio to be overheard).
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn masked(&self, code: u8) -> bool {
        self.state.event_mask & (1 << (code - 1)) != 0
    }

    fn answer(&mut self, opcode: u16, p: &[u8]) -> Option<Vec<Vec<u8>>> {
        let s = &mut self.state;
        let ok = |ret: &[u8]| {
            let mut r = vec![status::SUCCESS];
            r.extend_from_slice(ret);
            vec![hci::command_complete(opcode, &r)]
        };
        let fail = |code: u8, ret: &[u8]| {
            let mut r = vec![code];
            r.extend_from_slice(ret);
            vec![hci::command_complete(opcode, &r)]
        };
        let bad = || vec![hci::command_complete(opcode, &[status::INVALID_PARAMETERS])];
        let addr_of = |p: &[u8]| -> [u8; 6] { p.get(..6).and_then(|a| a.try_into().ok()).unwrap_or([0; 6]) };
        let u16_at = |p: &[u8], i: usize| p.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
        let events = match opcode {
            // Link control. With no peers: inquiries find nothing, pages time out, and there is
            // never a connection to accept, reject, authenticate or disconnect.
            op::INQUIRY => {
                let mut e = vec![hci::command_status(opcode, status::SUCCESS)];
                if self.state.event_mask & 1 != 0 {
                    e.push(hci::other(ev::INQUIRY_COMPLETE, &[status::SUCCESS]));
                }
                e
            }
            op::INQUIRY_CANCEL => ok(&[]),
            op::CREATE_CONNECTION | op::REMOTE_NAME_REQUEST => {
                if p.len() < 6 {
                    return Some(vec![hci::command_status(opcode, status::INVALID_PARAMETERS)]);
                }
                let peer = addr_of(p);
                let mut e = vec![hci::command_status(opcode, status::SUCCESS)];
                if opcode == op::CREATE_CONNECTION && self.masked(ev::CONNECTION_COMPLETE) {
                    // status, handle, address, link type ACL, encryption off
                    let mut c = vec![status::PAGE_TIMEOUT, 0, 0];
                    c.extend_from_slice(&peer);
                    c.extend_from_slice(&[0x01, 0x00]);
                    e.push(hci::other(ev::CONNECTION_COMPLETE, &c));
                }
                if opcode == op::REMOTE_NAME_REQUEST && self.masked(ev::REMOTE_NAME_REQUEST_COMPLETE) {
                    let mut c = vec![status::PAGE_TIMEOUT];
                    c.extend_from_slice(&peer);
                    c.extend_from_slice(&[0; 248]);
                    e.push(hci::other(ev::REMOTE_NAME_REQUEST_COMPLETE, &c));
                }
                e
            }
            op::DISCONNECT | op::ACCEPT_CONNECTION_REQUEST | op::REJECT_CONNECTION_REQUEST => {
                vec![hci::command_status(opcode, status::UNKNOWN_CONNECTION)]
            }
            op::CREATE_CONNECTION_CANCEL
            | op::REMOTE_NAME_REQUEST_CANCEL
            | op::LINK_KEY_REQUEST_REPLY
            | op::LINK_KEY_REQUEST_NEGATIVE_REPLY
            | op::PIN_CODE_REQUEST_REPLY
            | op::PIN_CODE_REQUEST_NEGATIVE_REPLY => fail(status::UNKNOWN_CONNECTION, &addr_of(p)),

            // Android's stack (API 31-32) requires this on every encrypted link; no link ever
            // exists here, so the answer is the full 128-bit key for whatever handle it names.
            op::READ_ENCRYPTION_KEY_SIZE => match u16_at(p, 0) {
                Some(h) => ok(&[&h.to_le_bytes()[..], &[16]].concat()),
                None => bad(),
            },

            // Link policy.
            op::READ_DEFAULT_LINK_POLICY_SETTINGS => ok(&s.default_link_policy.to_le_bytes()),
            op::WRITE_DEFAULT_LINK_POLICY_SETTINGS => match u16_at(p, 0) {
                Some(v) => {
                    s.default_link_policy = v;
                    ok(&[])
                }
                None => bad(),
            },

            // Controller and baseband.
            op::SET_EVENT_MASK | op::LE_SET_EVENT_MASK => match p.get(..8) {
                Some(m) => {
                    let m = u64::from_le_bytes(m.try_into().expect("8 bytes"));
                    if opcode == op::SET_EVENT_MASK {
                        s.event_mask = m;
                    } else {
                        s.le_event_mask = m;
                    }
                    ok(&[])
                }
                None => bad(),
            },
            op::RESET => {
                self.state = State::default();
                ok(&[])
            }
            op::SET_EVENT_FILTER
            | op::SET_EVENT_MASK_PAGE_2
            | op::SET_CONTROLLER_TO_HOST_FLOW_CONTROL
            | op::HOST_BUFFER_SIZE
            | op::LE_SET_ADVERTISING_PARAMETERS
            | op::LE_SET_ADVERTISING_DATA
            | op::LE_SET_SCAN_RESPONSE_DATA
            | op::LE_SET_ADVERTISING_ENABLE
            | op::LE_SET_SCAN_PARAMETERS
            | op::LE_SET_SCAN_ENABLE => ok(&[]),
            // No response at all, as the spec has it (flow control isn't claimed, so a host
            // shouldn't send it; one that does mustn't get an unexpected event).
            0x0C35 => Vec::new(),
            op::READ_STORED_LINK_KEY => ok(&[0, 0, 0, 0]),
            op::DELETE_STORED_LINK_KEY => ok(&[0, 0]),
            op::WRITE_LOCAL_NAME => {
                let n = p.len().min(248);
                s.name = [0; 248];
                s.name[..n].copy_from_slice(&p[..n]);
                ok(&[])
            }
            op::READ_LOCAL_NAME => ok(&s.name),
            op::READ_CONNECTION_ACCEPT_TIMEOUT => ok(&s.accept_timeout.to_le_bytes()),
            op::READ_PAGE_TIMEOUT => ok(&s.page_timeout.to_le_bytes()),
            op::READ_VOICE_SETTING => ok(&s.voice_setting.to_le_bytes()),
            op::WRITE_CONNECTION_ACCEPT_TIMEOUT | op::WRITE_PAGE_TIMEOUT | op::WRITE_VOICE_SETTING => {
                let Some(v) = u16_at(p, 0) else { return Some(bad()) };
                match opcode {
                    op::WRITE_CONNECTION_ACCEPT_TIMEOUT => s.accept_timeout = v,
                    op::WRITE_PAGE_TIMEOUT => s.page_timeout = v,
                    _ => s.voice_setting = v,
                }
                ok(&[])
            }
            op::READ_SCAN_ENABLE => ok(&[s.scan_enable]),
            op::READ_AUTHENTICATION_ENABLE => ok(&[s.auth_enable]),
            op::READ_INQUIRY_SCAN_TYPE => ok(&[s.inquiry_scan_type]),
            op::READ_INQUIRY_MODE => ok(&[s.inquiry_mode]),
            op::READ_PAGE_SCAN_TYPE => ok(&[s.page_scan_type]),
            op::READ_SIMPLE_PAIRING_MODE => ok(&[s.ssp_mode]),
            op::READ_DEFAULT_ERRONEOUS_DATA_REPORTING => ok(&[s.erroneous_data_reporting]),
            op::WRITE_SCAN_ENABLE
            | op::WRITE_AUTHENTICATION_ENABLE
            | op::WRITE_INQUIRY_SCAN_TYPE
            | op::WRITE_INQUIRY_MODE
            | op::WRITE_PAGE_SCAN_TYPE
            | op::WRITE_SIMPLE_PAIRING_MODE
            | op::WRITE_DEFAULT_ERRONEOUS_DATA_REPORTING => {
                let Some(&v) = p.first() else { return Some(bad()) };
                *match opcode {
                    op::WRITE_SCAN_ENABLE => &mut s.scan_enable,
                    op::WRITE_AUTHENTICATION_ENABLE => &mut s.auth_enable,
                    op::WRITE_INQUIRY_SCAN_TYPE => &mut s.inquiry_scan_type,
                    op::WRITE_INQUIRY_MODE => &mut s.inquiry_mode,
                    op::WRITE_PAGE_SCAN_TYPE => &mut s.page_scan_type,
                    op::WRITE_SIMPLE_PAIRING_MODE => &mut s.ssp_mode,
                    _ => &mut s.erroneous_data_reporting,
                } = v;
                ok(&[])
            }
            op::READ_PAGE_SCAN_ACTIVITY => ok(&s.page_scan_activity),
            op::READ_INQUIRY_SCAN_ACTIVITY => ok(&s.inquiry_scan_activity),
            op::WRITE_PAGE_SCAN_ACTIVITY | op::WRITE_INQUIRY_SCAN_ACTIVITY => {
                let Some(v) = p.get(..4) else { return Some(bad()) };
                let v: [u8; 4] = v.try_into().expect("4 bytes");
                if opcode == op::WRITE_PAGE_SCAN_ACTIVITY {
                    s.page_scan_activity = v;
                } else {
                    s.inquiry_scan_activity = v;
                }
                ok(&[])
            }
            op::READ_CLASS_OF_DEVICE => ok(&s.class_of_device),
            op::WRITE_CLASS_OF_DEVICE => match p.get(..3) {
                Some(c) => {
                    s.class_of_device.copy_from_slice(c);
                    ok(&[])
                }
                None => bad(),
            },
            op::READ_NUMBER_OF_SUPPORTED_IAC => ok(&[1]),
            op::READ_CURRENT_IAC_LAP => ok(&[1, s.iac_lap[0], s.iac_lap[1], s.iac_lap[2]]),
            op::WRITE_CURRENT_IAC_LAP => match p {
                [1, a, b, c, ..] => {
                    s.iac_lap = [*a, *b, *c];
                    ok(&[])
                }
                _ => bad(),
            },
            op::READ_EXTENDED_INQUIRY_RESPONSE => ok(&s.eir),
            op::WRITE_EXTENDED_INQUIRY_RESPONSE => {
                let n = p.len().min(241);
                s.eir = vec![0; 241];
                s.eir[..n].copy_from_slice(&p[..n]);
                ok(&[])
            }
            op::READ_INQUIRY_RESPONSE_TRANSMIT_POWER_LEVEL | op::LE_READ_ADVERTISING_PHYSICAL_CHANNEL_TX_POWER => ok(&[0]),
            op::READ_LE_HOST_SUPPORT => ok(&s.le_host),
            op::WRITE_LE_HOST_SUPPORT => match p.get(..2) {
                Some(v) => {
                    s.le_host.copy_from_slice(v);
                    ok(&[])
                }
                None => bad(),
            },

            // Informational.
            op::READ_LOCAL_VERSION_INFORMATION => {
                let mut r = vec![VERSION, 0, 0, VERSION];
                r.extend_from_slice(&MANUFACTURER.to_le_bytes());
                r.extend_from_slice(&[0, 0]);
                ok(&r)
            }
            op::READ_LOCAL_SUPPORTED_COMMANDS => ok(&self.supported),
            op::READ_LOCAL_SUPPORTED_FEATURES => ok(&FEATURES_PAGE0.to_le_bytes()),
            op::READ_LOCAL_EXTENDED_FEATURES => {
                let Some(&page) = p.first() else { return Some(bad()) };
                let features = match page {
                    0 => FEATURES_PAGE0,
                    // Host features: SSP (bit 0), LE (bit 1), simultaneous LE and BR/EDR (bit 2).
                    1 => u64::from(s.ssp_mode & 1) | u64::from(s.le_host[0] & 1) << 1 | u64::from(s.le_host[1] & 1) << 2,
                    _ => return Some(fail(status::INVALID_PARAMETERS, &[page, MAX_PAGE, 0, 0, 0, 0, 0, 0, 0, 0])),
                };
                let mut r = vec![page, MAX_PAGE];
                r.extend_from_slice(&features.to_le_bytes());
                ok(&r)
            }
            op::READ_BUFFER_SIZE => {
                let mut r = ACL.0.to_le_bytes().to_vec();
                r.push(SCO.0);
                r.extend_from_slice(&ACL.1.to_le_bytes());
                r.extend_from_slice(&SCO.1.to_le_bytes());
                ok(&r)
            }
            op::READ_BD_ADDR => ok(&self.addr),

            // LE.
            op::LE_READ_BUFFER_SIZE => {
                let mut r = LE_ACL.0.to_le_bytes().to_vec();
                r.push(LE_ACL.1);
                ok(&r)
            }
            op::LE_READ_LOCAL_SUPPORTED_FEATURES => ok(&[0; 8]),
            op::LE_READ_SUPPORTED_STATES => ok(&LE_STATES.to_le_bytes()),
            op::LE_SET_RANDOM_ADDRESS => match p.get(..6) {
                Some(a) => {
                    s.random_address.copy_from_slice(a);
                    ok(&[])
                }
                None => bad(),
            },
            op::LE_CREATE_CONNECTION => {
                let code = if s.le_connecting { status::COMMAND_DISALLOWED } else { status::SUCCESS };
                s.le_connecting = true;
                vec![hci::command_status(opcode, code)]
            }
            op::LE_CREATE_CONNECTION_CANCEL => {
                if !std::mem::take(&mut s.le_connecting) {
                    return Some(fail(status::COMMAND_DISALLOWED, &[]));
                }
                let mut e = ok(&[]);
                // The cancelled attempt ends with LE Connection Complete, Unknown Connection
                // Identifier (Vol 4 Part E 7.8.13).
                if self.masked(ev::LE_META) && self.state.le_event_mask & 1 != 0 {
                    let mut c = vec![ev::LE_CONNECTION_COMPLETE, status::UNKNOWN_CONNECTION];
                    c.extend_from_slice(&[0; 17]);
                    e.push(hci::other(ev::LE_META, &c));
                }
                e
            }
            op::LE_READ_FILTER_ACCEPT_LIST_SIZE => ok(&[ACCEPT_LIST_SIZE as u8]),
            op::LE_CLEAR_FILTER_ACCEPT_LIST => {
                s.accept_list.clear();
                ok(&[])
            }
            op::LE_ADD_DEVICE_TO_FILTER_ACCEPT_LIST | op::LE_REMOVE_DEVICE_FROM_FILTER_ACCEPT_LIST => {
                let Some(entry) = p.get(..7) else { return Some(bad()) };
                let entry: [u8; 7] = entry.try_into().expect("7 bytes");
                s.accept_list.retain(|e| *e != entry);
                if opcode == op::LE_ADD_DEVICE_TO_FILTER_ACCEPT_LIST {
                    if s.accept_list.len() >= ACCEPT_LIST_SIZE {
                        // Memory Capacity Exceeded.
                        return Some(fail(0x07, &[]));
                    }
                    s.accept_list.push(entry);
                }
                ok(&[])
            }
            op::LE_RAND => {
                let r = self.next_random().to_le_bytes();
                vec![hci::command_complete(opcode, &[&[status::SUCCESS][..], &r].concat())]
            }
            _ => return None,
        };
        Some(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: [u8; 6] = [0xBB, 0xBB, 0xBB, 0x12, 0x34, 0x56];

    fn cmd(opcode: u16, params: &[u8]) -> Vec<u8> {
        let mut c = opcode.to_le_bytes().to_vec();
        c.push(params.len() as u8);
        c.extend_from_slice(params);
        c
    }

    /// The single Command Complete `opcode` got: its return parameters, status first.
    fn complete(c: &mut Controller, opcode: u16, params: &[u8]) -> Vec<u8> {
        let (events, unknown) = c.command(&cmd(opcode, params));
        assert_eq!(unknown, None, "{opcode:#06x} unknown");
        assert_eq!(events.len(), 1, "{opcode:#06x}: {events:x?}");
        let e = &events[0];
        assert_eq!(e[0], ev::COMMAND_COMPLETE);
        assert_eq!(usize::from(e[1]), e.len() - 2, "length byte");
        assert_eq!(e[2], 1, "one command credit");
        assert_eq!(u16::from_le_bytes([e[3], e[4]]), opcode);
        e[5..].to_vec()
    }

    #[test]
    fn reset_answers_with_the_bytes_the_hal_waits_for() {
        let mut c = Controller::new(ADDR);
        let (events, _) = c.command(&[0x03, 0x0c, 0x00]);
        assert_eq!(events, [vec![0x0e, 0x04, 0x01, 0x03, 0x0c, 0x00]]);
        // Reset restores the defaults, the address excepted.
        complete(&mut c, op::WRITE_SCAN_ENABLE, &[3]);
        complete(&mut c, op::WRITE_LOCAL_NAME, b"emu");
        assert_eq!(complete(&mut c, op::READ_SCAN_ENABLE, &[]), [0, 3]);
        complete(&mut c, op::RESET, &[]);
        assert_eq!(complete(&mut c, op::READ_SCAN_ENABLE, &[]), [0, 0]);
        assert_eq!(complete(&mut c, op::READ_LOCAL_NAME, &[]), [vec![0], vec![0; 248]].concat());
    }

    #[test]
    fn read_version_address_buffers_and_features() {
        let mut c = Controller::new(ADDR);
        // HCI 4.2, revision 0, LMP 4.2, manufacturer 0xFFFF, subversion 0.
        assert_eq!(complete(&mut c, op::READ_LOCAL_VERSION_INFORMATION, &[]), [0, 0x08, 0, 0, 0x08, 0xff, 0xff, 0, 0]);
        // Little-endian on the wire: BB:BB:BB:12:34:56 is 56 34 12 BB BB BB.
        assert_eq!(complete(&mut c, op::READ_BD_ADDR, &[]), [0, 0x56, 0x34, 0x12, 0xbb, 0xbb, 0xbb]);
        // ACL 1021 × 8, SCO 64 × 8; LE 27 × 15.
        assert_eq!(complete(&mut c, op::READ_BUFFER_SIZE, &[]), [0, 0xfd, 0x03, 64, 8, 0, 8, 0]);
        assert_eq!(complete(&mut c, op::LE_READ_BUFFER_SIZE, &[]), [0, 27, 0, 15]);
        let page0 = complete(&mut c, op::READ_LOCAL_SUPPORTED_FEATURES, &[]);
        let f = u64::from_le_bytes(page0[1..].try_into().expect("8"));
        assert!(f & 1 << 38 != 0, "LE supported");
        assert!(f & 1 << 37 == 0, "BR/EDR supported");
        assert!(f & 1 << 51 != 0, "SSP");
        // Page 1 reflects what the host enabled.
        assert_eq!(complete(&mut c, op::READ_LOCAL_EXTENDED_FEATURES, &[1]), [0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
        complete(&mut c, op::WRITE_SIMPLE_PAIRING_MODE, &[1]);
        complete(&mut c, op::WRITE_LE_HOST_SUPPORT, &[1, 0]);
        assert_eq!(complete(&mut c, op::READ_LOCAL_EXTENDED_FEATURES, &[1]), [0, 1, 1, 3, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(complete(&mut c, op::READ_LOCAL_EXTENDED_FEATURES, &[0])[3..], page0[1..]);
        assert_eq!(complete(&mut c, op::READ_LOCAL_EXTENDED_FEATURES, &[2])[0], status::INVALID_PARAMETERS);
        assert_eq!(complete(&mut c, op::LE_READ_SUPPORTED_STATES, &[]), [0, 0xff, 0xff, 0xff, 0xff, 0xff, 0x03, 0, 0]);
    }

    #[test]
    fn read_encryption_key_size_answers_sixteen() {
        let mut c = Controller::new(ADDR);
        // Status, the handle echoed, a 128-bit key.
        assert_eq!(complete(&mut c, op::READ_ENCRYPTION_KEY_SIZE, &[0x2a, 0x00]), [0, 0x2a, 0x00, 16]);
        assert_eq!(complete(&mut c, op::READ_ENCRYPTION_KEY_SIZE, &[0x01]), [status::INVALID_PARAMETERS]);
        // Advertised: octet 20, bit 4.
        assert!(c.supported_commands()[20] & 1 << 4 != 0);
    }

    #[test]
    fn the_supported_commands_bitmap_is_the_table() {
        let mut c = Controller::new(ADDR);
        let bitmap = complete(&mut c, op::READ_LOCAL_SUPPORTED_COMMANDS, &[]);
        assert_eq!(bitmap.len(), 65);
        assert_eq!(&bitmap[1..], c.supported_commands());
        // Spot checks against the spec's table: Reset (5.7), Read BD_ADDR (15.1), LE Rand (27.7).
        assert!(bitmap[1 + 5] & 1 << 7 != 0);
        assert!(bitmap[1 + 15] & 1 << 1 != 0);
        assert!(bitmap[1 + 27] & 1 << 7 != 0);
        // Not claimed: Read Local Supported Codecs (29.5), Host Buffer Size (10.6), LE Set Data Length.
        assert!(bitmap[1 + 29] & 1 << 5 == 0);
        assert!(bitmap[1 + 10] & 1 << 6 == 0);
        // Every advertised command is answered with its own event, never Unknown HCI Command.
        for &(opcode, _) in COMMANDS {
            let mut c = Controller::new(ADDR);
            if opcode == op::LE_CREATE_CONNECTION_CANCEL {
                c.command(&cmd(op::LE_CREATE_CONNECTION, &[0; 25]));
            }
            let (events, unknown) = c.command(&cmd(opcode, &[1; 32]));
            assert_eq!(unknown, None, "{opcode:#06x}");
            let first = &events[0];
            let (code, at) = if first[0] == ev::COMMAND_COMPLETE { (first[5], 3) } else { (first[2], 4) };
            assert_eq!(u16::from_le_bytes([first[at], first[at + 1]]), opcode);
            assert_ne!(code, status::UNKNOWN_COMMAND, "{opcode:#06x}");
            assert_ne!(code, status::INVALID_PARAMETERS, "{opcode:#06x}");
        }
    }

    #[test]
    fn unknown_commands_get_the_event_android_expects() {
        let mut c = Controller::new(ADDR);
        // LE Set Data Length (not claimed): Command Complete, Unknown HCI Command.
        let (events, unknown) = c.command(&cmd(0x2022, &[0; 6]));
        assert_eq!(unknown, Some(0x2022));
        assert_eq!(events, [vec![0x0e, 0x04, 0x01, 0x22, 0x20, 0x01]]);
        // LE Get Vendor Capabilities: Command Status, which Android's stack reads as "none".
        let (events, unknown) = c.command(&cmd(0xFD53, &[]));
        assert_eq!(unknown, Some(0xFD53));
        assert_eq!(events, [vec![0x0f, 0x04, 0x01, 0x01, 0x53, 0xfd]]);
        // A runt packet: nothing.
        assert_eq!(c.command(&[0x03]), (Vec::new(), None));
        // Host Number Of Completed Packets: no event at all.
        assert_eq!(c.command(&cmd(0x0C35, &[0; 5])), (Vec::new(), None));
    }

    #[test]
    fn with_no_peers_inquiry_finds_nothing_and_pages_time_out() {
        let mut c = Controller::new(ADDR);
        let (e, _) = c.command(&cmd(op::INQUIRY, &[0x33, 0x8b, 0x9e, 0x08, 0x00]));
        assert_eq!(e, [vec![0x0f, 0x04, 0x00, 0x01, 0x01, 0x04], vec![0x01, 0x01, 0x00]]);
        let peer = [1, 2, 3, 4, 5, 6];
        let mut create = peer.to_vec();
        create.extend_from_slice(&[0x18, 0xcc, 0x01, 0x00, 0x00, 0x00, 0x01]);
        let (e, _) = c.command(&cmd(op::CREATE_CONNECTION, &create));
        assert_eq!(e[0], [0x0f, 0x04, 0x00, 0x01, 0x05, 0x04]);
        assert_eq!(e[1], [0x03, 0x0b, status::PAGE_TIMEOUT, 0, 0, 1, 2, 3, 4, 5, 6, 0x01, 0x00]);
        let (e, _) = c.command(&cmd(op::REMOTE_NAME_REQUEST, &[1, 2, 3, 4, 5, 6, 1, 0, 0, 0]));
        assert_eq!(e.len(), 2);
        assert_eq!(e[1][..9], [0x07, 255, status::PAGE_TIMEOUT, 1, 2, 3, 4, 5, 6]);
        assert_eq!(e[1].len(), 2 + 255);
        let (e, _) = c.command(&cmd(op::DISCONNECT, &[0, 0, 0x13]));
        assert_eq!(e, [vec![0x0f, 0x04, status::UNKNOWN_CONNECTION, 0x01, 0x06, 0x04]]);
        // Masked events aren't sent: Inquiry Complete is bit 0.
        complete(&mut c, op::SET_EVENT_MASK, &[0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        let (e, _) = c.command(&cmd(op::INQUIRY, &[0x33, 0x8b, 0x9e, 0x08, 0x00]));
        assert_eq!(e.len(), 1);
    }

    #[test]
    fn an_le_connection_waits_until_cancelled() {
        let mut c = Controller::new(ADDR);
        let (e, _) = c.command(&cmd(op::LE_CREATE_CONNECTION, &[0; 25]));
        assert_eq!(e, [vec![0x0f, 0x04, 0x00, 0x01, 0x0d, 0x20]]);
        let (e, _) = c.command(&cmd(op::LE_CREATE_CONNECTION, &[0; 25]));
        assert_eq!(e[0][2], status::COMMAND_DISALLOWED, "one at a time");
        // LE Meta is masked off by default (bit 61 is outside the reset mask).
        complete(&mut c, op::SET_EVENT_MASK, &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x3f]);
        let (e, _) = c.command(&cmd(op::LE_CREATE_CONNECTION_CANCEL, &[]));
        assert_eq!(e[0], [0x0e, 0x04, 0x01, 0x0e, 0x20, 0x00]);
        assert_eq!(e[1].len(), 2 + 19);
        assert_eq!(e[1][..4], [ev::LE_META, 19, ev::LE_CONNECTION_COMPLETE, status::UNKNOWN_CONNECTION]);
        assert_eq!(complete(&mut c, op::LE_CREATE_CONNECTION_CANCEL, &[]), [status::COMMAND_DISALLOWED]);
    }

    #[test]
    fn settings_read_back_what_the_host_wrote() {
        let mut c = Controller::new(ADDR);
        complete(&mut c, op::WRITE_CLASS_OF_DEVICE, &[0x0c, 0x02, 0x5a]);
        assert_eq!(complete(&mut c, op::READ_CLASS_OF_DEVICE, &[]), [0, 0x0c, 0x02, 0x5a]);
        complete(&mut c, op::WRITE_PAGE_TIMEOUT, &[0x00, 0x20]);
        assert_eq!(complete(&mut c, op::READ_PAGE_TIMEOUT, &[]), [0, 0x00, 0x20]);
        complete(&mut c, op::WRITE_PAGE_SCAN_ACTIVITY, &[0x00, 0x04, 0x12, 0x00]);
        assert_eq!(complete(&mut c, op::READ_PAGE_SCAN_ACTIVITY, &[]), [0, 0x00, 0x04, 0x12, 0x00]);
        assert_eq!(complete(&mut c, op::READ_CURRENT_IAC_LAP, &[]), [0, 1, 0x33, 0x8b, 0x9e]);
        assert_eq!(complete(&mut c, op::WRITE_CURRENT_IAC_LAP, &[2, 0, 0, 0, 0, 0, 0]), [status::INVALID_PARAMETERS]);
        assert_eq!(complete(&mut c, op::WRITE_CLASS_OF_DEVICE, &[1]), [status::INVALID_PARAMETERS]);
        // The accept list holds 16 distinct entries.
        for i in 0..16u8 {
            complete(&mut c, op::LE_ADD_DEVICE_TO_FILTER_ACCEPT_LIST, &[0, i, 0, 0, 0, 0, 0]);
        }
        complete(&mut c, op::LE_ADD_DEVICE_TO_FILTER_ACCEPT_LIST, &[0, 3, 0, 0, 0, 0, 0]);
        assert_eq!(complete(&mut c, op::LE_ADD_DEVICE_TO_FILTER_ACCEPT_LIST, &[1, 0, 0, 0, 0, 0, 0]), [0x07]);
        complete(&mut c, op::LE_CLEAR_FILTER_ACCEPT_LIST, &[]);
        complete(&mut c, op::LE_ADD_DEVICE_TO_FILTER_ACCEPT_LIST, &[1, 0, 0, 0, 0, 0, 0]);
        // LE Rand: eight bytes that change.
        let a = complete(&mut c, op::LE_RAND, &[]);
        let b = complete(&mut c, op::LE_RAND, &[]);
        assert_eq!(a.len(), 9);
        assert_ne!(a, b);
    }
}
