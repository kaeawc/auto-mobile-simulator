//! The simee unix socket API.
//!
//! One request per line, one response per line, JSON:
//!
//! ```text
//! → {"id": 1, "method": "tap", "params": {"device": "booted", "x": 600, "y": 1300}}
//! ← {"id": 1, "ok": true, "result": null}
//! ← {"id": 2, "ok": false, "error": "no device matches `nope`"}
//! ```
//!
//! `{"method": "help"}` lists every method with its parameters. Each connection gets its own
//! thread and its own service connection. Device methods need the iOS backend, which exists on
//! macOS hosts; elsewhere they fail with a clear reason.

mod android;
mod forward;
#[cfg(target_os = "macos")]
mod ios;
#[cfg(not(target_os = "macos"))]
#[path = "ios_unavailable.rs"]
mod ios;
mod methods;
mod params;
mod schema;
pub mod settle;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use forward::{HANDSHAKE_TIMEOUT, IN_CALLER, forwards, identity};
pub use methods::{METHODS, Session, call as call_method, platforms};
pub use schema::input_schema;
pub use sim_core::frame::encode_png;

/// The default socket path: `$SIMEE_SOCKET`, else `$TMPDIR/simee.sock`.
pub fn default_socket() -> PathBuf {
    if let Some(p) = std::env::var_os("SIMEE_SOCKET") {
        return PathBuf::from(p);
    }
    std::env::temp_dir().join("simee.sock")
}

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

/// A response line, serialized straight from the method's result (no copy into a new `Value`).
/// Fields in the order `json!` wrote them (sorted), so the bytes on the wire are unchanged.
#[derive(Serialize)]
struct Response<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    id: &'a Value,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a Value>,
}

/// Appends one method's response line (with its newline) to `out`.
fn respond(id: &Value, outcome: &Result<Value, String>, out: &mut Vec<u8>) {
    let response = match outcome {
        Ok(result) => Response { error: None, id, ok: true, result: Some(result) },
        Err(error) => Response { error: Some(error), id, ok: false, result: None },
    };
    // Serializing a `Value` and plain fields into memory cannot fail.
    serde_json::to_writer(&mut *out, &response).ok();
    out.push(b'\n');
}

/// Handles one request line, appending its response line (newline included) to `out`.
pub fn handle_line_into(session: &mut methods::Session, line: &str, out: &mut Vec<u8>) {
    match serde_json::from_str::<Request>(line) {
        Ok(req) => respond(&req.id, &methods::call(session, &req.method, &req.params), out),
        Err(e) => respond(&Value::Null, &Err(format!("bad request: {e}")), out),
    }
}

/// Handles one request line; always returns a response line.
pub fn handle_line(session: &mut methods::Session, line: &str) -> String {
    let mut out = Vec::new();
    handle_line_into(session, line, &mut out);
    out.pop();
    String::from_utf8(out).unwrap_or_default()
}

/// Requests are read through a buffer this large, and responses read back through one too.
const IO_BUFFER: usize = 64 * 1024;

fn serve_connection(stream: UnixStream, make: &(dyn Fn() -> methods::Session + Send + Sync)) {
    let Ok(reader) = stream.try_clone() else { return };
    let mut writer = stream;
    let mut session = make();
    let mut out = Vec::new();
    for line in BufReader::with_capacity(IO_BUFFER, reader).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        out.clear();
        handle_line_into(&mut session, &line, &mut out);
        if writer.write_all(&out).is_err() {
            break;
        }
        // Don't hold a multi-megabyte screenshot's buffer between requests.
        if out.capacity() > IO_BUFFER {
            out = Vec::new();
        }
    }
}

/// Listens on `path` (replacing a stale socket) and serves connections until the process exits.
pub fn serve(path: &Path) -> std::io::Result<()> {
    let listener = bind(path)?;
    for stream in listener.incoming().flatten() {
        std::thread::Builder::new().name("simee-api".into()).spawn(move || serve_connection(stream, &methods::Session::default))?;
    }
    Ok(())
}

/// Serves on a background thread; returns once the socket is bound.
pub fn serve_in_background(path: PathBuf) -> std::io::Result<()> {
    serve_in_background_with(path, methods::Session::default)
}

/// [`serve_in_background`] with each connection's session made by `make` (a test's own SDK and
/// AVD home, say).
pub fn serve_in_background_with(path: PathBuf, make: impl Fn() -> methods::Session + Send + Sync + 'static) -> std::io::Result<()> {
    let listener = bind(&path)?;
    let make = std::sync::Arc::new(make);
    std::thread::Builder::new().name("simee-api-accept".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let make = make.clone();
            let _ = std::thread::Builder::new().name("simee-api".into()).spawn(move || serve_connection(stream, &*make));
        }
    })?;
    Ok(())
}

