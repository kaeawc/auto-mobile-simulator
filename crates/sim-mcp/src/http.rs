//! MCP's Streamable HTTP transport (`simee mcp --http`): one endpoint, [`ENDPOINT`], on a small
//! blocking HTTP/1.1 server with a thread per connection, up to [`Limits::max_connections`].
//!
//! - `POST` carries one JSON-RPC message. A notification or response is accepted with 202. A
//!   request is answered with `application/json`, or, once the server has something to say about
//!   it before its response (progress, a subscription's notifications), with a `text/event-stream`
//!   that ends after the response.
//! - `GET` opens a session's stream for what belongs to no request (an older revision's resource
//!   updates). `DELETE` ends a session.
//! - A client of the current revision is stateless and needs no session. A client of an earlier one
//!   opens with `initialize`, which answers with an `Mcp-Session-Id`; every request after it names
//!   that session, and each session is its own [`Server`], as a stdio connection is.
//!
//! Every message goes through the same [`Server::handle_line`] that stdio uses. Request ids and
//! progress tokens are swapped for ones unique to this process on the way in and back on the way
//! out, so two clients that both send id 1 each get their own answer.
//!
//! Security: the bind is loopback unless the caller asks otherwise; a non-loopback bind needs
//! [`Config::insecure_remote`] (there is no TLS here) and always requires a bearer token from a
//! token file only its owner can read. A browser's `Origin` must be a loopback one, and on a
//! loopback bind so must `Host` (DNS rebinding). Request heads and bodies are bounded in size and
//! time. SSE streams carry no event ids, so a dropped stream can't be resumed.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use crate::backend::Backend;
use crate::rpc::{self, INVALID_REQUEST, Incoming, LEGACY, META_SUBSCRIPTION, MODERN, PARSE_ERROR};
use crate::server::{Server, Timing};
use crate::tasks::Sink;
use crate::tools::Toolsets;

/// The one path the transport serves.
pub const ENDPOINT: &str = "/mcp";
/// Where `simee mcp --http` listens when given no address.
pub const DEFAULT_ADDR: &str = "127.0.0.1:8377";
/// The token file's name in simee's state directory.
pub const TOKEN_FILE: &str = "mcp-token";

const SESSION_HEADER: &str = "mcp-session-id";
const VERSION_HEADER: &str = "mcp-protocol-version";
/// A token shorter than this is refused: it could be guessed.
const MIN_TOKEN: usize = 16;

/// Bounds on what one client can make the server hold.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The largest request body.
    pub max_body: usize,
    /// The largest request line and headers together.
    pub max_head: usize,
    /// Connections served at once (each on its own thread); more are answered 503.
    pub max_connections: usize,
    /// Sessions open at once; past it, idle ones are closed first, then `initialize` gets 503.
    pub max_sessions: usize,
    /// How long a request's head and body may take to arrive once it starts.
    pub request_timeout: Duration,
    /// How long a kept-alive connection may sit idle between requests.
    pub keep_alive: Duration,
    /// How long one write to a client may block.
    pub write_timeout: Duration,
    /// How often a waiting response checks that its client is still there (and an event stream
    /// sends a comment to keep proxies from closing it).
    pub ping: Duration,
    /// A session nobody has used for this long is closed when room is needed.
    pub session_idle: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_body: 8 << 20,
            max_head: 16 << 10,
            max_connections: 64,
            max_sessions: 32,
            request_timeout: Duration::from_secs(10),
            keep_alive: Duration::from_secs(15),
            write_timeout: Duration::from_secs(30),
            ping: Duration::from_secs(15),
            session_idle: Duration::from_secs(3600),
        }
    }
}

/// How to serve.
#[derive(Clone)]
pub struct Config {
    pub addr: SocketAddr,
    /// Bearer tokens, one per line. Required off loopback (default: [`default_token_file`]);
    /// optional on loopback, where giving one turns authentication on.
    pub token_file: Option<PathBuf>,
    /// Acknowledges that a non-loopback bind serves plain HTTP: the token crosses the network in
    /// the clear.
    pub insecure_remote: bool,
    pub toolsets: Toolsets,
    pub limits: Limits,
    pub timing: Timing,
}

impl Config {
    pub fn new(addr: SocketAddr) -> Config {
        Config {
            addr,
            token_file: None,
            insecure_remote: false,
            toolsets: Toolsets::default(),
            limits: Limits::default(),
            timing: Timing::default(),
        }
    }
}

/// simee's state directory: `$SIMEE_STATE_DIR`, else `~/Library/Application Support/simee` on
/// macOS and `$XDG_STATE_HOME/simee` (or `~/.local/state/simee`) elsewhere.
pub fn state_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SIMEE_STATE_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    if cfg!(target_os = "macos") {
        return home.map(|h| h.join("Library/Application Support/simee"));
    }
    std::env::var_os("XDG_STATE_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".local/state")))
        .map(|d| d.join("simee"))
}

/// The token file a non-loopback bind uses when given none.
pub fn default_token_file() -> io::Result<PathBuf> {
    state_dir()
        .map(|d| d.join(TOKEN_FILE))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no state directory: set HOME or SIMEE_STATE_DIR, or pass --token-file"))
}

/// Serves until the process ends.
pub fn serve(backend: Arc<dyn Backend>, config: Config) -> io::Result<()> {
    let running = start(backend, config)?;
    eprintln!("simee mcp: Streamable HTTP on http://{}{ENDPOINT}", running.addr());
    running.wait();
    Ok(())
}

