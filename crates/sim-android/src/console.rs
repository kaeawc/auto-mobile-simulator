//! A client for the emulator console (`adb emu`'s text protocol on the console port): one TCP
//! session per call, authenticating with `~/.emulator_console_auth_token` when it exists.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

/// The console's auth token, if the user has one.
fn auth_token() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let token = std::fs::read_to_string(PathBuf::from(home).join(".emulator_console_auth_token")).ok()?;
    Some(token.trim().to_owned()).filter(|t| !t.is_empty())
}

/// Sends `commands` in one session and returns the reply lines, without the banner, the auth
/// acknowledgement and the `OK`s. A `KO:` reply fails the call with its reason.
pub fn send(port: u16, commands: &[String]) -> Result<Vec<String>, String> {
    let addr = (Ipv4Addr::LOCALHOST, port).into();
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map_err(|e| format!("console {port}: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?;
    let mut script = String::new();
    if let Some(t) = auth_token() {
        script.push_str(&format!("auth {t}\r\n"));
    }
    for c in commands {
        script.push_str(c);
        script.push_str("\r\n");
    }
    script.push_str("quit\r\n");
    s.write_all(script.as_bytes()).map_err(|e| e.to_string())?;
    let lines: Vec<String> = BufReader::new(s).lines().map_while(Result::ok).map(|l| l.trim_end_matches('\r').to_owned()).collect();
    parse_replies(lines)
}

/// Drops the banner (everything up to the last `Android Console: type 'help'` line and its `OK`)
/// and the `OK`s; turns the first `KO:` into an error.
fn parse_replies(lines: Vec<String>) -> Result<Vec<String>, String> {
    let skip = lines.iter().rposition(|l| l.starts_with("Android Console: type 'help'")).map_or(0, |i| i + 2);
    let mut out = Vec::new();
    for l in lines.into_iter().skip(skip) {
        if let Some(reason) = l.strip_prefix("KO:") {
            return Err(reason.trim().to_owned());
        }
        if l != "OK" {
            out.push(l);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A console that greets like emu-rs, answers `ping` and `avd name`, and fails anything else.
    fn fake_console() -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut out = stream.try_clone().expect("clone");
                out.write_all(b"Android Console: type 'help' for a list of commands\r\nOK\r\n").expect("banner");
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    let reply = match line.trim() {
                        "quit" => break,
                        "ping" => "I am alive!\r\nOK",
                        "avd name" => "pixel\r\nOK",
                        l if l.starts_with("auth") => "Android Console: type 'help' for a list of commands\r\nOK",
                        _ => "KO: unknown command",
                    };
                    out.write_all(format!("{reply}\r\n").as_bytes()).expect("reply");
                }
            }
        });
        port
    }

    #[test]
    fn sessions_return_replies_without_banner_or_oks() {
        let port = fake_console();
        assert_eq!(send(port, &["ping".into(), "avd name".into()]), Ok(vec!["I am alive!".to_owned(), "pixel".to_owned()]));
        assert_eq!(send(port, &["dance".into()]), Err("unknown command".into()));
    }

    #[test]
    fn auth_banners_are_skipped() {
        let lines =
            ["Android Console: Authentication required", "OK", "Android Console: type 'help' for a list of commands", "OK", "x", "OK"];
        assert_eq!(parse_replies(lines.iter().map(|l| (*l).to_owned()).collect()), Ok(vec!["x".to_owned()]));
    }
}
