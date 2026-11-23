//! The Android Automotive vehicle HAL's emulator channel (`qemud:car`), host side.
//!
//! The emulator VHAL in AAOS images (`device/generic/car/emulator/vhal_aidl`) opens the qemud
//! `car` pipe when `ro.boot.qemu` is set and exchanges proto2 `vhal_proto.EmulatorMessage`s
//! (`hardware/interfaces/automotive/vehicle/proto/VehicleHalProto.proto`) over it:
//!
//! - framing (goldfish `libqemud`): 4 ASCII hex digits of payload length, then the payload; the
//!   guest sends payloads of 64 KiB or more with a 4-byte big-endian header `len | 0x8000_0000`
//!   instead. The guest reads into a 2048-byte buffer and drops the connection on a payload of
//!   2048 bytes or more, so host frames carry at most [`MAX_HOST_PAYLOAD`] bytes;
//! - the guest answers every command in order (`*_RESP`, `ERROR_UNIMPLEMENTED_CMD` for unknown
//!   ones) and sends `SET_PROPERTY_ASYNC` on its own whenever a property changes, from boot on.
//!
//! This module is the whole protocol: a hand-written codec for the messages (no protobuf
//! dependency), both framings, the property names the console accepts, typed value parsing and
//! formatting, and [`Tracker`], which follows a connection's replies and the latest value of
//! each property.

use std::collections::{BTreeMap, VecDeque};

/// Largest host → guest payload: the guest's read buffer is 2048 bytes and a full buffer counts
/// as an error that ends its read loop.
pub const MAX_HOST_PAYLOAD: usize = 2047;
/// Larger guest frames are skipped rather than buffered.
const MAX_GUEST_PAYLOAD: usize = 16 << 20;

/// `vhal_proto.MsgType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum MsgType {
    GetConfigCmd = 0,
    GetConfigResp = 1,
    GetConfigAllCmd = 2,
    GetConfigAllResp = 3,
    GetPropertyCmd = 4,
    GetPropertyResp = 5,
    GetPropertyAllCmd = 6,
    GetPropertyAllResp = 7,
    SetPropertyCmd = 8,
    SetPropertyResp = 9,
    SetPropertyAsync = 10,
    DebugCmd = 11,
    DebugResp = 12,
}

/// `vhal_proto.Status` names, by value.
const STATUS: &[&str] = &[
    "RESULT_OK",
    "ERROR_UNKNOWN",
    "ERROR_UNIMPLEMENTED_CMD",
    "ERROR_INVALID_PROPERTY",
    "ERROR_INVALID_AREA_ID",
    "ERROR_PROPERTY_UNINITIALIZED",
    "ERROR_WRITE_ONLY_PROPERTY",
    "ERROR_MEMORY_ALLOC_FAILED",
    "ERROR_INVALID_OPERATION",
];

/// The name of a `vhal_proto.Status` value.
pub fn status_name(status: i32) -> String {
    usize::try_from(status).ok().and_then(|i| STATUS.get(i)).map_or_else(|| format!("status {status}"), |s| (*s).to_owned())
}

/// `vhal_proto.VehiclePropValue`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PropValue {
    pub prop: i32,
    pub value_type: Option<i32>,
    pub timestamp: Option<i64>,
    pub area_id: Option<i32>,
    pub int32_values: Vec<i32>,
    pub int64_values: Vec<i64>,
    pub float_values: Vec<f32>,
    pub string_value: Option<String>,
    pub bytes_value: Option<Vec<u8>>,
    /// `VehiclePropStatus`: 0 available, 1 unavailable, 2 error.
    pub status: Option<i32>,
}

/// `vhal_proto.VehiclePropGet`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PropGet {
    pub prop: i32,
    pub area_id: Option<i32>,
}

/// `vhal_proto.VehicleAreaConfig`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AreaConfig {
    pub area_id: i32,
    pub min_int32: Option<i32>,
    pub max_int32: Option<i32>,
    pub min_int64: Option<i64>,
    pub max_int64: Option<i64>,
    pub min_float: Option<f32>,
    pub max_float: Option<f32>,
}

/// `vhal_proto.VehiclePropConfig` (without the deprecated `supported_areas`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PropConfig {
    pub prop: i32,
    pub access: Option<i32>,
    pub change_mode: Option<i32>,
    pub value_type: Option<i32>,
    pub area_configs: Vec<AreaConfig>,
    pub config_flags: Option<i32>,
    pub config_array: Vec<i32>,
    pub config_string: Option<String>,
    pub min_sample_rate: Option<f32>,
    pub max_sample_rate: Option<f32>,
}

/// `vhal_proto.EmulatorMessage`. `msg_type` stays a raw number so unknown types survive.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Message {
    pub msg_type: i32,
    pub status: Option<i32>,
    pub props: Vec<PropGet>,
    pub configs: Vec<PropConfig>,
    pub values: Vec<PropValue>,
    pub debug_commands: Vec<String>,
    pub debug_result: Option<String>,
}

/// Why bytes didn't decode as a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeError(pub &'static str);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bad EmulatorMessage: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

// Wire format (protobuf encoding): proto2 repeated scalars are unpacked unless marked packed
// (none here), so the encoder writes one key per element; the decoder accepts both.

const VARINT: u8 = 0;
const FIXED64: u8 = 1;
const LEN: u8 = 2;
const FIXED32: u8 = 5;

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_key(out: &mut Vec<u8>, field: u32, wire: u8) {
    put_varint(out, u64::from(field << 3 | u32::from(wire)));
}

/// `int32`: negative values are sign-extended to ten bytes, as protobuf does.
fn put_int32(out: &mut Vec<u8>, field: u32, v: i32) {
    put_key(out, field, VARINT);
    put_varint(out, i64::from(v) as u64);
}

fn put_int64(out: &mut Vec<u8>, field: u32, v: i64) {
    put_key(out, field, VARINT);
    put_varint(out, v as u64);
}

fn put_sint32(out: &mut Vec<u8>, field: u32, v: i32) {
    put_key(out, field, VARINT);
    put_varint(out, u64::from(((v << 1) ^ (v >> 31)) as u32));
}

fn put_sint64(out: &mut Vec<u8>, field: u32, v: i64) {
    put_key(out, field, VARINT);
    put_varint(out, ((v << 1) ^ (v >> 63)) as u64);
}

