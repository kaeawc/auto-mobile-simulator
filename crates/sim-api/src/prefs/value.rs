//! The typed preference value: every type either platform stores, as JSON on the wire.
//!
//! A value travels as `{"type": <type>, "value": <value>}`. The type is always named, so an `int`
//! stays an `int` and a `long` a `long` on the way back, and nested values (an iOS array's
//! elements, a dictionary's values) are typed values themselves:
//!
//! ```text
//! {"type": "int", "value": 3}                      a 32-bit integer (Android's int)
//! {"type": "long", "value": 9007199254740993}      a 64-bit integer
//! {"type": "float", "value": 0.1}                  32-bit; "NaN", "Infinity", "-Infinity" too
//! {"type": "double", "value": 2.5}                 64-bit; the same three strings
//! {"type": "string_set", "value": ["a", "b"]}      sorted, each string once (Android)
//! {"type": "date", "value": "2026-11-27T10:00:00.25Z"}   ISO 8601; written in UTC
//! {"type": "data", "value": "AAEC"}                bytes, standard base64 with padding
//! {"type": "array", "value": [{"type": "string", "value": "a"}]}
//! {"type": "dictionary", "value": {"k": {"type": "bool", "value": true}}}
//! {"type": "unsupported", "value": {"native": "integer", "raw": "18446744073709551615"}}
//! ```
//!
//! Reading a value refuses anything that isn't exactly its type: a fraction for an `int`, a number
//! past its range, a string for a `bool`, an untyped element in an `array`.
//!
//! `unsupported` is what a backend reads that no other type holds exactly (a plist integer past 64
//! bits, Swift's `UInt64.max`; a SharedPreferences `<double>` or `<null>` element another tool
//! wrote): the platform's name for it and its text as stored. It is read-only: a write naming it
//! is refused.
//!
//! iOS keeps no width: a plist has one `integer` and one `real`, so an iOS store reads every
//! integer as a `long` ([`PrefValue::ios_integer`]) and every real as a `double`
//! ([`PrefValue::ios_real`]), whatever type wrote it (`int` and `long` write an integer, `float`
//! and `double` a real).

use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde_json::{Map, Value, json};

/// The type of a preference value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Type {
    String,
    Int,
    Long,
    Float,
    Double,
    Bool,
    StringSet,
    Date,
    Data,
    Array,
    Dictionary,
    /// Read-only: a value no other type holds exactly ([`PrefValue::Unsupported`]).
    Unsupported,
}

impl Type {
    /// Every type a value can be written as ([`Type::Unsupported`] is only read).
    pub const ALL: [Type; 11] = [
        Type::String,
        Type::Int,
        Type::Long,
        Type::Float,
        Type::Double,
        Type::Bool,
        Type::StringSet,
        Type::Date,
        Type::Data,
        Type::Array,
        Type::Dictionary,
    ];

    /// The name on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Type::String => "string",
            Type::Int => "int",
            Type::Long => "long",
            Type::Float => "float",
            Type::Double => "double",
            Type::Bool => "bool",
            Type::StringSet => "string_set",
            Type::Date => "date",
            Type::Data => "data",
            Type::Array => "array",
            Type::Dictionary => "dictionary",
            Type::Unsupported => "unsupported",
        }
    }

    /// A type a value can be written as.
    pub fn parse(name: &str) -> Result<Type, String> {
        if name == Type::Unsupported.as_str() {
            return Err("`unsupported` values are read-only: they are what a store held that no type here holds exactly".into());
        }
        Type::ALL.into_iter().find(|t| t.as_str() == name).ok_or_else(|| {
            let names: Vec<&str> = Type::ALL.iter().map(|t| t.as_str()).collect();
            format!("unknown type `{name}` ({})", names.join(", "))
        })
    }

    /// What a JSON value of this type looks like, for errors.
    fn wants(self) -> &'static str {
        match self {
            Type::String => "a string",
            Type::Int => "a whole number from -2147483648 to 2147483647",
            Type::Long => "a whole number from -9223372036854775808 to 9223372036854775807",
            Type::Float => "a number in a 32-bit float's range, or \"NaN\", \"Infinity\" or \"-Infinity\"",
            Type::Double => "a number, or \"NaN\", \"Infinity\" or \"-Infinity\"",
            Type::Bool => "true or false",
            Type::StringSet => "an array of strings, each once",
            Type::Date => "an ISO 8601 date and time string (2026-11-27T10:00:00Z)",
            Type::Data => "a standard base64 string",
            Type::Array => "an array of typed values ({type, value})",
            Type::Dictionary => "an object whose values are typed values ({type, value})",
            Type::Unsupported => "nothing: unsupported values are read-only",
        }
    }
}

