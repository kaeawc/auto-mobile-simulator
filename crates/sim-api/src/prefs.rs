//! App preferences on both platforms through one typed API, without an SDK in the app: an app's
//! stores (Android SharedPreferences XML files and DataStore preferences, iOS the app's domain, its
//! app-group suites and any named domain), every key with a typed value ([`PrefValue`]), writes
//! and deletes (one key or a batch), and a feed of changes.
//!
//! Each platform implements [`Backend`]; the methods here parse and check every parameter (types,
//! what a store can hold, a batch's keys) before a backend sees it. Until a platform's backend
//! lands, [`Unavailable`] answers its devices with a clear error.
//!
//! - A **batch** (`prefs_batch`) is all or nothing: every op is checked first, and the backend
//!   applies them as one write of the store or none at all. Its result has one visibility.
//! - A **write** says when the running app sees it ([`Visibility`]): now, or after a relaunch (a
//!   platform that can't reach a running app's in-memory copy says so instead of a silent no-op).
//! - **`prefs_watch`** follows `logs`' `stream`: the first call starts one feed for the device, app
//!   and store in this (long-running) process and answers with a cursor; later calls with that
//!   cursor answer the changes since, waiting up to `wait_ms` for one. A feed nobody reads for 5
//!   minutes stops.

mod value;

#[cfg(test)]
pub(crate) mod fake;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sim_core::platform::Platform;

pub use value::{Date, PrefValue, Type};

use crate::params::need_str;

/// The kinds of preference store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StoreKind {
    /// Android `shared_prefs/<name>.xml`.
    SharedPreferences,
    /// Android Jetpack DataStore preferences, `datastore/<name>.preferences_pb`.
    DataStore,
    /// iOS: the app's own domain (its bundle id).
    AppDomain,
    /// iOS: an app-group suite (`group.…`).
    AppGroup,
    /// iOS: any other named domain.
    Domain,
}

impl StoreKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StoreKind::SharedPreferences => "shared_preferences",
            StoreKind::DataStore => "datastore",
            StoreKind::AppDomain => "app_domain",
            StoreKind::AppGroup => "app_group",
            StoreKind::Domain => "domain",
        }
    }

    pub fn platform(self) -> Platform {
        match self {
            StoreKind::SharedPreferences | StoreKind::DataStore => Platform::Android,
            StoreKind::AppDomain | StoreKind::AppGroup | StoreKind::Domain => Platform::Ios,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StoreKind::SharedPreferences => "Android SharedPreferences",
            StoreKind::DataStore => "Android DataStore preferences",
            StoreKind::AppDomain | StoreKind::AppGroup | StoreKind::Domain => "iOS user defaults",
        }
    }

    /// The types a store of this kind holds, at the top level and (iOS) nested.
    pub fn types(self) -> &'static [Type] {
        use Type::*;
        match self {
            StoreKind::SharedPreferences => &[String, Int, Long, Float, Bool, StringSet],
            StoreKind::DataStore => &[String, Int, Long, Float, Double, Bool, StringSet],
            StoreKind::AppDomain | StoreKind::AppGroup | StoreKind::Domain => {
                &[String, Int, Long, Float, Double, Bool, Date, Data, Array, Dictionary]
            }
        }
    }

    /// Refuses a value (or a value nested in it) of a type this kind doesn't hold; `at` names it.
    pub fn check(self, value: &PrefValue, at: &str) -> Result<(), String> {
        if !self.types().contains(&value.ty()) {
            let names: Vec<&str> = self.types().iter().map(|t| t.as_str()).collect();
            return Err(format!(
                "`{at}` is a `{}`, which {} can't hold (it holds {})",
                value.ty().as_str(),
                self.label(),
                names.join(", ")
            ));
        }
        value.children().try_for_each(|child| self.check(child, &format!("{at} (nested)")))
    }
}

