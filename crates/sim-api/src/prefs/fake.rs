//! A preferences backend in memory, for tests: stores and values set up by the test, batches
//! applied all or nothing, and a change feed that reports this backend's own writes in order.

use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::{Backend, Change, Date, Entries, Op, Store, Visibility, WriteResult};

/// Clones share one state, so a test keeps a handle on what the session's copy does.
#[derive(Clone, Default)]
pub struct Fake(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    /// Each store with its entries, in the order added.
    stores: Vec<(Store, Entries)>,
    /// What writes report; `Now` unless a test says otherwise.
    visible: Option<WriteResult>,
    /// A key whose write fails (a backend error part-way, as a full disk would be).
    fail_on: Option<String>,
    /// Each feed: the app, the store it follows (`None`: every one of the app's), its sender.
    watchers: Vec<(String, Option<Store>, Sender<Change>)>,
    /// Seconds after 2027-01-15T08:00:00Z of the next change, so `at` is known and increasing.
    tick: i64,
}

impl Fake {
    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn with_store(self, store: Store, entries: Entries) -> Fake {
        self.state().stores.push((store, entries));
        self
    }

    pub fn entries(&self, store: &Store) -> Option<Entries> {
        self.state().stores.iter().find(|(s, _)| s == store).map(|(_, e)| e.clone())
    }

    pub fn reports(&self, visible: WriteResult) {
        self.state().visible = Some(visible);
    }

    pub fn fail_on(&self, key: &str) {
        self.state().fail_on = Some(key.to_owned());
    }

    /// Feeds still open, as of the last write (a write finds the ones whose receiver is gone).
    pub fn watchers(&self) -> usize {
        self.state().watchers.len()
    }

    /// Closes every feed, as a device that stops does.
    pub fn close_feeds(&self) {
        self.state().watchers.clear();
    }
}

impl Backend for Fake {
    fn stores(&mut self, _: &str, app: &str) -> Result<Vec<Store>, String> {
        Ok(self.state().stores.iter().filter(|(s, _)| s.app == app).map(|(s, _)| s.clone()).collect())
    }

    fn read(&mut self, _: &str, store: &Store) -> Result<Entries, String> {
        self.entries(store).ok_or_else(|| format!("{} has no store `{}`", store.app, store.id()))
    }

    fn write_batch(&mut self, _: &str, store: &Store, ops: &[Op]) -> Result<WriteResult, String> {
        let mut state = self.state();
        let index = match state.stores.iter().position(|(s, _)| s == store) {
            Some(i) => i,
            None => {
                // Writing to a store that doesn't exist yet creates it, as both platforms do.
                state.stores.push((store.clone(), Entries::new()));
                state.stores.len() - 1
            }
        };
        // Applied to a copy, which replaces the store only when every op went in.
        let mut entries = state.stores[index].1.clone();
        let mut changes = Vec::new();
        for op in ops {
            if state.fail_on.as_deref() == Some(op.key()) {
                return Err(format!("the fake failed writing `{}`", op.key()));
            }
            let old = entries.get(op.key()).cloned();
            let new = match op {
                Op::Set { key, value } => {
                    entries.insert(key.clone(), value.clone());
                    Some(value.clone())
                }
                Op::Delete { key } => {
                    entries.remove(key);
                    None
                }
            };
            if old != new {
                changes.push((op.key().to_owned(), old, new));
            }
        }
        state.stores[index].1 = entries;
        let mut sent = Vec::new();
        for (key, old, new) in changes {
            let at = Date::from_unix(1_800_000_000 + state.tick, 0);
            state.tick += 1;
            sent.push(Change { store: store.clone(), key, old, new, at });
        }
        state.watchers.retain(|(app, only, tx)| {
            *app != store.app || only.as_ref().is_some_and(|s| s != store) || sent.iter().all(|change| tx.send(change.clone()).is_ok())
        });
        Ok(state.visible.clone().unwrap_or(WriteResult { visible: Visibility::Now, note: None }))
    }

    fn watch(&mut self, _: &str, app: &str, store: Option<&Store>) -> Result<std::sync::mpsc::Receiver<Change>, String> {
        let (tx, rx) = channel();
        self.state().watchers.push((app.to_owned(), store.cloned(), tx));
        Ok(rx)
    }
}

/// Where a test's feeds are kept apart from other tests' (feeds are per process).
pub fn unique(tag: &str) -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!("{tag}-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}