/// A preference value of one [`Type`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum PrefValue {
    String(String),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Bool(bool),
    StringSet(BTreeSet<String>),
    Date(Date),
    Data(Vec<u8>),
    Array(Vec<PrefValue>),
    Dictionary(BTreeMap<String, PrefValue>),
    /// What a store held that no other type holds exactly: the platform's name for its type
    /// (`integer`, `double`, `null`) and its text as stored. Read-only.
    Unsupported {
        native: String,
        raw: String,
    },
}

impl PrefValue {
    /// A plist integer as an iOS store reads it: a `long`, or `unsupported` past 64 bits (a plist
    /// integer is up to 128: Swift's `UInt64.max` is one).
    pub fn ios_integer(n: i128) -> PrefValue {
        i64::try_from(n).map_or_else(|_| PrefValue::Unsupported { native: "integer".into(), raw: n.to_string() }, PrefValue::Long)
    }

    /// A plist real as an iOS store reads it: a `double`.
    pub fn ios_real(f: f64) -> PrefValue {
        PrefValue::Double(f)
    }

    /// About how many bytes this value takes, for a feed's budget.
    pub fn size(&self) -> usize {
        16 + match self {
            PrefValue::String(s) => s.len(),
            PrefValue::StringSet(set) => set.iter().map(|s| s.len() + 16).sum(),
            PrefValue::Data(bytes) => bytes.len(),
            PrefValue::Array(items) => items.iter().map(PrefValue::size).sum(),
            PrefValue::Dictionary(map) => map.iter().map(|(k, v)| k.len() + v.size()).sum(),
            PrefValue::Unsupported { native, raw } => native.len() + raw.len(),
            _ => 0,
        }
    }

    pub fn ty(&self) -> Type {
        match self {
            PrefValue::String(_) => Type::String,
            PrefValue::Int(_) => Type::Int,
            PrefValue::Long(_) => Type::Long,
            PrefValue::Float(_) => Type::Float,
            PrefValue::Double(_) => Type::Double,
            PrefValue::Bool(_) => Type::Bool,
            PrefValue::StringSet(_) => Type::StringSet,
            PrefValue::Date(_) => Type::Date,
            PrefValue::Data(_) => Type::Data,
            PrefValue::Array(_) => Type::Array,
            PrefValue::Dictionary(_) => Type::Dictionary,
            PrefValue::Unsupported { .. } => Type::Unsupported,
        }
    }

    /// The typed value on the wire: `{"type": ..., "value": ...}`.
    pub fn to_json(&self) -> Value {
        json!({"type": self.ty().as_str(), "value": self.bare_json()})
    }

