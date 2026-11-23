//! SMS PDUs (3GPP TS 23.040) and the GSM 7-bit default alphabet (TS 23.038).
//!
//! Only what the modem needs: decoding the SMS-SUBMIT TPDUs the guest sends with `AT+CMGS`
//! and building SMS-DELIVER TPDUs for `+CMT`. Facts relied on:
//!
//! - A PDU handed over the AT interface in PDU mode starts with the SMSC address
//!   (`<len><toa><bcd...>`, `00` = none), then the TPDU. `AT+CMGS=<n>` counts TPDU octets only.
//! - First octet: TP-MTI in bits 0-1 (00 DELIVER, 01 SUBMIT), TP-MMS/TP-RD bit 2, TP-VPF bits
//!   3-4 (SUBMIT: 00 none, 10 relative 1 octet, 01 enhanced 7, 11 absolute 7), TP-SRR bit 5,
//!   TP-UDHI bit 6, TP-RP bit 7.
//! - Address: `<digit count><toa><BCD, low nibble first, F filler>`; toa 0x91 = international
//!   (`+`), 0x81 = unknown; TON 101 (toa 0xD0) is alphanumeric, packed GSM 7-bit, and the
//!   length counts semi-octets used.
//! - TP-UDL counts septets for GSM 7-bit (header septets included) and octets otherwise. With a
//!   user-data header the text starts at the next septet boundary after the header.
//! - Concatenation IEs: 0x00 (8-bit reference, 3 octets) and 0x08 (16-bit reference, 4 octets):
//!   `ref, total, seq`. A part carries at most 153 septets / 134 octets of text.
//! - TP-SCTS: seven swapped-BCD octets `YY MM DD hh mm ss tz`, tz in quarter hours with the
//!   sign in bit 3 of the octet.

/// GSM 03.38 default alphabet; index = septet. 0x1B is the escape to [`GSM7_EXT`].
const GSM7_BASIC: [char; 128] = [
    '@', '£', '$', '¥', 'è', 'é', 'ù', 'ì', 'ò', 'Ç', '\n', 'Ø', 'ø', '\r', 'Å', 'å', //
    'Δ', '_', 'Φ', 'Γ', 'Λ', 'Ω', 'Π', 'Ψ', 'Σ', 'Θ', 'Ξ', '\u{1b}', 'Æ', 'æ', 'ß', 'É', //
    ' ', '!', '"', '#', '¤', '%', '&', '\'', '(', ')', '*', '+', ',', '-', '.', '/', //
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', ':', ';', '<', '=', '>', '?', //
    '¡', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', //
    'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z', 'Ä', 'Ö', 'Ñ', 'Ü', '§', //
    '¿', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', //
    'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z', 'ä', 'ö', 'ñ', 'ü', 'à', //
];

const ESC: u8 = 0x1b;

/// The default extension table (reached with ESC).
const GSM7_EXT: [(u8, char); 10] = [
    (0x0a, '\u{0c}'),
    (0x14, '^'),
    (0x28, '{'),
    (0x29, '}'),
    (0x2f, '\\'),
    (0x3c, '['),
    (0x3d, '~'),
    (0x3e, ']'),
    (0x40, '|'),
    (0x65, '€'),
];

/// Text encoding of a message's user data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmsEncoding {
    Gsm7,
    /// 8-bit data; the decoded text is the payload in hex.
    EightBit,
    Ucs2,
}

/// Septets for `text`, or `None` if a character is outside the default alphabet and its
/// extension table.
pub fn gsm7_encode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len());
    for c in text.chars() {
        if c != '\u{1b}'
            && let Some(i) = GSM7_BASIC.iter().position(|&b| b == c)
        {
            out.push(i as u8);
        } else if let Some(&(code, _)) = GSM7_EXT.iter().find(|&&(_, e)| e == c) {
            out.extend([ESC, code]);
        } else {
            return None;
        }
    }
    Some(out)
}

