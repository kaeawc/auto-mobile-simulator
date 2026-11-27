//! Binary property lists (`bplist00`), read and written in process: an iOS app's defaults as
//! cfprefsd stores them (`Library/Preferences/<domain>.plist`), the container metadata that says
//! which app or app group a container is, and the requests the in-device helper takes. sim-core's
//! `plist` reads the flat XML dictionaries of the simulator's settings files; this is the binary
//! format, with every type nested to any depth.
//!
//! The reader checks every offset and length against the bytes it has, refuses a reference cycle
//! and caps the nodes a file with shared references can expand to, so a damaged or hostile file
//! is an error, never a panic or a runaway.

use std::collections::BTreeMap;

use super::{Date, Entries, PrefValue};

/// One property-list value.
#[derive(Clone, Debug, PartialEq)]
pub enum Plist {
    Bool(bool),
    /// Up to 128 bits: a plist integer past `i64` is stored in 16 bytes (Swift's `UInt64.max`).
    Integer(i128),
    Real(f64),
    /// A real written in 4 bytes, so a `float` write stays a 32-bit `CFNumber`. Read as [`Plist::Real`].
    Real32(f32),
    /// Seconds since 2001-01-01T00:00:00Z.
    Date(f64),
    Data(Vec<u8>),
    String(String),
    Array(Vec<Plist>),
    /// In file order; a key appears once.
    Dict(Vec<(String, Plist)>),
    /// What no preference type holds (a keyed archive's `uid`, `null`, a `set`, a dictionary
    /// with a key that isn't a string): the format's name for it and its text. Read-only.
    Other {
        native: &'static str,
        raw: String,
    },
}

impl Plist {
    /// The value of `key` in a dictionary.
    pub fn get(&self, key: &str) -> Option<&Plist> {
        match self {
            Plist::Dict(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Plist::String(s) => Some(s),
            _ => None,
        }
    }
}

/// Seconds from the Unix epoch to the plist epoch (2001-01-01T00:00:00Z).
const PLIST_EPOCH: i64 = 978_307_200;

/// The most nodes one file may expand to through shared references.
const MAX_NODES: usize = 1 << 22;
/// The most bytes one file may decode to: string and data payloads as they are materialised, plus
/// [`NODE_COST`] per value. A shared reference is decoded once per use, so a small file can ask
/// for far more than its own size; this stops it before the allocation.
const MAX_DECODED_BYTES: usize = 64 << 20;
/// What each decoded value costs against [`MAX_DECODED_BYTES`], on top of its payload.
const NODE_COST: usize = std::mem::size_of::<Plist>();
/// The deepest nesting read.
const MAX_DEPTH: usize = 512;

/// Whether `bytes` start like a binary property list.
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes.starts_with(b"bplist00")
}

/// Reads a binary property list.
pub fn decode(bytes: &[u8]) -> Result<Plist, String> {
    if !is_binary(bytes) || bytes.len() < 8 + 32 {
        return Err("not a binary property list (bplist00)".into());
    }
    let trailer = &bytes[bytes.len() - 32..];
    let (offset_size, ref_size) = (usize::from(trailer[6]), usize::from(trailer[7]));
    let count = be(&trailer[8..16]);
    let top = be(&trailer[16..24]);
    let table = be(&trailer[24..32]);
    let body = bytes.len() as u64 - 32;
    let bad = |what: &str| Err(format!("damaged binary property list: {what}"));
    if !(1..=8).contains(&offset_size) || !(1..=8).contains(&ref_size) {
        return bad("offset or reference size");
    }
    if count == 0
        || top >= count
        || table < 8
        || count.checked_mul(offset_size as u64).and_then(|n| n.checked_add(table)).is_none_or(|end| end > body)
    {
        return bad("object table");
    }
    let mut reader = Reader {
        bytes: &bytes[..body as usize],
        offset_size,
        ref_size,
        count,
        table: table as usize,
        nodes: 0,
        decoded: 0,
        stack: Vec::new(),
    };
    reader.object(top)
}

fn be(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0, |n, b| (n << 8) | u64::from(*b))
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset_size: usize,
    ref_size: usize,
    count: u64,
    table: usize,
    nodes: usize,
    /// Bytes decoded so far, against [`MAX_DECODED_BYTES`].
    decoded: usize,
    /// The objects being read, outermost first: a reference back to one is a cycle.
    stack: Vec<u64>,
}