/// One preference store of an app.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Store {
    pub kind: StoreKind,
    /// The app's package name or bundle id.
    pub app: String,
    /// Android: the file's name without its directory and extension; iOS: the domain.
    pub name: String,
}

impl Store {
    pub fn new(kind: StoreKind, app: &str, name: &str) -> Store {
        Store { kind, app: app.to_owned(), name: name.to_owned() }
    }

    /// The id a call names the store by: Android the file under the app's data directory
    /// (`shared_prefs/settings.xml`, `datastore/settings.preferences_pb`), iOS the domain.
    pub fn id(&self) -> String {
        match self.kind {
            StoreKind::SharedPreferences => format!("shared_prefs/{}.xml", self.name),
            StoreKind::DataStore => format!("datastore/{}.preferences_pb", self.name),
            StoreKind::AppDomain | StoreKind::AppGroup | StoreKind::Domain => self.name.clone(),
        }
    }

    /// The store `id` names for `app` on a `platform` device. An iOS id is the app's domain when
    /// it is the app's bundle id, an app-group suite when it starts `group.`, else a named domain.
    pub fn parse(platform: Platform, app: &str, id: &str) -> Result<Store, String> {
        let plain = |name: &str| !name.is_empty() && !name.contains('/') && name != "." && name != "..";
        match platform {
            Platform::Android => {
                let shared = id.strip_prefix("shared_prefs/").and_then(|r| r.strip_suffix(".xml")).filter(|n| plain(n));
                let datastore = id.strip_prefix("datastore/").and_then(|r| r.strip_suffix(".preferences_pb")).filter(|n| plain(n));
                match (shared, datastore) {
                    (Some(name), _) => Ok(Store::new(StoreKind::SharedPreferences, app, name)),
                    (_, Some(name)) => Ok(Store::new(StoreKind::DataStore, app, name)),
                    _ => Err(format!(
                        "`{id}` isn't an Android preferences store: name one as `shared_prefs/<name>.xml` or \
                         `datastore/<name>.preferences_pb` (prefs_stores lists the app's)"
                    )),
                }
            }
            Platform::Ios if !plain(id) => Err(format!("`{id}` isn't an iOS defaults domain: a domain is a name, not a path")),
            Platform::Ios if id == app => Ok(Store::new(StoreKind::AppDomain, app, id)),
            Platform::Ios if id.starts_with("group.") => Ok(Store::new(StoreKind::AppGroup, app, id)),
            Platform::Ios => Ok(Store::new(StoreKind::Domain, app, id)),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({"id": self.id(), "kind": self.kind.as_str(), "name": self.name, "app": self.app, "platform": self.kind.platform().as_str()})
    }
}

/// Every key of a store with its value.
pub type Entries = BTreeMap<String, PrefValue>;

pub fn entries_json(entries: &Entries) -> Value {
    Value::Object(entries.iter().map(|(k, v)| (k.clone(), v.to_json())).collect())
}

/// One change a write makes.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Set { key: String, value: PrefValue },
    Delete { key: String },
}

impl Op {
    pub fn key(&self) -> &str {
        match self {
            Op::Set { key, .. } | Op::Delete { key } => key,
        }
    }
}

/// When the running app sees a write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    /// The running app (and an app that isn't running, when it starts) sees it now.
    Now,
    /// The app sees it once it is relaunched; a running copy keeps its old values in memory.
    AfterRelaunch,
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Now => "now",
            Visibility::AfterRelaunch => "after_relaunch",
        }
    }
}

/// What a write (or a batch) did.
#[derive(Clone, Debug, PartialEq)]
pub struct WriteResult {
    pub visible: Visibility,
    /// Why, when the app sees it only after a relaunch, and what to do about it.
    pub note: Option<String>,
}

/// One change to a key, as a feed reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub store: Store,
    pub key: String,
    /// `None`: the key was added.
    pub old: Option<PrefValue>,
    /// `None`: the key was removed.
    pub new: Option<PrefValue>,
    pub at: Date,
}

