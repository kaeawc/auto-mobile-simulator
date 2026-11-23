//! Replays of the radio HAL's conversations, checked with a port of its response reader
//! (`AtChannel::receiveOneResponse`, `AtResponse::parse`) and the parsers in `AtResponse.cpp`.

use super::pdu::{self, decode_deliver, encode_address, gsm7_encode, hex, pack_septets};
use super::*;

// ---- port of the HAL's reader ---------------------------------------------------------------

/// What `AtResponse::parse` turns a response into.
#[derive(Clone, Debug, PartialEq)]
enum Resp {
    Ok,
    Error,
    Ring,
    Prompt,
    /// A value without a `+` prefix (IMSI, ICCID, IMEI), up to `\rOK\r`.
    Raw(String),
    /// `+CMD`/`%CMD`/`^CMD`: the id and the payload as the HAL hands it to the parser
    /// (multi-line commands keep their `+CMD:` prefixes and line breaks).
    Cmd(&'static str, String),
    Cmt(Vec<u8>),
}

/// `plusValueParsers` in order, with their multi-line flags.
const PLUS: &[(&str, bool)] = &[
    ("CPIN", false),
    ("CPINR", false),
    ("CRSM", false),
    ("CFUN", false),
    ("CREG", false),
    ("CEREG", false),
    ("CGREG", false),
    ("CTEC", false),
    ("COPS", true),
    ("WRMP", false),
    ("CCSS", false),
    ("CSQ", false),
    ("CLCC", true),
    ("CCFCU", true),
    ("CCWA", false),
    ("CUSATD", false),
    ("CUSATP", false),
    ("CUSATE", false),
    ("CUSATT", false),
    ("CUSATEND", false),
    ("CGDCONT", true),
    ("CGCONTRDP", false),
    ("CLCK", false),
    ("CSIM", false),
    ("CCHC", false),
    ("CLIP", false),
    ("CLIR", false),
    ("CMUT", false),
    ("WSOS", false),
    ("CSCA", false),
    ("CSCB", false),
    ("CMGS", false),
    ("CMGW", false),
    ("CME ERROR", false),
    ("CMS ERROR", false),
];
const PERCENT: &[(&str, bool)] = &[("CTZV", false), ("CGFPCCFG", false)];
const CARET: &[(&str, bool)] = &[("MBAU", false)];

fn ltrim(s: &str) -> &str {
    s.trim_start_matches(|c: char| c <= ' ')
}

/// `AtResponse::parse`: `None` = incomplete. A response the HAL cannot parse at all (which
/// makes its reading loop abort) fails the test.
fn hal_parse(s: &str) -> Option<(usize, Resp)> {
    if s.starts_with("RING\r") {
        return Some((5, Resp::Ring));
    }
    if let Some(rest) = s.strip_prefix("+CMT:") {
        let trimmed = ltrim(rest);
        let off = s.len() - trimmed.len();
        let mut p = P::new(trimmed);
        p.int().and_then(|_| p.c(b'\r')).unwrap_or_else(|| panic!("HAL aborts on {s:?}"));
        let hexpdu = p.until(b'\r')?;
        assert!(hexpdu.len() % 2 == 0, "HAL aborts on odd PDU {s:?}");
        let bin = pdu::unhex(&hexpdu).unwrap_or_else(|| panic!("bad hex {s:?}"));
        return Some((off + p.i, Resp::Cmt(bin)));
    }
    match s.as_bytes().first() {
        Some(b'+') => return parse_cmds(s, PLUS),
        Some(b'%') => return parse_cmds(s, PERCENT),
        Some(b'^') => return parse_cmds(s, CARET),
        _ => {}
    }
    if s.starts_with("> \r") {
        return Some((3, Resp::Prompt));
    }
    if s.starts_with("OK\r") {
        return Some((3, Resp::Ok));
    }
    if s.starts_with("ERROR\r") {
        return Some((6, Resp::Error));
    }
    let pos = s.find("\rOK\r")?;
    Some((pos + 4, Resp::Raw(s[..pos].to_string())))
}

fn parse_cmds(s: &str, table: &[(&'static str, bool)]) -> Option<(usize, Resp)> {
    let s1 = &s[1..];
    if s1.is_empty() {
        return None;
    }
    let mut maybe_incomplete = false;
    for &(cmd, multi) in table {
        if !s1.starts_with(&cmd[..cmd.len().min(s1.len())]) {
            continue;
        }
        if s1.len() <= cmd.len() {
            maybe_incomplete = true;
            continue;
        }
        let skip = match s1.as_bytes()[cmd.len()] {
            b':' => 1 + cmd.len() + 1,
            b'\r' => 1 + cmd.len(),
            _ => continue,
        };
        return if multi {
            let end = skip + s[skip..].find("\rOK\r")?;
            Some((end + 4, Resp::Cmd(cmd, s[..end + 1].to_string())))
        } else {
            let end = skip + s[skip..].find('\r')?;
            Some((end + 1, Resp::Cmd(cmd, ltrim(&s[skip..end]).to_string())))
        };
    }
    assert!(maybe_incomplete, "HAL aborts on unknown response {s:?}");
    None
}

/// `AtChannel`'s reading loop over a byte stream.
#[derive(Default)]
struct HalReader {
    buf: String,
}

impl HalReader {
    fn feed(&mut self, bytes: &[u8]) -> Vec<Resp> {
        self.buf.push_str(std::str::from_utf8(bytes).expect("modem output is ASCII"));
        let mut out = Vec::new();
        loop {
            while self.buf.starts_with(['\r', '\n']) {
                self.buf.remove(0);
            }
            if self.buf.is_empty() {
                break;
            }
            match hal_parse(&self.buf) {
                Some((n, r)) => {
                    out.push(r);
                    self.buf.drain(..n);
                }
                None => break,
            }
        }
        out
    }
}

/// `Parser` (Parser.cpp): `skip(' ')` skips everything <= ' ', ints are `std::from_chars`.
struct P<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> P<'a> {
    fn new(s: &'a str) -> Self {
        P { s: s.as_bytes(), i: 0 }
    }
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i] <= b' ' {
            self.i += 1;
        }
    }
    fn c(&mut self, ch: u8) -> Option<()> {
        (self.s.get(self.i) == Some(&ch)).then(|| self.i += 1)
    }
    fn lit(&mut self, l: &str) -> Option<()> {
        self.s[self.i..].starts_with(l.as_bytes()).then(|| self.i += l.len())
    }
    fn int_radix(&mut self, radix: u32) -> Option<i64> {
        let start = self.i;
        if self.s.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        let digits = self.i;
        while self.i < self.s.len() && (self.s[self.i] as char).is_digit(radix) {
            self.i += 1;
        }
        if self.i == digits {
            self.i = start;
            return None;
        }
        i64::from_str_radix(std::str::from_utf8(&self.s[start..self.i]).ok()?, radix).ok()
    }
    fn int(&mut self) -> Option<i64> {
        self.int_radix(10)
    }
    fn until(&mut self, end: u8) -> Option<String> {
        let rel = self.s[self.i..].iter().position(|&b| b == end)?;
        let v = String::from_utf8_lossy(&self.s[self.i..self.i + rel]).into_owned();
        self.i += rel + 1;
        Some(v)
    }
    fn done(&self) -> bool {
        self.i == self.s.len()
    }
    fn front(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }
    fn rest(&mut self) -> String {
        let r = String::from_utf8_lossy(&self.s[self.i..]).into_owned();
        self.i = self.s.len();
        r
    }
}