/// Binds and starts serving on a thread of its own.
pub fn start(backend: Arc<dyn Backend>, config: Config) -> io::Result<Running> {
    let tokens = prepare_auth(&config)?;
    let listener = TcpListener::bind(config.addr)?;
    let addr = listener.local_addr()?;
    let router = Arc::new(Router::default());
    let stateless = Server::with_timing(backend.clone(), sink(router.clone(), None), config.timing, config.toolsets.clone());
    let shared = Arc::new(Shared {
        backend,
        loopback: addr.ip().is_loopback(),
        config,
        tokens,
        router,
        stateless,
        sessions: Mutex::default(),
        connections: AtomicUsize::new(0),
        stopping: AtomicBool::new(false),
    });
    let accepting = shared.clone();
    let accept = std::thread::Builder::new().name("simee-mcp-http".into()).spawn(move || accept_loop(&accepting, &listener))?;
    Ok(Running { addr, shared, accept: Some(accept) })
}

/// A server that is listening. Dropping it stops accepting connections (open ones finish).
pub struct Running {
    addr: SocketAddr,
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
}

impl Running {
    /// The address bound (with the port the OS chose for port 0).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Blocks while the server accepts connections.
    pub fn wait(mut self) {
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }

    /// How many sessions are open.
    pub fn sessions(&self) -> usize {
        lock(&self.shared.sessions).len()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let Some(accept) = self.accept.take() else { return };
        self.shared.stopping.store(true, Ordering::SeqCst);
        // Wake the accept loop with a connection of our own.
        let wake = match self.addr.ip() {
            IpAddr::V4(ip) if ip.is_unspecified() => SocketAddr::new(Ipv4Addr::LOCALHOST.into(), self.addr.port()),
            IpAddr::V6(ip) if ip.is_unspecified() => SocketAddr::new(Ipv6Addr::LOCALHOST.into(), self.addr.port()),
            _ => self.addr,
        };
        let _ = TcpStream::connect_timeout(&wake, Duration::from_secs(1));
        let _ = accept.join();
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---- Authentication ----

/// Checks the bind against the policy and loads the token file it needs.
fn prepare_auth(config: &Config) -> io::Result<Option<Tokens>> {
    let loopback = config.addr.ip().is_loopback();
    if !loopback && !config.insecure_remote {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is not a loopback address: this server has no TLS, so its bearer token would cross the network in the clear; \
                 pass --insecure-remote to serve it anyway (on a trusted network or behind a TLS proxy)",
                config.addr
            ),
        ));
    }
    let path = match (&config.token_file, loopback) {
        (Some(path), _) => path.clone(),
        (None, false) => default_token_file()?,
        (None, true) => return Ok(None),
    };
    ensure_token_file(&path)?;
    let tokens = Tokens::new(path);
    if lock(&tokens.state).tokens.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{}: no token in it", tokens.path.display())));
    }
    Ok(Some(tokens))
}

/// `n` random bytes from the OS, as hex.
fn random_hex(n: usize) -> io::Result<String> {
    let mut bytes = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Makes `path` hold a token, readable only by its owner: an existing file is checked (and
/// refused if others can read it); a missing one is written with a new random token.
pub fn ensure_token_file(path: &Path) -> io::Result<()> {
    if path.exists() {
        return read_tokens(path).map(drop);
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(format!("{}\n", random_hex(32)?).as_bytes())?;
    eprintln!("simee mcp: wrote a new bearer token to {} (give it to clients as `Authorization: Bearer <token>`)", path.display());
    Ok(())
}

/// The tokens in a file: one per line, blank lines and `#` comments skipped. The file must be
/// readable by its owner only.
fn read_tokens(path: &Path) -> io::Result<Vec<String>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} can be read by other users (mode {:o}): `chmod 600` it", path.display(), mode & 0o777),
            ));
        }
    }
    let text = std::fs::read_to_string(path)?;
    let tokens: Vec<String> = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_owned).collect();
    if let Some(short) = tokens.iter().find(|t| t.len() < MIN_TOKEN) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: a token is {} characters; use at least {MIN_TOKEN}", path.display(), short.len()),
        ));
    }
    Ok(tokens)
}

/// What a file looked like when it was read, to notice a rewrite.
#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    inode: u64,
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Stamp {
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(meta);
        #[cfg(not(unix))]
        let inode = 0;
        Stamp { modified: meta.modified().ok(), len: meta.len(), inode }
    }
}

struct TokenState {
    stamp: Option<Stamp>,
    tokens: Vec<String>,
    /// The last error reading the file, said once.
    error: Option<String>,
}

/// The token file, re-read whenever it changes, so rotating a token is rewriting the file.
struct Tokens {
    path: PathBuf,
    state: Mutex<TokenState>,
}

impl Tokens {
    fn new(path: PathBuf) -> Tokens {
        let tokens = Tokens { path, state: Mutex::new(TokenState { stamp: None, tokens: Vec::new(), error: None }) };
        tokens.refresh(&mut lock(&tokens.state));
        tokens
    }