impl Reader<'_> {
    fn slice(&self, at: usize, len: usize) -> Result<&[u8], String> {
        at.checked_add(len)
            .and_then(|end| self.bytes.get(at..end))
            .ok_or_else(|| format!("damaged binary property list: {len} bytes at {at}"))
    }

    fn offset(&self, object: u64) -> Result<usize, String> {
        let at = self.table + object as usize * self.offset_size;
        let offset = be(self.slice(at, self.offset_size)?) as usize;
        if offset < 8 || offset >= self.bytes.len() {
            return Err(format!("damaged binary property list: object {object} at {offset}"));
        }
        Ok(offset)
    }

    /// A count after a marker: the marker's low nibble, or the integer object that follows it.
    fn length(&self, at: usize, low: u8) -> Result<(usize, usize), String> {
        if low != 0xF {
            return Ok((usize::from(low), at + 1));
        }
        let marker = self.slice(at + 1, 1)?[0];
        if marker >> 4 != 0x1 || marker & 0xF > 3 {
            return Err(format!("damaged binary property list: length at {at}"));
        }
        let size = 1usize << (marker & 0xF);
        let n = be(self.slice(at + 2, size)?);
        let n = usize::try_from(n).ok().filter(|n| *n <= self.bytes.len()).ok_or("damaged binary property list: length")?;
        Ok((n, at + 2 + size))
    }

    fn refs(&self, at: usize, n: usize) -> Result<Vec<u64>, String> {
        let bytes = self.slice(at, n.checked_mul(self.ref_size).ok_or("damaged binary property list: references")?)?;
        bytes
            .chunks(self.ref_size)
            .map(|r| {
                let r = be(r);
                if r < self.count { Ok(r) } else { Err(format!("damaged binary property list: reference {r} of {}", self.count)) }
            })
            .collect()
    }

    /// Charges `bytes` of decoded value to the budget, before they are allocated.
    fn charge(&mut self, bytes: usize) -> Result<(), String> {
        self.decoded = self.decoded.saturating_add(bytes);
        if self.decoded > MAX_DECODED_BYTES {
            return Err(format!("binary property list decodes to more than {} MiB", MAX_DECODED_BYTES >> 20));
        }
        Ok(())
    }

    fn object(&mut self, object: u64) -> Result<Plist, String> {
        self.nodes += 1;
        if self.nodes > MAX_NODES || self.stack.len() >= MAX_DEPTH {
            return Err("binary property list too large or too deep".into());
        }
        if self.stack.contains(&object) {
            return Err("damaged binary property list: a reference cycle".into());
        }
        self.charge(NODE_COST)?;
        let at = self.offset(object)?;
        let marker = self.bytes[at];
        let low = marker & 0xF;
        let value = match marker >> 4 {
            0x0 => match low {
                0x8 => Plist::Bool(false),
                0x9 => Plist::Bool(true),
                0x0 => Plist::Other { native: "null", raw: String::new() },
                _ => Plist::Other { native: "fill", raw: String::new() },
            },
            0x1 if low <= 4 => {
                let size = 1usize << low;
                let bytes = self.slice(at + 1, size)?;
                let n = bytes.iter().fold(0u128, |n, b| (n << 8) | u128::from(*b));
                Plist::Integer(match size {
                    // 1, 2 and 4 bytes are unsigned; 8 and 16 two's complement.
                    8 => i128::from(n as u64 as i64),
                    16 => n as i128,
                    _ => n as i128,
                })
            }
            0x2 if low == 2 => Plist::Real(f64::from(f32::from_be_bytes(self.slice(at + 1, 4)?.try_into().map_err(|_| "real")?))),
            0x2 if low == 3 => Plist::Real(f64::from_be_bytes(self.slice(at + 1, 8)?.try_into().map_err(|_| "real")?)),
            0x3 if low == 3 => Plist::Date(f64::from_be_bytes(self.slice(at + 1, 8)?.try_into().map_err(|_| "date")?)),
            0x4 => {
                let (n, from) = self.length(at, low)?;
                self.charge(n)?;
                Plist::Data(self.slice(from, n)?.to_vec())
            }
            0x5 => {
                let (n, from) = self.length(at, low)?;
                self.charge(n.saturating_mul(2))?;
                // ASCII by the format; a byte past it is read as Latin-1 rather than refused.
                Plist::String(self.slice(from, n)?.iter().map(|b| char::from(*b)).collect())
            }
            0x6 => {
                let (n, from) = self.length(at, low)?;
                self.charge(n.saturating_mul(3))?;
                let units: Vec<u16> =
                    self.slice(from, n.checked_mul(2).ok_or("string")?)?.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                Plist::String(String::from_utf16(&units).map_err(|_| "damaged binary property list: a UTF-16 string")?)
            }
            0x8 => Plist::Other { native: "uid", raw: be(self.slice(at + 1, usize::from(low) + 1)?).to_string() },
            0xA | 0xC => {
                let (n, from) = self.length(at, low)?;
                let refs = self.refs(from, n)?;
                self.stack.push(object);
                let items = refs.into_iter().map(|r| self.object(r)).collect::<Result<Vec<_>, _>>();
                self.stack.pop();
                if marker >> 4 == 0xA {
                    Plist::Array(items?)
                } else {
                    Plist::Other { native: "set", raw: format!("{} items", items?.len()) }
                }
            }
            0xD => {
                let (n, from) = self.length(at, low)?;
                let keys = self.refs(from, n)?;
                let values = self.refs(from + n * self.ref_size, n)?;
                self.stack.push(object);
                let mut entries: Vec<(String, Plist)> = Vec::with_capacity(n);
                let mut keyed = true;
                for (k, v) in keys.into_iter().zip(values) {
                    let key = self.object(k)?;
                    let value = self.object(v)?;
                    match key {
                        Plist::String(key) => match entries.iter_mut().find(|(k, _)| *k == key) {
                            Some(entry) => entry.1 = value,
                            None => entries.push((key, value)),
                        },
                        _ => keyed = false,
                    }
                }
                self.stack.pop();
                if keyed {
                    Plist::Dict(entries)
                } else {
                    Plist::Other { native: "dictionary", raw: format!("{n} entries, not every key a string") }
                }
            }
            _ => return Err(format!("damaged binary property list: marker {marker:#04x} at {at}")),
        };
        Ok(value)
    }
}

