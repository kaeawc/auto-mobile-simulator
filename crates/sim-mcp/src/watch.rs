//! Change notifications. The API has nothing to subscribe to, so a watcher re-reads what it was
//! asked to watch about once a second and speaks up when the bytes differ: the same read a client
//! would do, without the client polling. It runs only while something is subscribed.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::backend::Backend;
use crate::resources;

/// How often a watcher re-reads.
pub const INTERVAL: Duration = Duration::from_millis(1000);

/// Sends one notification: its method and params.
pub type Emit = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// What to watch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Spec {
    /// Resources whose contents to watch (`notifications/resources/updated`).
    pub uris: Vec<String>,
    /// Whether to watch the list of resources (`notifications/resources/list_changed`), which
    /// changes when a device appears, disappears or starts or stops running.
    pub list_changed: bool,
}

/// A running watcher; dropping it stops it.
pub struct Watcher {
    stop: Arc<AtomicBool>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn hash_of(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// A resource's contents as a number that changes when they do (`None` when it can't be read).
fn fingerprint(backend: &dyn Backend, uri: &str) -> Option<u64> {
    resources::read(backend, uri).ok().map(|contents| hash_of(&Value::Array(contents).to_string()))
}

/// What the resource list depends on: each device and whether it is running.
fn list_fingerprint(backend: &dyn Backend) -> Option<u64> {
    let devices = backend.call("devices", &json!({})).ok()?;
    let rows: Vec<String> = devices.as_array()?.iter().map(|d| format!("{}={}", d["udid"], d["state"])).collect();
    Some(hash_of(&rows.join(",")))
}

/// Starts watching `spec`, calling `emit` for each change, every `interval`. The first read is the
/// baseline: nothing is sent for what is already there.
pub fn spawn(backend: Arc<dyn Backend>, spec: Spec, interval: Duration, emit: Emit) -> Watcher {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let spawned = std::thread::Builder::new().name("simee-mcp-watch".into()).spawn(move || {
        let mut seen: Vec<Option<u64>> = spec.uris.iter().map(|u| fingerprint(&*backend, u)).collect();
        let mut listed = if spec.list_changed { list_fingerprint(&*backend) } else { None };
        let mut next = Instant::now() + interval;
        while !flag.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));
            if Instant::now() < next {
                continue;
            }
            next = Instant::now() + interval;
            for (uri, last) in spec.uris.iter().zip(seen.iter_mut()) {
                let now = fingerprint(&*backend, uri);
                if now != *last && !flag.load(Ordering::Relaxed) {
                    emit("notifications/resources/updated", json!({"uri": uri}));
                }
                *last = now;
            }
            if spec.list_changed {
                let now = list_fingerprint(&*backend);
                if now != listed && !flag.load(Ordering::Relaxed) {
                    emit("notifications/resources/list_changed", json!({}));
                }
                listed = now;
            }
        }
    });
    if let Err(e) = spawned {
        eprintln!("simee mcp: cannot start a watcher thread: {e}");
    }
    Watcher { stop }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, channel};

    use super::*;
    use crate::backend::fake::Fake;

    fn collecting() -> (Emit, Receiver<(String, Value)>) {
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        (Arc::new(move |method, params| drop(tx.lock().expect("tx").send((method.to_owned(), params)))), rx)
    }

    const TICK: Duration = Duration::from_millis(20);
    const WAIT: Duration = Duration::from_secs(5);

    #[test]
    fn a_changed_resource_is_announced_and_an_unchanged_one_is_not() {
        let fake = Arc::new(Fake::with_devices());
        fake.answer("info", Ok(json!({"state": "Shutdown"})));
        let (emit, rx) = collecting();
        let spec = Spec { uris: vec!["simee://devices/phone".into(), "simee://runtimes".into()], list_changed: false };
        let _watcher = spawn(fake.clone(), spec, TICK, emit);
        // Nothing for what was already there (the first read, of both, is the baseline).
        while fake.called("runtimes").is_empty() {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(rx.recv_timeout(Duration::from_millis(150)).is_err());
        // Only the resource whose answer changed is announced.
        fake.answer("info", Ok(json!({"state": "Booted"})));
        let (method, params) = rx.recv_timeout(WAIT).expect("update");
        assert_eq!((method.as_str(), params["uri"].clone()), ("notifications/resources/updated", json!("simee://devices/phone")));
        // Once per change.
        assert!(rx.recv_timeout(Duration::from_millis(150)).is_err());
    }

    #[test]
    fn the_resource_list_changes_when_a_device_starts_or_appears() {
        let fake = Arc::new(Fake::with_devices());
        let (emit, rx) = collecting();
        let _watcher = spawn(fake.clone(), Spec { uris: vec![], list_changed: true }, TICK, emit);
        while fake.called("devices").is_empty() {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(rx.recv_timeout(Duration::from_millis(150)).is_err());
        fake.answer("devices", Ok(json!([{"udid": "phone", "name": "phone", "state": "Shutdown", "platform": "android"}])));
        assert_eq!(rx.recv_timeout(WAIT).expect("list changed").0, "notifications/resources/list_changed");
    }

    #[test]
    fn a_dropped_watcher_stops_reading() {
        let fake = Arc::new(Fake::with_devices());
        let (emit, rx) = collecting();
        let watcher = spawn(fake.clone(), Spec { uris: vec![], list_changed: true }, TICK, emit);
        drop(watcher);
        // The thread may be mid-read when it is dropped: wait until its reads have stopped for good.
        let mut calls = fake.called("devices").len();
        loop {
            std::thread::sleep(Duration::from_millis(150));
            let now = fake.called("devices").len();
            if now == calls {
                break;
            }
            calls = now;
        }
        fake.answer("devices", Ok(json!([])));
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(fake.called("devices").len(), calls, "no more reads");
        assert!(rx.try_recv().is_err());
    }
}