    fn refresh(&self, state: &mut TokenState) {
        let stamp = std::fs::metadata(&self.path).ok().map(|m| Stamp::of(&m));
        if stamp.is_some() && stamp == state.stamp {
            return;
        }
        state.tokens = match read_tokens(&self.path) {
            Ok(tokens) => {
                state.error = None;
                tokens
            }
            Err(e) => {
                let error = e.to_string();
                if state.error.as_ref() != Some(&error) {
                    eprintln!("simee mcp: {error}; refusing every request until it is fixed");
                    state.error = Some(error);
                }
                Vec::new()
            }
        };
        state.stamp = stamp;
    }

    /// Whether an `Authorization` header carries one of the tokens.
    fn allows(&self, authorization: Option<&str>) -> bool {
        let Some(presented) = authorization.and_then(bearer) else { return false };
        let mut state = lock(&self.state);
        self.refresh(&mut state);
        // Every token is compared, each in time that depends only on its length.
        state.tokens.iter().fold(false, |found, t| same(t.as_bytes(), presented.as_bytes()) | found)
    }
}

fn bearer(header: &str) -> Option<&str> {
    let (scheme, token) = header.trim().split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim()).filter(|t| !t.is_empty())
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---- Origins and hosts ----

/// The host of an authority (`host`, `host:port`, `[v6]:port`).
fn host_of(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    }
}