fn put_float(out: &mut Vec<u8>, field: u32, v: f32) {
    put_key(out, field, FIXED32);
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, field: u32, b: &[u8]) {
    put_key(out, field, LEN);
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn put_message(out: &mut Vec<u8>, field: u32, encode: impl FnOnce(&mut Vec<u8>)) {
    let mut sub = Vec::new();
    encode(&mut sub);
    put_bytes(out, field, &sub);
}

/// One decoded field value.
enum Wire<'a> {
    Varint(u64),
    /// No field here is a fixed64; unknown ones are skipped.
    Fixed64,
    Bytes(&'a [u8]),
    Fixed32(u32),
}

struct Reader<'a> {
    b: &'a [u8],
}

impl<'a> Reader<'a> {
    fn varint(&mut self) -> Result<u64, DecodeError> {
        let mut v = 0u64;
        for i in 0..10 {
            let (&byte, rest) = self.b.split_first().ok_or(DecodeError("truncated varint"))?;
            self.b = rest;
            v |= u64::from(byte & 0x7f) << (7 * i);
            if byte & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(DecodeError("varint longer than 10 bytes"))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.b.len() < n {
            return Err(DecodeError("truncated field"));
        }
        let (head, rest) = self.b.split_at(n);
        self.b = rest;
        Ok(head)
    }

    /// The next (field number, value), or None at the end.
    fn field(&mut self) -> Result<Option<(u32, Wire<'a>)>, DecodeError> {
        if self.b.is_empty() {
            return Ok(None);
        }
        let key = self.varint()?;
        let field = u32::try_from(key >> 3).map_err(|_| DecodeError("field number out of range"))?;
        let value = match (key & 7) as u8 {
            VARINT => Wire::Varint(self.varint()?),
            FIXED64 => {
                self.take(8)?;
                Wire::Fixed64
            }
            LEN => {
                let n = usize::try_from(self.varint()?).map_err(|_| DecodeError("length out of range"))?;
                Wire::Bytes(self.take(n)?)
            }
            FIXED32 => Wire::Fixed32(u32::from_le_bytes(self.take(4)?.try_into().unwrap_or_default())),
            _ => return Err(DecodeError("unsupported wire type (groups)")),
        };
        Ok(Some((field, value)))
    }
}

fn fields(b: &[u8], mut each: impl FnMut(u32, Wire<'_>) -> Result<(), DecodeError>) -> Result<(), DecodeError> {
    let mut r = Reader { b };
    while let Some((n, w)) = r.field()? {
        each(n, w)?;
    }
    Ok(())
}

fn as_varint(w: &Wire<'_>) -> Result<u64, DecodeError> {
    match w {
        Wire::Varint(v) => Ok(*v),
        _ => Err(DecodeError("expected a varint")),
    }
}

fn as_int32(w: &Wire<'_>) -> Result<i32, DecodeError> {
    as_varint(w).map(|v| v as i32)
}

fn unzigzag32(v: u64) -> i32 {
    let v = v as u32;
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

fn unzigzag64(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

fn as_float(w: &Wire<'_>) -> Result<f32, DecodeError> {
    match w {
        Wire::Fixed32(v) => Ok(f32::from_bits(*v)),
        _ => Err(DecodeError("expected a float")),
    }
}

fn as_bytes<'a>(w: &Wire<'a>) -> Result<&'a [u8], DecodeError> {
    match w {
        Wire::Bytes(b) => Ok(b),
        _ => Err(DecodeError("expected a length-delimited field")),
    }
}

fn as_string(w: &Wire<'_>) -> Result<String, DecodeError> {
    String::from_utf8(as_bytes(w)?.to_vec()).map_err(|_| DecodeError("string is not UTF-8"))
}

/// A repeated varint field, unpacked (one value) or packed (a run of them).
fn repeated_varint<T>(w: &Wire<'_>, out: &mut Vec<T>, conv: impl Fn(u64) -> T) -> Result<(), DecodeError> {
    match w {
        Wire::Varint(v) => out.push(conv(*v)),
        Wire::Bytes(b) => {
            let mut r = Reader { b };
            while !r.b.is_empty() {
                out.push(conv(r.varint()?));
            }
        }
        _ => return Err(DecodeError("expected varints")),
    }
    Ok(())
}

fn repeated_float(w: &Wire<'_>, out: &mut Vec<f32>) -> Result<(), DecodeError> {
    match w {
        Wire::Fixed32(v) => out.push(f32::from_bits(*v)),
        Wire::Bytes(b) if b.len().is_multiple_of(4) => {
            out.extend(b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)));
        }
        _ => return Err(DecodeError("expected floats")),
    }
    Ok(())
}

impl PropValue {
    pub fn encode(&self, out: &mut Vec<u8>) {
        put_int32(out, 1, self.prop);
        if let Some(v) = self.value_type {
            put_int32(out, 2, v);
        }
        if let Some(v) = self.timestamp {
            put_int64(out, 3, v);
        }
        if let Some(v) = self.area_id {
            put_int32(out, 4, v);
        }
        for &v in &self.int32_values {
            put_sint32(out, 5, v);
        }
        for &v in &self.int64_values {
            put_sint64(out, 6, v);
        }
        for &v in &self.float_values {
            put_float(out, 7, v);
        }
        if let Some(s) = &self.string_value {
            put_bytes(out, 8, s.as_bytes());
        }
        if let Some(b) = &self.bytes_value {
            put_bytes(out, 9, b);
        }
        if let Some(v) = self.status {
            put_int32(out, 10, v);
        }
    }

    pub fn decode(b: &[u8]) -> Result<PropValue, DecodeError> {
        let mut v = PropValue::default();
        fields(b, |n, w| {
            match n {
                1 => v.prop = as_int32(&w)?,
                2 => v.value_type = Some(as_int32(&w)?),
                3 => v.timestamp = Some(as_varint(&w)? as i64),
                4 => v.area_id = Some(as_int32(&w)?),
                5 => repeated_varint(&w, &mut v.int32_values, unzigzag32)?,
                6 => repeated_varint(&w, &mut v.int64_values, unzigzag64)?,
                7 => repeated_float(&w, &mut v.float_values)?,
                8 => v.string_value = Some(as_string(&w)?),
                9 => v.bytes_value = Some(as_bytes(&w)?.to_vec()),
                10 => v.status = Some(as_int32(&w)?),
                _ => {}
            }
            Ok(())
        })?;
        Ok(v)
    }