fn full(p: &P) -> Option<()> {
    p.done().then_some(())
}

/// CREG/CGREG/CEREG::parse: `(state, Some((lac, ci, act)))`.
type Location = (i64, i64, i64);
fn parse_reg(s: &str) -> Option<(i64, Option<Location>)> {
    let mut p = P::new(s);
    let commas = s.matches(',').count();
    let loc = |p: &mut P| -> Option<Location> {
        p.c(b',')?;
        p.c(b'"')?;
        let lac = i64::from_str_radix(&p.until(b'"')?, 16).ok()?;
        p.c(b',')?;
        p.c(b'"')?;
        let ci = i64::from_str_radix(&p.until(b'"')?, 16).ok()?;
        p.c(b',')?;
        let act = p.int()?;
        Some((lac, ci, act))
    };
    let r = match commas {
        0 => (p.int()?, None),
        1 => {
            p.int()?;
            p.c(b',')?;
            (p.int()?, None)
        }
        3 => {
            let st = p.int()?;
            (st, Some(loc(&mut p)?))
        }
        4 => {
            p.int()?;
            p.c(b',')?;
            let st = p.int()?;
            (st, Some(loc(&mut p)?))
        }
        _ => return None,
    };
    full(&p)?;
    Some(r)
}

#[derive(Debug, Default, PartialEq)]
struct Cops {
    mode: i64,
    numeric: String,
    /// (long, short, numeric) of the current operator.
    current: Option<(String, String, String)>,
}

/// COPS::parse (non-list forms).
fn parse_cops(s: &str) -> Option<Cops> {
    let mut p = P::new(s);
    p.lit("+COPS:")?;
    p.ws();
    let mode = p.int()?;
    p.c(b',')?;
    let n = p.int()?;
    p.c(b',')?;
    let first = p.until(b'\r')?;
    if n == 2 && p.done() {
        return Some(Cops { mode, numeric: first, current: None });
    }
    if n != 0 {
        return None;
    }
    if first == "0" && p.done() {
        return Some(Cops { mode, ..Cops::default() });
    }
    p.lit("+COPS:")?;
    p.ws();
    p.lit("0,1,")?;
    let short = p.until(b'\r')?;
    p.lit("+COPS:")?;
    p.ws();
    p.lit("0,2,")?;
    let numeric = p.until(b'\r')?;
    full(&p)?;
    Some(Cops { mode, numeric: String::new(), current: Some((first, short, numeric)) })
}

/// CLCC::parse: (index, dir, state, mode, mpty, number, toa) per call.
type ClccCall = (i64, i64, i64, i64, i64, String, i64);
fn parse_clcc(s: &str) -> Option<Vec<ClccCall>> {
    let mut p = P::new(s);
    let mut calls = Vec::new();
    while !p.done() {
        p.lit("+CLCC:")?;
        p.ws();
        let idx = p.int()?;
        p.c(b',')?;
        let dir = p.int()?;
        p.c(b',')?;
        let st = p.int()?;
        p.c(b',')?;
        let mode = p.int()?;
        p.c(b',')?;
        let mpty = p.int()?;
        p.c(b',')?;
        let number = p.until(b',')?;
        let toa = p.int()?;
        p.c(b'\r')?;
        calls.push((idx, dir, st, mode, mpty, number, toa));
    }
    Some(calls)
}