fn loopback_host(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    host.eq_ignore_ascii_case("localhost")
        || host.to_ascii_lowercase().ends_with(".localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Whether a browser's `Origin` may talk to this server: loopback pages only. (`null`, the origin
/// of sandboxed frames and files, isn't one.)
fn origin_allowed(origin: &str) -> bool {
    let Some((scheme, rest)) = origin.trim().split_once("://") else { return false };
    let authority = rest.split('/').next().unwrap_or(rest);
    (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) && loopback_host(host_of(authority))
}

// ---- Routing what servers say back to the requests that asked ----

enum Out {
    Response(String),
    Note(String),
}

/// A request waiting for its answer: its own id and progress token, and where its messages go.
struct Route {
    id: Value,
    token: Option<Value>,
    tx: Sender<Out>,
}

/// A session's stream for messages that belong to no request, with the generation of the `GET`
/// that holds it.
type Standalone = Mutex<Option<(u64, Sender<Out>)>>;

#[derive(Default)]
struct Router {
    next: AtomicU64,
    /// By internal id (`h<n>`): the id, progress token and subscription id the server sees.
    routes: Mutex<HashMap<String, Route>>,
}

/// Where a response's id sits in its line, when it is one of ours: `(start, key, rest)`.
/// serde_json writes keys sorted (`id` first) or in insertion order (`jsonrpc`, then `id`).
fn fast_response(line: &str) -> Option<(usize, &str, &str)> {
    for prefix in ["{\"id\":\"", "{\"jsonrpc\":\"2.0\",\"id\":\""] {
        if let Some(after) = line.strip_prefix(prefix) {
            let end = after.find('"')?;
            let key = &after[..end];
            if key.len() > 1 && key.starts_with('h') && key[1..].bytes().all(|b| b.is_ascii_digit()) {
                return Some((prefix.len() - 1, key, &after[end + 1..]));
            }
        }
    }
    None
}

impl Router {
    fn key(&self) -> String {
        format!("h{}", self.next.fetch_add(1, Ordering::Relaxed))
    }

    fn insert(&self, key: String, route: Route) {
        lock(&self.routes).insert(key, route);
    }

    fn forget(&self, key: &str) {
        lock(&self.routes).remove(key);
    }

    /// Sends one line from a server to whoever it is for, with the client's own ids put back.
    fn deliver(&self, line: String, standalone: Option<&Standalone>) {
        // The common case, a result, without parsing what may be a large one.
        if let Some((start, key, rest)) = fast_response(&line) {
            let route = lock(&self.routes).remove(key);
            if let Some(route) = route {
                let id = route.id.to_string();
                let mut out = String::with_capacity(line.len() + id.len());
                out.push_str(&line[..start]);
                out.push_str(&id);
                out.push_str(rest);
                let _ = route.tx.send(Out::Response(out));
            }
            return;
        }
        let Ok(mut message) = serde_json::from_str::<Value>(&line) else { return };
        if message.get("method").is_none() {
            let key = message.get("id").and_then(Value::as_str).map(str::to_owned);
            let route = key.and_then(|k| lock(&self.routes).remove(&k));
            if let Some(route) = route {
                message["id"] = route.id;
                let _ = route.tx.send(Out::Response(message.to_string()));
            }
            return;
        }
        let params = message.get("params");
        let subscription =
            params.and_then(|p| p.get("_meta")).and_then(|m| m.get(META_SUBSCRIPTION)).and_then(Value::as_str).map(str::to_owned);
        if let Some(key) = subscription {
            let found = lock(&self.routes).get(&key).map(|r| (r.id.clone(), r.tx.clone()));
            if let Some((id, tx)) = found {
                message["params"]["_meta"][META_SUBSCRIPTION] = id;
                let _ = tx.send(Out::Note(message.to_string()));
            }
            return;
        }
        if message["method"] == "notifications/progress" {
            let key = params.and_then(|p| p.get("progressToken")).and_then(Value::as_str).map(str::to_owned);
            let found = key.and_then(|k| lock(&self.routes).get(&k).and_then(|r| Some((r.token.clone()?, r.tx.clone()))));
            if let Some((token, tx)) = found {
                message["params"]["progressToken"] = token;
                let _ = tx.send(Out::Note(message.to_string()));
            }
            return;
        }
        if let Some((_, tx)) = standalone.and_then(|s| lock(s).clone()) {
            let _ = tx.send(Out::Note(line));
        }
    }
}

fn sink(router: Arc<Router>, standalone: Option<Arc<Standalone>>) -> Sink {
    Arc::new(move |line: String| router.deliver(line, standalone.as_deref()))
}

// ---- Sessions ----

struct Session {
    server: Server,
    standalone: Arc<Standalone>,
    last_used: Mutex<Instant>,
    /// Requests and streams open on it.
    busy: AtomicUsize,
    /// The client's request ids in flight, to the internal ones, for `notifications/cancelled`.
    cancels: Mutex<HashMap<String, String>>,
}

impl Session {
    fn touch(&self) {
        *lock(&self.last_used) = Instant::now();
    }
}

/// Counts a request or stream against a session while it lives.
struct Busy(Arc<Session>);

impl Busy {
    fn new(session: &Arc<Session>) -> Busy {
        session.busy.fetch_add(1, Ordering::SeqCst);
        session.touch();
        Busy(session.clone())
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.touch();
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Shared {
    backend: Arc<dyn Backend>,
    config: Config,
    /// Whether the bind is loopback, so `Host` must be too.
    loopback: bool,
    tokens: Option<Tokens>,
    router: Arc<Router>,
    /// Serves the current revision's requests that name no session.
    stateless: Server,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    connections: AtomicUsize,
    stopping: AtomicBool,
}

impl Shared {
    fn session(&self, id: &str) -> Option<Arc<Session>> {
        lock(&self.sessions).get(id).cloned()
    }

    /// A new session, if there is room (made by closing idle ones if need be).
    fn open_session(&self) -> io::Result<Option<(String, Arc<Session>)>> {
        let id = random_hex(16)?;
        let limits = &self.config.limits;
        let mut sessions = lock(&self.sessions);
        sessions.retain(|_, s| s.busy.load(Ordering::SeqCst) > 0 || lock(&s.last_used).elapsed() < limits.session_idle);
        if sessions.len() >= limits.max_sessions {
            // Room for a new client: the least recently used idle session goes.
            let idle = sessions
                .iter()
                .filter(|(_, s)| s.busy.load(Ordering::SeqCst) == 0)
                .min_by_key(|(_, s)| *lock(&s.last_used))
                .map(|(k, _)| k.clone());
            match idle {
                Some(k) => drop(sessions.remove(&k)),
                None => return Ok(None),
            }
        }
        let standalone: Arc<Standalone> = Arc::default();
        let server = Server::with_timing(
            self.backend.clone(),
            sink(self.router.clone(), Some(standalone.clone())),
            self.config.timing,
            self.config.toolsets.clone(),
        );
        let session = Arc::new(Session {
            server,
            standalone,
            last_used: Mutex::new(Instant::now()),
            busy: AtomicUsize::new(0),
            cancels: Mutex::default(),
        });
        sessions.insert(id.clone(), session.clone());
        Ok(Some((id, session)))
    }
}

// ---- Connections ----

/// Holds one of the connection slots.
struct Slot(Arc<Shared>);

impl Slot {
    fn take(shared: &Arc<Shared>) -> Option<Slot> {
        let max = shared.config.limits.max_connections;
        shared.connections.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < max).then_some(n + 1)).ok()?;
        Some(Slot(shared.clone()))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::SeqCst);
    }
}

fn accept_loop(shared: &Arc<Shared>, listener: &TcpListener) {
    for stream in listener.incoming() {
        if shared.stopping.load(Ordering::SeqCst) {
            return;
        }
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                // Out of descriptors, say: don't spin.
                eprintln!("simee mcp: accept: {e}");
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        let Some(slot) = Slot::take(shared) else {
            let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
            let body = error_body(INVALID_REQUEST, "too many connections; retry shortly");
            let _ = write_response(&stream, 503, &[("Retry-After", "1".into())], body.as_bytes(), true);
            let _ = stream.shutdown(Shutdown::Both);
            continue;
        };
        let spawned = std::thread::Builder::new().name("simee-mcp-conn".into()).spawn(move || {
            let shared = slot.0.clone();
            connection(&shared, stream);
            drop(slot);
        });
        if let Err(e) = spawned {
            eprintln!("simee mcp: cannot start a connection thread: {e}");
        }
    }
}

/// A request's line and headers.
struct Head {
    method: String,
    path: String,
    http11: bool,
    /// Names lowercased, in order.
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// Whether the client may take the connection back for another request.
    fn keep_alive(&self) -> bool {
        let connection = self.header("connection").map(str::to_ascii_lowercase).unwrap_or_default();
        if self.http11 {
            !connection.split(',').any(|t| t.trim() == "close")
        } else {
            connection.split(',').any(|t| t.trim() == "keep-alive")
        }
    }

    /// Whether the `Accept` header admits `mime` (no header admits everything).
    fn accepts(&self, mime: &str) -> bool {
        let Some(accept) = self.header("accept") else { return true };
        let (kind, _) = mime.split_once('/').unwrap_or((mime, ""));
        accept
            .split(',')
            .map(|a| a.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
            .any(|a| a == mime || a == "*/*" || a.strip_suffix("/*").is_some_and(|k| k == kind))
    }
}

enum HeadError {
    /// The client went away or stayed silent: nothing to answer.
    Gone,
    TooLarge,
    Malformed(&'static str),
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "the request took too long"))
}

fn read_head(reader: &mut BufReader<TcpStream>, idle: Duration, limits: &Limits) -> Result<Head, HeadError> {
    let mut raw: Vec<u8> = Vec::with_capacity(512);
    let mut deadline = Instant::now() + idle;
    loop {
        let left = remaining(deadline).map_err(|_| HeadError::Gone)?;
        reader.get_ref().set_read_timeout(Some(left)).map_err(|_| HeadError::Gone)?;
        let buf = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(HeadError::Gone),
        };
        if buf.is_empty() {
            return Err(HeadError::Gone);
        }
        if raw.is_empty() {
            // The request has started: it must arrive in time.
            deadline = Instant::now() + limits.request_timeout;
        }
        let mut used = 0;
        let mut done = false;
        for &b in buf {
            used += 1;
            // Blank lines before a request line are allowed and skipped.
            if raw.is_empty() && (b == b'\r' || b == b'\n') {
                continue;
            }
            raw.push(b);
            if raw.ends_with(b"\r\n\r\n") || raw.ends_with(b"\n\n") {
                done = true;
                break;
            }
        }
        reader.consume(used);
        if raw.len() > limits.max_head {
            return Err(HeadError::TooLarge);
        }
        if done {
            break;
        }
    }
    let text = std::str::from_utf8(&raw).map_err(|_| HeadError::Malformed("the request head is not UTF-8"))?;
    let mut lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).filter(|l| !l.is_empty());
    let line = lines.next().ok_or(HeadError::Malformed("no request line"))?;
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(HeadError::Malformed("a request line is `METHOD target HTTP/1.1`"));
    };
    let http11 = match version {
        "HTTP/1.1" => true,
        "HTTP/1.0" => false,
        _ => return Err(HeadError::Malformed("only HTTP/1.0 and HTTP/1.1 are served")),
    };
    let mut headers = Vec::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { return Err(HeadError::Malformed("a header is `Name: value`")) };
        if name.is_empty() || name.bytes().any(|b| b.is_ascii_whitespace()) {
            return Err(HeadError::Malformed("a header name has no spaces"));
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
        if headers.len() > 100 {
            return Err(HeadError::TooLarge);
        }
    }
    let path = target.split('?').next().unwrap_or(target).to_owned();
    Ok(Head { method: method.to_owned(), path, http11, headers })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

