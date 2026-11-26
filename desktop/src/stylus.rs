//! The desktop's stylus (Android tablets): pen samples go to the device on a thread of their own
//! over one console session kept open across them, so a pen that hovers or draws at the frame
//! rate neither waits on the window nor fills the console worker's queue (whose overflow drops
//! jobs). Samples that queue while the device is slow are coalesced
//! ([`sim_core::stylus::enqueue`]): moves collapse to the newest, and a down, an up or a leave is
//! never dropped, so the pen can't be left touching or in range.

use std::sync::mpsc;
use std::thread::JoinHandle;

use sim_core::stylus::PenSample;

use crate::android::Lookup;
use crate::work::Outbox;

/// Sends one sample on an open session.
pub type Sink = Box<dyn FnMut(&PenSample) -> Result<(), String> + Send>;
/// Opens a session (looks the device up and connects to its console).
pub type Open = Box<dyn FnMut() -> Result<Sink, String> + Send>;

pub struct Pen {
    tx: Option<mpsc::Sender<PenSample>>,
    /// Joined only by tests ([`Pen::finish`]): the window never waits for the thread.
    #[cfg_attr(not(test), allow(dead_code))]
    thread: Option<JoinHandle<()>>,
}

impl Pen {
    /// The pen for the device `find` looks up: its samples go out as simee's console `pen` lines.
    pub fn for_device(find: Lookup, outbox: Outbox) -> Pen {
        Pen::start(
            Box::new(move || {
                let mut session = find()?.pen_session()?;
                Ok(Box::new(move |s: &PenSample| session.command(&s.console()).map(drop)) as Sink)
            }),
            outbox,
        )
    }

    /// Starts the pen's thread; failures go to `outbox` (each once while it repeats).
    pub fn start(mut open: Open, outbox: Outbox) -> Pen {
        let (tx, rx) = mpsc::channel::<PenSample>();
        let thread = std::thread::Builder::new().name("simee-stylus".into()).spawn(move || {
            let mut sink: Option<Sink> = None;
            let mut last_error: Option<String> = None;
            while let Ok(first) = rx.recv() {
                // Everything that queued while the last samples went out, coalesced.
                let mut queue = vec![first];
                while let Ok(s) = rx.try_recv() {
                    sim_core::stylus::enqueue(&mut queue, s);
                }
                for s in &queue {
                    match send(&mut sink, &mut open, s) {
                        Ok(()) => last_error = None,
                        Err(e) => {
                            if last_error.as_ref() != Some(&e) {
                                outbox.post(format!("stylus: {e}"));
                            }
                            last_error = Some(e);
                        }
                    }
                }
            }
        });
        Pen { tx: Some(tx), thread: thread.ok() }
    }