    /// The area this value belongs to (absent means 0, the global area).
    pub fn area(&self) -> i32 {
        self.area_id.unwrap_or(0)
    }
}

impl PropGet {
    fn encode(&self, out: &mut Vec<u8>) {
        put_int32(out, 1, self.prop);
        if let Some(a) = self.area_id {
            put_int32(out, 2, a);
        }
    }

    fn decode(b: &[u8]) -> Result<PropGet, DecodeError> {
        let mut g = PropGet::default();
        fields(b, |n, w| {
            match n {
                1 => g.prop = as_int32(&w)?,
                2 => g.area_id = Some(as_int32(&w)?),
                _ => {}
            }
            Ok(())
        })?;
        Ok(g)
    }
}

impl AreaConfig {
    fn encode(&self, out: &mut Vec<u8>) {
        put_int32(out, 1, self.area_id);
        if let Some(v) = self.min_int32 {
            put_sint32(out, 2, v);
        }
        if let Some(v) = self.max_int32 {
            put_sint32(out, 3, v);
        }
        if let Some(v) = self.min_int64 {
            put_sint64(out, 4, v);
        }
        if let Some(v) = self.max_int64 {
            put_sint64(out, 5, v);
        }
        if let Some(v) = self.min_float {
            put_float(out, 6, v);
        }
        if let Some(v) = self.max_float {
            put_float(out, 7, v);
        }
    }

    fn decode(b: &[u8]) -> Result<AreaConfig, DecodeError> {
        let mut a = AreaConfig::default();
        fields(b, |n, w| {
            match n {
                1 => a.area_id = as_int32(&w)?,
                2 => a.min_int32 = Some(unzigzag32(as_varint(&w)?)),
                3 => a.max_int32 = Some(unzigzag32(as_varint(&w)?)),
                4 => a.min_int64 = Some(unzigzag64(as_varint(&w)?)),
                5 => a.max_int64 = Some(unzigzag64(as_varint(&w)?)),
                6 => a.min_float = Some(as_float(&w)?),
                7 => a.max_float = Some(as_float(&w)?),
                _ => {}
            }
            Ok(())
        })?;
        Ok(a)
    }
}

impl PropConfig {
    fn encode(&self, out: &mut Vec<u8>) {
        put_int32(out, 1, self.prop);
        for (field, v) in [(2, self.access), (3, self.change_mode), (4, self.value_type)] {
            if let Some(v) = v {
                put_int32(out, field, v);
            }
        }
        for a in &self.area_configs {
            put_message(out, 6, |o| a.encode(o));
        }
        if let Some(v) = self.config_flags {
            put_int32(out, 7, v);
        }
        for &v in &self.config_array {
            put_int32(out, 8, v);
        }
        if let Some(s) = &self.config_string {
            put_bytes(out, 9, s.as_bytes());
        }
        if let Some(v) = self.min_sample_rate {
            put_float(out, 10, v);
        }
        if let Some(v) = self.max_sample_rate {
            put_float(out, 11, v);
        }
    }

    fn decode(b: &[u8]) -> Result<PropConfig, DecodeError> {
        let mut c = PropConfig::default();
        fields(b, |n, w| {
            match n {
                1 => c.prop = as_int32(&w)?,
                2 => c.access = Some(as_int32(&w)?),
                3 => c.change_mode = Some(as_int32(&w)?),
                4 => c.value_type = Some(as_int32(&w)?),
                6 => c.area_configs.push(AreaConfig::decode(as_bytes(&w)?)?),
                7 => c.config_flags = Some(as_int32(&w)?),
                8 => repeated_varint(&w, &mut c.config_array, |v| v as i32)?,
                9 => c.config_string = Some(as_string(&w)?),
                10 => c.min_sample_rate = Some(as_float(&w)?),
                11 => c.max_sample_rate = Some(as_float(&w)?),
                _ => {}
            }
            Ok(())
        })?;
        Ok(c)
    }
}

impl Message {
    fn command(msg_type: MsgType) -> Message {
        // Stock's host tools send `status: RESULT_OK` on commands too.
        Message { msg_type: msg_type as i32, status: Some(0), ..Message::default() }
    }

    /// `SET_PROPERTY_CMD` for one value.
    pub fn set_property(value: PropValue) -> Message {
        Message { values: vec![value], ..Message::command(MsgType::SetPropertyCmd) }
    }

    /// `GET_PROPERTY_CMD` for one property and area.
    pub fn get_property(prop: i32, area_id: i32) -> Message {
        Message { props: vec![PropGet { prop, area_id: Some(area_id) }], ..Message::command(MsgType::GetPropertyCmd) }
    }

    /// `GET_PROPERTY_ALL_CMD`: every property's current value.
    pub fn get_property_all() -> Message {
        Message::command(MsgType::GetPropertyAllCmd)
    }

    /// `GET_CONFIG_ALL_CMD`: every property's configuration.
    pub fn get_config_all() -> Message {
        Message::command(MsgType::GetConfigAllCmd)
    }

    pub fn is(&self, t: MsgType) -> bool {
        self.msg_type == t as i32
    }

    /// Whether the status is absent or `RESULT_OK`.
    pub fn ok(&self) -> bool {
        self.status.unwrap_or(0) == 0
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_int32(&mut out, 1, self.msg_type);
        if let Some(s) = self.status {
            put_int32(&mut out, 2, s);
        }
        for g in &self.props {
            put_message(&mut out, 3, |o| g.encode(o));
        }
        for c in &self.configs {
            put_message(&mut out, 4, |o| c.encode(o));
        }
        for v in &self.values {
            put_message(&mut out, 5, |o| v.encode(o));
        }
        for d in &self.debug_commands {
            put_bytes(&mut out, 6, d.as_bytes());
        }
        if let Some(r) = &self.debug_result {
            put_bytes(&mut out, 7, r.as_bytes());
        }
        out
    }