    /// The `value` half of [`PrefValue::to_json`].
    pub fn bare_json(&self) -> Value {
        match self {
            PrefValue::String(s) => Value::String(s.clone()),
            PrefValue::Int(n) => Value::from(*n),
            PrefValue::Long(n) => Value::from(*n),
            // The shortest decimal that reads back as this f32 (0.1, not 0.10000000149011612).
            PrefValue::Float(f) => float_json(f.to_string().parse().unwrap_or(f64::from(*f))),
            PrefValue::Double(f) => float_json(*f),
            PrefValue::Bool(b) => Value::Bool(*b),
            PrefValue::StringSet(set) => set.iter().cloned().map(Value::String).collect(),
            PrefValue::Date(d) => Value::String(d.to_iso8601()),
            PrefValue::Data(bytes) => Value::String(base64::engine::general_purpose::STANDARD.encode(bytes)),
            PrefValue::Array(items) => items.iter().map(PrefValue::to_json).collect(),
            PrefValue::Dictionary(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), v.to_json())).collect()),
            PrefValue::Unsupported { native, raw } => json!({"native": native, "raw": raw}),
        }
    }

    /// Reads a typed value `{"type": ..., "value": ...}`; `at` names it in errors (`value[2]`).
    pub fn from_typed(v: &Value, at: &str) -> Result<PrefValue, String> {
        let Some(obj) = v.as_object() else { return Err(format!("`{at}` must be a typed value {{type, value}}, not {v}")) };
        let ty = match obj.get("type") {
            Some(Value::String(name)) => Type::parse(name).map_err(|e| format!("`{at}`: {e}"))?,
            _ => return Err(format!("`{at}` has no `type` (a typed value is {{type, value}})")),
        };
        if let Some(extra) = obj.keys().find(|k| *k != "type" && *k != "value") {
            return Err(format!("`{at}` has `{extra}`; a typed value is {{type, value}}"));
        }
        match obj.get("value") {
            Some(value) => PrefValue::from_json(ty, value, &format!("{at}.value")),
            None => Err(format!("`{at}` has no `value` (a typed value is {{type, value}})")),
        }
    }

    /// Reads `value` as a `ty`; `at` names it in errors.
    pub fn from_json(ty: Type, value: &Value, at: &str) -> Result<PrefValue, String> {
        let mismatch = || format!("`{at}` must be {} for type `{}`, not {value}", ty.wants(), ty.as_str());
        Ok(match ty {
            Type::String => PrefValue::String(value.as_str().ok_or_else(mismatch)?.to_owned()),
            Type::Int => PrefValue::Int(value.as_i64().and_then(|n| i32::try_from(n).ok()).ok_or_else(mismatch)?),
            Type::Long => PrefValue::Long(value.as_i64().ok_or_else(mismatch)?),
            Type::Float => {
                // From the decimal itself: through f64 first could round twice.
                let f = match value {
                    Value::Number(n) => n.to_string().parse::<f32>().ok().filter(|f| f.is_finite()),
                    Value::String(s) => special_float(s).map(|f| f as f32),
                    _ => None,
                };
                PrefValue::Float(f.ok_or_else(mismatch)?)
            }
            Type::Double => {
                let f = match value {
                    Value::Number(n) => n.as_f64(),
                    Value::String(s) => special_float(s),
                    _ => None,
                };
                PrefValue::Double(f.ok_or_else(mismatch)?)
            }
            Type::Bool => PrefValue::Bool(value.as_bool().ok_or_else(mismatch)?),
            Type::StringSet => {
                let mut set = BTreeSet::new();
                for item in value.as_array().ok_or_else(mismatch)? {
                    let s = item.as_str().ok_or_else(mismatch)?;
                    if !set.insert(s.to_owned()) {
                        return Err(format!("`{at}` lists \"{s}\" twice; a string set holds each string once"));
                    }
                }
                PrefValue::StringSet(set)
            }
            Type::Date => PrefValue::Date(Date::parse_iso8601(value.as_str().ok_or_else(mismatch)?).map_err(|e| format!("`{at}`: {e}"))?),
            Type::Data => {
                let text = value.as_str().ok_or_else(mismatch)?;
                PrefValue::Data(base64::engine::general_purpose::STANDARD.decode(text).map_err(|_| mismatch())?)
            }
            Type::Array => {
                let items = value.as_array().ok_or_else(mismatch)?;
                PrefValue::Array(
                    items.iter().enumerate().map(|(i, v)| PrefValue::from_typed(v, &format!("{at}[{i}]"))).collect::<Result<_, _>>()?,
                )
            }
            Type::Dictionary => {
                let map: &Map<String, Value> = value.as_object().ok_or_else(mismatch)?;
                PrefValue::Dictionary(
                    map.iter()
                        .map(|(k, v)| Ok((k.clone(), PrefValue::from_typed(v, &format!("{at}.{k}"))?)))
                        .collect::<Result<_, String>>()?,
                )
            }
            Type::Unsupported => return Err(format!("`{at}`: {}", Type::parse("unsupported").err().unwrap_or_default())),
        })
    }

    /// The values nested in this one (an array's elements, a dictionary's values).
    pub fn children(&self) -> Box<dyn Iterator<Item = &PrefValue> + '_> {
        match self {
            PrefValue::Array(items) => Box::new(items.iter()),
            PrefValue::Dictionary(map) => Box::new(map.values()),
            _ => Box::new(std::iter::empty()),
        }
    }
}

