//! Forcing vCPU threads out of `KVM_RUN`: a signal to the thread makes the ioctl return
//! `EINTR` ([`crate::Exit::Interrupted`]).
//!
//! A signal that lands after the thread last checked why it should stop but before it entered
//! `KVM_RUN` is lost, so a stop kicks again until every thread has gone ([`Kicks::kick_running`]
//! reports how many it reached). Each thread registers itself while it runs, so a kick never
//! targets a thread that has exited (whose `pthread_t` may have been reused).
use std::sync::{Mutex, Once};

/// The signal that interrupts `KVM_RUN` (SIGRTMIN, as Firecracker uses; glibc reserves the
/// ones below it).
fn kick_signal() -> libc::c_int {
    vmm_sys_util::signal::SIGRTMIN()
}

extern "C" fn on_kick(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {}

/// Every vCPU thread's state, by index.
pub struct Kicks {
    threads: Mutex<Vec<Option<libc::pthread_t>>>,
}

/// Registration of the calling thread as vCPU `idx`; dropping it unregisters it.
pub struct Running<'a> {
    kicks: &'a Kicks,
    idx: usize,
}

impl Kicks {
    /// For `cpus` vCPU threads. Installs the kick signal's (empty) handler once per process,
    /// without `SA_RESTART`, so the signal interrupts `KVM_RUN` instead of killing the process.
    pub fn new(cpus: usize) -> std::io::Result<Kicks> {
        static HANDLER: Once = Once::new();
        let mut result = Ok(());
        HANDLER.call_once(|| {
            result = vmm_sys_util::signal::register_signal_handler(kick_signal(), on_kick)
                .map_err(|e| std::io::Error::from_raw_os_error(e.errno()));
        });
        result?;
        Ok(Kicks { threads: Mutex::new(vec![None; cpus]) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Option<libc::pthread_t>>> {
        self.threads.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Registers the calling thread as vCPU `idx` until the returned guard drops (drop it before
    /// the thread ends).
    pub fn register(&self, idx: usize) -> Running<'_> {
        // SAFETY: pthread_self has no preconditions and returns the calling thread's handle.
        let me = unsafe { libc::pthread_self() };
        self.lock()[idx] = Some(me);
        Running { kicks: self, idx }
    }

    /// Signals every registered vCPU thread; returns how many there were. Callable from any
    /// thread, a vCPU's own included (the signal then interrupts its next `KVM_RUN`... or nothing,
    /// if it is about to exit: it has already seen why).
    pub fn kick_running(&self) -> usize {
        let threads = self.lock();
        let mut n = 0;
        for t in threads.iter().flatten() {
            // SAFETY: `t` is a registered thread, which unregisters (under this lock) before it
            // exits, so the handle refers to a live thread; the signal has a handler installed.
            unsafe { libc::pthread_kill(*t, kick_signal()) };
            n += 1;
        }
        n
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.kicks.lock()[self.idx] = None;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    use super::*;

    /// A thread blocked in a syscall that doesn't restart (like `KVM_RUN`) is interrupted by a
    /// kick; threads that unregistered aren't counted.
    #[test]
    fn a_kick_interrupts_a_blocked_registered_thread() {
        let kicks = Arc::new(Kicks::new(2).expect("handler"));
        assert_eq!(kicks.kick_running(), 0, "nobody registered yet");
        let ready = Arc::new(Barrier::new(2));
        let interrupted = Arc::new(AtomicBool::new(false));
        let t = std::thread::spawn({
            let (kicks, ready, interrupted) = (kicks.clone(), ready.clone(), interrupted.clone());
            move || {
                let _running = kicks.register(1);
                ready.wait();
                // pause(2) returns only when a signal is handled: EINTR, like KVM_RUN.
                // SAFETY: pause has no preconditions.
                let rc = unsafe { libc::pause() };
                interrupted.store(rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR), Ordering::SeqCst);
            }
        });
        ready.wait();
        let start = Instant::now();
        // The first kick can land before the thread is inside pause: kick until it returns.
        while !t.is_finished() && start.elapsed() < Duration::from_secs(5) {
            kicks.kick_running();
            std::thread::sleep(Duration::from_millis(1));
        }
        t.join().expect("thread");
        assert!(interrupted.load(Ordering::SeqCst), "pause returned EINTR");
        assert_eq!(kicks.kick_running(), 0, "the thread unregistered as it ended");
    }
}