/// Writes `value` as a binary property list.
pub fn encode(value: &Plist) -> Result<Vec<u8>, String> {
    let mut flat = Vec::new();
    flatten(value, &mut flat)?;
    let ref_size = size_for(flat.len() as u64 - 1);
    let mut out = b"bplist00".to_vec();
    let mut offsets = Vec::with_capacity(flat.len());
    for node in &flat {
        offsets.push(out.len() as u64);
        match node {
            Flat::Key(s) => string(&mut out, s),
            Flat::Leaf(v) => leaf(&mut out, v),
            Flat::Array(items) => {
                header(&mut out, 0xA, items.len());
                items.iter().for_each(|r| push_be(&mut out, *r as u64, ref_size));
            }
            Flat::Dict(keys, values) => {
                header(&mut out, 0xD, keys.len());
                keys.iter().chain(values).for_each(|r| push_be(&mut out, *r as u64, ref_size));
            }
        }
    }
    let table = out.len() as u64;
    let offset_size = size_for(table);
    offsets.iter().for_each(|o| push_be(&mut out, *o, offset_size));
    out.extend_from_slice(&[0; 6]);
    out.push(offset_size as u8);
    out.push(ref_size as u8);
    out.extend_from_slice(&(flat.len() as u64).to_be_bytes());
    out.extend_from_slice(&0u64.to_be_bytes());
    out.extend_from_slice(&table.to_be_bytes());
    Ok(out)
}

enum Flat<'a> {
    Key(&'a str),
    Leaf(&'a Plist),
    Array(Vec<usize>),
    Dict(Vec<usize>, Vec<usize>),
}

/// Lays `value` out as objects, `value` first (the top object is 0); returns its index.
fn flatten<'a>(value: &'a Plist, out: &mut Vec<Flat<'a>>) -> Result<usize, String> {
    let at = out.len();
    match value {
        Plist::Array(items) => {
            out.push(Flat::Array(Vec::new()));
            let refs = items.iter().map(|v| flatten(v, out)).collect::<Result<Vec<_>, _>>()?;
            out[at] = Flat::Array(refs);
        }
        Plist::Dict(entries) => {
            out.push(Flat::Dict(Vec::new(), Vec::new()));
            let mut keys = Vec::with_capacity(entries.len());
            for (k, _) in entries {
                keys.push(out.len());
                out.push(Flat::Key(k));
            }
            let values = entries.iter().map(|(_, v)| flatten(v, out)).collect::<Result<Vec<_>, _>>()?;
            out[at] = Flat::Dict(keys, values);
        }
        Plist::Other { native, .. } => return Err(format!("a plist `{native}` can't be written")),
        leaf => out.push(Flat::Leaf(leaf)),
    }
    Ok(at)
}