fn bind(path: &Path) -> std::io::Result<UnixListener> {
    // Fix this server's identity now, before its executable can be rebuilt under it.
    identity();
    if path.exists() && UnixStream::connect(path).is_err() {
        std::fs::remove_file(path)?;
    }
    UnixListener::bind(path)
}

/// Sends one request and waits for its response.
pub fn call(path: &Path, method: &str, params: Value) -> Result<Value, String> {
    Client::connect(path).map_err(|e| format!("{}: {e}", path.display()))?.call(method, &params)
}

/// A connection to a running server: one request at a time, each answered before the next.
pub struct Client {
    stream: BufReader<UnixStream>,
    next_id: u64,
    line: Vec<u8>,
}

#[derive(Serialize)]
struct Outgoing<'a> {
    id: u64,
    method: &'a str,
    params: &'a Value,
}

impl Client {
    /// Connects to the server on `path`. A missing or stale socket (nothing listening) fails at
    /// once rather than waiting.
    pub fn connect(path: &Path) -> std::io::Result<Client> {
        Ok(Client::from_stream(UnixStream::connect(path)?))
    }

    fn from_stream(stream: UnixStream) -> Client {
        Client { stream: BufReader::with_capacity(IO_BUFFER, stream), next_id: 1, line: Vec::new() }
    }

    /// Calls one method; its result, or the server's error.
    pub fn call(&mut self, method: &str, params: &Value) -> Result<Value, String> {
        let mut request = serde_json::to_vec(&Outgoing { id: self.next_id, method, params }).map_err(|e| e.to_string())?;
        self.next_id += 1;
        request.push(b'\n');
        self.stream.get_mut().write_all(&request).map_err(|e| e.to_string())?;
        self.line.clear();
        self.stream.read_until(b'\n', &mut self.line).map_err(|e| e.to_string())?;
        let mut value: Value = serde_json::from_slice(&self.line).map_err(|e| format!("bad response: {e}"))?;
        if self.line.capacity() > IO_BUFFER {
            self.line = Vec::new();
        }
        if value["ok"] == true { Ok(value["result"].take()) } else { Err(value["error"].as_str().unwrap_or("error").to_owned()) }
    }
}

