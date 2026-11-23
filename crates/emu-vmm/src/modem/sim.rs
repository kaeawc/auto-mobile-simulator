//! The SIM's elementary files, served through `AT+CRSM` (TS 27.007 restricted SIM access).
//!
//! The framework loads SIM records with `iccIoForApp`, which the HAL turns into
//! `AT+CRSM=<cmd>,<fileid>,<p1>,<p2>,<p3>[,<data>,<aid>]` (decimal file ids, no path). It first
//! sends GET RESPONSE (192, p3 = 15) and reads the file size (bytes 2-3), structure (byte 13)
//! and record length (byte 14) from the classic 15-byte GSM 11.11 header; the HAL passes such a
//! header through unchanged (it only converts USIM FCP templates, which start with 0x62). Then
//! READ BINARY (176, offset p1:p2, length p3) or READ RECORD (178, record p1, mode p2 = 4
//! absolute, length p3). Status words go back as `+CRSM: <sw1>,<sw2>[,<hex>]`; 0x94 0x04
//! (file not found) and 0x6A 0x83 (record not found) make the framework skip the file
//! gracefully. The contents follow the stock emulator's SIM (sim_card.c).

use std::collections::BTreeMap;

use super::pdu::{hex, unhex};

const GET_RESPONSE: u32 = 192;
const READ_BINARY: u32 = 176;
const READ_RECORD: u32 = 178;
const UPDATE_BINARY: u32 = 214;
const UPDATE_RECORD: u32 = 220;
const STATUS: u32 = 242;

enum Ef {
    Transparent(Vec<u8>),
    LinearFixed { record_len: usize, records: Vec<Vec<u8>> },
}

pub const EF_ICCID: u16 = 0x2fe2;
pub const EF_MSISDN: u16 = 0x6f40;

pub struct SimFiles {
    files: BTreeMap<u16, Ef>,
}

impl SimFiles {
    pub fn new(iccid: &str, msisdn: &str) -> Self {
        let mut files = BTreeMap::new();
        let mut put = |id: u16, ef: Ef| {
            files.insert(id, ef);
        };
        let b = |s: &str| unhex(s).unwrap_or_default();
        put(EF_ICCID, Ef::Transparent(iccid_bytes(iccid)));
        // EF_AD: normal operation, MNC length 3 (310 260).
        put(0x6fad, Ef::Transparent(b("00000003")));
        // CPHS operator name string.
        put(0x6f14, Ef::Transparent(b("416e64726f6964ffffffffffffffffffffffffff")));
        // CPHS voice message waiting and call forwarding flags: none.
        put(0x6f11, Ef::Transparent(b("55")));
        put(0x6f13, Ef::Transparent(b("55")));
        // CPHS information.
        put(0x6f16, Ef::Transparent(b("0233")));
        // SIM service table.
        put(0x6f38, Ef::Transparent(b("ff30ffff3c003c03000c0000f03f00")));
        // Forbidden PLMNs: none.
        put(0x6f7b, Ef::Transparent(vec![0xff; 15]));
        // Mailbox identifier and message waiting status.
        put(0x6fc9, linear(4, 2, &[b("01000000")]));
        put(0x6fca, linear(5, 2, &[b("0000000000")]));
        // PLMN network name: "Android".
        put(0x6fc5, linear(24, 10, &[b("43058441aa890affffffffffffffffffffffffffffffffff")]));
        // Voice mail number: "Voicemail" +15552175049.
        put(0x6fc7, linear(32, 2, &[b("566f6963656d61696cffffffffffffffffff07915155125740f9ffffffffffff")]));
        let mut sim = SimFiles { files };
        sim.set_msisdn(msisdn);
        sim
    }

    /// Rewrites EF_MSISDN record 1 with `number` (the device's own number).
    pub fn set_msisdn(&mut self, number: &str) {
        let mut rec = vec![0xff; 32];
        let (ton, digits) = match number.strip_prefix('+') {
            Some(d) => (0x91, d),
            None => (0x81, number),
        };
        let nibbles: Vec<u8> = digits.bytes().filter(u8::is_ascii_digit).take(20).map(|d| d - b'0').collect();
        let bcd: Vec<u8> = nibbles.chunks(2).map(|p| p[0] | (p.get(1).copied().unwrap_or(0xf) << 4)).collect();
        // Alpha identifier (18) | length | TON/NPI | dialling number (10) | CCP | EXT.
        rec[18] = (bcd.len() + 1) as u8;
        rec[19] = ton;
        rec[20..20 + bcd.len()].copy_from_slice(&bcd);
        self.files.insert(EF_MSISDN, linear(32, 4, &[rec]));
    }