    pub fn decode(b: &[u8]) -> Result<Message, DecodeError> {
        let mut m = Message::default();
        let mut has_type = false;
        fields(b, |n, w| {
            match n {
                1 => {
                    m.msg_type = as_int32(&w)?;
                    has_type = true;
                }
                2 => m.status = Some(as_int32(&w)?),
                3 => m.props.push(PropGet::decode(as_bytes(&w)?)?),
                4 => m.configs.push(PropConfig::decode(as_bytes(&w)?)?),
                5 => m.values.push(PropValue::decode(as_bytes(&w)?)?),
                6 => m.debug_commands.push(as_string(&w)?),
                7 => m.debug_result = Some(as_string(&w)?),
                _ => {}
            }
            Ok(())
        })?;
        if !has_type {
            return Err(DecodeError("missing msg_type"));
        }
        Ok(m)
    }

    /// The message as one host → guest frame, if it fits the guest's read buffer.
    pub fn frame(&self) -> Result<Vec<u8>, String> {
        frame(&self.encode())
    }
}

/// One host → guest frame: 4 hex digits of length, then `payload` (1..=[`MAX_HOST_PAYLOAD`]
/// bytes; the guest ignores empty frames and drops the pipe on bigger ones).
pub fn frame(payload: &[u8]) -> Result<Vec<u8>, String> {
    if payload.is_empty() || payload.len() > MAX_HOST_PAYLOAD {
        return Err(format!("a {}-byte message doesn't fit the vehicle HAL's {MAX_HOST_PAYLOAD}-byte frames", payload.len()));
    }
    let mut out = format!("{:04x}", payload.len()).into_bytes();
    out.extend_from_slice(payload);
    Ok(out)
}

/// Splits guest → host bytes into payloads, in either framing.
#[derive(Debug, Default)]
pub struct Deframer {
    buf: Vec<u8>,
    /// Bytes still to discard of a frame too large to keep.
    skip: usize,
}

impl Deframer {
    /// Adds bytes; returns the payloads they complete.
    pub fn push(&mut self, mut data: &[u8]) -> Vec<Vec<u8>> {
        let n = self.skip.min(data.len());
        self.skip -= n;
        data = &data[n..];
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        while self.skip == 0 && self.buf.len() >= 4 {
            let len = if self.buf[0] & 0x80 != 0 {
                (u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) & 0x7fff_ffff) as usize
            } else {
                match std::str::from_utf8(&self.buf[..4]).ok().and_then(|h| usize::from_str_radix(h, 16).ok()) {
                    Some(n) => n,
                    None => {
                        // Not this framing, and no way to resynchronize: drop what we have.
                        self.buf.clear();
                        break;
                    }
                }
            };
            if len > MAX_GUEST_PAYLOAD {
                self.buf.drain(..4);
                let n = len.min(self.buf.len());
                self.buf.drain(..n);
                self.skip = len - n;
                continue;
            }
            if self.buf.len() < 4 + len {
                break;
            }
            out.push(self.buf[4..4 + len].to_vec());
            self.buf.drain(..4 + len);
        }
        out
    }
}

/// The type a property id encodes (`VehiclePropertyType`, bits 16–23).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropType {
    String,
    Boolean,
    Int32,
    Int32Vec,
    Int64,
    Int64Vec,
    Float,
    FloatVec,
    Bytes,
    Mixed,
    Unknown,
}

impl PropType {
    pub fn of(prop: i32) -> PropType {
        match (prop as u32) & 0x00ff_0000 {
            0x0010_0000 => PropType::String,
            0x0020_0000 => PropType::Boolean,
            0x0040_0000 => PropType::Int32,
            0x0041_0000 => PropType::Int32Vec,
            0x0050_0000 => PropType::Int64,
            0x0051_0000 => PropType::Int64Vec,
            0x0060_0000 => PropType::Float,
            0x0061_0000 => PropType::FloatVec,
            0x0070_0000 => PropType::Bytes,
            0x00e0_0000 => PropType::Mixed,
            _ => PropType::Unknown,
        }
    }
}

/// System properties the console knows by name (`VehicleProperty.aidl`); any other property is
/// given by its numeric id.
pub const PROPERTIES: &[(&str, i32)] = &[
    ("INFO_VIN", 0x1110_0100),
    ("INFO_MAKE", 0x1110_0101),
    ("INFO_MODEL", 0x1110_0102),
    ("INFO_MODEL_YEAR", 0x1140_0103),
    ("PERF_ODOMETER", 0x1160_0204),
    ("PERF_VEHICLE_SPEED", 0x1160_0207),
    ("PERF_VEHICLE_SPEED_DISPLAY", 0x1160_0208),
    ("ENGINE_RPM", 0x1160_0305),
    ("FUEL_LEVEL", 0x1160_0307),
    ("FUEL_DOOR_OPEN", 0x1120_0308),
    ("RANGE_REMAINING", 0x1160_0308),
    ("EV_BATTERY_LEVEL", 0x1160_0309),
    ("EV_CHARGE_PORT_OPEN", 0x1120_030a),
    ("EV_CHARGE_PORT_CONNECTED", 0x1120_030b),
    ("GEAR_SELECTION", 0x1140_0400),
    ("CURRENT_GEAR", 0x1140_0401),
    ("PARKING_BRAKE_ON", 0x1120_0402),
    ("NIGHT_MODE", 0x1120_0407),
    ("TURN_SIGNAL_STATE", 0x1140_0408),
    ("IGNITION_STATE", 0x1140_0409),
    ("ABS_ACTIVE", 0x1120_040a),
    ("HVAC_FAN_SPEED", 0x1540_0500),
    ("HVAC_TEMPERATURE_SET", 0x1560_0503),
    ("HVAC_POWER_ON", 0x1520_0510),
    ("ENV_OUTSIDE_TEMPERATURE", 0x1160_0703),
    ("AP_POWER_STATE_REQ", 0x1141_0a00),
    ("AP_POWER_STATE_REPORT", 0x1141_0a01),
    ("DISPLAY_BRIGHTNESS", 0x1140_0a03),
    ("HEADLIGHTS_STATE", 0x1140_0e00),
];

/// Short names for the properties a driving test touches most.
const ALIASES: &[(&str, &str)] = &[
    ("gear", "GEAR_SELECTION"),
    ("speed", "PERF_VEHICLE_SPEED"),
    ("ignition", "IGNITION_STATE"),
    ("night", "NIGHT_MODE"),
    ("parking-brake", "PARKING_BRAKE_ON"),
];