/// CSQ::parse: the values, if their count is one the HAL accepts.
fn parse_csq(s: &str) -> Option<Vec<i64>> {
    let mut p = P::new(s);
    let mut v = vec![p.int()?];
    while !p.done() && v.len() < 22 {
        p.c(b',')?;
        v.push(p.int()?);
    }
    full(&p)?;
    [12, 13, 14, 22].contains(&v.len()).then_some(v)
}

/// CTEC::getCurrentModemTechnology.
fn ctec_current(s: &str) -> Option<u32> {
    let values: Vec<&str> = s.split(',').collect();
    if values.is_empty() || values.len() > 2 || values == ["DONE"] {
        return None;
    }
    let m: u32 = values[0].parse().ok()?;
    (0..=6).find(|i| m & (1 << i) != 0)
}

/// CGCONTRDP::parse: (cid, bearer, apn, local, prefix, gateway, dns1).
type Rdp = (i64, i64, String, String, i64, String, String);
fn parse_cgcontrdp(s: &str) -> Option<Rdp> {
    let mut p = P::new(s);
    let cid = p.int()?;
    p.c(b',')?;
    let bearer = p.int()?;
    p.c(b',')?;
    p.c(b'"')?;
    let apn = p.until(b'"')?;
    p.c(b',')?;
    let local = p.until(b',')?;
    let gw = p.until(b',')?;
    let dns = p.rest();
    let mut lp = P::new(&local);
    let (addr, prefix) = match (lp.until(b'/'), lp.int()) {
        (Some(a), Some(n)) if lp.done() => (a, n),
        _ => (local.clone(), 0),
    };
    Some((cid, bearer, apn, addr, prefix, gw, dns))
}

/// Two comma-separated ints with a full match (CCWA, CLIP, CLIR, CUSATD-like parsers).
fn two_ints(s: &str) -> Option<(i64, i64)> {
    let mut p = P::new(s);
    let a = p.int()?;
    p.c(b',')?;
    p.ws();
    let b = p.int()?;
    full(&p)?;
    Some((a, b))
}

fn one_int(s: &str) -> Option<i64> {
    let mut p = P::new(s);
    let v = p.int()?;
    full(&p)?;
    Some(v)
}

/// CTZV::parse: (yy, mm, dd, hh, mi, ss, offset quarters, dst, name).
type Ctzv = (i64, i64, i64, i64, i64, i64, i64, u8, String);
fn parse_ctzv(s: &str) -> Option<Ctzv> {
    let mut p = P::new(s);
    p.ws();
    let yy = p.int()?;
    p.c(b'/')?;
    let mo = p.int()?;
    p.c(b'/')?;
    let d = p.int()?;
    p.c(b':')?;
    let h = p.int()?;
    p.c(b':')?;
    let mi = p.int()?;
    p.c(b':')?;
    let sec = p.int()?;
    let sign = p.front()?;
    p.i += 1;
    let q = p.int()?;
    p.c(b':')?;
    let dst = p.front()?;
    p.i += 1;
    p.c(b':')?;
    let q = match sign {
        b'+' => q,
        b'-' => -q,
        _ => return None,
    };
    Some((yy, mo, d, h, mi, sec, q, dst, p.rest()))
}

// ---- rig ------------------------------------------------------------------------------------

const T0: u64 = 1_730_826_092; // 2024-11-05 17:01:32 UTC

fn fixed_clock() -> u64 {
    T0
}

struct Rig {
    modem: Modem,
    hal: HalReader,
}

impl Rig {
    fn new() -> Self {
        let mut modem = Modem::new();
        modem.set_clock(fixed_clock);
        Rig { modem, hal: HalReader::default() }
    }

    /// A rig that has run the HAL's init sequence and turned the radio on.
    fn up() -> Self {
        let mut r = Rig::new();
        for cmd in INIT {
            r.send(cmd);
        }
        r.send("AT+CFUN=1");
        r.send("AT+CFUN?");
        r
    }

    /// Writes a request the way `RequestPipe` does (text + CR) and parses the reply; all
    /// output must be consumed.
    fn send(&mut self, cmd: &str) -> Vec<Resp> {
        let out = self.modem.input(format!("{cmd}\r").as_bytes());
        self.read(&out)
    }

    fn read(&mut self, bytes: &[u8]) -> Vec<Resp> {
        let r = self.hal.feed(bytes);
        assert!(self.hal.buf.is_empty(), "unparsed output: {:?}", self.hal.buf);
        r
    }

    /// The first `+id` payload in a reply.
    fn cmd(&mut self, cmd: &str, id: &str) -> String {
        let resps = self.send(cmd);
        find(&resps, id).unwrap_or_else(|| panic!("no +{id} in reply to {cmd}: {resps:?}"))
    }

    fn calls(&mut self) -> Vec<ClccCall> {
        let resps = self.send("AT+CLCC");
        // A multi-line +CLCC block swallows its OK, as in the HAL; a RING may follow.
        let end = resps.iter().rev().find(|x| **x != Resp::Ring);
        assert!(matches!(end, Some(Resp::Ok | Resp::Cmd("CLCC", _))), "{resps:?}");
        find(&resps, "CLCC").map(|p| parse_clcc(&p).expect("CLCC parses")).unwrap_or_default()
    }
}

fn find(resps: &[Resp], id: &str) -> Option<String> {
    resps.iter().find_map(|r| match r {
        Resp::Cmd(i, p) if *i == id => Some(p.clone()),
        _ => None,
    })
}