pub fn gsm7_decode(septets: &[u8]) -> String {
    let mut out = String::with_capacity(septets.len());
    let mut it = septets.iter();
    while let Some(&s) = it.next() {
        if s == ESC {
            match it.next() {
                Some(&e) => match GSM7_EXT.iter().find(|&&(code, _)| code == e) {
                    Some(&(_, c)) => out.push(c),
                    // Unknown extension: show the base character (TS 23.038 6.2.1.1).
                    None => out.push(GSM7_BASIC[usize::from(e & 0x7f)]),
                },
                None => out.push(' '),
            }
        } else {
            out.push(GSM7_BASIC[usize::from(s & 0x7f)]);
        }
    }
    out
}

/// Packs septets LSB first, 8 septets into 7 octets.
pub fn pack_septets(septets: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; (septets.len() * 7).div_ceil(8)];
    for (i, &s) in septets.iter().enumerate() {
        let bit = i * 7;
        let (byte, shift) = (bit / 8, bit % 8);
        out[byte] |= (s & 0x7f) << shift;
        if shift > 1 {
            out[byte + 1] |= (s & 0x7f) >> (8 - shift);
        }
    }
    out
}

pub fn unpack_septets(octets: &[u8], count: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let bit = i * 7;
        let (byte, shift) = (bit / 8, bit % 8);
        let Some(&lo) = octets.get(byte) else { break };
        let mut v = u16::from(lo) >> shift;
        if shift > 1 {
            v |= u16::from(octets.get(byte + 1).copied().unwrap_or(0)) << (8 - shift);
        }
        out.push((v & 0x7f) as u8);
    }
    out
}

/// Encodes an address field (`<len><toa><value>`): digits (optionally `+`-prefixed, with `*`
/// and `#`) as BCD, anything else as an alphanumeric GSM 7-bit address of up to 11 characters.
pub fn encode_address(addr: &str) -> Vec<u8> {
    let (intl, digits) = match addr.strip_prefix('+') {
        Some(rest) => (true, rest),
        None => (false, addr),
    };
    let numeric = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit() || b == b'*' || b == b'#');
    if numeric {
        let nibbles: Vec<u8> = digits
            .bytes()
            .map(|b| match b {
                b'*' => 0xa,
                b'#' => 0xb,
                d => d - b'0',
            })
            .collect();
        let mut out = vec![nibbles.len() as u8, if intl { 0x91 } else { 0x81 }];
        out.extend(bcd_pack(&nibbles));
        return out;
    }
    let text: String = addr.chars().take(11).collect();
    let septets = gsm7_encode(&text).unwrap_or_else(|| {
        let replaced: String = text.chars().map(|c| if gsm7_encode(&c.to_string()).is_some() { c } else { '?' }).collect();
        gsm7_encode(&replaced).unwrap_or_default()
    });
    let packed = pack_septets(&septets);
    let semi_octets = (septets.len() * 7).div_ceil(4);
    let mut out = vec![semi_octets as u8, 0xd0];
    out.extend(packed);
    out
}

fn bcd_pack(nibbles: &[u8]) -> Vec<u8> {
    nibbles.chunks(2).map(|p| p[0] | (p.get(1).copied().unwrap_or(0xf) << 4)).collect()
}

/// Decodes an address field at the start of `b`; returns the address and the bytes used.
pub fn decode_address(b: &[u8]) -> Result<(String, usize), PduError> {
    let len = usize::from(*b.first().ok_or(PduError::Truncated)?);
    let toa = *b.get(1).ok_or(PduError::Truncated)?;
    let octets = len.div_ceil(2);
    let value = b.get(2..2 + octets).ok_or(PduError::Truncated)?;
    if toa & 0x70 == 0x50 {
        let septets = unpack_septets(value, len * 4 / 7);
        return Ok((gsm7_decode(&septets), 2 + octets));
    }
    let mut s = String::new();
    if toa & 0x70 == 0x10 {
        s.push('+');
    }
    for i in 0..len {
        let nib = (value[i / 2] >> (4 * (i % 2))) & 0xf;
        s.push(match nib {
            0..=9 => char::from(b'0' + nib),
            0xa => '*',
            0xb => '#',
            0xc => 'a',
            0xd => 'b',
            0xe => 'c',
            _ => break,
        });
    }
    Ok((s, 2 + octets))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PduError {
    Truncated,
    NotSubmit,
    NotDeliver,
}

impl std::fmt::Display for PduError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PduError::Truncated => "PDU is truncated",
            PduError::NotSubmit => "PDU is not an SMS-SUBMIT",
            PduError::NotDeliver => "PDU is not an SMS-DELIVER",
        })
    }
}