/// Writes a whole response in one write.
fn write_response(mut stream: &TcpStream, status: u16, headers: &[(&str, String)], body: &[u8], close: bool) -> io::Result<()> {
    let mut out = format!("HTTP/1.1 {status} {}\r\n", reason(status));
    if !body.is_empty() {
        out.push_str("Content-Type: application/json\r\n");
    }
    if status != 204 {
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    if close {
        out.push_str("Connection: close\r\n");
    }
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    stream.write_all(&bytes)?;
    stream.flush()
}

fn error_body(code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": null, "error": {"code": code, "message": message}}).to_string()
}

/// An event stream on a response: chunked on HTTP/1.1 (so the connection survives it), to the
/// connection's end on 1.0.
struct Sse<'a> {
    stream: &'a TcpStream,
    chunked: bool,
}

impl<'a> Sse<'a> {
    fn start(stream: &'a TcpStream, http11: bool, headers: &[(&str, String)]) -> io::Result<Sse<'a>> {
        let mut out = String::from("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n");
        out.push_str(if http11 { "Transfer-Encoding: chunked\r\n" } else { "Connection: close\r\n" });
        for (name, value) in headers {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        out.push_str("\r\n");
        let mut w = stream;
        w.write_all(out.as_bytes())?;
        w.flush()?;
        Ok(Sse { stream, chunked: http11 })
    }

    fn write(&self, text: &str) -> io::Result<()> {
        let mut w = self.stream;
        if self.chunked {
            let mut out = format!("{:x}\r\n", text.len()).into_bytes();
            out.extend_from_slice(text.as_bytes());
            out.extend_from_slice(b"\r\n");
            w.write_all(&out)?;
        } else {
            w.write_all(text.as_bytes())?;
        }
        w.flush()
    }

    /// One message. A JSON line has no newlines, so it is one `data` line.
    fn event(&self, line: &str) -> io::Result<()> {
        self.write(&format!("event: message\ndata: {line}\n\n"))
    }

    fn ping(&self) -> io::Result<()> {
        self.write(": ping\n\n")
    }

    fn end(&self) -> io::Result<()> {
        if self.chunked {
            let mut w = self.stream;
            w.write_all(b"0\r\n\r\n")?;
            w.flush()?;
        }
        Ok(())
    }
}

/// Whether the client closed its end (checked without blocking or consuming anything).
fn peer_closed(stream: &TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return false;
    }
    let mut byte = [0u8; 1];
    let closed = match stream.peek(&mut byte) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => e.kind() != io::ErrorKind::WouldBlock && e.kind() != io::ErrorKind::Interrupted,
    };
    let _ = stream.set_nonblocking(false);
    closed
}