/// The fewest bytes (1, 2, 4 or 8) that hold `n`.
fn size_for(n: u64) -> usize {
    match n {
        0..=0xFF => 1,
        0x100..=0xFFFF => 2,
        0x1_0000..=0xFFFF_FFFF => 4,
        _ => 8,
    }
}

fn push_be(out: &mut Vec<u8>, n: u64, size: usize) {
    out.extend_from_slice(&n.to_be_bytes()[8 - size..]);
}

/// A marker with a count: in its low nibble, or as an integer object after it.
fn header(out: &mut Vec<u8>, kind: u8, n: usize) {
    if n < 15 {
        out.push(kind << 4 | n as u8);
    } else {
        out.push(kind << 4 | 0xF);
        integer(out, n as i128);
    }
}

fn integer(out: &mut Vec<u8>, n: i128) {
    match n {
        0..=0xFF => out.extend_from_slice(&[0x10, n as u8]),
        0x100..=0xFFFF => {
            out.push(0x11);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        0x1_0000..=0xFFFF_FFFF => {
            out.push(0x12);
            out.extend_from_slice(&(n as u32).to_be_bytes());
        }
        _ => match i64::try_from(n) {
            Ok(n) => {
                out.push(0x13);
                out.extend_from_slice(&n.to_be_bytes());
            }
            Err(_) => {
                out.push(0x14);
                out.extend_from_slice(&n.to_be_bytes());
            }
        },
    }
}

fn string(out: &mut Vec<u8>, s: &str) {
    if s.is_ascii() {
        header(out, 0x5, s.len());
        out.extend_from_slice(s.as_bytes());
    } else {
        let units: Vec<u16> = s.encode_utf16().collect();
        header(out, 0x6, units.len());
        units.iter().for_each(|u| out.extend_from_slice(&u.to_be_bytes()));
    }
}

fn leaf(out: &mut Vec<u8>, value: &Plist) {
    match value {
        Plist::Bool(b) => out.push(if *b { 0x09 } else { 0x08 }),
        Plist::Integer(n) => integer(out, *n),
        Plist::Real(f) => {
            out.push(0x23);
            out.extend_from_slice(&f.to_be_bytes());
        }
        Plist::Real32(f) => {
            out.push(0x22);
            out.extend_from_slice(&f.to_be_bytes());
        }
        Plist::Date(secs) => {
            out.push(0x33);
            out.extend_from_slice(&secs.to_be_bytes());
        }
        Plist::Data(bytes) => {
            header(out, 0x4, bytes.len());
            out.extend_from_slice(bytes);
        }
        Plist::String(s) => string(out, s),
        // Containers and `Other` never reach here (`flatten`).
        Plist::Array(_) | Plist::Dict(_) | Plist::Other { .. } => {}
    }
}

/// A plist date as a [`Date`]; `None` for one that isn't a finite instant.
pub fn date_from_plist(secs: f64) -> Option<Date> {
    if !secs.is_finite() || secs.abs() > 1e15 {
        return None;
    }
    let whole = secs.floor();
    let mut nanos = ((secs - whole) * 1e9).round() as u32;
    let mut whole = whole as i64;
    if nanos >= 1_000_000_000 {
        whole += 1;
        nanos -= 1_000_000_000;
    }
    Some(Date::from_unix(whole + PLIST_EPOCH, nanos))
}

/// A [`Date`] as a plist date: seconds since 2001 in a double, so it keeps about a microsecond.
pub fn date_to_plist(date: Date) -> f64 {
    (date.unix_secs() - PLIST_EPOCH) as f64 + f64::from(date.subsec_nanos()) / 1e9
}

/// A plist value as an iOS store reads it: integers as `long` (or `unsupported` past 64 bits),
/// reals as `double`, and what no type holds as `unsupported`.
pub fn to_pref(value: &Plist) -> PrefValue {
    match value {
        Plist::Bool(b) => PrefValue::Bool(*b),
        Plist::Integer(n) => PrefValue::ios_integer(*n),
        Plist::Real(f) => PrefValue::ios_real(*f),
        Plist::Real32(f) => PrefValue::ios_real(f64::from(*f)),
        Plist::Date(secs) => {
            date_from_plist(*secs).map_or_else(|| PrefValue::Unsupported { native: "date".into(), raw: secs.to_string() }, PrefValue::Date)
        }
        Plist::Data(bytes) => PrefValue::Data(bytes.clone()),
        Plist::String(s) => PrefValue::String(s.clone()),
        Plist::Array(items) => PrefValue::Array(items.iter().map(to_pref).collect()),
        Plist::Dict(entries) => PrefValue::Dictionary(entries.iter().map(|(k, v)| (k.clone(), to_pref(v))).collect::<BTreeMap<_, _>>()),
        Plist::Other { native, raw } => PrefValue::Unsupported { native: (*native).to_owned(), raw: raw.clone() },
    }
}

/// A preference value as the plist an iOS store holds: `int` and `long` an integer, `float` a
/// 4-byte real and `double` an 8-byte one. A `string_set` or `unsupported` value has no plist
/// form (the API refuses both for iOS stores before a backend sees them).
pub fn from_pref(value: &PrefValue) -> Result<Plist, String> {
    Ok(match value {
        PrefValue::String(s) => Plist::String(s.clone()),
        PrefValue::Int(n) => Plist::Integer(i128::from(*n)),
        PrefValue::Long(n) => Plist::Integer(i128::from(*n)),
        PrefValue::Float(f) => Plist::Real32(*f),
        PrefValue::Double(f) => Plist::Real(*f),
        PrefValue::Bool(b) => Plist::Bool(*b),
        PrefValue::Date(d) => Plist::Date(date_to_plist(*d)),
        PrefValue::Data(bytes) => Plist::Data(bytes.clone()),
        PrefValue::Array(items) => Plist::Array(items.iter().map(from_pref).collect::<Result<_, _>>()?),
        PrefValue::Dictionary(map) => Plist::Dict(map.iter().map(|(k, v)| Ok((k.clone(), from_pref(v)?))).collect::<Result<_, String>>()?),
        other => return Err(format!("a `{}` has no property-list form", other.ty().as_str())),
    })
}

/// A defaults file's keys and values: its top object is a dictionary.
pub fn entries(bytes: &[u8]) -> Result<Entries, String> {
    match decode(bytes)? {
        Plist::Dict(entries) => Ok(entries.iter().map(|(k, v)| (k.clone(), to_pref(v))).collect()),
        _ => Err("the property list isn't a dictionary".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Written by cfprefsd on iOS 27.2 for a test app: one key of every type an iOS store holds.
    const EVERY_TYPE: &[u8] = include_bytes!("fixtures/cfprefsd-every-type.plist");
    /// Written by cfprefsd on iOS 27.2: integer widths from 1 to 16 bytes, a 20-element array (a
    /// count past the marker's nibble), nesting, UTF-16 strings, dates before 1970 and in 4001.
    const WIDE: &[u8] = include_bytes!("fixtures/cfprefsd-wide.plist");
    /// A data container's `.com.apple.mobile_container_manager.metadata.plist`.
    const METADATA: &[u8] = include_bytes!("fixtures/container-metadata.plist");

    fn typed(entries: &Entries) -> serde_json::Value {
        super::super::entries_json(entries)
    }

    #[test]
    fn a_cfprefsd_file_reads_every_type_typed() {
        let entries = entries(EVERY_TYPE).expect("decode");
        assert_eq!(
            typed(&entries),
            serde_json::json!({
                "app_tick": {"type": "long", "value": 49},
                "t_array": {"type": "array", "value": [{"type": "long", "value": 1}, {"type": "string", "value": "a"}]},
                "t_bool": {"type": "bool", "value": true},
                "t_data": {"type": "data", "value": "AAEC"},
                "t_date": {"type": "date", "value": "2026-11-27T10:00:00.25Z"},
                "t_dict": {"type": "dictionary", "value": {"k": {"type": "bool", "value": false}}},
                "t_double": {"type": "double", "value": 2.5},
                "t_long": {"type": "long", "value": 1_099_511_627_776_i64},
                "t_string": {"type": "string", "value": "héllo"},
            })
        );
    }

    #[test]
    fn widths_counts_and_dates_read_exactly() {
        let entries = entries(WIDE).expect("decode");
        let get = |k: &str| entries.get(k).cloned().unwrap_or_else(|| panic!("{k}"));
        assert_eq!(get("u64max"), PrefValue::Unsupported { native: "integer".into(), raw: "18446744073709551615".into() });
        assert_eq!((get("neg"), get("zero"), get("i8")), (PrefValue::Long(-5), PrefValue::Long(0), PrefValue::Long(200)));
        assert_eq!((get("i16"), get("i32")), (PrefValue::Long(40_000), PrefValue::Long(3_000_000_000)));
        assert_eq!(get("pi"), PrefValue::Double(std::f64::consts::PI));
        assert_eq!(get("unicodé ☃"), PrefValue::String("snow ☃".into()));
        let PrefValue::Array(many) = get("many") else { panic!("array") };
        assert_eq!(many.len(), 20);
        assert_eq!(many[19], PrefValue::String("item19".into()));
        assert_eq!(get("far").to_json()["value"], "4001-01-01T00:00:00Z");
        assert_eq!(get("early").to_json()["value"], "1969-12-31T23:59:59.5Z");
        assert_eq!(
            get("nested").to_json(),
            serde_json::json!({"type": "dictionary", "value": {
                "a": {"type": "array", "value": [{"type": "dictionary", "value": {"b": {"type": "array", "value": [
                    {"type": "bool", "value": true}, {"type": "double", "value": 1.5}]}}}]},
                "d": {"type": "data", "value": ""},
                "e": {"type": "string", "value": ""},
            }})
        );
    }

    #[test]
    fn container_metadata_names_its_app() {
        let meta = decode(METADATA).expect("decode");
        assert_eq!(meta.get("MCMMetadataIdentifier").and_then(Plist::as_str), Some("dev.simee.prefsprobe"));
    }

    #[test]
    fn encoding_round_trips_through_the_decoder() {
        for fixture in [EVERY_TYPE, WIDE] {
            let decoded = decode(fixture).expect("decode");
            assert_eq!(decode(&encode(&decoded).expect("encode")).expect("again"), decoded);
        }
        let date = Date::parse_iso8601("2026-11-27T10:15:30.125Z").expect("date");
        let values: Entries = [
            ("s".to_owned(), PrefValue::String("ascii".into())),
            ("u".to_owned(), PrefValue::String("ünï ☃".into())),
            ("i".to_owned(), PrefValue::Int(-7)),
            ("l".to_owned(), PrefValue::Long(i64::MIN)),
            ("big".to_owned(), PrefValue::Long(i64::MAX)),
            ("f".to_owned(), PrefValue::Float(0.1)),
            ("d".to_owned(), PrefValue::Double(-2.5e300)),
            ("b".to_owned(), PrefValue::Bool(false)),
            ("t".to_owned(), PrefValue::Date(date)),
            ("x".to_owned(), PrefValue::Data((0..=255).collect())),
            ("a".to_owned(), PrefValue::Array((0..300).map(PrefValue::Long).collect())),
            ("m".to_owned(), PrefValue::Dictionary([("k".to_owned(), PrefValue::Array(vec![]))].into())),
        ]
        .into();
        let plist = Plist::Dict(values.iter().map(|(k, v)| (k.clone(), from_pref(v).expect("plist"))).collect());
        let back = entries(&encode(&plist).expect("encode")).expect("decode");
        // iOS keeps no width: an int reads back a long, a float a double (its exact f32 value).
        let mut want = values.clone();
        want.insert("i".into(), PrefValue::Long(-7));
        want.insert("f".into(), PrefValue::Double(f64::from(0.1f32)));
        assert_eq!(back, want);
        assert!(matches!(from_pref(&PrefValue::Float(0.1)), Ok(Plist::Real32(_))), "a float is written as a 4-byte real");
    }

    #[test]
    fn what_has_no_plist_form_is_refused() {
        assert_eq!(from_pref(&PrefValue::StringSet(["a".to_owned()].into())).expect_err("set"), "a `string_set` has no property-list form");
        let other = Plist::Other { native: "uid", raw: "3".into() };
        assert_eq!(encode(&Plist::Array(vec![other])).expect_err("uid"), "a plist `uid` can't be written");
    }

    #[test]
    fn damaged_files_are_errors_not_panics() {
        assert!(decode(b"<?xml version").is_err(), "XML");
        assert!(decode(b"bplist00").is_err(), "no trailer");
        for len in 0..EVERY_TYPE.len() {
            let _ = decode(&EVERY_TYPE[..len]);
        }
        // Every single-byte corruption decodes or fails cleanly.
        for at in 0..WIDE.len() {
            for flip in [0x01, 0x10, 0xFF] {
                let mut bytes = WIDE.to_vec();
                bytes[at] ^= flip;
                let _ = decode(&bytes);
            }
        }
    }

    /// An array whose only element is itself.
    fn cyclic() -> Vec<u8> {
        let mut b = b"bplist00".to_vec();
        b.extend_from_slice(&[0xA1, 0x00]);
        let table = b.len() as u64;
        b.push(8);
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 1, 1]);
        b.extend_from_slice(&1u64.to_be_bytes());
        b.extend_from_slice(&0u64.to_be_bytes());
        b.extend_from_slice(&table.to_be_bytes());
        b
    }

    #[test]
    fn a_reference_cycle_or_an_expansion_bomb_is_refused() {
        assert_eq!(decode(&cyclic()).expect_err("cycle"), "damaged binary property list: a reference cycle");
        // 40 arrays, each holding the next one twice: 2^40 nodes if expanded.
        let mut b = b"bplist00".to_vec();
        let mut offsets = Vec::new();
        for i in 0..40u8 {
            offsets.push(b.len() as u8);
            b.extend_from_slice(&[0xA2, i + 1, i + 1]);
        }
        offsets.push(b.len() as u8);
        b.push(0x09);
        let table = b.len() as u64;
        b.extend_from_slice(&offsets);
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 1, 1]);
        b.extend_from_slice(&41u64.to_be_bytes());
        b.extend_from_slice(&0u64.to_be_bytes());
        b.extend_from_slice(&table.to_be_bytes());
        let err = decode(&b).expect_err("bomb");
        assert!(err.starts_with("binary property list"), "{err}");
    }

    #[test]
    fn one_data_object_shared_by_millions_of_references_is_refused_before_it_allocates() {
        const REFS: u32 = 4_000_000; // under MAX_NODES, so only the byte budget stops it
        let mut b = b"bplist00".to_vec();
        let array = b.len() as u32;
        b.extend_from_slice(&[0xAF, 0x12]);
        b.extend_from_slice(&REFS.to_be_bytes());
        b.resize(b.len() + REFS as usize, 1); // every element is object 1
        let data = b.len() as u32;
        b.extend_from_slice(&[0x4F, 0x12]);
        b.extend_from_slice(&(64u32 << 10).to_be_bytes());
        b.resize(b.len() + (64 << 10), 0xAB);
        let table = b.len() as u64;
        b.extend_from_slice(&array.to_be_bytes());
        b.extend_from_slice(&data.to_be_bytes());
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 4, 1]);
        b.extend_from_slice(&2u64.to_be_bytes());
        b.extend_from_slice(&0u64.to_be_bytes());
        b.extend_from_slice(&table.to_be_bytes());
        assert!(b.len() < 5 << 20, "small on disk: {}", b.len());
        let err = decode(&b).expect_err("shared data bomb");
        assert!(err.contains("decodes to more than"), "{err}");
    }

    #[test]
    fn a_large_but_sane_plist_still_reads() {
        let value = Plist::Dict(vec![
            ("blob".into(), Plist::Data(vec![7; 8 << 20])),
            ("items".into(), Plist::Array((0..20_000).map(|i| Plist::String(format!("item-{i}-{}", "x".repeat(100)))).collect())),
        ]);
        let back = decode(&encode(&value).expect("encode")).expect("a sane plist reads");
        assert_eq!(back, value);
    }

    #[test]
    fn plist_dates_keep_about_a_microsecond() {
        assert_eq!(date_from_plist(0.0), Some(Date::from_unix(978_307_200, 0)));
        assert_eq!(date_from_plist(-0.5), Some(Date::from_unix(978_307_199, 500_000_000)));
        assert_eq!(date_from_plist(f64::NAN), None);
        let d = Date::from_unix(1_800_000_000, 123_456_000);
        let back = date_from_plist(date_to_plist(d)).expect("date");
        assert_eq!(back.unix_secs(), d.unix_secs());
        assert!(back.subsec_nanos().abs_diff(d.subsec_nanos()) < 1_000, "{back:?}");
        assert_eq!(to_pref(&Plist::Date(f64::INFINITY)), PrefValue::Unsupported { native: "date".into(), raw: "inf".into() });
    }
}
