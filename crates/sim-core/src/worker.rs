//! Long-lived worker threads, one per key, each owning state that never leaves its thread. The iOS
//! backend runs each device's input on its own worker: the simulator service's objects (the HID
//! client, the display's port) belong to the thread that made them, and API connections come and
//! go (one per `simee call`), so the connection is kept by the worker instead. Jobs for one key
//! run one at a time, in order; different keys run in parallel. A worker idle for its timeout
//! exits and drops its state; a job that panics fails its caller and resets its worker's state.

use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

type Job<S> = Box<dyn FnOnce(&mut S) + Send>;

/// Workers keyed by `K`, each with a state `S` made (by `Default`) on its own thread.
pub struct Workers<K, S> {
    name: &'static str,
    idle: Duration,
    /// Live workers' queues. Held while a job is queued and while an idle worker leaves, so a job
    /// is never queued to a worker that has decided to exit.
    queues: Mutex<Vec<(K, Sender<Job<S>>)>>,
}

impl<K, S> Workers<K, S>
where
    K: PartialEq + Clone + Send + 'static,
    S: Default + 'static,
{
    /// No workers yet; each starts on its key's first job. `name` names their threads.
    pub const fn new(name: &'static str, idle: Duration) -> Self {
        Self { name, idle, queues: Mutex::new(Vec::new()) }
    }

    /// Runs `job` on `key`'s worker (starting one if needed) and waits for its result.
    pub fn run<R: Send + 'static>(&'static self, key: &K, job: impl FnOnce(&mut S) -> R + Send + 'static) -> Result<R, String> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.queue(
            key,
            Box::new(move |state| {
                tx.send(job(state)).ok();
            }),
        )?;
        rx.recv().map_err(|_| format!("the {} worker's job panicked", self.name))
    }

    /// The number of live workers.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no worker is live.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(K, Sender<Job<S>>)>> {
        self.queues.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn queue(&'static self, key: &K, mut job: Job<S>) -> Result<(), String> {
        let mut queues = self.lock();
        if let Some(i) = queues.iter().position(|(k, _)| k == key) {
            match queues[i].1.send(job) {
                Ok(()) => return Ok(()),
                // Its thread is gone: start another.
                Err(mpsc::SendError(back)) => {
                    queues.swap_remove(i);
                    job = back;
                }
            }
        }
        let (tx, rx) = mpsc::channel();
        let k = key.clone();
        std::thread::Builder::new().name(self.name.into()).spawn(move || self.serve(&k, &rx)).map_err(|e| format!("{}: {e}", self.name))?;
        tx.send(job).map_err(|_| format!("the {} worker didn't start", self.name))?;
        queues.push((key.clone(), tx));
        Ok(())
    }

    fn serve(&self, key: &K, rx: &Receiver<Job<S>>) {
        let mut state = S::default();
        // A panicking job drops its result channel (its caller gets an error) and leaves the state
        // suspect: start over from a fresh one, on this same thread.
        let run = |job: Job<S>, state: &mut S| {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(state))).is_err() {
                *state = S::default();
            }
        };
        loop {
            match rx.recv_timeout(self.idle) {
                Ok(job) => run(job, &mut state),
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {
                    let mut queues = self.lock();
                    // A job queued since the timeout runs first; otherwise leave while holding the
                    // lock, so nothing more is queued here.
                    if let Ok(job) = rx.try_recv() {
                        drop(queues);
                        run(job, &mut state);
                        continue;
                    }
                    queues.retain(|(k, _)| k != key);
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread::ThreadId;
    use std::time::Instant;

    use super::*;

    /// A worker's state: how many jobs it ran, and on which thread.
    #[derive(Default)]
    struct Count(u32);

    fn workers(idle: Duration) -> &'static Workers<String, Count> {
        Box::leak(Box::new(Workers::new("test-worker", idle)))
    }

    fn bump(w: &'static Workers<String, Count>, key: &str) -> (u32, ThreadId) {
        w.run(&key.to_owned(), |c: &mut Count| {
            c.0 += 1;
            (c.0, std::thread::current().id())
        })
        .expect("job")
    }

    #[test]
    fn a_key_keeps_its_state_and_thread_across_jobs() {
        let w = workers(Duration::from_secs(60));
        let (first, thread) = bump(w, "a");
        assert_eq!(first, 1);
        assert_ne!(thread, std::thread::current().id(), "jobs run on the worker, not the caller");
        assert_eq!(bump(w, "a"), (2, thread));
        assert_eq!(bump(w, "a"), (3, thread));
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn keys_have_their_own_workers_running_in_parallel() {
        let w = workers(Duration::from_secs(60));
        let (_, a) = bump(w, "a");
        let (n, b) = bump(w, "b");
        assert_eq!(n, 1, "a new key starts from fresh state");
        assert_ne!(a, b);
        assert_eq!(w.len(), 2);
        // Each job waits for the other: this only finishes if both run at once.
        let barrier: &'static Barrier = Box::leak(Box::new(Barrier::new(2)));
        let other = std::thread::spawn(move || w.run(&"a".to_owned(), move |_| barrier.wait().is_leader()));
        let here = w.run(&"b".to_owned(), move |_| barrier.wait().is_leader()).expect("b");
        let there = other.join().expect("thread").expect("a");
        assert_ne!(here, there);
    }

    #[test]
    fn jobs_for_one_key_run_in_order_one_at_a_time() {
        let w = workers(Duration::from_secs(60));
        let running = &*Box::leak(Box::new(AtomicU32::new(0)));
        let callers: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(move || {
                    w.run(&"k".to_owned(), move |c: &mut Count| {
                        assert_eq!(running.fetch_add(1, Ordering::SeqCst), 0, "another job is running");
                        std::thread::sleep(Duration::from_millis(2));
                        c.0 += 1;
                        running.fetch_sub(1, Ordering::SeqCst);
                    })
                })
            })
            .collect();
        for c in callers {
            c.join().expect("thread").expect("job");
        }
        assert_eq!(bump(w, "k").0, 9);
    }

    #[test]
    fn a_panicked_job_is_an_error_and_resets_its_workers_state() {
        let w = workers(Duration::from_secs(60));
        let (_, thread) = bump(w, "p");
        bump(w, "p");
        let err = w.run(&"p".to_owned(), |_: &mut Count| -> u32 { panic!("boom") }).expect_err("panicked");
        assert!(err.contains("panicked"), "{err}");
        assert_eq!(bump(w, "p"), (1, thread), "fresh state, same thread");
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn an_idle_worker_exits_and_a_later_job_starts_another() {
        let w = workers(Duration::from_millis(20));
        bump(w, "i");
        assert_eq!(bump(w, "i").0, 2);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !w.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(w.is_empty(), "the idle worker left");
        assert_eq!(bump(w, "i").0, 1, "a new worker, fresh state");
    }

    #[test]
    fn jobs_racing_an_idle_exit_are_never_lost() {
        // A timeout of zero makes every worker try to leave between jobs.
        let w = workers(Duration::ZERO);
        for _ in 0..200 {
            assert!(bump(w, "r").0 >= 1);
        }
    }
}