pub const GEAR_SELECTION: i32 = 0x1140_0400;
pub const CURRENT_GEAR: i32 = 0x1140_0401;
pub const IGNITION_STATE: i32 = 0x1140_0409;

/// `VehicleGear` names.
const GEARS: &[(&str, i32)] = &[("neutral", 1), ("n", 1), ("reverse", 2), ("r", 2), ("park", 4), ("p", 4), ("drive", 8), ("d", 8)];
/// `VehicleIgnitionState` names.
const IGNITION: &[(&str, i32)] = &[("undefined", 0), ("lock", 1), ("off", 2), ("acc", 3), ("on", 4), ("start", 5)];

fn parse_int(s: &str) -> Option<i64> {
    let (neg, digits) = s.strip_prefix('-').map_or((false, s), |d| (true, d));
    let v = match digits.strip_prefix("0x").or_else(|| digits.strip_prefix("0X")) {
        Some(hex) => i64::from_str_radix(hex, 16).ok()?,
        None => digits.parse().ok()?,
    };
    Some(if neg { -v } else { v })
}

/// A property by name (case-insensitive: `GEAR_SELECTION`, `gear_selection`, or an alias like
/// `gear`) or by numeric id (`0x11400400` or decimal).
pub fn property(s: &str) -> Result<i32, String> {
    let upper = s.to_ascii_uppercase().replace('-', "_");
    let alias = ALIASES.iter().find(|(a, _)| a.eq_ignore_ascii_case(s)).map(|(_, name)| *name);
    if let Some(&(_, id)) = PROPERTIES.iter().find(|(n, _)| Some(*n) == alias || *n == upper) {
        return Ok(id);
    }
    parse_id(s).ok_or_else(|| format!("unknown vehicle property `{s}`; give a name (`car names` lists them) or a numeric id"))
}

/// The property's name, if it's one this module knows.
pub fn name(prop: i32) -> Option<&'static str> {
    PROPERTIES.iter().find(|(_, id)| *id == prop).map(|(n, _)| *n)
}

/// The property's name, else its id in hex.
pub fn label(prop: i32) -> String {
    name(prop).map_or_else(|| format!("{:#010x}", prop as u32), str::to_owned)
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// A value for `prop` in `area_id` from text, typed by the property id: `true`/`false` for
/// booleans, numbers (gear and ignition also by name: `drive`, `P`, `on`, ...), comma-separated
/// lists for vector types, hex for bytes, the text itself for strings.
pub fn parse_value(prop: i32, area_id: i32, text: &str) -> Result<PropValue, String> {
    let ty = PropType::of(prop);
    let bad = || format!("`{text}` isn't a {ty:?} value for {}", label(prop));
    let mut v = PropValue {
        prop,
        // `VehiclePropertyType`: the id's type bits.
        value_type: Some((prop as u32 & 0x00ff_0000) as i32),
        // Stock's host tools leave the global area out.
        area_id: (area_id != 0).then_some(area_id),
        ..PropValue::default()
    };
    let named = |table: &[(&str, i32)]| table.iter().find(|(n, _)| n.eq_ignore_ascii_case(text)).map(|(_, v)| *v);
    let int32 = |s: &str| parse_int(s.trim()).and_then(|n| i32::try_from(n).ok());
    let list: Vec<&str> = text.split(',').map(str::trim).filter(|p| !p.is_empty()).collect();
    match ty {
        PropType::Boolean => v.int32_values = vec![i32::from(parse_bool(text).ok_or_else(bad)?)],
        PropType::Int32 => {
            let n = match prop {
                GEAR_SELECTION | CURRENT_GEAR => named(GEARS),
                IGNITION_STATE => named(IGNITION),
                _ => None,
            };
            v.int32_values = vec![n.or_else(|| int32(text)).ok_or_else(bad)?];
        }
        PropType::Int32Vec => v.int32_values = list.iter().map(|p| int32(p)).collect::<Option<_>>().ok_or_else(bad)?,
        PropType::Int64 => v.int64_values = vec![parse_int(text.trim()).ok_or_else(bad)?],
        PropType::Int64Vec => v.int64_values = list.iter().map(|p| parse_int(p)).collect::<Option<_>>().ok_or_else(bad)?,
        PropType::Float => v.float_values = vec![text.trim().parse().map_err(|_| bad())?],
        PropType::FloatVec => {
            v.float_values = list.iter().map(|p| p.parse().ok()).collect::<Option<_>>().ok_or_else(bad)?;
        }
        PropType::String => v.string_value = Some(text.to_owned()),
        PropType::Bytes => {
            let hex: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            if !hex.len().is_multiple_of(2) {
                return Err(bad());
            }
            let bytes = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok()).collect::<Option<_>>();
            v.bytes_value = Some(bytes.ok_or_else(bad)?);
        }
        PropType::Mixed | PropType::Unknown => {
            return Err(format!("{} has a mixed or unknown type; it can't be set from text", label(prop)));
        }
    }
    Ok(v)
}

/// A value as text, the inverse of [`parse_value`] (numbers stay numbers).
pub fn format_value(v: &PropValue) -> String {
    match PropType::of(v.prop) {
        PropType::Boolean => v.int32_values.first().map_or_else(String::new, |&b| (b != 0).to_string()),
        PropType::String => v.string_value.clone().unwrap_or_default(),
        PropType::Bytes => v.bytes_value.as_deref().unwrap_or_default().iter().map(|b| format!("{b:02x}")).collect(),
        _ => {
            let mut parts: Vec<String> = v.int32_values.iter().map(i32::to_string).collect();
            parts.extend(v.int64_values.iter().map(i64::to_string));
            parts.extend(v.float_values.iter().map(f32::to_string));
            if let Some(s) = &v.string_value {
                parts.push(s.clone());
            }
            parts.join(",")
        }
    }
}

/// A value as one console line: `property=<name> id=0x... area=<n> status=<s> value=<text>`
/// (`value` last: string values may hold spaces).
pub fn describe(v: &PropValue) -> String {
    let status = match v.status {
        None | Some(0) => "available",
        Some(1) => "unavailable",
        Some(_) => "error",
    };
    format!("property={} id={:#010x} area={} status={status} value={}", label(v.prop), v.prop as u32, v.area(), format_value(v))
}

/// One [`describe`]d value, read back by a console client.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Reading {
    pub property: String,
    pub id: i32,
    pub area_id: i32,
    pub status: String,
    pub value: String,
}