/// A finite float as a JSON number; NaN and the infinities, which JSON has no numbers for, as the
/// strings Java and plists print them as.
fn float_json(f: f64) -> Value {
    match serde_json::Number::from_f64(f) {
        Some(n) => Value::Number(n),
        None if f.is_nan() => Value::String("NaN".into()),
        None if f > 0.0 => Value::String("Infinity".into()),
        None => Value::String("-Infinity".into()),
    }
}

fn special_float(s: &str) -> Option<f64> {
    match s {
        "NaN" => Some(f64::NAN),
        "Infinity" => Some(f64::INFINITY),
        "-Infinity" => Some(f64::NEG_INFINITY),
        _ => None,
    }
}

/// An instant: seconds and nanoseconds since the Unix epoch, UTC. It reads and writes ISO 8601
/// (RFC 3339) with a four-digit year, and keeps nanoseconds, so a plist date (a double of seconds
/// since 2001) or `NSDate.distantFuture` (year 4001) survives the trip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    secs: i64,
    nanos: u32,
}

impl Date {
    /// `secs` and `nanos` since the Unix epoch (nanos past a second carry into `secs`).
    pub fn from_unix(secs: i64, nanos: u32) -> Date {
        Date { secs: secs.saturating_add(i64::from(nanos / 1_000_000_000)), nanos: nanos % 1_000_000_000 }
    }

    pub fn unix_secs(self) -> i64 {
        self.secs
    }

    pub fn subsec_nanos(self) -> u32 {
        self.nanos
    }

    pub fn now() -> Date {
        Date::from_system_time(SystemTime::now())
    }

    pub fn from_system_time(t: SystemTime) -> Date {
        match t.duration_since(UNIX_EPOCH) {
            Ok(d) => Date::from_unix(i64::try_from(d.as_secs()).unwrap_or(i64::MAX), d.subsec_nanos()),
            Err(e) => {
                let d = e.duration();
                let secs = -i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
                match d.subsec_nanos() {
                    0 => Date::from_unix(secs, 0),
                    n => Date::from_unix(secs - 1, 1_000_000_000 - n),
                }
            }
        }
    }

