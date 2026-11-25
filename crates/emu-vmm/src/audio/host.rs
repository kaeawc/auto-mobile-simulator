//! The host's audio devices, behind one thread that owns their streams.
//!
//! The card's worker never waits on the host: it sends [`Command`]s and moves on. The thread
//! starts with the first command and blocks on its channel otherwise, so a card whose guest plays
//! nothing costs nothing on the host, and no host stream is open while the guest's is stopped.
//! On macOS the streams are CoreAudio's (through `cpal`), at the device's own rate and channel
//! count, so opening one never changes the device's settings; elsewhere there is no host audio
//! and the card stays silent.
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use super::Ring;

/// Which way a stream goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Output,
    Input,
}

#[derive(Debug)]
pub enum Command {
    /// Start the host stream of `direction` with `ring`: an output stream reads the guest's
    /// frames from it, letting up to the guest's buffer (`buffer_frames`) wait; an input stream
    /// fills it (and sets its rate).
    Open { direction: Direction, ring: Arc<Ring>, buffer_frames: usize },
    /// Stop that host stream, if open.
    Close(Direction),
}

/// The handle the card's worker holds; dropping it closes every stream and ends the thread.
pub struct Host {
    verbose: bool,
    tx: Option<Sender<Command>>,
    thread: Option<JoinHandle<()>>,
}

impl Host {
    pub fn new(verbose: bool) -> Host {
        Host { verbose, tx: None, thread: None }
    }