impl Reading {
    /// Parses a [`describe`] line.
    pub fn parse(line: &str) -> Option<Reading> {
        let rest = line.strip_prefix("property=")?;
        let (property, rest) = rest.split_once(" id=")?;
        let (id, rest) = rest.split_once(" area=")?;
        let (area, rest) = rest.split_once(" status=")?;
        let (status, value) = rest.split_once(" value=")?;
        Some(Reading { property: property.into(), id: parse_id(id)?, area_id: parse_id(area)?, status: status.into(), value: value.into() })
    }

    /// The value's type, from the property id.
    pub fn prop_type(&self) -> PropType {
        PropType::of(self.id)
    }
}

/// `car set` / `car get` arguments after the subcommand: `<property> [<area>] <value...>` and
/// `<property> [<area>]`. The area is taken only when a value still follows it, so string
/// values that start with a number need the area written out (`car set INFO_MODEL 0 3 Series`).
pub fn parse_set(words: &[&str]) -> Result<PropValue, String> {
    let [prop, rest @ ..] = words else { return Err("usage: car set <property> [<area>] <value>".into()) };
    let prop = property(prop)?;
    let (area, value) = match rest {
        [] => return Err("usage: car set <property> [<area>] <value>".into()),
        [area, value @ ..] if !value.is_empty() && parse_int(area).is_some() => (parse_area(area)?, value),
        value => (0, value),
    };
    if value.len() > 1 && PropType::of(prop) != PropType::String {
        return Err(format!("too many values for {}; vector values are comma-separated", label(prop)));
    }
    parse_value(prop, area, &value.join(" "))
}

/// `car get` arguments: `<property> [<area>]`.
pub fn parse_get(words: &[&str]) -> Result<(i32, i32), String> {
    match words {
        [prop] => Ok((property(prop)?, 0)),
        [prop, area] => Ok((property(prop)?, parse_area(area)?)),
        _ => Err("usage: car get <property> [<area>]".into()),
    }
}

fn parse_area(s: &str) -> Result<i32, String> {
    parse_id(s).ok_or_else(|| format!("bad area id `{s}`"))
}

/// A 32-bit id or area in decimal or `0x` hex, signed or not (vendor ids and some area masks
/// set the top bit).
pub fn parse_id(s: &str) -> Option<i32> {
    parse_int(s).and_then(|v| i32::try_from(v).ok().or_else(|| u32::try_from(v).ok().map(|u| u as i32)))
}

/// The host's view of one `qemud:car` connection: it decodes the guest's frames, remembers the
/// latest value of every (property, area) the guest reports, and pairs replies with the host
/// commands that asked for them.
///
/// The guest answers in order, so replies pair first-in first-out by type. A command whose
/// caller gave up stays queued (abandoned) until its late reply arrives, so that reply can't be
/// taken for the next command's.
#[derive(Debug, Default)]
pub struct Tracker {
    frames: Deframer,
    values: BTreeMap<(i32, i32), PropValue>,
    pending: VecDeque<Pending>,
    next_id: u64,
    /// Guest frames that didn't decode.
    pub bad_frames: u64,
}

#[derive(Debug)]
struct Pending {
    id: u64,
    reply: i32,
    abandoned: bool,
}

/// The reply type for a command type.
pub fn reply_type(command: i32) -> Option<MsgType> {
    use MsgType::*;
    Some(match command {
        0 => GetConfigResp,
        2 => GetConfigAllResp,
        4 => GetPropertyResp,
        6 => GetPropertyAllResp,
        8 => SetPropertyResp,
        11 => DebugResp,
        _ => return None,
    })
}

impl Tracker {
    /// Registers a command about to be sent; its reply comes back from [`Tracker::feed`] with
    /// the returned id.
    pub fn expect(&mut self, command: &Message) -> Option<u64> {
        let reply = reply_type(command.msg_type)?;
        let id = self.next_id;
        self.next_id += 1;
        self.pending.push_back(Pending { id, reply: reply as i32, abandoned: false });
        // A guest that stopped answering shouldn't grow this without bound.
        while self.pending.len() > 64 {
            self.pending.pop_front();
        }
        Some(id)
    }

    /// The caller of command `id` stopped waiting; its reply will be dropped.
    pub fn abandon(&mut self, id: u64) {
        if let Some(p) = self.pending.iter_mut().find(|p| p.id == id) {
            p.abandoned = true;
        }
    }

    /// Takes guest bytes; returns replies to registered commands as (id, message).
    pub fn feed(&mut self, data: &[u8]) -> Vec<(u64, Message)> {
        let mut replies = Vec::new();
        for payload in self.frames.push(data) {
            let Ok(msg) = Message::decode(&payload) else {
                self.bad_frames += 1;
                continue;
            };
            let records = msg.is(MsgType::SetPropertyAsync)
                || (msg.ok() && (msg.is(MsgType::GetPropertyResp) || msg.is(MsgType::GetPropertyAllResp)));
            if records {
                for v in &msg.values {
                    self.values.insert((v.prop, v.area()), v.clone());
                }
            }
            if let Some(i) = self.pending.iter().position(|p| p.reply == msg.msg_type)
                && let Some(p) = self.pending.remove(i)
                && !p.abandoned
            {
                replies.push((p.id, msg));
            }
        }
        replies
    }

    /// The latest value the guest reported for (prop, area).
    pub fn value(&self, prop: i32, area_id: i32) -> Option<&PropValue> {
        self.values.get(&(prop, area_id))
    }

    /// Every value the guest has reported, by property then area.
    pub fn values(&self) -> impl Iterator<Item = &PropValue> {
        self.values.values()
    }

