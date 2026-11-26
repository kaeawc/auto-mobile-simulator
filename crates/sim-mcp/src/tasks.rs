//! Tasks (the `io.modelcontextprotocol/tasks` extension): a long method call run on its own thread,
//! with a handle the client polls (`tasks/get`) or is told about (`notifications/tasks`).
//!
//! Cancellation is cooperative, as the extension allows: a cancelled task is `cancelled` at once and
//! its result is thrown away, but a device call already in flight (a boot, an install) can't be
//! interrupted through the API, so the call itself runs to its end in the background.

use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::rpc::{INTERNAL_ERROR, META_SUBSCRIPTION, RpcError};

/// How long a task is kept, from its creation (the work isn't stopped when it passes).
const TTL: Duration = Duration::from_secs(3600);
/// How often a client should poll.
const POLL_MS: u64 = 1000;
/// How many tasks' calls may be running at once. A cancelled task's call keeps running (the API
/// can't interrupt it), so it counts until it ends.
pub const MAX_RUNNING: usize = 8;

/// Writes one outgoing JSON-RPC line.
pub type Sink = Arc<dyn Fn(String) + Send + Sync>;

enum State {
    Working,
    Completed(Value),
    Failed(Value),
    Cancelled,
}

struct Entry {
    method: String,
    created: SystemTime,
    updated: SystemTime,
    started: Instant,
    finished: Option<Instant>,
    state: State,
}

/// A client listening for status changes of some tasks (`subscriptions/listen` with `taskIds`).
struct Listener {
    subscription: Value,
    ids: HashSet<String>,
    sink: Sink,
}

#[derive(Default)]
struct Table {
    entries: HashMap<String, Entry>,
    listeners: Vec<Listener>,
}

/// The tasks of one server.
#[derive(Clone)]
pub struct Tasks {
    table: Arc<Mutex<Table>>,
    /// Calls running now, cancelled tasks' included.
    running: Arc<AtomicUsize>,
    ttl: Duration,
}