impl Change {
    pub fn to_json(&self) -> Value {
        let typed = |v: &Option<PrefValue>| v.as_ref().map_or(Value::Null, PrefValue::to_json);
        json!({"store": self.store.id(), "key": self.key, "old": typed(&self.old), "new": typed(&self.new), "at": self.at.to_iso8601()})
    }
}

/// The changes from `before` to `after`, by key: for a backend that sees a store's whole contents
/// before and after (a file it re-reads).
pub fn changes(store: &Store, before: &Entries, after: &Entries, at: Date) -> Vec<Change> {
    let keys: std::collections::BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    keys.into_iter()
        .filter(|k| before.get(*k) != after.get(*k))
        .map(|k| Change { store: store.clone(), key: k.clone(), old: before.get(k).cloned(), new: after.get(k).cloned(), at })
        .collect()
}

/// One platform's preferences. `device` is the device's id as the dispatcher resolved it (a
/// simulator's UDID, an AVD's name). Every parameter has been checked before a backend sees it:
/// a store is the device's platform's, each value is of a type its store holds, a batch names
/// each key once.
pub trait Backend {
    /// The app's stores.
    fn stores(&mut self, device: &str, app: &str) -> Result<Vec<Store>, String>;

    /// Every key of `store` with its value.
    fn read(&mut self, device: &str, store: &Store) -> Result<Entries, String>;

    /// Applies `ops` in order, all or nothing: one write of the store, or an error and no change.
    fn write_batch(&mut self, device: &str, store: &Store, ops: &[Op]) -> Result<WriteResult, String>;

    fn write(&mut self, device: &str, store: &Store, key: &str, value: &PrefValue) -> Result<WriteResult, String> {
        self.write_batch(device, store, &[Op::Set { key: key.to_owned(), value: value.clone() }])
    }

    fn delete(&mut self, device: &str, store: &Store, key: &str) -> Result<WriteResult, String> {
        self.write_batch(device, store, &[Op::Delete { key: key.to_owned() }])
    }

    /// Starts a feed of the changes to `store` (every store of the app when `None`), from whoever
    /// makes them (the app, this API, another tool), in the order they happen. The backend stops
    /// it when the receiver is dropped (a send fails) and closes it when the device stops.
    fn watch(&mut self, device: &str, app: &str, store: Option<&Store>) -> Result<Receiver<Change>, String>;
}

/// The backend of a platform whose preferences haven't landed yet: every call says so.
pub struct Unavailable(pub Platform);

impl Unavailable {
    fn refuse<T>(&self) -> Result<T, String> {
        Err(format!("app preferences (`prefs_*`) are not yet available on {} devices", self.0.label()))
    }
}

impl Backend for Unavailable {
    fn stores(&mut self, _: &str, _: &str) -> Result<Vec<Store>, String> {
        self.refuse()
    }

    fn read(&mut self, _: &str, _: &Store) -> Result<Entries, String> {
        self.refuse()
    }

    fn write_batch(&mut self, _: &str, _: &Store, _: &[Op]) -> Result<WriteResult, String> {
        self.refuse()
    }

    fn watch(&mut self, _: &str, _: &str, _: Option<&Store>) -> Result<Receiver<Change>, String> {
        self.refuse()
    }
}

/// Each platform's backend, per connection.
pub struct Session {
    pub(crate) ios: Box<dyn Backend>,
    pub(crate) android: Box<dyn Backend>,
}

impl Default for Session {
    fn default() -> Session {
        Session { ios: Box::new(Unavailable(Platform::Ios)), android: Box::new(Unavailable(Platform::Android)) }
    }
}

impl Session {
    pub(crate) fn backend(&mut self, platform: Platform) -> &mut dyn Backend {
        match platform {
            Platform::Ios => &mut *self.ios,
            Platform::Android => &mut *self.android,
        }
    }
}

/// The preference methods.
pub(crate) const METHODS: &[&str] = &["prefs_stores", "prefs_read", "prefs_write", "prefs_delete", "prefs_batch", "prefs_watch"];