    pub fn send(&mut self, cmd: Command) {
        if self.tx.is_none() {
            if matches!(cmd, Command::Close(_)) {
                return;
            }
            let (tx, rx) = channel();
            let verbose = self.verbose;
            match std::thread::Builder::new().name("audio-host".into()).spawn(move || serve(rx, verbose)) {
                Ok(h) => {
                    self.tx = Some(tx);
                    self.thread = Some(h);
                }
                Err(e) => {
                    eprintln!("emu: audio: no host audio thread: {e}");
                    return;
                }
            }
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

fn serve(rx: Receiver<Command>, verbose: bool) {
    crate::device_thread();
    let mut streams: [Option<imp::Stream>; 2] = [None, None];
    // A host with no output (or input) device would say so on every guest START.
    let mut warned = [false; 2];
    let slot = |d: Direction| match d {
        Direction::Output => 0,
        Direction::Input => 1,
    };
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Command::Open { direction: d, ring, buffer_frames } => {
                if let Some(s) = streams[slot(d)].take() {
                    s.close(verbose);
                }
                match imp::Stream::open(d, ring, buffer_frames, verbose) {
                    Ok(s) => {
                        if verbose {
                            eprintln!("emu: audio: {}", s.describe());
                        }
                        streams[slot(d)] = Some(s);
                    }
                    Err(e) => {
                        if report(verbose, &mut warned[slot(d)]) {
                            eprintln!("emu: audio: no host {}: {e}", name(d));
                        }
                    }
                }
            }
            Command::Close(d) => {
                if let Some(s) = streams[slot(d)].take() {
                    s.close(verbose);
                }
            }
        }
    }
    for s in streams.into_iter().flatten() {
        s.close(verbose);
    }
}

/// Whether to log a failure to open a host stream: the first time, and every time with -verbose.
fn report(verbose: bool, warned: &mut bool) -> bool {
    let first = !*warned;
    *warned = true;
    verbose || first
}

fn name(d: Direction) -> &'static str {
    match d {
        Direction::Output => "output",
        Direction::Input => "input",
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    use super::super::{Frame, Reader, Ring, gather, max_latency, spread};
    use super::{Direction, name};

    pub struct Stream {
        direction: Direction,
        ring: Arc<Ring>,
        /// Host frames the callback has handled.
        frames: Arc<AtomicU64>,
        what: String,
        _stream: cpal::Stream,
    }

    impl Stream {
        pub fn open(direction: Direction, ring: Arc<Ring>, buffer_frames: usize, verbose: bool) -> Result<Stream, String> {
            let host = cpal::default_host();
            let device = match direction {
                Direction::Output => host.default_output_device(),
                Direction::Input => host.default_input_device(),
            }
            .ok_or("no default device")?;
            let supported = match direction {
                Direction::Output => device.default_output_config(),
                Direction::Input => device.default_input_config(),
            }
            .map_err(|e| e.to_string())?;
            // The device's own rate: cpal would otherwise retune the device for every app on it.
            let config = supported.config();
            let (hz, channels) = (config.sample_rate, usize::from(config.channels.max(1)));
            let label = device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "default device".into());
            let frames = Arc::new(AtomicU64::new(0));
            // Host xruns (the process was descheduled) are routine; only -verbose reports them.
            let on_error = move |e: cpal::Error| {
                if verbose {
                    eprintln!("emu: audio: host stream: {e}");
                }
            };
            let stream = match direction {
                Direction::Output => {
                    // Start once 10 ms of guest audio is queued; keep at most 200 ms, or the
                    // guest's whole buffer when that is more.
                    let guest_hz = ring.rate().max(1);
                    let prime = guest_hz as usize / 100;
                    let mut reader = Reader::new(hz, prime, max_latency(guest_hz, buffer_frames, prime));
                    let (ring, counted) = (ring.clone(), frames.clone());
                    let mut scratch: Vec<Frame> = vec![[0.0; 2]; 4096];
                    device.build_output_stream::<f32, _, _>(
                        config,
                        move |data: &mut [f32], _| {
                            let n = data.len() / channels;
                            // The scratch only grows if the host asks for more than 4096 frames.
                            if scratch.len() < n {
                                scratch.resize(n, [0.0; 2]);
                            }
                            reader.read(&ring, &mut scratch[..n]);
                            for (f, out) in scratch[..n].iter().zip(data.chunks_exact_mut(channels)) {
                                spread(*f, out);
                            }
                            counted.fetch_add(n as u64, Ordering::Relaxed);
                        },
                        on_error,
                        None,
                    )
                }
                Direction::Input => {
                    ring.set_rate(hz);
                    let (ring, counted) = (ring.clone(), frames.clone());
                    device.build_input_stream::<f32, _, _>(
                        config,
                        move |data: &[f32], _| {
                            for f in data.chunks_exact(channels) {
                                ring.push(gather(f));
                            }
                            counted.fetch_add((data.len() / channels) as u64, Ordering::Relaxed);
                        },
                        on_error,
                        None,
                    )
                }
            }
            .map_err(|e| e.to_string())?;
            stream.play().map_err(|e| e.to_string())?;
            let what = format!("{} open on {label}: {hz} Hz, {channels} ch (guest {} Hz)", name(direction), ring.rate());
            Ok(Stream { direction, ring, frames, what, _stream: stream })
        }

        pub fn describe(&self) -> &str {
            &self.what
        }

        pub fn close(self, verbose: bool) {
            let Stream { direction, ring, frames, _stream, .. } = self;
            drop(_stream);
            if verbose {
                let s = ring.stats();
                eprintln!(
                    "emu: audio: {} closed: {} host frames; guest side {} frames ({} audible), {} dropped, {} skipped, {} silence-filled",
                    name(direction),
                    frames.load(Ordering::Relaxed),
                    s.pushed,
                    s.audible,
                    s.dropped,
                    s.skipped,
                    s.starved
                );
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use std::sync::Arc;

    use super::super::Ring;
    use super::Direction;

    pub enum Stream {}

    impl Stream {
        pub fn open(_: Direction, _: Arc<Ring>, _: usize, _: bool) -> Result<Stream, String> {
            Err("host audio is macOS-only so far".into())
        }

        pub fn describe(&self) -> &str {
            match *self {}
        }

        pub fn close(self, _: bool) {
            match self {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_before_opening_starts_no_thread() {
        let mut h = Host::new(false);
        h.send(Command::Close(Direction::Output));
        assert!(h.thread.is_none() && h.tx.is_none());
    }

    #[test]
    fn a_missing_host_device_is_reported_once_unless_verbose() {
        let mut warned = false;
        assert_eq!([report(false, &mut warned), report(false, &mut warned), report(false, &mut warned)], [true, false, false]);
        let mut warned = false;
        assert_eq!([report(true, &mut warned), report(true, &mut warned)], [true, true]);
    }
}