/// One running call, counted until it ends (or its thread can't start).
struct Running(Arc<AtomicUsize>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Default for Tasks {
    fn default() -> Tasks {
        Tasks::with_ttl(TTL)
    }
}

/// `2025-11-25T10:30:00Z` for a time.
pub fn iso8601(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// An unguessable task id: 128 random bits as hex.
fn new_id() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).is_err() {
        // No urandom: mix the clock and the process's random hash keys instead.
        use std::hash::{BuildHasher, Hasher};
        for chunk in bytes.chunks_mut(8) {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
            chunk.copy_from_slice(&h.finish().to_le_bytes()[..chunk.len()]);
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Tasks {
    pub fn with_ttl(ttl: Duration) -> Tasks {
        Tasks { table: Arc::default(), running: Arc::default(), ttl }
    }

    fn lock(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The task as `tasks/get` and `notifications/tasks` carry it.
    fn detail(&self, id: &str, e: &Entry) -> Value {
        let secs = e.finished.unwrap_or_else(Instant::now).duration_since(e.started).as_secs();
        let mut task = json!({
            "taskId": id,
            "createdAt": iso8601(e.created),
            "lastUpdatedAt": iso8601(e.updated),
            "ttlMs": self.ttl.as_millis() as u64,
            "pollIntervalMs": POLL_MS,
        });
        let (status, message) = match &e.state {
            State::Working => ("working", format!("{} has been running for {secs} s", e.method)),
            State::Completed(result) => {
                task["result"] = result.clone();
                if result["isError"] == true {
                    let text = result["content"][0]["text"].as_str().unwrap_or("");
                    ("completed", format!("{} failed after {secs} s: {text}", e.method))
                } else {
                    ("completed", format!("{} finished in {secs} s", e.method))
                }
            }
            State::Failed(error) => {
                task["error"] = error.clone();
                ("failed", format!("{} failed: {}", e.method, error["message"].as_str().unwrap_or("internal error")))
            }
            State::Cancelled => (
                "cancelled",
                format!(
                    "cancelled by the client; a running {} call can't be interrupted, so it finishes in the background and its result is discarded",
                    e.method
                ),
            ),
        };
        task["status"] = status.into();
        task["statusMessage"] = message.into();
        task
    }

    /// Drops finished tasks older than their time to live.
    fn sweep(&self, table: &mut Table) {
        let ttl = self.ttl;
        table.entries.retain(|_, e| e.finished.is_none() || e.started.elapsed() < ttl);
    }

    /// Starts `work` on its own thread as a task for `method`. Its return value is the task's
    /// result (a `CallToolResult`). The handle (a `CreateTaskResult`'s fields) is returned at once;
    /// with [`MAX_RUNNING`] calls running (cancelled tasks' too), it fails instead.
    pub fn create(&self, method: &str, work: impl FnOnce() -> Value + Send + 'static) -> Result<Value, String> {
        let id = new_id();
        let now = SystemTime::now();
        let (handle, running) = {
            let mut table = self.lock();
            self.sweep(&mut table);
            let running = self.running.load(Ordering::SeqCst);
            if running >= MAX_RUNNING {
                return Err(format!(
                    "{running} calls are already running (a cancelled task's call counts until it ends); wait for one to finish"
                ));
            }
            self.running.fetch_add(1, Ordering::SeqCst);
            let entry = Entry {
                method: method.to_owned(),
                created: now,
                updated: now,
                started: Instant::now(),
                finished: None,
                state: State::Working,
            };
            let handle = self.detail(&id, &entry);
            table.entries.insert(id.clone(), entry);
            (handle, Running(self.running.clone()))
        };
        let (tasks, task_id) = (self.clone(), id.clone());
        let spawned = std::thread::Builder::new().name(format!("simee-task-{}", &id[..8])).spawn(move || {
            let _running = running;
            let outcome = catch_unwind(AssertUnwindSafe(work));
            tasks.finish(&task_id, outcome.map_err(|_| RpcError::new(INTERNAL_ERROR, "the task's thread panicked").to_json()));
        });
        if let Err(e) = spawned {
            self.finish(&id, Err(RpcError::new(INTERNAL_ERROR, format!("cannot start the task's thread: {e}")).to_json()));
        }
        Ok(handle)
    }

    /// Records how a task ended, unless it was cancelled first, and tells its listeners.
    fn finish(&self, id: &str, outcome: Result<Value, Value>) {
        let mut table = self.lock();
        let Some(entry) = table.entries.get_mut(id) else { return };
        if !matches!(entry.state, State::Working) {
            return;
        }
        entry.state = match outcome {
            Ok(result) => State::Completed(result),
            Err(error) => State::Failed(error),
        };
        entry.updated = SystemTime::now();
        entry.finished = Some(Instant::now());
        self.announce(&table, id);
    }

    /// Sends the task's state to each listener that asked about it. (Lines go out with the table
    /// locked, so they are in order; sinks only write a line.)
    fn announce(&self, table: &Table, id: &str) {
        let Some(entry) = table.entries.get(id) else { return };
        for l in table.listeners.iter().filter(|l| l.ids.contains(id)) {
            let mut params = self.detail(id, entry);
            params["_meta"] = json!({META_SUBSCRIPTION: l.subscription});
            (l.sink)(json!({"jsonrpc": "2.0", "method": "notifications/tasks", "params": params}).to_string());
        }
    }

    /// The task, for `tasks/get`.
    pub fn get(&self, id: &str) -> Option<Value> {
        let mut table = self.lock();
        self.sweep(&mut table);
        table.entries.get(id).map(|e| self.detail(id, e))
    }

    /// Cancels a working task; `false` for an id that isn't one. A task already finished stays as
    /// it ended.
    pub fn cancel(&self, id: &str) -> bool {
        let mut table = self.lock();
        let Some(entry) = table.entries.get_mut(id) else { return false };
        if matches!(entry.state, State::Working) {
            entry.state = State::Cancelled;
            entry.updated = SystemTime::now();
            entry.finished = Some(Instant::now());
            self.announce(&table, id);
        }
        true
    }

    /// Starts telling `sink` about these tasks, under `subscription`. `acknowledge` is called first
    /// with the ids that exist, before any notification can be sent for them.
    pub fn subscribe(&self, subscription: Value, ids: &[String], sink: Sink, acknowledge: impl FnOnce(&[String])) {
        let mut table = self.lock();
        let known: Vec<String> = ids.iter().filter(|id| table.entries.contains_key(*id)).cloned().collect();
        acknowledge(&known);
        table.listeners.push(Listener { subscription, ids: known.into_iter().collect(), sink });
    }

    pub fn unsubscribe(&self, subscription: &Value) {
        self.lock().listeners.retain(|l| &l.subscription != subscription);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{Sender, channel};

    use super::*;

    fn gated() -> (Sender<()>, impl FnOnce() -> Value + Send + 'static) {
        let (release, wait) = channel::<()>();
        (release, move || {
            wait.recv().ok();
            json!({"content": [{"type": "text", "text": "done"}], "isError": false})
        })
    }

    fn status(tasks: &Tasks, id: &str) -> String {
        tasks.get(id).expect("task")["status"].as_str().expect("status").to_owned()
    }

    fn wait_for(tasks: &Tasks, id: &str, want: &str) -> Value {
        for _ in 0..500 {
            let task = tasks.get(id).expect("task");
            if task["status"] == want {
                return task;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("task never became {want}");
    }

    #[test]
    fn a_task_works_then_completes_with_its_result() {
        let tasks = Tasks::default();
        let (release, work) = gated();
        let handle = tasks.create("boot", work).expect("create");
        let id = handle["taskId"].as_str().expect("id").to_owned();
        assert_eq!(
            (handle["status"].clone(), handle["ttlMs"].clone(), handle["pollIntervalMs"].clone()),
            (json!("working"), json!(3_600_000), json!(1000))
        );
        assert!(id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(tasks.get(&id).expect("task")["statusMessage"].as_str().is_some_and(|m| m.starts_with("boot has been running")));
        assert_eq!(status(&tasks, &id), "working");
        release.send(()).expect("release");
        let done = wait_for(&tasks, &id, "completed");
        assert_eq!(done["result"]["content"][0]["text"], "done");
        assert!(done["statusMessage"].as_str().is_some_and(|m| m.starts_with("boot finished in")));
        assert_eq!(done.get("error"), None);
        assert!(done["createdAt"].as_str().is_some_and(|t| t.ends_with('Z') && t.len() == 20));
    }

    #[test]
    fn a_tool_error_is_a_completed_task_and_a_panic_a_failed_one() {
        let tasks = Tasks::default();
        let handle =
            tasks.create("boot", || json!({"content": [{"type": "text", "text": "no such device"}], "isError": true})).expect("create");
        let done = wait_for(&tasks, handle["taskId"].as_str().expect("id"), "completed");
        assert_eq!(done["result"]["isError"], true);
        assert_eq!(done["statusMessage"].as_str().map(|m| m.contains("no such device")), Some(true));
        let handle = tasks.create("install", || -> Value { panic!("boom") }).expect("create");
        let failed = wait_for(&tasks, handle["taskId"].as_str().expect("id"), "failed");
        assert_eq!(failed["error"]["code"], INTERNAL_ERROR);
        assert_eq!(failed.get("result"), None);
    }

    #[test]
    fn cancelling_a_working_task_discards_its_result() {
        let tasks = Tasks::default();
        let (release, work) = gated();
        let id = tasks.create("boot", work).expect("create")["taskId"].as_str().expect("id").to_owned();
        assert!(tasks.cancel(&id));
        let cancelled = tasks.get(&id).expect("task");
        assert_eq!(cancelled["status"], "cancelled");
        assert!(cancelled["statusMessage"].as_str().is_some_and(|m| m.contains("finishes in the background")));
        // The call finishes; the task stays cancelled and carries no result.
        release.send(()).expect("release");
        std::thread::sleep(Duration::from_millis(50));
        let after = tasks.get(&id).expect("task");
        assert_eq!((after["status"].clone(), after.get("result")), (json!("cancelled"), None));
        // Cancelling again, or a finished task, changes nothing; an unknown id isn't a task.
        assert!(tasks.cancel(&id));
        assert!(!tasks.cancel("nope") && tasks.get("nope").is_none());
        let finished = tasks.create("boot", || json!({"content": []})).expect("create")["taskId"].as_str().expect("id").to_owned();
        wait_for(&tasks, &finished, "completed");
        assert!(tasks.cancel(&finished));
        assert_eq!(status(&tasks, &finished), "completed");
    }

    #[test]
    fn at_most_eight_calls_run_at_once_even_if_their_tasks_are_cancelled() {
        let tasks = Tasks::default();
        let mut releases = Vec::new();
        for _ in 0..MAX_RUNNING {
            let (release, work) = gated();
            tasks.create("boot", work).expect("create");
            releases.push(release);
        }
        let (extra, work) = gated();
        assert!(tasks.create("boot", work).expect_err("full").contains("8 calls are already running"));
        drop(extra);
        // Cancelling doesn't stop the calls, so it doesn't make room: create and cancel in a loop
        // can't start more boots than the limit.
        let ids: Vec<String> = tasks.lock().entries.keys().cloned().collect();
        for id in &ids {
            assert!(tasks.cancel(id));
        }
        for _ in 0..3 {
            assert!(tasks.create("boot", || json!({"content": []})).is_err(), "still 8 calls running");
        }
        // When a call ends there is room again.
        releases.remove(0).send(()).expect("release");
        for _ in 0..500 {
            if tasks.create("boot", || json!({"content": []})).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(tasks.running.load(Ordering::SeqCst) <= MAX_RUNNING);
        for r in releases {
            r.send(()).ok();
        }
    }

    #[test]
    fn finished_tasks_expire_after_their_ttl() {
        let ttl = Duration::from_secs(60);
        let tasks = Tasks::with_ttl(ttl);
        let (release, work) = gated();
        let working = tasks.create("boot", work).expect("create")["taskId"].as_str().expect("id").to_owned();
        let done = tasks.create("boot", || json!({"content": []})).expect("create")["taskId"].as_str().expect("id").to_owned();
        wait_for(&tasks, &done, "completed");
        // Both are older than the time to live (their clocks are set back, so no test waits on a
        // timer): the finished one is swept, the one still working is not.
        let old = Instant::now().checked_sub(ttl + Duration::from_secs(1)).expect("the host has been up a minute");
        for entry in tasks.lock().entries.values_mut() {
            entry.started = old;
        }
        assert!(tasks.get(&done).is_none(), "swept");
        assert_eq!(status(&tasks, &working), "working");
        release.send(()).ok();
    }

    #[test]
    fn listeners_hear_about_the_tasks_they_named() {
        let tasks = Tasks::default();
        let (release, work) = gated();
        let id = tasks.create("boot", work).expect("create")["taskId"].as_str().expect("id").to_owned();
        let other = tasks.create("boot", || json!({"content": []})).expect("create")["taskId"].as_str().expect("id").to_owned();
        let (tx, rx) = channel::<String>();
        let tx = Mutex::new(tx);
        let sink: Sink = Arc::new(move |line| {
            tx.lock().expect("tx").send(line).ok();
        });
        let mut agreed = Vec::new();
        tasks.subscribe(json!(7), &[id.clone(), "unknown".to_owned()], sink, |known| agreed = known.to_vec());
        assert_eq!(agreed, std::slice::from_ref(&id));
        wait_for(&tasks, &other, "completed");
        release.send(()).expect("release");
        let line = rx.recv_timeout(Duration::from_secs(5)).expect("notification");
        let note: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(note["method"], "notifications/tasks");
        assert_eq!((note["params"]["taskId"].clone(), note["params"]["status"].clone()), (json!(id), json!("completed")));
        assert_eq!(note["params"]["_meta"][META_SUBSCRIPTION], 7);
        assert!(rx.try_recv().is_err(), "nothing about the task it didn't name");
        tasks.unsubscribe(&json!(7));
        assert!(tasks.lock().listeners.is_empty());
    }

    #[test]
    fn timestamps_are_iso_8601_utc() {
        assert_eq!(iso8601(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(UNIX_EPOCH + Duration::from_secs(1_700_000_000)), "2023-11-14T22:13:20Z");
        assert_eq!(iso8601(UNIX_EPOCH + Duration::from_secs(951_782_400)), "2000-02-29T00:00:00Z");
        assert_eq!(iso8601(UNIX_EPOCH + Duration::from_secs(4_102_444_799)), "2099-12-31T23:59:59Z");
        let (a, b) = (new_id(), new_id());
        assert_ne!(a, b);
    }
}