    /// Forgets everything: a new connection (the guest's VHAL restarted).
    pub fn reset(&mut self) {
        *self = Tracker { next_id: self.next_id, ..Tracker::default() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace().map(|b| u8::from_str_radix(b, 16).expect("hex")).collect()
    }

    #[test]
    fn encodes_the_stock_host_commands() {
        let drive = parse_value(GEAR_SELECTION, 0, "drive").expect("drive");
        let drive = PropValue { value_type: None, ..drive };
        assert_eq!(Message::set_property(drive).encode(), hex("08 08 10 00 2a 08 08 80 88 80 8a 01 28 10"));
        let speed = PropValue { value_type: None, ..parse_value(property("speed").expect("speed"), 0, "10").expect("10.0") };
        assert_eq!(Message::set_property(speed).encode(), hex("08 08 10 00 2a 0b 08 87 84 80 8b 01 3d 00 00 20 41"));
        assert_eq!(Message::get_property(IGNITION_STATE, 0).encode(), hex("08 04 10 00 1a 08 08 89 88 80 8a 01 10 00"));
        assert_eq!(Message::get_config_all().encode(), hex("08 02 10 00"));
        assert_eq!(Message::get_property_all().encode(), hex("08 06 10 00"));
        let resp = Message::decode(&hex("08 09 10 00")).expect("SET_PROPERTY_RESP");
        assert!(resp.is(MsgType::SetPropertyResp) && resp.ok());
        // value_type carries VehiclePropertyType: INT32 for the gear.
        assert_eq!(parse_value(GEAR_SELECTION, 0, "P").expect("park").value_type, Some(0x0040_0000));
    }

    #[test]
    fn varints_zigzag_and_negative_int32() {
        let v = PropValue {
            prop: -1,
            area_id: Some(-2),
            int32_values: vec![-1, 0, 1, i32::MIN],
            int64_values: vec![i64::MIN, -3],
            ..Default::default()
        };
        let mut out = Vec::new();
        v.encode(&mut out);
        // int32 -1 is sign-extended to ten bytes; sint32 -1 is zigzag 1.
        assert_eq!(&out[..11], &hex("08 ff ff ff ff ff ff ff ff ff 01")[..]);
        assert_eq!(PropValue::decode(&out).expect("decode"), v);
        assert_eq!(unzigzag32(1), -1);
        assert_eq!(unzigzag32(0xffff_ffff), i32::MIN);
        assert_eq!(unzigzag64(u64::MAX), i64::MIN);
    }

    #[test]
    fn decodes_packed_and_unknown_fields() {
        // value { prop: 0x11410a00, int32_values: [1, 2] packed }, an unknown field 15 (varint)
        // and an unknown fixed64 field 14.
        let mut sub = Vec::new();
        put_int32(&mut sub, 1, 0x1141_0a00);
        put_bytes(&mut sub, 5, &[2, 4]);
        put_bytes(&mut sub, 7, &1.5f32.to_le_bytes());
        let mut msg = Vec::new();
        put_int32(&mut msg, 1, MsgType::SetPropertyAsync as i32);
        put_bytes(&mut msg, 5, &sub);
        put_int32(&mut msg, 15, 7);
        put_key(&mut msg, 14, FIXED64);
        msg.extend_from_slice(&[0; 8]);
        let m = Message::decode(&msg).expect("decode");
        assert_eq!((m.values[0].int32_values.as_slice(), m.values[0].float_values.as_slice()), (&[1, 2][..], &[1.5][..]));
        assert_eq!(Message::decode(&hex("10 00")), Err(DecodeError("missing msg_type")));
        assert!(Message::decode(&hex("08 08 2a 05 08")).is_err(), "truncated");
        assert!(Message::decode(&hex("0b")).is_err(), "groups are refused");
    }

    #[test]
    fn whole_messages_round_trip() {
        let m = Message {
            msg_type: MsgType::GetConfigAllResp as i32,
            status: Some(0),
            props: vec![PropGet { prop: 5, area_id: None }],
            configs: vec![PropConfig {
                prop: GEAR_SELECTION,
                access: Some(1),
                change_mode: Some(1),
                value_type: Some(0x0040_0000),
                area_configs: vec![AreaConfig {
                    area_id: 0,
                    min_int32: Some(-4),
                    max_int32: Some(8),
                    min_float: Some(0.5),
                    ..Default::default()
                }],
                config_flags: Some(0),
                config_array: vec![1, 2, 4, 8],
                config_string: Some("gears".into()),
                min_sample_rate: Some(0.0),
                max_sample_rate: Some(10.0),
            }],
            values: vec![PropValue {
                prop: 0x1110_0101,
                timestamp: Some(1_700_000_000_000_000_000),
                string_value: Some("Toy Car".into()),
                bytes_value: Some(vec![0, 255]),
                status: Some(1),
                ..Default::default()
            }],
            debug_commands: vec!["--help".into()],
            debug_result: Some("ok".into()),
        };
        assert_eq!(Message::decode(&m.encode()), Ok(m));
    }

    #[test]
    fn frames_both_ways() {
        assert_eq!(frame(b"ab").expect("frame"), b"0002ab");
        assert!(frame(&[0; MAX_HOST_PAYLOAD]).is_ok());
        assert!(frame(&[0; MAX_HOST_PAYLOAD + 1]).is_err(), "the guest drops the pipe on a full buffer");
        assert!(frame(b"").is_err());
        let mut d = Deframer::default();
        assert!(d.push(b"000").is_empty());
        assert_eq!(d.push(b"3abc0001"), vec![b"abc".to_vec()]);
        assert_eq!(d.push(b"z"), vec![b"z".to_vec()]);
        // 64 KiB and up: a big-endian length with the top bit set.
        let big = vec![7u8; 70_000];
        let mut wire = (70_000u32 | 0x8000_0000).to_be_bytes().to_vec();
        wire.extend_from_slice(&big);
        wire.extend_from_slice(b"0001x");
        let got: Vec<Vec<u8>> = wire.chunks(1000).flat_map(|c| d.push(c)).collect();
        assert_eq!(got, vec![big, b"x".to_vec()]);
        // Frames over the cap are skipped, and the stream carries on after them.
        let mut wire = ((MAX_GUEST_PAYLOAD as u32 + 1) | 0x8000_0000).to_be_bytes().to_vec();
        wire.extend_from_slice(b"..");
        assert!(d.push(&wire).is_empty());
        assert!(d.push(&vec![0; MAX_GUEST_PAYLOAD - 1]).is_empty());
        assert_eq!(d.push(b"0002ok"), vec![b"ok".to_vec()]);
    }

    #[test]
    fn properties_by_name_alias_or_id() {
        assert_eq!(property("GEAR_SELECTION"), Ok(0x1140_0400));
        assert_eq!(property("gear_selection"), Ok(0x1140_0400));
        assert_eq!(property("gear"), Ok(0x1140_0400));
        assert_eq!(property("parking-brake"), Ok(0x1120_0402));
        assert_eq!(property("0x11600207"), Ok(0x1160_0207));
        assert_eq!(property("291504647"), Ok(0x1160_0207));
        assert_eq!(property("0xf0000001"), Ok(0xf000_0001u32 as i32), "vendor ids above i32::MAX");
        assert!(property("warp_drive").expect_err("unknown").contains("unknown vehicle property `warp_drive`"));
        assert_eq!(label(0x1160_0207), "PERF_VEHICLE_SPEED");
        assert_eq!(label(0x2140_0001), "0x21400001");
        for (n, id) in PROPERTIES {
            assert_ne!(PropType::of(*id), PropType::Unknown, "{n}");
        }
    }

    #[test]
    fn values_parse_and_format_by_type() {
        let v = parse_value(0x1120_0402, 0, "on").expect("bool");
        assert_eq!((v.int32_values.as_slice(), format_value(&v)), (&[1][..], "true".to_owned()));
        assert_eq!(parse_value(IGNITION_STATE, 0, "ACC").expect("acc").int32_values, [3]);
        assert_eq!(parse_value(GEAR_SELECTION, 0, "0x10").expect("gear 1").int32_values, [16]);
        let v = parse_value(0x1141_0a01, 0, "2, 0").expect("int32 vec");
        assert_eq!(format_value(&v), "2,0");
        let v = parse_value(0x1560_0503, 0x31, "21.5").expect("seat float");
        assert_eq!((v.area_id, format_value(&v)), (Some(0x31), "21.5".to_owned()));
        assert_eq!(format_value(&parse_value(0x1110_0101, 0, "Toy Car").expect("string")), "Toy Car");
        assert!(parse_value(GEAR_SELECTION, 0, "warp").expect_err("bad").contains("isn't a Int32 value for GEAR_SELECTION"));
        assert!(parse_value(0x1160_0207, 0, "fast").is_err());
        let line = describe(&PropValue { status: Some(0), area_id: Some(0), ..parse_value(0x1160_0207, 0, "12.5").expect("speed") });
        assert_eq!(line, "property=PERF_VEHICLE_SPEED id=0x11600207 area=0 status=available value=12.5");
        let r = Reading::parse(&line).expect("reading");
        assert_eq!(
            (r.property.as_str(), r.id, r.area_id, r.value.as_str(), r.prop_type()),
            ("PERF_VEHICLE_SPEED", 0x1160_0207, 0, "12.5", PropType::Float)
        );
        let model = describe(&parse_value(0x1110_0102, 0x31, "3 Series").expect("string"));
        assert_eq!(Reading::parse(&model).map(|r| (r.area_id, r.value)), Some((0x31, "3 Series".to_owned())));
        assert_eq!(Reading::parse("I am alive!"), None);
    }

    #[test]
    fn console_arguments() {
        let v = parse_set(&["gear", "D"]).expect("set gear");
        assert_eq!((v.prop, v.area_id, v.int32_values.as_slice()), (GEAR_SELECTION, None, &[8][..]));
        let v = parse_set(&["HVAC_TEMPERATURE_SET", "49", "22"]).expect("with area");
        assert_eq!((v.area_id, v.float_values.as_slice()), (Some(49), &[22.0][..]));
        let v = parse_set(&["INFO_MODEL", "0", "3", "Series"]).expect("string with spaces");
        assert_eq!(v.string_value.as_deref(), Some("3 Series"));
        assert!(parse_set(&["speed", "1", "2", "3"]).is_err());
        assert!(parse_set(&["speed"]).is_err());
        assert_eq!(parse_get(&["ignition"]), Ok((IGNITION_STATE, 0)));
        assert_eq!(parse_get(&["0x15600503", "0x31"]), Ok((0x1560_0503, 0x31)));
        assert!(parse_get(&[]).is_err());
    }

    fn guest(msg: &Message) -> Vec<u8> {
        let payload = msg.encode();
        let mut out = format!("{:04x}", payload.len()).into_bytes();
        out.extend_from_slice(&payload);
        out
    }

    #[test]
    fn tracker_records_values_and_pairs_replies() {
        let mut t = Tracker::default();
        let speed = PropValue { prop: 0x1160_0207, area_id: Some(0), float_values: vec![3.0], status: Some(0), ..Default::default() };
        let async_set =
            Message { msg_type: MsgType::SetPropertyAsync as i32, status: Some(0), values: vec![speed.clone()], ..Default::default() };
        // Unsolicited: recorded, nobody to answer.
        assert!(t.feed(&guest(&async_set)).is_empty());
        assert_eq!(t.value(0x1160_0207, 0), Some(&speed));

        let set = t.expect(&Message::set_property(parse_value(GEAR_SELECTION, 0, "D").expect("D"))).expect("id");
        let get = t.expect(&Message::get_property(GEAR_SELECTION, 0)).expect("id");
        assert_eq!(t.expect(&async_set), None, "not a command");
        let gear = PropValue { prop: GEAR_SELECTION, area_id: Some(0), int32_values: vec![8], ..Default::default() };
        let get_resp =
            Message { msg_type: MsgType::GetPropertyResp as i32, status: Some(0), values: vec![gear.clone()], ..Default::default() };
        let set_resp = Message { msg_type: MsgType::SetPropertyResp as i32, status: Some(0), ..Default::default() };
        let mut wire = guest(&set_resp);
        wire.extend(guest(&async_set));
        wire.extend(guest(&get_resp));
        let replies = t.feed(&wire);
        assert_eq!(replies, vec![(set, set_resp.clone()), (get, get_resp)]);
        assert_eq!(t.value(GEAR_SELECTION, 0), Some(&gear));
        assert_eq!(t.values().count(), 2);

        // An abandoned command swallows its late reply; the next one gets its own.
        let late = t.expect(&Message::set_property(gear.clone())).expect("id");
        t.abandon(late);
        let next = t.expect(&Message::set_property(gear)).expect("id");
        assert!(t.feed(&guest(&set_resp)).is_empty());
        assert_eq!(t.feed(&guest(&set_resp)), vec![(next, set_resp)]);

        assert!(t.feed(b"0002\xff\xff").is_empty());
        assert_eq!(t.bad_frames, 1);
        t.reset();
        assert_eq!(t.values().count(), 0);
    }
}
