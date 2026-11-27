//! A minimal blocking HTTP/1.1 client over `std::net`: one request per connection
//! (`Connection: close`), `Content-Length` or chunked bodies, and one deadline for the whole
//! exchange (connect, write, headers and body), so a stalled server can't hang a publisher.
//! Plain `http` only: TLS is a follow-up (`docs/streaming.md`).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use crate::url::{Scheme, Url};
use crate::{Error, Result};

/// Headers past this are a broken or hostile server.
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// An SDP answer is a few KB; error pages are cut short anyway.
const MAX_BODY_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct Response {
    pub status: u16,
    /// Header names lowercased, values trimmed, in the order received.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// The first header with this name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The body as text, lossily (SDP and error pages are UTF-8 or ASCII).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Send one request and read the whole response before `timeout` runs out.
pub fn request(method: &str, url: &Url, headers: &[(&str, &str)], body: &[u8], timeout: Duration) -> Result<Response> {
    if url.scheme() == Scheme::Https {
        return Err(Error::Unsupported(format!("{url}: https is not supported yet (no TLS client); use http to a loopback or LAN host")));
    }
    for (name, value) in headers {
        if !valid_token(name) || value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
            return Err(Error::InvalidInput(format!("header {name:?} has a forbidden character")));
        }
    }
    let deadline = Instant::now() + timeout;
    let mut stream = connect(url, deadline)?;

    let mut head = format!(
        "{method} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: simee-rtc/{}\r\nConnection: close\r\n",
        url.path(),
        url.authority(),
        env!("CARGO_PKG_VERSION"),
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty() || matches!(method, "POST" | "PUT" | "PATCH") {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    set_timeouts(&stream, deadline)?;
    stream.write_all(head.as_bytes()).map_err(|e| io_or_timeout(e, method, url))?;
    stream.write_all(body).map_err(|e| io_or_timeout(e, method, url))?;
    stream.flush().map_err(|e| io_or_timeout(e, method, url))?;

    let mut reader = Reader { stream, buf: Vec::new(), pos: 0, deadline, eof: false };
    loop {
        let head = reader.read_head().map_err(|e| e.with_context(method, url))?;
        let mut response = parse_head(&head)?;
        // Interim responses (100 Continue, 103 Early Hints) precede the real one.
        if (100..200).contains(&response.status) && response.status != 101 {
            continue;
        }
        if method != "HEAD" && !matches!(response.status, 204 | 304) {
            response.body = reader.read_body(&response).map_err(|e| e.with_context(method, url))?;
        }
        return Ok(response);
    }
}

/// RFC 9110 §5.6.2.
fn valid_token(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn connect(url: &Url, deadline: Instant) -> Result<TcpStream> {
    let addrs: Vec<SocketAddr> = match url.loopback_ip() {
        Some(ip) => vec![SocketAddr::new(ip, url.port())],
        None => (url.host(), url.port()).to_socket_addrs()?.collect(),
    };
    let mut last = None;
    for addr in addrs {
        let left = remaining(deadline).ok_or_else(|| Error::Timeout(format!("connecting to {url}")))?;
        match TcpStream::connect_timeout(&addr, left) {
            Ok(s) => {
                // Small requests and responses: don't wait for Nagle.
                s.set_nodelay(true)?;
                return Ok(s);
            }
            Err(e) => last = Some(e),
        }
    }
    match last {
        Some(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => {
            Err(Error::Timeout(format!("connecting to {url}")))
        }
        Some(e) => Err(Error::Io(e)),
        None => Err(Error::InvalidInput(format!("{url}: the host resolves to no address"))),
    }
}

fn remaining(deadline: Instant) -> Option<Duration> {
    deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())
}

fn set_timeouts(stream: &TcpStream, deadline: Instant) -> Result<()> {
    let left = remaining(deadline).ok_or_else(|| Error::Timeout("HTTP request".into()))?;
    stream.set_read_timeout(Some(left))?;
    stream.set_write_timeout(Some(left))?;
    Ok(())
}

fn io_or_timeout(e: std::io::Error, method: &str, url: &Url) -> Error {
    match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => Error::Timeout(format!("{method} {url}")),
        _ => Error::Io(e),
    }
}

enum ReadError {
    Timeout,
    Io(std::io::Error),
    Protocol(String),
}

impl ReadError {
    fn with_context(self, method: &str, url: &Url) -> Error {
        match self {
            ReadError::Timeout => Error::Timeout(format!("{method} {url}")),
            ReadError::Io(e) => Error::Io(e),
            ReadError::Protocol(m) => Error::Protocol(format!("{method} {url}: {m}")),
        }
    }
}

struct Reader {
    stream: TcpStream,
    buf: Vec<u8>,
    pos: usize,
    deadline: Instant,
    eof: bool,
}

impl Reader {
    /// Read more into `buf`; false at end of stream.
    fn fill(&mut self) -> Result<bool, ReadError> {
        if self.eof {
            return Ok(false);
        }
        let left = remaining(self.deadline).ok_or(ReadError::Timeout)?;
        self.stream.set_read_timeout(Some(left)).map_err(ReadError::Io)?;
        let mut chunk = [0u8; 8192];
        match self.stream.read(&mut chunk) {
            Ok(0) => {
                self.eof = true;
                Ok(false)
            }
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                Ok(true)
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => Err(ReadError::Timeout),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(true),
            Err(e) => Err(ReadError::Io(e)),
        }
    }

    fn unread(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    /// The status line and headers, up to and without the blank line.
    fn read_head(&mut self) -> Result<String, ReadError> {
        loop {
            if let Some(end) = find(self.unread(), b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&self.unread()[..end]).into_owned();
                self.pos += end + 4;
                return Ok(head);
            }
            if self.unread().len() > MAX_HEADER_BYTES {
                return Err(ReadError::Protocol("headers too large".into()));
            }
            if !self.fill()? {
                return Err(ReadError::Protocol("connection closed before the headers ended".into()));
            }
        }
    }

    /// Exactly `n` more bytes.
    fn take(&mut self, n: usize) -> Result<Vec<u8>, ReadError> {
        while self.unread().len() < n {
            if !self.fill()? {
                return Err(ReadError::Protocol("connection closed inside the body".into()));
            }
        }
        let out = self.unread()[..n].to_vec();
        self.pos += n;
        Ok(out)
    }

    fn line(&mut self) -> Result<String, ReadError> {
        loop {
            if let Some(end) = find(self.unread(), b"\r\n") {
                let line = String::from_utf8_lossy(&self.unread()[..end]).into_owned();
                self.pos += end + 2;
                return Ok(line);
            }
            if self.unread().len() > MAX_HEADER_BYTES {
                return Err(ReadError::Protocol("chunk line too long".into()));
            }
            if !self.fill()? {
                return Err(ReadError::Protocol("connection closed inside a chunk".into()));
            }
        }
    }

    fn read_body(&mut self, response: &Response) -> Result<Vec<u8>, ReadError> {
        let chunked = response.header("transfer-encoding").is_some_and(|v| v.split(',').any(|c| c.trim().eq_ignore_ascii_case("chunked")));
        if chunked {
            let mut body = Vec::new();
            loop {
                let line = self.line()?;
                let size = line.split(';').next().unwrap_or_default().trim();
                let size = usize::from_str_radix(size, 16).map_err(|_| ReadError::Protocol(format!("bad chunk size {size:?}")))?;
                if size == 0 {
                    // Trailers, then the blank line.
                    while !self.line()?.is_empty() {}
                    return Ok(body);
                }
                if body.len() + size > MAX_BODY_BYTES {
                    return Err(ReadError::Protocol("body too large".into()));
                }
                body.extend(self.take(size)?);
                if !self.line()?.is_empty() {
                    return Err(ReadError::Protocol("chunk without its CRLF".into()));
                }
            }
        }
        if let Some(len) = response.header("content-length") {
            let len: usize = len.trim().parse().map_err(|_| ReadError::Protocol(format!("bad Content-Length {len:?}")))?;
            if len > MAX_BODY_BYTES {
                return Err(ReadError::Protocol("body too large".into()));
            }
            return self.take(len);
        }
        // Neither: the body runs to the end of the connection.
        while self.fill()? {
            if self.unread().len() > MAX_BODY_BYTES {
                return Err(ReadError::Protocol("body too large".into()));
            }
        }
        let body = self.unread().to_vec();
        self.pos = self.buf.len();
        Ok(body)
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn parse_head(head: &str) -> Result<Response> {
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    let status = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(Error::Protocol(format!("bad status line {status_line:?}")));
    }
    let status: u16 = status
        .parse()
        .ok()
        .filter(|s| (100..600).contains(s))
        .ok_or_else(|| Error::Protocol(format!("bad status line {status_line:?}")))?;
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or_else(|| Error::Protocol(format!("bad header line {line:?}")))?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
    Ok(Response { status, headers, body: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// Serve one connection: read the request head (and a Content-Length body), reply `reply`.
    fn serve_once(reply: &'static [u8]) -> (Url, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = thread::spawn(move || {
            let (mut s, _) = listener.accept().expect("accept");
            let mut req = Vec::new();
            let mut chunk = [0u8; 4096];
            while find(&req, b"\r\n\r\n").is_none() {
                let n = s.read(&mut chunk).expect("read");
                req.extend_from_slice(&chunk[..n]);
            }
            let text = String::from_utf8_lossy(&req).into_owned();
            s.write_all(reply).expect("write");
            text
        });
        (Url::parse(&format!("http://127.0.0.1:{port}/p?q=1")).expect("url"), handle)
    }

    #[test]
    fn sends_the_request_and_reads_a_sized_body() {
        let (url, server) = serve_once(b"HTTP/1.1 201 Created\r\nLocation: /r/1\r\nContent-Length: 5\r\n\r\nhello");
        let r = request("POST", &url, &[("Content-Type", "application/sdp")], b"v=0", Duration::from_secs(5)).expect("request");
        assert_eq!((r.status, r.header("LOCATION"), r.text().as_str()), (201, Some("/r/1"), "hello"));
        let req = server.join().expect("server");
        assert!(req.starts_with("POST /p?q=1 HTTP/1.1\r\n"), "{req}");
        assert!(req.contains(&format!("\r\nHost: 127.0.0.1:{}\r\n", url.port())), "{req}");
        assert!(req.contains("\r\nContent-Type: application/sdp\r\n"), "{req}");
        assert!(req.contains("\r\nContent-Length: 3\r\n"), "{req}");
    }

    #[test]
    fn reads_chunked_bodies_and_skips_interim_responses() {
        let (url, server) = serve_once(
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4;x=y\r\nv=0\n\r\n3\r\nabc\r\n0\r\nTrailer: t\r\n\r\n",
        );
        let r = request("GET", &url, &[], b"", Duration::from_secs(5)).expect("request");
        assert_eq!((r.status, r.text().as_str()), (200, "v=0\nabc"));
        server.join().expect("server");
    }

    #[test]
    fn reads_to_eof_without_a_length_and_nothing_for_204() {
        let (url, server) = serve_once(b"HTTP/1.0 200 OK\r\n\r\nall of it");
        assert_eq!(request("GET", &url, &[], b"", Duration::from_secs(5)).expect("eof").text(), "all of it");
        server.join().expect("server");
        let (url, server) = serve_once(b"HTTP/1.1 204 No Content\r\nETag: \"2\"\r\n\r\n");
        let r = request("DELETE", &url, &[], b"", Duration::from_secs(5)).expect("204");
        assert_eq!((r.status, r.header("etag"), r.body.len()), (204, Some("\"2\""), 0));
        server.join().expect("server");
    }

    #[test]
    fn a_stalled_server_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = Url::parse(&format!("http://127.0.0.1:{}/", listener.local_addr().expect("addr").port())).expect("url");
        let start = Instant::now();
        let err = request("GET", &url, &[], b"", Duration::from_millis(300)).expect_err("stall");
        assert!(matches!(err, Error::Timeout(_)), "{err}");
        assert!(start.elapsed() < Duration::from_secs(5));
        drop(listener);
    }

    #[test]
    fn refuses_broken_responses_and_header_injection() {
        let (url, server) = serve_once(b"SIP/2.0 200 OK\r\n\r\n");
        assert!(matches!(request("GET", &url, &[], b"", Duration::from_secs(5)), Err(Error::Protocol(_))));
        server.join().expect("server");
        let (url, server) = serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort");
        assert!(matches!(request("GET", &url, &[], b"", Duration::from_secs(5)), Err(Error::Protocol(_))));
        server.join().expect("server");
        let url = Url::parse("http://127.0.0.1:9/").expect("url");
        let err = request("GET", &url, &[("Authorization", "Bearer x\r\nEvil: 1")], b"", Duration::from_secs(1));
        assert!(matches!(err, Err(Error::InvalidInput(_))));
        let url = Url::parse("https://127.0.0.1:9/").expect("url");
        assert!(matches!(request("GET", &url, &[], b"", Duration::from_secs(1)), Err(Error::Unsupported(_))));
    }
}
