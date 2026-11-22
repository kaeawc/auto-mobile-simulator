//! The sim-rs unix socket API.
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
//! thread and its own service connection.

mod methods;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

pub use methods::{METHODS, Session, call as call_method, encode_png};

/// The default socket path: `$SIM_RS_SOCKET`, else `$TMPDIR/sim-rs.sock`.
pub fn default_socket() -> PathBuf {
    if let Some(p) = std::env::var_os("SIM_RS_SOCKET") {
        return PathBuf::from(p);
    }
    std::env::temp_dir().join("sim-rs.sock")
}

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

/// Handles one request line; always returns a response line.
pub fn handle_line(session: &mut methods::Session, line: &str) -> String {
    let response = match serde_json::from_str::<Request>(line) {
        Ok(req) => match methods::call(session, &req.method, &req.params) {
            Ok(result) => json!({"id": req.id, "ok": true, "result": result}),
            Err(error) => json!({"id": req.id, "ok": false, "error": error}),
        },
        Err(e) => json!({"id": null, "ok": false, "error": format!("bad request: {e}")}),
    };
    response.to_string()
}

fn serve_connection(stream: UnixStream) {
    let Ok(reader) = stream.try_clone() else { return };
    let mut writer = stream;
    let mut session = methods::Session::default();
    for line in BufReader::new(reader).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let mut out = handle_line(&mut session, &line);
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() {
            break;
        }
    }
}

/// Listens on `path` (replacing a stale socket) and serves connections until the process exits.
pub fn serve(path: &Path) -> std::io::Result<()> {
    let listener = bind(path)?;
    for stream in listener.incoming().flatten() {
        std::thread::Builder::new().name("sim-rs-api".into()).spawn(move || serve_connection(stream))?;
    }
    Ok(())
}

/// Serves on a background thread; returns once the socket is bound.
pub fn serve_in_background(path: PathBuf) -> std::io::Result<()> {
    let listener = bind(&path)?;
    std::thread::Builder::new().name("sim-rs-api-accept".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || serve_connection(stream));
        }
    })?;
    Ok(())
}

fn bind(path: &Path) -> std::io::Result<UnixListener> {
    if path.exists() && UnixStream::connect(path).is_err() {
        std::fs::remove_file(path)?;
    }
    UnixListener::bind(path)
}

/// Sends one request and waits for its response.
pub fn call(path: &Path, method: &str, params: Value) -> Result<Value, String> {
    let mut stream = UnixStream::connect(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut line = json!({"id": 1, "method": method, "params": params}).to_string();
    line.push('\n');
    stream.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_str(&response).map_err(|e| format!("bad response: {e}"))?;
    if value["ok"] == true { Ok(value["result"].clone()) } else { Err(value["error"].as_str().unwrap_or("error").to_owned()) }
}

#[cfg(test)]
mod tests {
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
        let path = std::env::temp_dir().join(format!("sim-rs-api-test-{}.sock", std::process::id()));
        serve_in_background(path.clone()).expect("bind");
        let result = call(&path, "help", Value::Null).expect("help");
        assert!(result.as_array().is_some_and(|m| m.iter().any(|x| x["method"] == "tap")));
        std::fs::remove_file(path).ok();
    }
}