/// One part of a concatenated message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Concat {
    pub reference: u16,
    pub total: u8,
    pub seq: u8,
}

/// A decoded SMS-SUBMIT.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submit {
    pub to: String,
    pub text: String,
    pub encoding: SmsEncoding,
    pub concat: Option<Concat>,
}

/// Decodes `smsc + TPDU` as sent after `AT+CMGS`.
pub fn decode_submit(pdu: &[u8]) -> Result<Submit, PduError> {
    let smsc_len = usize::from(*pdu.first().ok_or(PduError::Truncated)?);
    let t = pdu.get(1 + smsc_len..).ok_or(PduError::Truncated)?;
    let first = *t.first().ok_or(PduError::Truncated)?;
    if first & 0x03 != 0x01 {
        return Err(PduError::NotSubmit);
    }
    // first, MR
    let (to, used) = decode_address(t.get(2..).ok_or(PduError::Truncated)?)?;
    let mut i = 2 + used;
    let _pid = *t.get(i).ok_or(PduError::Truncated)?;
    let dcs = *t.get(i + 1).ok_or(PduError::Truncated)?;
    i += 2;
    i += match (first >> 3) & 0x03 {
        0b00 => 0,
        0b10 => 1,
        _ => 7,
    };
    let udl = usize::from(*t.get(i).ok_or(PduError::Truncated)?);
    let ud = t.get(i + 1..).ok_or(PduError::Truncated)?;
    let (text, encoding, concat) = decode_user_data(first & 0x40 != 0, dcs, udl, ud)?;
    Ok(Submit { to, text, encoding, concat })
}

/// A decoded SMS-DELIVER (used by tests to check what the guest receives).
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deliver {
    pub from: String,
    pub text: String,
    pub encoding: SmsEncoding,
    pub concat: Option<Concat>,
    pub scts: [u8; 7],
}

#[cfg(test)]
pub fn decode_deliver(pdu: &[u8]) -> Result<Deliver, PduError> {
    let smsc_len = usize::from(*pdu.first().ok_or(PduError::Truncated)?);
    let t = pdu.get(1 + smsc_len..).ok_or(PduError::Truncated)?;
    let first = *t.first().ok_or(PduError::Truncated)?;
    if first & 0x03 != 0x00 {
        return Err(PduError::NotDeliver);
    }
    let (from, used) = decode_address(t.get(1..).ok_or(PduError::Truncated)?)?;
    let i = 1 + used;
    let dcs = *t.get(i + 1).ok_or(PduError::Truncated)?;
    let scts: [u8; 7] = t.get(i + 2..i + 9).ok_or(PduError::Truncated)?.try_into().map_err(|_| PduError::Truncated)?;
    let udl = usize::from(*t.get(i + 9).ok_or(PduError::Truncated)?);
    let ud = t.get(i + 10..).ok_or(PduError::Truncated)?;
    let (text, encoding, concat) = decode_user_data(first & 0x40 != 0, dcs, udl, ud)?;
    Ok(Deliver { from, text, encoding, concat, scts })
}

fn alphabet(dcs: u8) -> SmsEncoding {
    let by_bits = |d: u8| match (d >> 2) & 0x03 {
        1 => SmsEncoding::EightBit,
        2 => SmsEncoding::Ucs2,
        _ => SmsEncoding::Gsm7,
    };
    match dcs >> 4 {
        // General data coding (and the automatic-deletion group); bit 5 = compressed.
        0x0..=0x7 if dcs & 0x20 != 0 => SmsEncoding::EightBit,
        0x0..=0x7 => by_bits(dcs),
        0xc | 0xd => SmsEncoding::Gsm7,
        0xe => SmsEncoding::Ucs2,
        0xf if dcs & 0x04 != 0 => SmsEncoding::EightBit,
        0xf => SmsEncoding::Gsm7,
        _ => SmsEncoding::EightBit,
    }
}

type UserData = (String, SmsEncoding, Option<Concat>);