pub(crate) fn is_method(method: &str) -> bool {
    METHODS.contains(&method)
}

/// The longest `prefs_watch` waits for a change.
pub const MAX_WAIT: Duration = Duration::from_secs(60);

/// An optional string parameter: absent or null is `None`, any other non-string is refused.
fn opt_string<'a>(p: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => Err(format!("`{key}` must be a string, not {other}")),
    }
}

/// `value` and `type` of a write (both required), read as a typed value for `store`.
fn typed_value(store: &Store, ty: &str, value: &Value, at: &str) -> Result<PrefValue, String> {
    let value = PrefValue::from_json(Type::parse(ty)?, value, at)?;
    store.kind.check(&value, at)?;
    Ok(value)
}

fn write_json(store: &Store, keys: Value, done: &WriteResult) -> Value {
    let mut out = json!({"store": store.id(), "visible": done.visible.as_str()});
    match keys {
        Value::String(key) => out["key"] = Value::String(key),
        keys => out["keys"] = keys,
    }
    if let Some(note) = &done.note {
        out["note"] = Value::String(note.clone());
    }
    out
}

/// A batch's `ops`: `{key, type, value}` sets and `{key, delete: true}` deletes, each key once.
fn batch_ops(store: &Store, ops: &[Value]) -> Result<Vec<Op>, String> {
    if ops.is_empty() {
        return Err("`ops` is empty: give at least one {key, type, value} or {key, delete: true}".into());
    }
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(ops.len());
    for (i, op) in ops.iter().enumerate() {
        let at = format!("ops[{i}]");
        let Some(obj) = op.as_object() else {
            return Err(format!("`{at}` must be {{key, type, value}} or {{key, delete: true}}, not {op}"));
        };
        let Some(key) = obj.get("key").and_then(Value::as_str) else { return Err(format!("`{at}` has no string `key`")) };
        if let Some(extra) = obj.keys().find(|k| !["key", "type", "value", "delete"].contains(&k.as_str())) {
            return Err(format!("`{at}` has `{extra}`; an op is {{key, type, value}} or {{key, delete: true}}"));
        }
        let delete = match obj.get("delete") {
            None | Some(Value::Bool(false)) => false,
            Some(Value::Bool(true)) => true,
            Some(other) => return Err(format!("`{at}.delete` must be true or false, not {other}")),
        };
        let op = match (delete, obj.get("type"), obj.get("value")) {
            (true, None, None) => Op::Delete { key: key.to_owned() },
            (true, ..) => return Err(format!("`{at}` deletes `{key}`, so it takes no `type` or `value`")),
            (false, Some(Value::String(ty)), Some(value)) => {
                Op::Set { key: key.to_owned(), value: typed_value(store, ty, value, &format!("{at}.value"))? }
            }
            (false, ..) => return Err(format!("`{at}` sets `{key}`: give a string `type` and a `value` (or `delete: true`)")),
        };
        if !seen.insert(key.to_owned()) {
            return Err(format!("`ops` names `{key}` twice; a batch changes each key once"));
        }
        out.push(op);
    }
    Ok(out)
}