fn connection(shared: &Arc<Shared>, stream: TcpStream) {
    let limits = shared.config.limits;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(limits.write_timeout));
    let mut reader = BufReader::new(stream);
    let mut idle = limits.request_timeout;
    loop {
        let head = match read_head(&mut reader, idle, &limits) {
            Ok(head) => head,
            Err(HeadError::Gone) => break,
            Err(HeadError::TooLarge) => {
                let _ = write_response(
                    reader.get_ref(),
                    431,
                    &[],
                    error_body(INVALID_REQUEST, "the request head is too large").as_bytes(),
                    true,
                );
                break;
            }
            Err(HeadError::Malformed(why)) => {
                let _ = write_response(reader.get_ref(), 400, &[], error_body(INVALID_REQUEST, why).as_bytes(), true);
                break;
            }
        };
        let keep = Exchange { shared, reader: &mut reader, head: &head }.serve() && head.keep_alive();
        if !keep || shared.stopping.load(Ordering::SeqCst) {
            break;
        }
        idle = limits.keep_alive;
    }
    linger(reader.get_ref());
}

/// Closes a connection without losing the last response: a close with unread input in the socket
/// resets it, and the client may never read what was written (a 413 sent before the body is read).
/// So: stop writing, read and drop what is still arriving for a moment, then close.
fn linger(mut stream: &TcpStream) {
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let start = Instant::now();
    let mut scratch = [0u8; 8192];
    let mut total = 0;
    while total < (1 << 20) && start.elapsed() < Duration::from_millis(500) {
        match stream.read(&mut scratch) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n,
        }
    }
}

/// One request on a connection. Each method returns whether the connection can carry another.
struct Exchange<'a> {
    shared: &'a Arc<Shared>,
    reader: &'a mut BufReader<TcpStream>,
    head: &'a Head,
}