fn decode_user_data(udhi: bool, dcs: u8, udl: usize, ud: &[u8]) -> Result<UserData, PduError> {
    let encoding = alphabet(dcs);
    let mut header_len = 0usize;
    let mut concat = None;
    if udhi {
        let udhl = usize::from(*ud.first().ok_or(PduError::Truncated)?);
        let header = ud.get(1..1 + udhl).ok_or(PduError::Truncated)?;
        header_len = 1 + udhl;
        let mut j = 0;
        while j + 1 < header.len() {
            let (iei, len) = (header[j], usize::from(header[j + 1]));
            let data = header.get(j + 2..j + 2 + len).ok_or(PduError::Truncated)?;
            match (iei, len) {
                (0x00, 3) => concat = Some(Concat { reference: u16::from(data[0]), total: data[1], seq: data[2] }),
                (0x08, 4) => concat = Some(Concat { reference: u16::from_be_bytes([data[0], data[1]]), total: data[2], seq: data[3] }),
                _ => {}
            }
            j += 2 + len;
        }
    }
    let text = match encoding {
        SmsEncoding::Gsm7 => {
            let septets = unpack_septets(ud, udl);
            let skip = (header_len * 8).div_ceil(7);
            gsm7_decode(septets.get(skip..).unwrap_or(&[]))
        }
        SmsEncoding::Ucs2 => {
            let body = ud.get(header_len..udl.min(ud.len())).unwrap_or(&[]);
            let units: Vec<u16> = body.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect();
            String::from_utf16_lossy(&units)
        }
        SmsEncoding::EightBit => hex(ud.get(header_len..udl.min(ud.len())).unwrap_or(&[])),
    };
    Ok((text, encoding, concat))
}

/// Builds the SMS-DELIVER TPDUs (no SMSC prefix) carrying `text` from `from`: GSM 7-bit when
/// every character has a septet, UCS2 otherwise, split into concatenated parts (8-bit
/// reference `reference`) when it does not fit one PDU. `scts` is the service-centre time
/// stamp from [`scts`].
pub fn build_deliver(from: &str, text: &str, scts: [u8; 7], reference: u8) -> Vec<Vec<u8>> {
    let oa = encode_address(from);
    let header = |udhi: bool, dcs: u8| {
        let mut p = vec![if udhi { 0x44 } else { 0x04 }];
        p.extend_from_slice(&oa);
        p.extend([0x00, dcs]);
        p.extend_from_slice(&scts);
        p
    };
    let concat_ie = |total: usize, seq: usize| [0x05, 0x00, 0x03, reference, total as u8, seq as u8];

    if let Some(septets) = gsm7_encode(text) {
        if septets.len() <= 160 {
            let mut p = header(false, 0x00);
            p.push(septets.len() as u8);
            p.extend(pack_septets(&septets));
            return vec![p];
        }
        let parts = split_keep_pairs(&septets, 153, |s| s == ESC);
        let total = parts.len();
        return parts
            .iter()
            .enumerate()
            .map(|(i, part)| {
                let udh = concat_ie(total, i + 1);
                // The 6-octet header takes 7 septets (48 bits + 1 fill bit).
                let mut all = vec![0u8; 7];
                all.extend_from_slice(part);
                let mut ud = pack_septets(&all);
                ud[..udh.len()].copy_from_slice(&udh);
                let mut p = header(true, 0x00);
                p.push(all.len() as u8);
                p.extend(ud);
                p
            })
            .collect();
    }

    let units: Vec<u16> = text.encode_utf16().collect();
    let to_bytes = |u: &[u16]| u.iter().flat_map(|x| x.to_be_bytes()).collect::<Vec<u8>>();
    if units.len() <= 70 {
        let mut p = header(false, 0x08);
        p.push((units.len() * 2) as u8);
        p.extend(to_bytes(&units));
        return vec![p];
    }
    let parts = split_keep_pairs(&units, 67, |u| (0xd800..0xdc00).contains(&u));
    let total = parts.len();
    parts
        .iter()
        .enumerate()
        .map(|(i, part)| {
            let mut ud = concat_ie(total, i + 1).to_vec();
            ud.extend(to_bytes(part));
            let mut p = header(true, 0x08);
            p.push(ud.len() as u8);
            p.extend(ud);
            p
        })
        .collect()
}