    /// Handles the arguments of `AT+CRSM=`; returns the `+CRSM:` line, or `None` for
    /// malformed arguments.
    pub fn crsm(&mut self, args: &str) -> Option<String> {
        let mut fields = args.split(',').map(|f| f.trim().trim_matches('"'));
        let mut num = || -> Option<u32> { fields.next()?.parse().ok() };
        let cmd = num()?;
        let id = u16::try_from(num().unwrap_or(0)).ok()?;
        let p1 = num().unwrap_or(0) as usize;
        let p2 = num().unwrap_or(0) as usize;
        let p3 = num().unwrap_or(0) as usize;
        let data = fields.next().and_then(unhex).unwrap_or_default();

        let sw = |sw1: u8, sw2: u8| Some(format!("+CRSM: {sw1},{sw2}"));
        let ok = |body: &[u8]| Some(format!("+CRSM: 144,0,{}", hex(body)));
        if cmd == STATUS {
            return sw(144, 0);
        }
        let Some(ef) = self.files.get_mut(&id) else {
            return match cmd {
                GET_RESPONSE | READ_BINARY | READ_RECORD | UPDATE_BINARY | UPDATE_RECORD => sw(0x94, 0x04),
                _ => sw(0x6d, 0x00),
            };
        };
        match (cmd, ef) {
            (GET_RESPONSE, Ef::Transparent(d)) => ok(&header(id, d.len(), 0, 0)),
            (GET_RESPONSE, Ef::LinearFixed { record_len, records }) => ok(&header(id, *record_len * records.len(), 1, *record_len)),
            (READ_BINARY, Ef::Transparent(d)) => {
                let off = (p1 << 8) | p2;
                let len = if p3 == 0 { d.len().saturating_sub(off) } else { p3 };
                match d.get(off..off + len) {
                    Some(s) => ok(s),
                    None => sw(0x67, 0x00),
                }
            }
            (READ_RECORD, Ef::LinearFixed { records, .. }) => match p1.checked_sub(1).and_then(|i| records.get(i)) {
                Some(r) if p2 == 4 => ok(r),
                Some(_) => sw(0x6b, 0x00),
                None => sw(0x6a, 0x83),
            },
            (UPDATE_BINARY, Ef::Transparent(d)) => {
                let off = (p1 << 8) | p2;
                match d.get_mut(off..off + data.len()) {
                    Some(dst) => {
                        dst.copy_from_slice(&data);
                        sw(144, 0)
                    }
                    None => sw(0x67, 0x00),
                }
            }
            (UPDATE_RECORD, Ef::LinearFixed { record_len, records }) => match p1.checked_sub(1).and_then(|i| records.get_mut(i)) {
                Some(r) if data.len() == *record_len => {
                    r.copy_from_slice(&data);
                    sw(144, 0)
                }
                Some(_) => sw(0x67, 0x00),
                None => sw(0x6a, 0x83),
            },
            // Wrong file structure for the command.
            (READ_BINARY | READ_RECORD | UPDATE_BINARY | UPDATE_RECORD, _) => sw(0x69, 0x81),
            _ => sw(0x6d, 0x00),
        }
    }
}

fn linear(record_len: usize, count: usize, filled: &[Vec<u8>]) -> Ef {
    let records = (0..count)
        .map(|i| {
            let mut r = vec![0xff; record_len];
            if let Some(f) = filled.get(i) {
                let n = f.len().min(record_len);
                r[..n].copy_from_slice(&f[..n]);
            }
            r
        })
        .collect();
    Ef::LinearFixed { record_len, records }
}

/// GSM 11.11 GET RESPONSE for an EF: RFU, size, file id, type EF, access conditions, status,
/// structure (0 transparent, 1 linear fixed) and record length.
fn header(id: u16, size: usize, structure: u8, record_len: usize) -> [u8; 15] {
    let [s0, s1] = (size as u16).to_be_bytes();
    let [f0, f1] = id.to_be_bytes();
    [0x00, 0x00, s0, s1, f0, f1, 0x04, 0x00, 0x11, 0xa0, 0xaa, 0x01, 0x02, structure, record_len as u8]
}

/// EF_ICCID holds the ICCID in swapped BCD, F-padded to 10 bytes.
fn iccid_bytes(iccid: &str) -> Vec<u8> {
    let nibbles: Vec<u8> = iccid.bytes().filter(u8::is_ascii_digit).map(|d| d - b'0').collect();
    let mut out: Vec<u8> = nibbles.chunks(2).map(|p| p[0] | (p.get(1).copied().unwrap_or(0xf) << 4)).collect();
    out.resize(10, 0xff);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iccid_and_msisdn_match_the_stock_sim() {
        let mut sim = SimFiles::new("89014103211118510720", "+15555215554");
        assert_eq!(sim.crsm("192,12258,0,0,15").as_deref(), Some("+CRSM: 144,0,0000000A2FE2040011A0AA01020000"));
        assert_eq!(sim.crsm("176,12258,0,0,10").as_deref(), Some("+CRSM: 144,0,98101430121181157002"));
        assert_eq!(sim.crsm("192,28480,0,0,15").as_deref(), Some("+CRSM: 144,0,000000806F40040011A0AA01020120"));
        let rec = sim.crsm("178,28480,1,4,32").expect("msisdn");
        assert!(rec.ends_with(&format!("07915155255155F4{}", "FF".repeat(6))), "{rec}");
        assert_eq!(sim.crsm("178,28480,9,4,32").as_deref(), Some("+CRSM: 106,131"));
        assert_eq!(sim.crsm("192,28486,0,0,15").as_deref(), Some("+CRSM: 148,4"));
    }

    #[test]
    fn records_can_be_updated() {
        let mut sim = SimFiles::new("8901", "123");
        let data = "41".repeat(32);
        assert_eq!(sim.crsm(&format!("220,28615,1,4,32,{data},")).as_deref(), Some("+CRSM: 144,0"));
        assert_eq!(sim.crsm("178,28615,1,4,32"), Some(format!("+CRSM: 144,0,{data}")));
    }
}