/// Runs one preference method on the `platform` device `device` (resolved by the dispatcher).
pub(crate) fn call(backend: &mut dyn Backend, platform: Platform, device: &str, method: &str, p: &Value) -> Result<Value, String> {
    let app = need_str(p, "app")?;
    // Every parameter's presence is checked before any is read, so a call missing one says so.
    match method {
        "prefs_stores" => Ok(backend.stores(device, app)?.iter().map(Store::to_json).collect()),
        "prefs_read" => {
            let stores = match opt_string(p, "store")? {
                Some(id) => vec![Store::parse(platform, app, id)?],
                None => backend.stores(device, app)?,
            };
            let mut out = Vec::with_capacity(stores.len());
            for store in stores {
                let entries = backend.read(device, &store)?;
                out.push(json!({"store": store.to_json(), "values": entries_json(&entries)}));
            }
            Ok(Value::Array(out))
        }
        "prefs_write" => {
            let (id, key, ty) = (need_str(p, "store")?, need_str(p, "key")?, need_str(p, "type")?);
            let value = p.get("value").filter(|v| !v.is_null()).ok_or("missing parameter `value`")?;
            let store = Store::parse(platform, app, id)?;
            let value = typed_value(&store, ty, value, "value")?;
            let done = backend.write(device, &store, key, &value)?;
            Ok(write_json(&store, Value::String(key.to_owned()), &done))
        }
        "prefs_delete" => {
            let (id, key) = (need_str(p, "store")?, need_str(p, "key")?);
            let store = Store::parse(platform, app, id)?;
            let done = backend.delete(device, &store, key)?;
            Ok(write_json(&store, Value::String(key.to_owned()), &done))
        }
        "prefs_batch" => {
            let id = need_str(p, "store")?;
            let ops = match p.get("ops") {
                None | Some(Value::Null) => return Err("missing array parameter `ops`".into()),
                Some(Value::Array(ops)) => ops,
                Some(other) => return Err(format!("`ops` must be an array of {{key, type, value}} or {{key, delete: true}}, not {other}")),
            };
            let store = Store::parse(platform, app, id)?;
            let ops = batch_ops(&store, ops)?;
            let done = backend.write_batch(device, &store, &ops)?;
            Ok(write_json(&store, ops.iter().map(|op| Value::String(op.key().to_owned())).collect(), &done))
        }
        "prefs_watch" => {
            let store = opt_string(p, "store")?.map(|id| Store::parse(platform, app, id)).transpose()?;
            let cursor = opt_string(p, "cursor")?;
            let wait = match p.get("wait_ms") {
                None | Some(Value::Null) => Duration::ZERO,
                Some(v) => {
                    Duration::from_millis(v.as_u64().ok_or_else(|| format!("`wait_ms` must be a whole number of milliseconds, not {v}"))?)
                }
            };
            let key = FeedKey { platform, device: device.to_owned(), app: app.to_owned(), store: store.as_ref().map(Store::id) };
            let feed = with_feeds(|feeds| feeds.get(key, || backend.watch(device, app, store.as_ref())))?;
            Ok(lock(&feed).poll(cursor, wait.min(MAX_WAIT)))
        }
        other => Err(format!("`{other}` isn't a preferences method")),
    }
}

/// How long a feed nobody reads keeps running.
const FEED_IDLE: Duration = Duration::from_secs(300);

/// The changes a feed keeps for callers that are behind; older ones are dropped (and counted).
const FEED_EVENTS: usize = 4096;

static FEEDS: Mutex<Option<Feeds>> = Mutex::new(None);
static NEXT_FEED: AtomicU64 = AtomicU64::new(1);

fn with_feeds<T>(f: impl FnOnce(&mut Feeds) -> T) -> T {
    let mut guard = FEEDS.lock().unwrap_or_else(PoisonError::into_inner);
    f(guard.get_or_insert_with(|| Feeds::new(FEED_IDLE)))
}

type SharedFeed = Arc<Mutex<Feed>>;

fn lock(feed: &SharedFeed) -> MutexGuard<'_, Feed> {
    feed.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a feed follows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FeedKey {
    platform: Platform,
    device: String,
    app: String,
    /// `None`: every store of the app.
    store: Option<String>,
}

/// The running feeds of this process, one per [`FeedKey`].
struct Feeds {
    feeds: HashMap<FeedKey, SharedFeed>,
    idle: Duration,
}

impl Feeds {
    fn new(idle: Duration) -> Feeds {
        Feeds { feeds: HashMap::new(), idle }
    }