/// A socket path for tests: under the temp dir when it fits a `sockaddr_un` (104 bytes on macOS),
/// else under `/tmp`, since build wrappers point `TMPDIR` at long private directories.
#[cfg(test)]
pub(crate) fn test_socket(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(name);
    let path = if path.as_os_str().len() < 100 { path } else { PathBuf::from("/tmp").join(name) };
    std::fs::remove_file(&path).ok();
    path
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn help_and_errors_need_no_device() {
        let mut s = methods::Session::default();
        let help: Value = serde_json::from_str(&handle_line(&mut s, r#"{"id":7,"method":"help"}"#)).expect("json");
        assert_eq!(help["id"], 7);
        assert!(help["result"].as_array().is_some_and(|m| m.len() > 40));
        let bad: Value = serde_json::from_str(&handle_line(&mut s, "not json")).expect("json");
        assert_eq!(bad["ok"], false);
        let unknown: Value = serde_json::from_str(&handle_line(&mut s, r#"{"method":"nope"}"#)).expect("json");
        assert!(unknown["error"].as_str().is_some_and(|e| e.contains("unknown method")));
        let missing: Value = serde_json::from_str(&handle_line(&mut s, r#"{"method":"ca_debug","params":{"device":"x"}}"#)).expect("json");
        assert_eq!(missing["ok"], false);
    }

    #[test]
    fn serves_over_a_socket() {
        let path = crate::test_socket(&format!("simee-api-test-{}.sock", std::process::id()));
        serve_in_background(path.clone()).expect("bind");
        let result = call(&path, "help", Value::Null).expect("help");
        assert!(result.as_array().is_some_and(|m| m.iter().any(|x| x["method"] == "tap")));
        std::fs::remove_file(path).ok();
    }

    /// The response line as it was built before: a `json!` object, then a string.
    fn old_response(id: &Value, outcome: &Result<Value, String>) -> String {
        match outcome {
            Ok(result) => serde_json::json!({"id": id, "ok": true, "result": result}),
            Err(error) => serde_json::json!({"id": id, "ok": false, "error": error}),
        }
        .to_string()
    }

    #[test]
    fn responses_are_the_same_bytes_as_before() {
        let cases = [
            (json!(7), Ok(Value::Null)),
            (Value::Null, Ok(json!({"width": 1206, "height": 2622.5, "png": "iVBOR\"\\\u{1}é"}))),
            (json!("abc"), Ok(json!([1, "two", {"z": 1, "a": [true, null]}]))),
            (json!({"k": 1}), Err("no device matches `nope`\n\"quoted\"".to_owned())),
            (Value::Null, Err(String::new())),
        ];
        for (id, outcome) in &cases {
            let mut out = Vec::new();
            respond(id, outcome, &mut out);
            assert_eq!(String::from_utf8(out).expect("utf-8"), old_response(id, outcome) + "\n");
        }
        let mut s = methods::Session::default();
        assert_eq!(handle_line(&mut s, "not json"), old_response(&Value::Null, &Err(bad_request("not json"))));
    }

    fn bad_request(line: &str) -> String {
        format!("bad request: {}", serde_json::from_str::<Request>(line).err().expect("bad"))
    }

    /// A server answering every request with `result` (or `error` for method `fail`), with the
    /// old or the new response path.
    fn fixed_server(tag: &str, result: Value, old: bool) -> PathBuf {
        let path = crate::test_socket(&format!("simee-api-{tag}-{}.sock", std::process::id()));
        let listener = bind(&path).expect("bind");
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let result = result.clone();
                std::thread::spawn(move || {
                    let mut writer = stream.try_clone().expect("clone");
                    let mut out = Vec::new();
                    for line in BufReader::new(stream).lines() {
                        let req: Request = serde_json::from_str(&line.expect("line")).expect("request");
                        let outcome = if req.method == "fail" { Err("it failed".to_owned()) } else { Ok(result.clone()) };
                        if old {
                            let mut line = old_response(&req.id, &outcome);
                            line.push('\n');
                            writer.write_all(line.as_bytes()).expect("write");
                        } else {
                            out.clear();
                            respond(&req.id, &outcome, &mut out);
                            writer.write_all(&out).expect("write");
                        }
                    }
                });
            }
        });
        path
    }

    /// The client as it was: a new connection, `read_line`, then the result cloned out.
    fn old_call(path: &Path, method: &str, params: Value) -> Result<Value, String> {
        let mut stream = UnixStream::connect(path).map_err(|e| e.to_string())?;
        let mut line = json!({"id": 1, "method": method, "params": params}).to_string();
        line.push('\n');
        stream.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).map_err(|e| e.to_string())?;
        let value: Value = serde_json::from_str(&response).map_err(|e| format!("bad response: {e}"))?;
        if value["ok"] == true { Ok(value["result"].clone()) } else { Err(value["error"].as_str().unwrap_or("error").to_owned()) }
    }

    #[test]
    fn a_client_keeps_its_connection_across_calls() {
        let shot = json!({"width": 2, "height": 1, "png": "x".repeat(200_000)});
        let path = fixed_server("client", shot.clone(), false);
        let mut client = Client::connect(&path).expect("connect");
        for _ in 0..3 {
            assert_eq!(client.call("screenshot", &json!({"device": "booted"})), Ok(shot.clone()));
            assert_eq!(client.call("fail", &Value::Null), Err("it failed".to_owned()));
        }
        assert_eq!(call(&path, "screenshot", Value::Null), old_call(&path, "screenshot", Value::Null));
        std::fs::remove_file(&path).ok();
        let gone = Client::connect(&path).err().expect("nothing listening");
        assert_eq!(gone.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn a_stale_socket_fails_at_once() {
        let path = crate::test_socket(&format!("simee-api-stale-{}.sock", std::process::id()));
        drop(UnixListener::bind(&path).expect("bind"));
        let start = std::time::Instant::now();
        assert!(Client::connect(&path).is_err(), "the listener is gone");
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        std::fs::remove_file(&path).ok();
    }

    /// A 3 MB screenshot response's round trip over the socket, old path against new (server
    /// response building and client parsing), the median of 15 runs each, interleaved.
    ///     cargo test --release -p sim-api large_response_round_trip -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn large_response_round_trip() {
        let png: String = (0..3 << 20).map(|i: u32| char::from(b'A' + (i.wrapping_mul(2_654_435_761) >> 27) as u8)).collect();
        let shot = json!({"width": 1206, "height": 2622, "png": png});
        let (old_path, new_path) = (fixed_server("bench-old", shot.clone(), true), fixed_server("bench-new", shot.clone(), false));
        let mut client = Client::connect(&new_path).expect("connect");
        let (mut old, mut new, mut new_kept) = (Vec::new(), Vec::new(), Vec::new());
        let time = |f: &mut dyn FnMut() -> Value| {
            let t = std::time::Instant::now();
            assert_eq!(f()["png"].as_str().map(str::len), Some(3 << 20));
            t.elapsed().as_secs_f64() * 1000.0
        };
        for _ in 0..15 {
            old.push(time(&mut || old_call(&old_path, "screenshot", Value::Null).expect("old")));
            new.push(time(&mut || call(&new_path, "screenshot", Value::Null).expect("new")));
            new_kept.push(time(&mut || client.call("screenshot", &Value::Null).expect("kept")));
        }
        let median = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        println!(
            "3 MB response: old {:.2} ms, new {:.2} ms, new on a kept connection {:.2} ms",
            median(&mut old),
            median(&mut new),
            median(&mut new_kept)
        );
        std::fs::remove_file(old_path).ok();
        std::fs::remove_file(new_path).ok();
    }
}