/// Splits into chunks of at most `max`, never ending a chunk on an element that must stay
/// with its successor (an ESC septet, a high surrogate).
fn split_keep_pairs<T: Copy>(items: &[T], max: usize, leads: impl Fn(T) -> bool) -> Vec<Vec<T>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let mut end = (i + max).min(items.len());
        if end < items.len() && leads(items[end - 1]) {
            end -= 1;
        }
        out.push(items[i..end].to_vec());
        i = end;
    }
    out
}

/// TP-SCTS for `unix_secs` in a zone `tz_quarters` quarter hours east of UTC.
pub fn scts(unix_secs: u64, tz_quarters: i32) -> [u8; 7] {
    let local = unix_secs as i64 + i64::from(tz_quarters) * 15 * 60;
    let (y, mo, d, h, mi, s) = civil(local);
    let swap = |v: u32| {
        let v = v % 100;
        (((v % 10) << 4) | (v / 10)) as u8
    };
    let mut tz = swap(tz_quarters.unsigned_abs());
    if tz_quarters < 0 {
        tz |= 0x08;
    }
    [swap(y as u32), swap(mo), swap(d), swap(h), swap(mi), swap(s), tz]
}

/// Broken-down UTC time `(year, month, day, hour, minute, second)` for seconds since the epoch.
pub fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d, (rem / 3600) as u32, (rem % 3600 / 60) as u32, (rem % 60) as u32)
}

pub fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    b.iter().fold(String::with_capacity(b.len() * 2), |mut s, x| {
        let _ = write!(s, "{x:02X}");
        s
    })
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn septet_packing_matches_the_spec_example() {
        // "hellohello" -> E8329BFD4697D9EC37 (TS 23.040 / common reference vector).
        let s = gsm7_encode("hellohello").expect("gsm7");
        assert_eq!(hex(&pack_septets(&s)), "E8329BFD4697D9EC37");
        assert_eq!(gsm7_decode(&unpack_septets(&pack_septets(&s), 10)), "hellohello");
    }

    #[test]
    fn extension_characters_round_trip() {
        let s = gsm7_encode("[€]{|}").expect("gsm7");
        assert_eq!(s.len(), 12);
        assert_eq!(gsm7_decode(&s), "[€]{|}");
        assert!(gsm7_encode("😀").is_none());
    }

    #[test]
    fn addresses_round_trip() {
        for a in ["+15555215554", "5551234", "*123#", "Google"] {
            let enc = encode_address(a);
            let (dec, used) = decode_address(&enc).expect("address");
            assert_eq!((dec.as_str(), used), (a, enc.len()));
        }
        assert_eq!(hex(&encode_address("5551234")), "0781551532F4");
    }

    #[test]
    fn scts_is_swapped_bcd() {
        // 2024-11-05 17:01:32 UTC, zone -8h (-32 quarters).
        let t = 1_730_826_092;
        assert_eq!(civil(t as i64), (2024, 11, 5, 17, 1, 32));
        assert_eq!(scts(t, 0), [0x42, 0x11, 0x50, 0x71, 0x10, 0x23, 0x00]);
        assert_eq!(scts(t, -32)[3], 0x90); // 09h local
        assert_eq!(scts(t, -32)[6], 0x23 | 0x08);
    }

    #[test]
    fn long_messages_split_without_breaking_pairs() {
        let text = "€".repeat(100); // 200 septets
        let parts = build_deliver("123", &text, [0; 7], 7);
        assert_eq!(parts.len(), 2);
        let joined: String = parts
            .iter()
            .map(|p| {
                let mut with_smsc = vec![0];
                with_smsc.extend_from_slice(p);
                decode_deliver(&with_smsc).expect("deliver").text
            })
            .collect();
        assert_eq!(joined, text);

        let emoji = "😀".repeat(40); // 80 UTF-16 units
        let parts = build_deliver("123", &emoji, [0; 7], 8);
        assert_eq!(parts.len(), 2);
        let joined: String = parts
            .iter()
            .map(|p| {
                let mut with_smsc = vec![0];
                with_smsc.extend_from_slice(p);
                decode_deliver(&with_smsc).expect("deliver").text
            })
            .collect();
        assert_eq!(joined, emoji);
    }
}