    /// The running feed for `key`, started with `start` when there is none (or it ended). Feeds
    /// nobody has read for `idle` stop first (one being read now is busy, so kept).
    fn get(&mut self, key: FeedKey, start: impl FnOnce() -> Result<Receiver<Change>, String>) -> Result<SharedFeed, String> {
        let idle = self.idle;
        self.feeds.retain(|_, feed| feed.try_lock().map_or(true, |f| !f.ended && f.used.elapsed() < idle));
        if let Some(feed) = self.feeds.get(&key) {
            return Ok(feed.clone());
        }
        let feed = Arc::new(Mutex::new(Feed::new(start()?)));
        self.feeds.insert(key, feed.clone());
        Ok(feed)
    }
}

/// One feed: the backend's receiver, read only when a caller asks (no thread of its own), and the
/// changes kept for callers.
struct Feed {
    id: u64,
    rx: Receiver<Change>,
    /// Each kept change with its sequence number (1, 2, ...), oldest first.
    events: VecDeque<(u64, Change)>,
    /// The sequence number of the last change received.
    head: u64,
    used: Instant,
    /// The backend closed it (the device stopped, or the backend gave up).
    ended: bool,
}

impl Feed {
    fn new(rx: Receiver<Change>) -> Feed {
        Feed { id: NEXT_FEED.fetch_add(1, Ordering::Relaxed), rx, events: VecDeque::new(), head: 0, used: Instant::now(), ended: false }
    }

    fn push(&mut self, change: Change) {
        self.head += 1;
        self.events.push_back((self.head, change));
        if self.events.len() > FEED_EVENTS {
            self.events.pop_front();
        }
    }

    /// Takes what the backend has sent.
    fn drain(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(change) => self.push(change),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.ended = true;
                    return;
                }
            }
        }
    }

    fn cursor(&self) -> String {
        format!("{}:{}", self.id, self.head)
    }

    /// The answer to one `prefs_watch` call.
    ///
    /// - No cursor: the changes from now on are this caller's; `{cursor, events: []}`.
    /// - A cursor of this feed: the changes after it, waiting up to `wait` for one;
    ///   `{cursor, events, missed?}`, `missed` counting changes dropped before they were read.
    /// - A cursor of an earlier feed (stopped when idle or when its device stopped): `reset: true`
    ///   and a new cursor; changes in between may be lost, so re-read the store.
    ///
    /// `ended: true` says the backend closed the feed: the next call starts a new one.
    fn poll(&mut self, cursor: Option<&str>, wait: Duration) -> Value {
        self.used = Instant::now();
        self.drain();
        let after = match cursor.map(parse_cursor) {
            None => None,
            Some(Some((id, seq))) if id == self.id && seq <= self.head => Some(seq),
            Some(_) => {
                let mut out = json!({"cursor": self.cursor(), "events": [], "reset": true});
                if self.ended {
                    out["ended"] = true.into();
                }
                return out;
            }
        };
        let Some(after) = after else {
            let mut out = json!({"cursor": self.cursor(), "events": []});
            if self.ended {
                out["ended"] = true.into();
            }
            return out;
        };
        if self.head == after && !self.ended && !wait.is_zero() {
            match self.rx.recv_timeout(wait) {
                Ok(change) => {
                    self.push(change);
                    self.drain();
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.ended = true,
            }
            self.used = Instant::now();
        }
        let oldest = self.events.front().map_or(self.head + 1, |(seq, _)| *seq);
        let events: Vec<Value> = self.events.iter().filter(|(seq, _)| *seq > after).map(|(_, c)| c.to_json()).collect();
        let mut out = json!({"cursor": self.cursor(), "events": events});
        if oldest > after + 1 {
            out["missed"] = (oldest - after - 1).into();
        }
        if self.ended {
            out["ended"] = true.into();
        }
        out
    }
}

/// `<feed>:<seq>`.
fn parse_cursor(cursor: &str) -> Option<(u64, u64)> {
    let (id, seq) = cursor.split_once(':')?;
    Some((id.parse().ok()?, seq.parse().ok()?))
}

#[cfg(test)]
mod tests;