const INIT: [&str; 15] = [
    "ATE0Q0V1",
    "AT+CMEE=1",
    "AT+CREG=2",
    "AT+CGREG=2",
    "AT+CEREG=2",
    "AT+CCWA=1",
    "AT+CMOD=0",
    "AT+CMUT=0",
    "AT+CSSN=0,1",
    "AT+COLP=0",
    "AT+CSCS=\"HEX\"",
    "AT+CUSD=1",
    "AT+CGEREP=1,0",
    "AT+CMGF=0",
    "AT+CFUN?",
];

fn luhn_ok(digits: &str) -> bool {
    let sum: u32 = digits
        .bytes()
        .rev()
        .enumerate()
        .map(|(i, b)| {
            let d = u32::from(b - b'0');
            if i % 2 == 1 {
                let x = d * 2;
                x / 10 + x % 10
            } else {
                d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

// ---- tests ----------------------------------------------------------------------------------

#[test]
fn init_sequence_gets_ok_for_every_command() {
    let mut r = Rig::new();
    for cmd in INIT {
        let resps = r.send(cmd);
        // The init conversation waits for OK; nothing may be an error.
        assert!(resps.contains(&Resp::Ok), "{cmd}: {resps:?}");
        assert!(!resps.iter().any(|x| matches!(x, Resp::Error | Resp::Cmd("CME ERROR", _))), "{cmd}: {resps:?}");
    }
    assert!(r.modem.initialized());
}

#[test]
fn end_of_init_pushes_time_and_network_state() {
    let mut r = Rig::new();
    for cmd in &INIT[..13] {
        r.send(cmd);
    }
    r.modem.set_timezone(-480, false, "America/Los_Angeles");
    let resps = r.send("AT+CMGF=0");
    assert_eq!(resps[0], Resp::Ok);
    let ctzv = parse_ctzv(&find(&resps, "CTZV").expect("CTZV")).expect("CTZV parses");
    assert_eq!(ctzv, (24, 11, 5, 17, 1, 32, -32, b'0', "America!Los_Angeles".to_string()));
    // Radio still off: not registered, signal unknown.
    assert_eq!(parse_reg(&find(&resps, "CREG").expect("CREG")).expect("CREG").0, 0);
    let csq = parse_csq(&find(&resps, "CSQ").expect("CSQ")).expect("CSQ parses");
    assert_eq!(csq[0], 99);
    assert_eq!(r.cmd("AT+CFUN?", "CFUN"), "0");
}

#[test]
fn radio_on_registers_home_on_lte() {
    let mut r = Rig::new();
    for cmd in INIT {
        r.send(cmd);
    }
    // RadioModem::setRadioPowerImpl writes both without waiting.
    let out = r.modem.input(b"AT+CFUN=1\rAT+CFUN?\r");
    let resps = r.read(&out);
    assert_eq!(one_int(&find(&resps, "CFUN").expect("CFUN")), Some(1));
    let creg = parse_reg(&find(&resps, "CREG").expect("CREG")).expect("CREG parses");
    assert_eq!(creg, (1, Some((3, 91, 7))));
    for id in ["CGREG", "CEREG"] {
        let reg = parse_reg(&find(&resps, id).expect(id)).expect("parses");
        assert_eq!(reg.0, 1, "{id}");
    }
    let csq = parse_csq(&find(&resps, "CSQ").expect("CSQ")).expect("CSQ parses");
    assert_eq!(csq.len(), 12);
    assert_eq!(&csq[..2], &[20, 99]);
    // LTE: signal 20, rsrp -105 dBm.
    assert_eq!(&csq[7..9], &[20, 105]);
    // Query forms (n = 2): unsolMode,state,lac,ci,act.
    let q = parse_reg(&r.cmd("AT+CREG?", "CREG")).expect("CREG? parses");
    assert_eq!(q, (1, Some((3, 91, 7))));
}

#[test]
fn operator_queries_parse_under_the_hal_grammar() {
    let mut r = Rig::up();
    let compound = "AT+COPS=3,0;+COPS?;+COPS=3,1;+COPS?;+COPS=3,2;+COPS?";
    let resps = r.send(compound);
    let cops = parse_cops(&find(&resps, "COPS").expect("COPS")).expect("COPS parses");
    let (long, short, numeric) = cops.current.expect("current operator");
    assert_eq!((long.as_str(), short.as_str()), ("Android", "Android"));
    assert_eq!((&numeric[..3], &numeric[3..]), ("310", "260"));
    // The multi-line block consumed the OK; nothing else follows.
    assert_eq!(resps.len(), 1);

    // getNetworkSelectionMode: standalone COPS? after the compound left format 2.
    let sel = parse_cops(&r.cmd("AT+COPS?", "COPS")).expect("COPS? parses");
    assert_eq!((sel.mode, sel.numeric.as_str()), (0, "310260"));

    r.modem.set_voice_state(RegState::Roaming);
    let cops = parse_cops(&find(&r.send(compound), "COPS").expect("COPS")).expect("parses");
    assert_eq!(cops.current.expect("current").2, "310295");

    // No network: the HAL accepts a CME error for both queries.
    r.send("AT+CFUN=0");
    assert_eq!(r.cmd(compound, "CME ERROR"), "30");
    assert_eq!(r.cmd("AT+COPS?", "CME ERROR"), "30");
}

#[test]
fn sim_and_identity_queries() {
    let mut r = Rig::up();
    assert_eq!(r.cmd("AT+CPIN?", "CPIN"), "READY");
    let raw = |r: &mut Rig, cmd: &str| match r.send(cmd).as_slice() {
        [Resp::Raw(v)] => v.clone(),
        other => panic!("{cmd}: {other:?}"),
    };
    assert_eq!(raw(&mut r, "AT+CICCID"), "89014103211118510720");
    let imsi = raw(&mut r, "AT+CIMI");
    assert_eq!((imsi.len(), &imsi[..6]), (15, "310260"));
    // getImei: imei = substr(0, 15), svn = substr(15, 2).
    let imeisv = raw(&mut r, "AT+CGSN=2");
    assert_eq!(imeisv.len(), 17);
    assert!(luhn_ok(&imeisv[..15]));

    // CTEC: supported list, current = LTE, set.
    let techs: Vec<i64> = r.cmd("AT+CTEC=?", "CTEC").split(',').map(|v| v.parse().expect("int")).collect();
    assert_eq!(techs, [0, 1, 5]);
    assert_eq!(ctec_current(&r.cmd("AT+CTEC?", "CTEC")), Some(5));
    assert_eq!(r.cmd("AT+CTEC=2,\"23\"", "CTEC"), "2");
    assert_eq!(ctec_current(&r.cmd("AT+CTEC?", "CTEC")), Some(1));
    assert_eq!(r.cmd("AT+CTEC=2,\"23\"", "CTEC"), "DONE");

    // SIM file reads through CRSM: sw1,sw2,hex.
    let crsm = r.cmd("AT+CRSM=176,12258,0,0,10", "CRSM");
    assert_eq!(crsm, "144,0,98101430121181157002");
    let cpinr = r.cmd("AT+CPINR=\"SIM PIN\"", "CPINR");
    let mut p = P::new(&cpinr);
    p.until(b',').expect("code");
    assert_eq!(two_ints(&p.rest()), Some((3, 3)));
    assert_eq!(r.cmd("AT+CLCK=\"SC\",2,\"\",7", "CLCK"), "0");
    assert_eq!(two_ints(&r.cmd("AT+CUSATD?", "CUSATD")), Some((0, 1)));
}

/// Every request the HAL sends, with the response its conversation filter waits for.
#[test]
fn hal_requests_get_the_shape_their_filters_wait_for() {
    let mut r = Rig::up();
    let is_ok = |x: &Resp| *x == Resp::Ok;
    let cme = |x: &Resp| matches!(x, Resp::Cmd("CME ERROR", _));
    type Accept<'a> = &'a dyn Fn(&Resp) -> bool;
    let cases: &[(&str, Accept)] = &[
        // RadioNetwork
        ("AT+COPS=0", &is_ok),
        ("AT+COPS=1,2,\"310260\",7", &|x| is_ok(x) || cme(x)),
        ("AT+COPS=1,2,\"310260\"", &|x| is_ok(x) || cme(x)),
        ("AT+CSSN=1,1", &is_ok),
        ("AT+WRMP=1", &is_ok),
        ("AT+WRMP?", &|x| matches!(x, Resp::Cmd("WRMP", p) if one_int(p).is_some())),
        ("AT+CPWD=\"SC\",\"1234\",\"4321\"", &|x| is_ok(x) || cme(x)),
        ("AT+CTEC=32,\"23\"", &|x| matches!(x, Resp::Cmd("CTEC", _))),
        // RadioSim
        ("AT+CCSS?", &|x| matches!(x, Resp::Cmd("CCSS", p) if one_int(p).is_some())),
        ("AT+CCSS=0", &is_ok),
        ("AT+CPIN=1234", &|x| is_ok(x) || cme(x)),
        ("AT+CLCK=\"SC\",1,\"1234\"", &|x| is_ok(x) || cme(x)),
        ("AT+CCHO=A000000151000000", &|x| cme(x) || matches!(x, Resp::Raw(_))),
        ("AT+CCHC=1", &|x| cme(x) || matches!(x, Resp::Cmd("CCHC", _))),
        ("AT+CSIM=10,\"0070000001\"", &|x| cme(x) || matches!(x, Resp::Cmd("CSIM", _))),
        ("AT+CSIM=8,00A40004", &|x| cme(x) || matches!(x, Resp::Cmd("CSIM", _))),
        ("AT+CGLA=1,10,00A4000400", &cme),
        ("AT+CRSM=192,28480,0,0,15", &|x| matches!(x, Resp::Cmd("CRSM", _))),
        ("AT+CRSM=192,20272,0,0,15", &|x| matches!(x, Resp::Cmd("CRSM", p) if p == "148,4")),
        ("AT+CRSM=242,0,0,0,0", &|x| matches!(x, Resp::Cmd("CRSM", _))),
        ("AT^MBAU=\"0011\"", &|x| cme(x) || matches!(x, Resp::Cmd("MBAU", _))),
        ("AT+CUSATE=\"D30182\"", &|x| matches!(x, Resp::Cmd("CUSATE", _))),
        ("AT+CUSATT=\"810301\"", &|x| matches!(x, Resp::Cmd("CUSATT", p) if one_int(p).is_some())),
        // RadioVoice
        ("AT+CCWA=1,2,1", &|x| matches!(x, Resp::Cmd("CCWA", p) if two_ints(p) == Some((1, 1)))),
        ("AT+CCWA=1,0,1", &is_ok),
        ("AT+CLIP?", &|x| matches!(x, Resp::Cmd("CLIP", p) if two_ints(p).is_some())),
        ("AT+CLIR?", &|x| matches!(x, Resp::Cmd("CLIR", p) if two_ints(p).is_some())),
        ("AT+CLIR: 1", &is_ok),
        ("AT+CMUT=1", &is_ok),
        // getMute polls AT+CLCC but waits for +CMUT.
        ("AT+CLCC", &|x| matches!(x, Resp::Cmd("CMUT", p) if one_int(p) == Some(1))),
        ("AT+WSOS=0", &is_ok),
        ("AT+CCFCU=0,2,2,129,\"\",1", &|x| matches!(x, Resp::Cmd("CCFCU", _)) || is_ok(x)),
        ("AT+CCFCU=0,3,2,129,\"5551234\",1", &is_ok),
        // RadioMessaging
        ("AT+CNMA=1", &is_ok),
        ("AT+CNMA=2", &is_ok),
        ("AT+CMGD=3", &|x| is_ok(x) || cme(x)),
        ("AT+CSCA?", &|x| matches!(x, Resp::Cmd("CSCA", _))),
        ("AT+CSCA=\"+15555000001\",0", &is_ok),
        ("AT+CSCB=0,\"1,4352-4356\",\"0-255\"", &is_ok),
        ("AT+CSCB?", &|x| matches!(x, Resp::Cmd("CSCB", _))),
        // RadioModem
        ("AT+CFUN=1", &is_ok),
    ];
    for (cmd, accept) in cases {
        let resps = r.send(cmd);
        assert!(resps.iter().any(accept), "{cmd}: {resps:?}");
    }

    // Call forwarding round trip under the CCFCU grammar.
    let ccfcu = r.cmd("AT+CCFCU=0,2,2,129,\"\",1", "CCFCU");
    let mut p = P::new(&ccfcu);
    p.lit("+CCFCU:").expect("prefix");
    p.ws();
    assert_eq!(p.int(), Some(1));
    assert!(ccfcu.contains("\"5551234\""));

    // SMSC and broadcast config round trip under the CSCA/CSCB grammar.
    let csca = r.cmd("AT+CSCA?", "CSCA");
    let mut p = P::new(&csca);
    assert_eq!(p.until(b',').as_deref(), Some("\"+15555000001\""));
    assert_eq!(p.int(), Some(145));
    assert_eq!(r.cmd("AT+CSCB?", "CSCB"), "0,\"1,4352-4356\",\"0-255\"");
}

#[test]
fn unknown_commands_get_a_cme_error() {
    let mut r = Rig::up();
    assert_eq!(r.cmd("AT+XYZZY", "CME ERROR"), "4");
    assert_eq!(r.send("AT"), [Resp::Ok]);
    assert_eq!(r.send("ATZ"), [Resp::Ok]);
    // Non-AT lines are ignored.
    assert!(r.send("hello").is_empty());
}

fn submit_gsm7(to: &str, text: &str, udh: Option<[u8; 6]>) -> Vec<u8> {
    let mut septets = gsm7_encode(text).expect("gsm7");
    let first = if udh.is_some() { 0x41 } else { 0x01 };
    let mut tpdu = vec![first, 0x00];
    tpdu.extend(encode_address(to));
    tpdu.extend([0x00, 0x00]);
    match udh {
        Some(h) => {
            let mut all = vec![0u8; 7];
            all.append(&mut septets);
            let mut ud = pack_septets(&all);
            ud[..6].copy_from_slice(&h);
            tpdu.push(all.len() as u8);
            tpdu.extend(ud);
        }
        None => {
            tpdu.push(septets.len() as u8);
            tpdu.extend(pack_septets(&septets));
        }
    }
    tpdu
}

/// RadioMessaging::sendSmsImpl: CMGS=<tpdu len>, prompt, smsc + pdu + Ctrl-Z (+ CR).
fn send_sms(r: &mut Rig, tpdu: &[u8]) -> i64 {
    assert_eq!(r.send(&format!("AT+CMGS={}", tpdu.len())), [Resp::Prompt]);
    let resps = r.send(&format!("00{}\x1a", hex(tpdu)));
    assert_eq!(resps.last(), Some(&Resp::Ok));
    one_int(&find(&resps, "CMGS").expect("CMGS")).expect("CMGS parses")
}

#[test]
fn cmgs_round_trip_logs_decoded_messages() {
    let mut r = Rig::up();
    // A classic reference SUBMIT: to +46708251358, "hellohello", relative validity.
    let reference = pdu::unhex("11000B916407281553F80000AA0AE8329BFD4697D9EC37").expect("hex");
    assert_eq!(send_sms(&mut r, &reference), 0);

    // UCS2 with an emoji (surrogate pair), no validity period.
    let mut ucs2 = vec![0x01, 0x00];
    ucs2.extend(encode_address("+15555215554"));
    ucs2.extend([0x00, 0x08, 10]);
    ucs2.extend(pdu::unhex("004800690020D83DDE00").expect("hex"));
    assert_eq!(send_sms(&mut r, &ucs2), 1);

    // A two-part concatenated message.
    let p1 = submit_gsm7("5551234", "Hello, ", Some([0x05, 0x00, 0x03, 0x42, 2, 1]));
    let p2 = submit_gsm7("5551234", "world [€]", Some([0x05, 0x00, 0x03, 0x42, 2, 2]));
    send_sms(&mut r, &p2);
    send_sms(&mut r, &p1);

    let sent = r.modem.sent_sms();
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert_eq!((sent[0].to.as_str(), sent[0].text.as_str(), sent[0].encoding), ("+46708251358", "hellohello", SmsEncoding::Gsm7));
    assert_eq!((sent[1].to.as_str(), sent[1].text.as_str(), sent[1].encoding), ("+15555215554", "Hi 😀", SmsEncoding::Ucs2));
    assert_eq!((sent[2].to.as_str(), sent[2].text.as_str(), sent[2].parts), ("5551234", "Hello, world [€]", 2));
}

#[test]
fn writing_to_sim_prompts_and_answers_cmgw() {
    let mut r = Rig::up();
    // The HAL literally sends "%d,%d" (a std::format bug).
    assert_eq!(r.send("AT+CMGW=%d,%d"), [Resp::Prompt]);
    let resps = r.send("0011000B916407281553F80000AA0AE8329BFD4697D9EC37\x1a");
    assert!(find(&resps, "CMGW").is_some_and(|p| one_int(&p).is_some()));
}

fn deliver(r: &mut Rig, from: &str, text: &str) -> Vec<pdu::Deliver> {
    let out = r.modem.sms_deliver(from, text).expect("deliver");
    // "+CMT: <tpdu len>" must agree with the PDU.
    let s = String::from_utf8(out.clone()).expect("ascii");
    for chunk in s.split("+CMT: ").skip(1) {
        let (len, rest) = chunk.split_once('\r').expect("header");
        let pdu_hex = rest.split('\r').next().expect("pdu");
        assert_eq!(len.parse::<usize>().expect("len") * 2 + 2, pdu_hex.len());
    }
    r.read(&out)
        .into_iter()
        .map(|x| match x {
            Resp::Cmt(pdu) => decode_deliver(&pdu).expect("decodes"),
            other => panic!("expected +CMT, got {other:?}"),
        })
        .collect()
}

#[test]
fn sms_deliver_builds_pdus_that_decode_back() {
    let mut r = Rig::up();
    let d = deliver(&mut r, "5551234", "Hello from the host {€}");
    assert_eq!(d.len(), 1);
    assert_eq!((d[0].from.as_str(), d[0].text.as_str(), d[0].encoding), ("5551234", "Hello from the host {€}", SmsEncoding::Gsm7));
    // 2024-11-05 17:01:32 UTC.
    assert_eq!(d[0].scts, [0x42, 0x11, 0x50, 0x71, 0x10, 0x23, 0x00]);

    let d = deliver(&mut r, "+447700900123", "Emoji 😀 and ü");
    assert_eq!((d[0].from.as_str(), d[0].text.as_str(), d[0].encoding), ("+447700900123", "Emoji 😀 and ü", SmsEncoding::Ucs2));

    let long = "0123456789".repeat(20);
    let d = deliver(&mut r, "Bank", &long);
    assert_eq!(d.len(), 2);
    assert!(d.iter().all(|x| x.from == "Bank"));
    let c: Vec<_> = d.iter().map(|x| x.concat.expect("concat")).collect();
    assert_eq!((c[0].total, c[0].seq, c[1].seq), (2, 1, 2));
    assert_eq!(c[0].reference, c[1].reference);
    assert_eq!(d.iter().map(|x| x.text.as_str()).collect::<String>(), long);

    // acknowledgeLastIncomingGsmSms.
    assert_eq!(r.send("AT+CNMA=1"), [Resp::Ok]);
    assert!(r.modem.sms_deliver("", "x").is_err());
}

#[test]
fn incoming_call_answer_and_hang_up() {
    let mut r = Rig::up();
    let out = r.modem.call_incoming("5551234").expect("incoming");
    assert_eq!(r.read(&out), [Resp::Ring]);
    assert_eq!(r.calls(), [(1, 1, 4, 0, 0, "5551234".to_string(), 129)]);
    assert_eq!(r.modem.list_calls(), ["inbound from 5551234    : incoming"]);

    assert_eq!(r.send("ATA"), [Resp::Ok]);
    assert_eq!(r.calls()[0].2, 0);
    assert_eq!(r.modem.list_calls(), ["inbound from 5551234    : active"]);

    // A second caller waits; AT+CHLD=2 swaps.
    r.modem.call_incoming("+15550001111").expect("waiting");
    let calls = r.calls();
    assert_eq!((calls[1].2, calls[1].6), (5, 145));
    r.send("AT+CHLD=2");
    let states: Vec<i64> = r.calls().iter().map(|c| c.2).collect();
    assert_eq!(states, [1, 0]);
    r.send("AT+CHLD=12");
    assert_eq!(r.calls().len(), 1);

    assert_eq!(r.send("ATH"), [Resp::Ok]);
    assert!(r.calls().is_empty());
    assert_eq!(r.send("AT+CLCC"), [Resp::Cmd("CMUT", "0".into()), Resp::Ok]);
}

#[test]
fn outgoing_call_dials_alerts_and_connects() {
    let mut r = Rig::up();
    assert_eq!(r.send("ATD5551234;"), [Resp::Ok]);
    // First poll: dialing, then RING so the guest polls again and sees alerting.
    let resps = r.send("AT+CLCC");
    let calls = parse_clcc(&find(&resps, "CLCC").expect("CLCC")).expect("parses");
    assert_eq!(calls, [(1, 0, 2, 0, 0, "5551234".to_string(), 129)]);
    assert_eq!(resps.last(), Some(&Resp::Ring));
    assert_eq!(r.calls()[0].2, 3);
    assert_eq!(r.modem.list_calls(), ["outbound to  5551234    : ringing"]);

    let out = r.modem.call_accept("5551234").expect("accept");
    assert_eq!(r.read(&out), [Resp::Ring]);
    assert_eq!(r.calls()[0].2, 0);

    let out = r.modem.call_hold("5551234").expect("hold");
    assert_eq!(r.read(&out), [Resp::Ring]);
    assert_eq!(r.calls()[0].2, 1);

    assert!(r.modem.call_cancel("5551234").is_ok());
    assert!(r.calls().is_empty());
    assert_eq!(r.modem.call_cancel("5551234"), Err(ModemError::NoSuchCall("5551234".into())));

    // Busy only applies to outgoing calls; emergency dial strings carry a suffix.
    r.send("ATD112@,#;");
    assert_eq!(r.calls()[0].5, "112");
    assert!(r.modem.call_busy("112").is_ok());
    r.modem.call_incoming("999").expect("incoming");
    assert!(matches!(r.modem.call_busy("999"), Err(ModemError::NotOutbound(_))));

    // No radio, no calls.
    r.send("AT+CFUN=0");
    assert_eq!(r.cmd("ATD5551234;", "CME ERROR"), "30");
    assert_eq!(r.modem.call_incoming("5551234"), Err(ModemError::RadioOff));
}

#[test]
fn setup_data_call_conversation() {
    let mut r = Rig::up();
    // RadioData::setupDataCall: CGDCONT, then CGCONTRDP for the same cid.
    assert_eq!(r.send("AT+CGDCONT=1,\"IPV4V6\",\"fast.t-mobile.com\",,0,0"), [Resp::Ok]);
    let resps = r.send("AT+CGCONTRDP=1");
    assert_eq!(resps.last(), Some(&Resp::Ok));
    let rdp = parse_cgcontrdp(&find(&resps, "CGCONTRDP").expect("CGCONTRDP")).expect("parses");
    assert_eq!(rdp, (1, 5, "fast.t-mobile.com".to_string(), "10.0.2.15".to_string(), 24, "10.0.2.2".to_string(), "10.0.2.3".to_string()));
    // The context is defined and active.
    let cgdcont = r.cmd("AT+CGDCONT?", "CGDCONT");
    assert_eq!(cgdcont, "+CGDCONT: 1,\"IPV4V6\",\"fast.t-mobile.com\",,0,0\r");
    let out = String::from_utf8(r.modem.input(b"AT+CGACT?\r")).expect("ascii");
    assert_eq!(out, "+CGACT: 1,1\rOK\r");

    // Unknown cid and no data service come back as CME errors, which the HAL handles.
    assert_eq!(r.cmd("AT+CGCONTRDP=7", "CME ERROR"), "50");
    let out = r.modem.set_data_state(RegState::Unregistered);
    let resps = r.read(&out);
    assert_eq!(parse_reg(&find(&resps, "CGREG").expect("CGREG")).expect("parses").0, 0);
    assert_eq!(r.cmd("AT+CGCONTRDP=1", "CME ERROR"), "30");
}

#[test]
fn host_state_changes_produce_parseable_updates() {
    let mut r = Rig::up();
    let out = r.modem.set_voice_state(RegState::Searching);
    let creg = parse_reg(&find(&r.read(&out), "CREG").expect("CREG")).expect("parses");
    assert_eq!(creg.0, 2);
    assert_eq!(RegState::from_name("off"), Some(RegState::Unregistered));
    assert_eq!(r.modem.voice_state().name(), "searching");

    let out = r.modem.set_signal(31, 0);
    let csq = parse_csq(&find(&r.read(&out), "CSQ").expect("CSQ")).expect("parses");
    assert_eq!((csq[0], csq[1], csq[8]), (31, 0, 85));
    let out = r.modem.set_signal(99, 99);
    let csq = parse_csq(&find(&r.read(&out), "CSQ").expect("CSQ")).expect("parses");
    assert_eq!((csq[0], csq[8]), (99, i64::from(i32::MAX)));

    let out = r.modem.set_timezone(60, true, "Europe/Paris");
    let ctzv = parse_ctzv(&find(&r.read(&out), "CTZV").expect("CTZV")).expect("parses");
    assert_eq!((ctzv.6, ctzv.7, ctzv.8.as_str()), (4, b'1', "Europe!Paris"));

    let out = r.modem.network_status();
    assert_eq!(r.read(&out).len(), 4);
}

#[test]
fn partial_input_is_buffered() {
    let mut m = Modem::new();
    assert!(m.input(b"AT+CS").is_empty());
    assert!(m.input(b"Q").is_empty());
    let out = m.input(b"\r");
    assert!(out.starts_with(b"+CSQ: "), "{}", String::from_utf8_lossy(&out));

    // The whole init sequence fed a byte at a time gives the same output as line by line.
    let script: String = INIT.iter().map(|c| format!("{c}\r")).collect();
    let mut a = Modem::new();
    a.set_clock(fixed_clock);
    let mut b = Modem::new();
    b.set_clock(fixed_clock);
    let whole = a.input(script.as_bytes());
    let bytewise: Vec<u8> = script.bytes().flat_map(|x| b.input(&[x])).collect();
    assert_eq!(whole, bytewise);

    // An SMS PDU split across reads.
    let mut r = Rig::up();
    assert_eq!(r.send("AT+CMGS=23"), [Resp::Prompt]);
    let pdu = "0011000B916407281553F80000AA0AE8329BFD4697D9EC37\x1a\r";
    let (x, y) = pdu.split_at(20);
    assert!(r.modem.input(x.as_bytes()).is_empty());
    let out = r.modem.input(y.as_bytes());
    let resps = r.read(&out);
    assert!(find(&resps, "CMGS").is_some());
    assert_eq!(r.modem.sent_sms()[0].text, "hellohello");
}