    /// `YYYY-MM-DDTHH:MM:SS[.fraction]Z`, the fraction without trailing zeros (none when whole).
    pub fn to_iso8601(self) -> String {
        let (days, sod) = (self.secs.div_euclid(86_400), self.secs.rem_euclid(86_400));
        let (y, m, d) = civil_from_days(days);
        let mut out = format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}", sod / 3600, sod % 3600 / 60, sod % 60);
        if self.nanos > 0 {
            out.push('.');
            out.push_str(format!("{:09}", self.nanos).trim_end_matches('0'));
        }
        out.push('Z');
        out
    }

    /// Reads `YYYY-MM-DDTHH:MM:SS`, an optional fraction of up to 9 digits, and `Z` or an offset
    /// (`+02:00`, `-0530`).
    pub fn parse_iso8601(s: &str) -> Result<Date, String> {
        let bad = || format!("`{s}` isn't an ISO 8601 date and time (YYYY-MM-DDTHH:MM:SS[.fff] then Z or ±HH:MM)");
        let digits = |from: usize, len: usize| -> Option<i64> {
            let t = s.get(from..from + len)?;
            t.bytes().all(|c| c.is_ascii_digit()).then(|| t.parse().ok())?
        };
        let b = s.as_bytes();
        let sep = |i: usize, allowed: &[u8]| b.get(i).is_some_and(|c| allowed.contains(c));
        if !(sep(4, b"-") && sep(7, b"-") && sep(10, b"Tt") && sep(13, b":") && sep(16, b":")) {
            return Err(bad());
        }
        let (year, month, day) = (digits(0, 4).ok_or_else(bad)?, digits(5, 2).ok_or_else(bad)?, digits(8, 2).ok_or_else(bad)?);
        let (hour, minute, second) = (digits(11, 2).ok_or_else(bad)?, digits(14, 2).ok_or_else(bad)?, digits(17, 2).ok_or_else(bad)?);
        if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) || hour > 23 || minute > 59 || second > 59 {
            return Err(bad());
        }
        let mut rest = &s[19..];
        let mut nanos = 0u32;
        if let Some(frac) = rest.strip_prefix('.') {
            let len = frac.bytes().take_while(u8::is_ascii_digit).count();
            if len == 0 || len > 9 {
                return Err(bad());
            }
            nanos = format!("{:0<9}", &frac[..len]).parse().map_err(|_| bad())?;
            rest = &frac[len..];
        }
        let offset = match rest {
            "Z" | "z" => 0,
            _ => {
                let sign = match rest.as_bytes().first() {
                    Some(b'+') => 1,
                    Some(b'-') => -1,
                    _ => return Err(bad()),
                };
                // `HH:MM` or `HHMM`.
                let zone = &rest[1..];
                let hm = match zone.len() {
                    5 if zone.as_bytes()[2] == b':' => zone.replace(':', ""),
                    4 => zone.to_owned(),
                    _ => return Err(bad()),
                };
                if !hm.bytes().all(|c| c.is_ascii_digit()) {
                    return Err(bad());
                }
                let (h, m): (i64, i64) = (hm[..2].parse().map_err(|_| bad())?, hm[2..].parse().map_err(|_| bad())?);
                if h > 23 || m > 59 {
                    return Err(bad());
                }
                sign * (h * 3600 + m * 60)
            }
        };
        let secs = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset;
        Ok(Date::from_unix(secs, nanos))
    }
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The proleptic Gregorian date of a day count since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(v: &PrefValue) -> PrefValue {
        let wire = serde_json::to_string(&v.to_json()).expect("json");
        PrefValue::from_typed(&serde_json::from_str(&wire).expect("parse"), "value").expect("typed")
    }

    #[test]
    fn every_type_round_trips_through_json() {
        let date = Date::parse_iso8601("2026-11-27T10:15:30.125Z").expect("date");
        let values = [
            PrefValue::String("héllo \"quoted\"\n".into()),
            PrefValue::Int(i32::MIN),
            PrefValue::Int(i32::MAX),
            PrefValue::Long(i64::MIN),
            PrefValue::Long(9_007_199_254_740_993),
            PrefValue::Float(0.1),
            PrefValue::Float(3.4e38),
            PrefValue::Float(f32::INFINITY),
            PrefValue::Double(0.1),
            PrefValue::Double(-1e300),
            PrefValue::Double(f64::NEG_INFINITY),
            PrefValue::Bool(false),
            PrefValue::StringSet(["b".to_owned(), "a".to_owned()].into()),
            PrefValue::StringSet(BTreeSet::new()),
            PrefValue::Date(date),
            PrefValue::Data(vec![0, 1, 2, 0xff, b'\n']),
            PrefValue::Data(Vec::new()),
            PrefValue::Array(Vec::new()),
            PrefValue::Dictionary(BTreeMap::new()),
        ];
        for v in &values {
            assert_eq!(&round_trip(v), v, "{}", v.to_json());
        }
        // NaN never equals itself: its wire form does.
        let nan = PrefValue::Double(f64::NAN).to_json();
        assert_eq!(nan, json!({"type": "double", "value": "NaN"}));
        assert!(matches!(round_trip(&PrefValue::Float(f32::NAN)), PrefValue::Float(f) if f.is_nan()));
        // The wire forms, exactly.
        assert_eq!(PrefValue::Float(0.1).to_json(), json!({"type": "float", "value": 0.1}));
        assert_eq!(PrefValue::Long(9_007_199_254_740_993).to_json()["value"].to_string(), "9007199254740993");
        assert_eq!(PrefValue::Data(vec![0, 1, 2]).to_json(), json!({"type": "data", "value": "AAEC"}));
        assert_eq!(PrefValue::Date(date).to_json(), json!({"type": "date", "value": "2026-11-27T10:15:30.125Z"}));
        assert_eq!(PrefValue::StringSet(["b".to_owned(), "a".to_owned()].into()).bare_json(), json!(["a", "b"]));
    }

    #[test]
    fn nested_values_round_trip_with_their_types() {
        let wire = json!({"type": "dictionary", "value": {
            "name": {"type": "string", "value": "Ada"},
            "launches": {"type": "long", "value": 12},
            "scores": {"type": "array", "value": [
                {"type": "double", "value": 1.5},
                {"type": "int", "value": 2},
                {"type": "array", "value": [{"type": "data", "value": "/w=="}]},
                {"type": "dictionary", "value": {"at": {"type": "date", "value": "2001-01-01T00:00:00Z"}}}
            ]}
        }});
        let v = PrefValue::from_typed(&wire, "value").expect("nested");
        assert_eq!(v.to_json(), wire, "the same JSON back, every type kept");
        let PrefValue::Dictionary(map) = &v else { panic!("a dictionary") };
        let PrefValue::Array(scores) = &map["scores"] else { panic!("an array") };
        assert_eq!(scores[1], PrefValue::Int(2), "not widened to a double");
        assert_eq!(scores[2], PrefValue::Array(vec![PrefValue::Data(vec![0xff])]));
        assert_eq!(v.children().count(), 3);
    }

    #[test]
    fn type_mismatches_are_refused_with_where_they_are() {
        let err = |ty: &str, value: Value| PrefValue::from_json(Type::parse(ty).expect("type"), &value, "value").expect_err(ty);
        assert!(err("int", json!(1.5)).starts_with("`value` must be a whole number from -2147483648"), "{}", err("int", json!(1.5)));
        assert!(err("int", json!(2_147_483_648_i64)).contains("for type `int`"));
        assert!(err("long", json!(u64::MAX)).contains("for type `long`"));
        assert!(err("long", json!("12")).contains("not \"12\""));
        assert!(err("float", json!(1e39)).contains("32-bit float's range"));
        assert!(err("double", json!("1.5")).contains("for type `double`"));
        assert!(err("bool", json!("true")).contains("true or false"));
        assert!(err("string", json!(1)).contains("a string"));
        assert!(err("string_set", json!(["a", 1])).contains("an array of strings"));
        assert_eq!(err("string_set", json!(["a", "a"])), "`value` lists \"a\" twice; a string set holds each string once");
        assert!(err("data", json!("not base64!")).contains("base64"));
        assert!(err("date", json!("2026-13-01T00:00:00Z")).starts_with("`value`: `2026-13-01T00:00:00Z` isn't an ISO 8601"));
        assert!(err("date", json!(1_800_000_000)).contains("ISO 8601"));
        // Nested values are typed values, and errors say which one.
        assert_eq!(err("array", json!(["a"])), "`value[0]` must be a typed value {type, value}, not \"a\"");
        let deep = err("dictionary", json!({"k": {"type": "array", "value": [{"type": "int", "value": "x"}]}}));
        assert!(deep.starts_with("`value.k.value[0].value` must be a whole number"), "{deep}");
        assert_eq!(err("array", json!([{"value": 1}])), "`value[0]` has no `type` (a typed value is {type, value})");
        assert_eq!(err("array", json!([{"type": "int"}])), "`value[0]` has no `value` (a typed value is {type, value})");
        assert_eq!(err("array", json!([{"type": "int", "value": 1, "x": 2}])), "`value[0]` has `x`; a typed value is {type, value}");
        assert!(err("array", json!([{"type": "integer", "value": 1}])).starts_with("`value[0]`: unknown type `integer` (string, int"));
        assert_eq!(Type::parse("str").expect_err("unknown").split(" (").next(), Some("unknown type `str`"));
    }

    #[test]
    fn dates_read_and_write_iso_8601() {
        let parse = |s: &str| Date::parse_iso8601(s).expect(s);
        assert_eq!(parse("1970-01-01T00:00:00Z"), Date::from_unix(0, 0));
        assert_eq!(parse("2001-01-01T00:00:00Z").unix_secs(), 978_307_200, "the plist epoch");
        assert_eq!(parse("2026-11-27T12:00:00+02:00"), parse("2026-11-27T10:00:00Z"));
        assert_eq!(parse("2026-11-27T04:30:00-0530"), parse("2026-11-27T10:00:00z"));
        assert_eq!(parse("2024-02-29t23:59:59.000000001Z").subsec_nanos(), 1);
        assert_eq!(parse("1969-12-31T23:59:59.5Z"), Date::from_unix(-1, 500_000_000));
        for s in [
            "1970-01-01T00:00:00Z",
            "2026-11-27T10:15:30.125Z",
            "0001-01-01T00:00:00Z",
            "4001-01-01T00:00:00Z",
            "2000-02-29T00:00:00.000000001Z",
        ] {
            assert_eq!(parse(s).to_iso8601(), s);
        }
        // Written in UTC, the fraction trimmed.
        assert_eq!(parse("2026-11-27T12:00:00.500+02:00").to_iso8601(), "2026-11-27T10:00:00.5Z");
        let a_second_before = Date::from_system_time(UNIX_EPOCH - std::time::Duration::from_millis(1500));
        assert_eq!(a_second_before.to_iso8601(), "1969-12-31T23:59:58.5Z");
        for bad in [
            "2026-11-27",
            "2026-11-27T10:00:00",
            "2026-11-27 10:00:00Z",
            "2025-02-29T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-11-27T24:00:00Z",
            "2026-11-27T10:00:60Z",
            "2026-11-27T10:00:00.Z",
            "2026-11-27T10:00:00.1234567890Z",
            "2026-11-27T10:00:00+2:00",
            "2026-11-27T10:00:00+02:00:00",
            "+2026-11-27T10:00:00Z",
            "2026-11-27T10:00:00ZZ",
        ] {
            assert!(Date::parse_iso8601(bad).is_err(), "{bad}");
        }
        for days in [-719_468, -1, 0, 1, 11_016, 19_000, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        }
    }

    #[test]
    fn values_no_type_holds_read_as_unsupported_and_are_never_written() {
        // Swift's UInt64.max in a plist: past a long, so `unsupported` with its digits.
        let big = PrefValue::ios_integer(i128::from(u64::MAX));
        assert_eq!(big.to_json(), json!({"type": "unsupported", "value": {"native": "integer", "raw": "18446744073709551615"}}));
        // A SharedPreferences `<double>` or `<null>` element (Android's own API writes neither).
        let double = PrefValue::Unsupported { native: "double".into(), raw: "1.5".into() };
        let null = PrefValue::Unsupported { native: "null".into(), raw: String::new() };
        assert_eq!(double.to_json(), json!({"type": "unsupported", "value": {"native": "double", "raw": "1.5"}}));
        assert_eq!(null.ty(), Type::Unsupported);
        // Nested in an array (an iOS array holding a 128-bit integer) it says the same.
        assert_eq!(PrefValue::Array(vec![big.clone()]).to_json()["value"][0]["type"], "unsupported");
        // Read-only: neither a top-level write nor a nested element can name it.
        let refused = Type::parse("unsupported").expect_err("read-only");
        assert!(refused.starts_with("`unsupported` values are read-only"), "{refused}");
        let nested = PrefValue::from_typed(&json!({"type": "array", "value": [big.to_json()]}), "value").expect_err("nested");
        assert!(nested.starts_with("`value.value[0]`: `unsupported` values are read-only"), "{nested}");
        assert!(PrefValue::from_json(Type::Unsupported, &json!({}), "value").is_err());
        assert!(!Type::ALL.contains(&Type::Unsupported), "never offered as a write type");
    }

    #[test]
    fn ios_reads_integers_as_long_and_reals_as_double() {
        // A plist keeps no width: whatever `int` or `float` wrote comes back as `long` or `double`.
        assert_eq!(PrefValue::ios_integer(3), PrefValue::Long(3));
        assert_eq!(PrefValue::ios_integer(i128::from(i64::MIN)), PrefValue::Long(i64::MIN));
        assert_eq!(PrefValue::ios_integer(i128::from(i64::MAX) + 1).ty(), Type::Unsupported);
        assert_eq!(PrefValue::ios_real(f64::from(0.5f32)), PrefValue::Double(0.5));
        assert_eq!(PrefValue::ios_integer(7).to_json(), json!({"type": "long", "value": 7}));
    }
}