    /// Queues a sample; it never blocks.
    pub fn send(&self, sample: PenSample) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(sample);
        }
    }

    /// Sends what is queued and ends the thread, waiting for it (tests).
    #[cfg(test)]
    pub fn finish(mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Pen {
    /// The thread sends what is still queued (a leave queued just before dropping goes out),
    /// then ends; the window doesn't wait for it.
    fn drop(&mut self) {
        self.tx = None;
    }
}

/// Sends `s` on the session kept open, opening one first if there is none; a session that fails
/// (the emulator restarted) is replaced once.
fn send(sink: &mut Option<Sink>, open: &mut Open, s: &PenSample) -> Result<(), String> {
    for attempt in 0..2 {
        if sink.is_none() {
            *sink = Some(open()?);
        }
        let Some(k) = sink.as_mut() else { continue };
        match k(s) {
            Ok(()) => return Ok(()),
            Err(e) => {
                *sink = None;
                if attempt == 1 {
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_core::stylus::PenPhase::{self, *};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Says the first sample arrived, then waits for the release (a stalled console).
    type Gate = (mpsc::Sender<()>, mpsc::Receiver<()>);

    /// A pen whose sessions record what they're sent; the first sample waits at `gate`.
    fn recording(gate: Gate) -> (Pen, Arc<Mutex<Vec<PenSample>>>, Arc<AtomicUsize>) {
        let (sent, opens) = (Arc::new(Mutex::new(Vec::new())), Arc::new(AtomicUsize::new(0)));
        let (s, o) = (sent.clone(), opens.clone());
        let gate = Arc::new(Mutex::new(Some(gate)));
        let pen = Pen::start(
            Box::new(move || {
                o.fetch_add(1, Ordering::SeqCst);
                let (s, gate) = (s.clone(), gate.clone());
                Ok(Box::new(move |p: &PenSample| {
                    if let Some((arrived, release)) = gate.lock().expect("gate").take() {
                        arrived.send(()).ok();
                        release.recv().ok();
                    }
                    s.lock().expect("sent").push(*p);
                    Ok(())
                }) as Sink)
            }),
            Outbox::default(),
        );
        (pen, sent, opens)
    }

    /// A stalled console while the pen hovers in, draws and leaves: one session, the moves
    /// coalesced, and the up and the leave delivered (the console worker's 32-job queue dropped
    /// them before).
    #[test]
    fn a_stalled_device_gets_every_up_and_leave_over_one_session() {
        let ((release, gate), (arrived, stalled)) = (mpsc::channel(), mpsc::channel());
        let (pen, sent, opens) = recording((arrived, gate));
        pen.send(PenSample::new(0, 0, Hover));
        stalled.recv().expect("the first sample is on its way");
        // Queued behind the stalled first sample.
        (1..60).for_each(|x| pen.send(PenSample::new(x, 0, Hover)));
        pen.send(PenSample::new(60, 0, Down));
        (61..200).for_each(|x| pen.send(PenSample::new(x, 0, Move)));
        pen.send(PenSample::new(200, 0, Up));
        pen.send(PenSample::new(200, 0, Leave));
        release.send(()).expect("release");
        pen.finish();
        let phases: Vec<(i32, PenPhase)> = sent.lock().expect("sent").iter().map(|s| (s.x, s.phase)).collect();
        assert_eq!(phases, [(0, Hover), (59, Hover), (60, Down), (199, Move), (200, Up), (200, Leave)]);
        assert_eq!(opens.load(Ordering::SeqCst), 1, "one session for all of it");
    }

    /// A session that fails (the emulator restarted) is reopened for the same sample; a device
    /// that can't be reached says so once, not once per sample.
    #[test]
    fn a_failed_session_is_reopened_and_errors_are_posted_once() {
        let (sent, opens) = (Arc::new(Mutex::new(Vec::new())), Arc::new(AtomicUsize::new(0)));
        let (s, o) = (sent.clone(), opens.clone());
        let pen = Pen::start(
            Box::new(move || {
                let n = o.fetch_add(1, Ordering::SeqCst);
                let s = s.clone();
                let mut calls = 0;
                Ok(Box::new(move |p: &PenSample| {
                    calls += 1;
                    if n == 0 && calls == 2 {
                        return Err("console closed the session".into());
                    }
                    s.lock().expect("sent").push(p.phase);
                    Ok(())
                }) as Sink)
            }),
            Outbox::default(),
        );
        [Down, Up, Leave].into_iter().for_each(|p| pen.send(PenSample::new(1, 1, p)));
        pen.finish();
        assert_eq!(*sent.lock().expect("sent"), [Down, Up, Leave]);
        assert_eq!(opens.load(Ordering::SeqCst), 2);
        let outbox = Outbox::default();
        let pen = Pen::start(Box::new(|| Err("pixel isn't running; boot it first".into())), outbox.clone());
        (0..5).for_each(|x| pen.send(PenSample::new(x, 0, Down)));
        pen.finish();
        assert_eq!(outbox.take(), ["stylus: pixel isn't running; boot it first"]);
    }
}