impl Exchange<'_> {
    fn stream(&self) -> &TcpStream {
        self.reader.get_ref()
    }

    /// Answers with an error and closes the connection unless `keep` (the body, if any, was read).
    fn fail(&self, status: u16, code: i64, message: &str, headers: &[(&str, String)], keep: bool) -> bool {
        write_response(self.stream(), status, headers, error_body(code, message).as_bytes(), !keep).is_ok() && keep
    }

    fn serve(self) -> bool {
        let head = self.head;
        if head.path != ENDPOINT {
            return self.fail(404, INVALID_REQUEST, &format!("the MCP endpoint is {ENDPOINT}"), &[], false);
        }
        if self.shared.loopback && head.header("host").is_some_and(|h| !loopback_host(host_of(h))) {
            return self.fail(403, INVALID_REQUEST, "this server is bound to loopback and answers only loopback host names", &[], false);
        }
        if head.header("origin").is_some_and(|o| !origin_allowed(o)) {
            return self.fail(403, INVALID_REQUEST, "requests from this origin are refused (only loopback origins are served)", &[], false);
        }
        if let Some(tokens) = &self.shared.tokens
            && !tokens.allows(head.header("authorization"))
        {
            let challenge = [("WWW-Authenticate", "Bearer realm=\"simee\"".to_owned())];
            return self.fail(401, INVALID_REQUEST, "a valid bearer token is required", &challenge, false);
        }
        match head.method.as_str() {
            "POST" => self.post(),
            "GET" => self.get(),
            "DELETE" => self.delete(),
            _ => self.fail(
                405,
                INVALID_REQUEST,
                "the MCP endpoint takes POST, GET and DELETE",
                &[("Allow", "GET, POST, DELETE".into())],
                false,
            ),
        }
    }

    /// Reads the body, within the size and time limits. `Err` holds whether the error was answered
    /// such that the connection survives (never: an unread body can't be skipped safely).
    fn body(&mut self) -> Result<Vec<u8>, ()> {
        let limits = self.shared.config.limits;
        let deadline = Instant::now() + limits.request_timeout;
        let chunked = match self.head.header("transfer-encoding") {
            None => false,
            Some(te) if te.eq_ignore_ascii_case("chunked") => true,
            Some(_) => {
                self.fail(501, INVALID_REQUEST, "only chunked transfer encoding is understood", &[], false);
                return Err(());
            }
        };
        let mut lengths = self.head.headers.iter().filter(|(n, _)| n == "content-length").map(|(_, v)| v);
        if (chunked && self.head.header("content-length").is_some()) || lengths.next().is_some_and(|first| lengths.any(|v| v != first)) {
            // Two framings at once is how requests are smuggled past proxies.
            self.fail(400, INVALID_REQUEST, "conflicting Content-Length and Transfer-Encoding", &[], false);
            return Err(());
        }
        let length = if chunked {
            None
        } else {
            match self.head.header("content-length").map(|l| l.parse::<usize>()) {
                Some(Ok(n)) => Some(n),
                Some(Err(_)) => {
                    self.fail(400, INVALID_REQUEST, "bad Content-Length", &[], false);
                    return Err(());
                }
                None => {
                    self.fail(411, INVALID_REQUEST, "a POST needs a Content-Length", &[], false);
                    return Err(());
                }
            }
        };
        if length.is_some_and(|n| n > limits.max_body) {
            self.fail(413, INVALID_REQUEST, &format!("the body is larger than {} bytes", limits.max_body), &[], false);
            return Err(());
        }
        if self.head.header("expect").is_some_and(|e| e.eq_ignore_ascii_case("100-continue")) {
            let mut w = self.stream();
            if w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").is_err() {
                return Err(());
            }
        }
        let read = match length {
            Some(n) => read_exact(self.reader, n, deadline),
            None => read_chunked(self.reader, limits.max_body, deadline),
        };
        match read {
            Ok(body) => Ok(body),
            Err(e) if e.kind() == io::ErrorKind::FileTooLarge => {
                self.fail(413, INVALID_REQUEST, &format!("the body is larger than {} bytes", limits.max_body), &[], false);
                Err(())
            }
            Err(_) => Err(()),
        }
    }

    fn post(mut self) -> bool {
        let head = self.head;
        if !head.accepts("application/json") && !head.accepts("text/event-stream") {
            return self.fail(406, INVALID_REQUEST, "the client must accept application/json or text/event-stream", &[], false);
        }
        if head.header("content-type").is_some_and(|t| !t.split(';').next().unwrap_or("").trim().eq_ignore_ascii_case("application/json")) {
            return self.fail(415, INVALID_REQUEST, "the body is application/json", &[], false);
        }
        let Ok(body) = self.body() else { return false };
        let Ok(text) = std::str::from_utf8(&body) else {
            return self.fail(400, PARSE_ERROR, "the body is not UTF-8", &[], true);
        };
        let incoming = match rpc::parse(text) {
            Ok(incoming) => incoming,
            Err(error) => {
                // A response to the server (it sends no requests, so nothing waits for it).
                let response = serde_json::from_str::<Value>(text).is_ok_and(|v| {
                    v.get("method").is_none() && v.get("id").is_some() && (v.get("result").is_some() || v.get("error").is_some())
                });
                if response {
                    return write_response(self.stream(), 202, &[], b"", false).is_ok();
                }
                let body = json!({"jsonrpc": "2.0", "id": null, "error": error.to_json()}).to_string();
                return write_response(self.stream(), 400, &[], body.as_bytes(), false).is_ok();
            }
        };
        let (method, params) = match &incoming {
            Incoming::Request { method, params, .. } | Incoming::Notification { method, params } => (method.as_str(), params),
        };
        if let Some(version) = head.header(VERSION_HEADER) {
            if version != MODERN && !LEGACY.contains(&version) {
                let body = json!({"jsonrpc": "2.0", "id": null, "error": rpc::unsupported_version(version).to_json()}).to_string();
                return write_response(self.stream(), 400, &[], body.as_bytes(), false).is_ok();
            }
            if let Some(named) = rpc::requested_version(params).filter(|v| *v != version) {
                let message = format!("MCP-Protocol-Version is {version} but the request's _meta names {named}");
                return self.fail(400, INVALID_REQUEST, &message, &[], true);
            }
        }
        let is_request = matches!(incoming, Incoming::Request { .. });
        let mut created = None;
        let session = match head.header(SESSION_HEADER) {
            Some(id) => match self.shared.session(id) {
                Some(session) => Some(session),
                None => {
                    return self.fail(
                        404,
                        INVALID_REQUEST,
                        "no such session (it ended or expired): open a new one with initialize",
                        &[],
                        true,
                    );
                }
            },
            None if is_request && method == "initialize" => match self.shared.open_session() {
                Ok(Some((id, session))) => {
                    created = Some(id);
                    Some(session)
                }
                Ok(None) => return self.fail(503, INVALID_REQUEST, "too many sessions are open", &[("Retry-After", "5".into())], true),
                Err(e) => return self.fail(500, rpc::INTERNAL_ERROR, &format!("cannot make a session id: {e}"), &[], true),
            },
            None if !is_request || method == "server/discover" || method == "ping" || rpc::requested_version(params).is_some() => None,
            None => {
                let message = format!(
                    "`{method}` needs an Mcp-Session-Id header: open a session with initialize, or name the revision in \
                     _meta[\"{}\"] (\"{MODERN}\") to be served without one",
                    rpc::META_VERSION
                );
                return self.fail(400, INVALID_REQUEST, &message, &[], true);
            }
        };
        let server = session.as_ref().map_or(&self.shared.stateless, |s| &s.server).clone();
        match incoming {
            Incoming::Notification { method, mut params } => {
                if method == "notifications/cancelled" {
                    // Only a session's own requests can be named; a stateless client cancels a
                    // request by closing its response.
                    let internal = session.as_ref().and_then(|s| {
                        let key = rpc::id_key(params.get("requestId")?);
                        lock(&s.cancels).get(&key).cloned()
                    });
                    let Some(internal) = internal else { return write_response(self.stream(), 202, &[], b"", false).is_ok() };
                    params["requestId"] = internal.into();
                }
                server.handle_line(&json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string());
                write_response(self.stream(), 202, &[], b"", false).is_ok()
            }
            Incoming::Request { id, method, mut params } => {
                let _busy = session.as_ref().map(Busy::new);
                let key = self.shared.router.key();
                let token =
                    params.get_mut("_meta").and_then(|m| m.get_mut("progressToken")).map(|t| std::mem::replace(t, key.clone().into()));
                let (tx, rx) = channel();
                let client_key = rpc::id_key(&id);
                self.shared.router.insert(key.clone(), Route { id, token, tx });
                if let Some(s) = &session {
                    lock(&s.cancels).insert(client_key.clone(), key.clone());
                }
                server.handle_line(&json!({"jsonrpc": "2.0", "id": key, "method": method, "params": params}).to_string());
                let headers: Vec<(&str, String)> = created.into_iter().map(|id| ("Mcp-Session-Id", id)).collect();
                let keep = self.answer(&rx, &server, &key, &headers);
                self.shared.router.forget(&key);
                if let Some(s) = &session {
                    let mut cancels = lock(&s.cancels);
                    if cancels.get(&client_key) == Some(&key) {
                        cancels.remove(&client_key);
                    }
                }
                keep
            }
        }
    }

    /// Waits for a request's response and writes it: as JSON, or as an event stream once a
    /// notification for it comes first. A client that goes away cancels the request.
    fn answer(&self, rx: &Receiver<Out>, server: &Server, key: &str, headers: &[(&str, String)]) -> bool {
        let stream = self.stream();
        let streams = self.head.accepts("text/event-stream");
        let cancel = || {
            server.handle_line(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": key}}).to_string())
        };
        let mut sse: Option<Sse<'_>> = None;
        loop {
            match rx.recv_timeout(self.shared.config.limits.ping) {
                Ok(Out::Response(line)) => {
                    return match &sse {
                        None => write_response(stream, 200, headers, line.as_bytes(), false).is_ok(),
                        Some(sse) => sse.event(&line).and_then(|()| sse.end()).is_ok() && sse.chunked,
                    };
                }
                Ok(Out::Note(line)) => {
                    if !streams {
                        continue;
                    }
                    if sse.is_none() {
                        match Sse::start(stream, self.head.http11, headers) {
                            Ok(started) => sse = Some(started),
                            Err(_) => {
                                cancel();
                                return false;
                            }
                        }
                    }
                    if sse.as_ref().is_some_and(|s| s.event(&line).is_err()) {
                        cancel();
                        return false;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    let gone = match &sse {
                        Some(sse) => sse.ping().is_err(),
                        None => peer_closed(stream),
                    };
                    if gone {
                        cancel();
                        return false;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return sse.is_none() && self.fail(500, rpc::INTERNAL_ERROR, "the request was dropped", &[], false);
                }
            }
        }
    }

    /// A session's stream of what belongs to no request, until the client closes it, the session
    /// ends, or another `GET` takes its place.
    fn get(self) -> bool {
        if !self.head.accepts("text/event-stream") {
            return self.fail(406, INVALID_REQUEST, "a GET stream is text/event-stream", &[], true);
        }
        let Some(id) = self.head.header(SESSION_HEADER) else {
            let message = "a GET stream belongs to a session (open one with initialize); the current revision's \
                           notifications come on the POST of their subscriptions/listen";
            return self.fail(405, INVALID_REQUEST, message, &[("Allow", "POST, DELETE".into())], true);
        };
        let Some(session) = self.shared.session(id) else {
            return self.fail(404, INVALID_REQUEST, "no such session (it ended or expired)", &[], true);
        };
        let _busy = Busy::new(&session);
        let (tx, rx) = channel();
        let generation = self.shared.router.next.fetch_add(1, Ordering::Relaxed);
        *lock(&session.standalone) = Some((generation, tx));
        let ended = match Sse::start(self.stream(), self.head.http11, &[]) {
            Ok(sse) => loop {
                match rx.recv_timeout(self.shared.config.limits.ping) {
                    Ok(Out::Note(line) | Out::Response(line)) => {
                        if sse.event(&line).is_err() {
                            break false;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if sse.ping().is_err() {
                            break false;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break sse.end().is_ok() && sse.chunked,
                }
            },
            Err(_) => false,
        };
        let mut slot = lock(&session.standalone);
        if slot.as_ref().is_some_and(|(g, _)| *g == generation) {
            *slot = None;
        }
        ended
    }

    fn delete(self) -> bool {
        let Some(id) = self.head.header(SESSION_HEADER) else {
            return self.fail(400, INVALID_REQUEST, "DELETE ends the session its Mcp-Session-Id names", &[], true);
        };
        let removed = lock(&self.shared.sessions).remove(id);
        let Some(session) = removed else {
            return self.fail(404, INVALID_REQUEST, "no such session (it ended or expired)", &[], true);
        };
        // Its GET stream ends; requests still running finish on their own.
        *lock(&session.standalone) = None;
        write_response(self.stream(), 204, &[], b"", false).is_ok()
    }
}

fn read_exact(reader: &mut BufReader<TcpStream>, n: usize, deadline: Instant) -> io::Result<Vec<u8>> {
    let mut body = vec![0u8; n];
    let mut filled = 0;
    while filled < n {
        reader.get_ref().set_read_timeout(Some(remaining(deadline)?))?;
        match reader.read(&mut body[filled..]) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(k) => filled += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(body)
}

/// One CRLF-terminated line of at most `max` bytes.
fn read_line(reader: &mut BufReader<TcpStream>, max: usize, deadline: Instant) -> io::Result<String> {
    let mut line = Vec::new();
    loop {
        reader.get_ref().set_read_timeout(Some(remaining(deadline)?))?;
        let mut byte = [0u8; 1];
        match reader.read(&mut byte) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
        if line.len() > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a chunk line is too long"));
        }
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "a chunk line is not UTF-8"))
}

/// A chunked body of at most `max` bytes (`FileTooLarge` past it).
fn read_chunked(reader: &mut BufReader<TcpStream>, max: usize, deadline: Instant) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = read_line(reader, 1024, deadline)?;
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "a bad chunk size"))?;
        if size == 0 {
            // Trailers, up to the blank line.
            while !read_line(reader, 8 << 10, deadline)?.is_empty() {}
            return Ok(body);
        }
        if body.len().saturating_add(size) > max {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        body.extend(read_exact(reader, size, deadline)?);
        if !read_line(reader, 1, deadline)?.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a chunk is longer than its size"));
        }
    }
}

#[cfg(test)]
mod tests;
