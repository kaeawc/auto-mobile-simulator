//! virtio-snd (virtio 1.2 §5.14): a silent sound card.
//!
//! One output and one input PCM stream (a single ALSA card, `pcmC0D0p` / `pcmC0D0c`), no jacks,
//! a stereo channel map per stream. Playback buffers are consumed and dropped; capture buffers
//! come back full of silence. Both run at the stream's real rate, because the guest's audio HAL
//! takes its timing from blocking PCM writes and reads: completing buffers instantly would make
//! AudioFlinger spin.
//!
//! What needs it: the Automotive audio HAL (`audio.primary.caremu`) calls `mixer_open(0)` and
//! then `mixer_get_name()` without a NULL check, so it crashes without ALSA card 0.
//!
//! [`Pcm`] is the device's logic (control requests, stream states, buffer pacing) over opaque
//! buffer handles; [`Snd`] connects it to the virtqueues.
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use emu_hvf::GuestRam;

use super::queue::{Chain, Queue};
use super::{Activation, INT_USED_RING, Interrupt, VirtioDevice};

pub const ID_SOUND: u32 = 25;

const QUEUE_CONTROL: usize = 0;
const QUEUE_TX: usize = 2;
const QUEUE_RX: usize = 3;

// Request codes.
const R_JACK_INFO: u32 = 1;
const R_JACK_REMAP: u32 = 2;
const R_PCM_INFO: u32 = 0x0100;
const R_PCM_SET_PARAMS: u32 = 0x0101;
const R_PCM_PREPARE: u32 = 0x0102;
const R_PCM_RELEASE: u32 = 0x0103;
const R_PCM_START: u32 = 0x0104;
const R_PCM_STOP: u32 = 0x0105;
const R_CHMAP_INFO: u32 = 0x0200;

// Status codes.
pub const S_OK: u32 = 0x8000;
pub const S_BAD_MSG: u32 = 0x8001;
pub const S_NOT_SUPP: u32 = 0x8002;

pub const D_OUTPUT: u8 = 0;
pub const D_INPUT: u8 = 1;

// `VIRTIO_SND_PCM_FMT_*` bits offered, with their sample sizes in bytes. Only signed and float
// formats, whose silence is all zero bytes.
const FORMATS: &[(u8, u32)] = &[(5, 2), (15, 4), (17, 4), (19, 4)]; // S16, S24 (in 32), S32, FLOAT
// `VIRTIO_SND_PCM_RATE_*` bits offered (what the goldfish and car audio HALs ask for).
const RATES: &[(u8, u32)] = &[(1, 8000), (2, 11025), (3, 16000), (4, 22050), (5, 32000), (6, 44100), (7, 48000)];
const CHANNELS: (u8, u8) = (1, 2);
const CHMAP_FL: u8 = 3;
const CHMAP_FR: u8 = 4;

const PCM_INFO_SIZE: usize = 32;
const CHMAP_INFO_SIZE: usize = 24;
/// `virtio_snd_pcm_status`: status and latency, at the end of every I/O buffer.
pub const IO_STATUS_SIZE: u32 = 8;

fn le32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Accepted `PCM_SET_PARAMS` for one stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    pub buffer_bytes: u32,
    pub period_bytes: u32,
    pub channels: u8,
    pub format: u8,
    pub rate: u8,
}

impl Params {
    /// Bytes per second.
    fn byte_rate(&self) -> u64 {
        let sample = FORMATS.iter().find(|(f, _)| *f == self.format).map_or(2, |(_, s)| *s);
        let hz = RATES.iter().find(|(r, _)| *r == self.rate).map_or(48_000, |(_, h)| *h);
        u64::from(sample) * u64::from(self.channels) * u64::from(hz)
    }

    /// How long `bytes` of audio last.
    fn duration(&self, bytes: u32) -> Duration {
        Duration::from_nanos(u64::from(bytes) * 1_000_000_000 / self.byte_rate().max(1))
    }
}

/// An I/O buffer the guest queued: an opaque handle and its audio byte count.
#[derive(Debug, PartialEq, Eq)]
pub struct Io<T> {
    pub handle: T,
    pub bytes: u32,
}

/// A buffer to return to the guest with its status and the latency to report (bytes still
/// queued behind it).
#[derive(Debug, PartialEq, Eq)]
pub struct Done<T> {
    pub handle: T,
    pub status: u32,
    pub latency: u32,
}

#[derive(Debug)]
struct Stream<T> {
    direction: u8,
    params: Option<Params>,
    prepared: bool,
    running: bool,
    /// When the audio consumed so far ends (running streams only).
    cursor: Instant,
    pending: VecDeque<Io<T>>,
}

impl<T> Stream<T> {
    fn complete(&mut self, io: Io<T>, status: u32, done: &mut Vec<Done<T>>) {
        let latency = self.pending.iter().map(|p| p.bytes).sum();
        done.push(Done { handle: io.handle, status, latency });
    }

    /// Returns every queued buffer now (STOP, RELEASE: the driver waits for them).
    fn flush(&mut self, done: &mut Vec<Done<T>>) {
        while let Some(io) = self.pending.pop_front() {
            self.complete(io, S_OK, done);
        }
    }
}

/// The sound card's streams and control logic, over buffer handles `T`.
#[derive(Debug)]
pub struct Pcm<T> {
    streams: Vec<Stream<T>>,
}

impl<T> Pcm<T> {
    pub fn new(now: Instant) -> Pcm<T> {
        let stream = |direction| Stream { direction, params: None, prepared: false, running: false, cursor: now, pending: VecDeque::new() };
        Pcm { streams: vec![stream(D_OUTPUT), stream(D_INPUT)] }
    }

    /// `virtio_snd_config`: jacks, streams, chmaps.
    pub fn config(&self) -> [u8; 12] {
        let mut c = [0u8; 12];
        c[4..8].copy_from_slice(&(self.streams.len() as u32).to_le_bytes());
        c[8..12].copy_from_slice(&(self.streams.len() as u32).to_le_bytes());
        c
    }

    fn pcm_info(&self, id: usize) -> Vec<u8> {
        let mut b = vec![0u8; PCM_INFO_SIZE];
        // hda_fn_nid 0 for both: one ALSA PCM device with a playback and a capture substream.
        let formats: u64 = FORMATS.iter().fold(0, |m, (f, _)| m | 1 << f);
        let rates: u64 = RATES.iter().fold(0, |m, (r, _)| m | 1 << r);
        b[8..16].copy_from_slice(&formats.to_le_bytes());
        b[16..24].copy_from_slice(&rates.to_le_bytes());
        b[24] = self.streams[id].direction;
        b[25] = CHANNELS.0;
        b[26] = CHANNELS.1;
        b
    }

    fn chmap_info(&self, id: usize) -> Vec<u8> {
        let mut b = vec![0u8; CHMAP_INFO_SIZE];
        b[4] = self.streams[id].direction;
        b[5] = 2;
        b[6] = CHMAP_FL;
        b[7] = CHMAP_FR;
        b
    }

    /// Handles one control request; returns the response (status, then any info structures).
    /// Buffers that a STOP or RELEASE completes go to `done`.
    pub fn control(&mut self, req: &[u8], now: Instant, done: &mut Vec<Done<T>>) -> Vec<u8> {
        let status = |s: u32| s.to_le_bytes().to_vec();
        let Some(code) = le32(req, 0) else { return status(S_BAD_MSG) };
        match code {
            R_PCM_INFO | R_CHMAP_INFO | R_JACK_INFO => {
                let (Some(start), Some(count), Some(size)) = (le32(req, 4), le32(req, 8), le32(req, 12)) else {
                    return status(S_BAD_MSG);
                };
                let items = if code == R_JACK_INFO { 0 } else { self.streams.len() };
                let (start, count, size) = (start as usize, count as usize, size as usize);
                if size == 0 || start.checked_add(count).is_none_or(|end| end > items) {
                    return status(S_BAD_MSG);
                }
                let mut out = status(S_OK);
                for id in start..start + count {
                    let mut item = if code == R_PCM_INFO { self.pcm_info(id) } else { self.chmap_info(id) };
                    // Each item takes the driver's `size`: padded, or cut to it.
                    item.resize(size, 0);
                    out.extend_from_slice(&item);
                }
                out
            }
            R_JACK_REMAP => status(S_BAD_MSG),
            R_PCM_SET_PARAMS | R_PCM_PREPARE | R_PCM_RELEASE | R_PCM_START | R_PCM_STOP => {
                let Some(s) = le32(req, 4).and_then(|id| self.streams.get_mut(id as usize)) else { return status(S_BAD_MSG) };
                status(match code {
                    R_PCM_SET_PARAMS => {
                        let (Some(buffer_bytes), Some(period_bytes), Some(&[channels, format, rate])) =
                            (le32(req, 8), le32(req, 12), req.get(20..23))
                        else {
                            return status(S_BAD_MSG);
                        };
                        let p = Params { buffer_bytes, period_bytes, channels, format, rate };
                        if s.running {
                            S_BAD_MSG
                        } else if !(CHANNELS.0..=CHANNELS.1).contains(&channels)
                            || !FORMATS.iter().any(|(f, _)| *f == format)
                            || !RATES.iter().any(|(r, _)| *r == rate)
                            || period_bytes == 0
                            || buffer_bytes < period_bytes
                        {
                            S_NOT_SUPP
                        } else {
                            s.params = Some(p);
                            s.prepared = false;
                            S_OK
                        }
                    }
                    R_PCM_PREPARE if s.running || s.params.is_none() => S_BAD_MSG,
                    R_PCM_PREPARE => {
                        s.prepared = true;
                        S_OK
                    }
                    R_PCM_START if s.running || !s.prepared => S_BAD_MSG,
                    R_PCM_START => {
                        s.running = true;
                        s.cursor = now;
                        S_OK
                    }
                    R_PCM_STOP if !s.running => S_BAD_MSG,
                    R_PCM_STOP => {
                        s.running = false;
                        s.flush(done);
                        S_OK
                    }
                    // RELEASE
                    _ if s.running => S_BAD_MSG,
                    _ => {
                        s.prepared = false;
                        s.flush(done);
                        S_OK
                    }
                })
            }
            _ => status(S_NOT_SUPP),
        }
    }

    /// Queues an I/O buffer for `stream_id`, which must be a stream of `direction` (tx buffers
    /// go to the output stream, rx buffers to the input one). Bad buffers complete at once.
    pub fn queue(&mut self, stream_id: u32, direction: u8, io: Io<T>, now: Instant, done: &mut Vec<Done<T>>) {
        match self.streams.get_mut(stream_id as usize) {
            Some(s) if s.direction == direction && s.params.is_some() => {
                if s.pending.is_empty() && s.running && s.cursor < now {
                    // Restarting after an underrun: time runs from now, not from the gap.
                    s.cursor = now;
                }
                s.pending.push_back(io);
            }
            _ => done.push(Done { handle: io.handle, status: S_BAD_MSG, latency: 0 }),
        }
    }

    /// Completes the buffers whose audio has been played (or captured) by `now`; returns when
    /// the next one will be.
    pub fn tick(&mut self, now: Instant, done: &mut Vec<Done<T>>) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        for s in &mut self.streams {
            let Some(p) = s.params.filter(|_| s.running) else { continue };
            while let Some(bytes) = s.pending.front().map(|io| io.bytes) {
                let end = s.cursor + p.duration(bytes);
                if end > now {
                    next = Some(next.map_or(end, |n| n.min(end)));
                    break;
                }
                s.cursor = end;
                if let Some(io) = s.pending.pop_front() {
                    s.complete(io, S_OK, done);
                }
            }
        }
        next
    }
}

enum Msg {
    Kick,
    Stop,
}

pub struct Snd {
    tx: Sender<Msg>,
    rx: Option<Receiver<Msg>>,
    worker: Option<JoinHandle<Receiver<Msg>>>,
}

impl Snd {
    pub fn new() -> Snd {
        let (tx, rx) = channel();
        Snd { tx, rx: Some(rx), worker: None }
    }

    fn stop_worker(&mut self) {
        if let Some(h) = self.worker.take() {
            let _ = self.tx.send(Msg::Stop);
            if let Ok(rx) = h.join() {
                self.rx = Some(rx);
            }
        }
    }
}

impl Default for Snd {
    fn default() -> Self {
        Snd::new()
    }
}

impl Drop for Snd {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

impl VirtioDevice for Snd {
    fn device_id(&self) -> u32 {
        ID_SOUND
    }
    fn features(&self) -> u64 {
        0
    }
    fn num_queues(&self) -> usize {
        4
    }
    fn queue_max_size(&self) -> u16 {
        64
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let cfg = Pcm::<()>::new(Instant::now()).config();
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
    }
    fn activate(&mut self, act: Activation) -> Result<(), String> {
        let q = |i: usize| act.queues.get(i).copied().flatten().map(|c| Queue::new(c, false));
        let control = q(QUEUE_CONTROL).ok_or("sound control queue not enabled")?;
        self.stop_worker();
        let rx = self.rx.take().ok_or("sound worker receiver missing")?;
        let mut w = Worker { mem: act.mem, irq: act.irq, control, txq: q(QUEUE_TX), rxq: q(QUEUE_RX), pcm: Pcm::new(Instant::now()) };
        let h = std::thread::Builder::new()
            .name("virtio-snd".into())
            .spawn(move || {
                crate::device_thread();
                let mut deadline: Option<Instant> = None;
                loop {
                    let msg = match deadline {
                        Some(d) => rx.recv_timeout(d.saturating_duration_since(Instant::now())),
                        None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                    };
                    match msg {
                        Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                        Ok(Msg::Kick) | Err(RecvTimeoutError::Timeout) => deadline = w.run(),
                    }
                }
                rx
            })
            .map_err(|e| e.to_string())?;
        self.worker = Some(h);
        Ok(())
    }
    fn notify(&mut self, _queue: u16) {
        let _ = self.tx.send(Msg::Kick);
    }
    fn reset(&mut self) {
        self.stop_worker();
    }
}

struct Worker {
    mem: Arc<GuestRam>,
    irq: Arc<Interrupt>,
    control: Queue,
    txq: Option<Queue>,
    rxq: Option<Queue>,
    pcm: Pcm<(usize, Chain)>,
}

impl Worker {
    /// Serves every queue; returns when the next buffer is due.
    fn run(&mut self) -> Option<Instant> {
        let now = Instant::now();
        let mut done = Vec::new();
        let mut control_used = false;
        while let Ok(Some(chain)) = self.control.pop(&self.mem) {
            let req = chain.read_all(&self.mem, 64).unwrap_or_default();
            let resp = self.pcm.control(&req, now, &mut done);
            let n = chain.write_all(&self.mem, &resp).unwrap_or(0);
            let _ = self.control.add_used(&self.mem, chain.head, n);
            control_used = true;
        }
        for (queue, direction) in [(QUEUE_TX, D_OUTPUT), (QUEUE_RX, D_INPUT)] {
            let q = if queue == QUEUE_TX { self.txq.as_mut() } else { self.rxq.as_mut() };
            let Some(q) = q else { continue };
            while let Ok(Some(chain)) = q.pop(&self.mem) {
                // The header (`virtio_snd_pcm_xfer`: the stream id) leads the readable part; the
                // status ends the writable part. Playback data is readable, capture writable.
                let header = chain.read_all(&self.mem, 4).unwrap_or_default();
                let readable: u32 = chain.readable().map(|d| d.len).sum();
                let writable: u32 = chain.writable().map(|d| d.len).sum();
                let bytes = if direction == D_OUTPUT { readable.saturating_sub(4) } else { writable.saturating_sub(IO_STATUS_SIZE) };
                let io = Io { handle: (queue, chain), bytes };
                match le32(&header, 0) {
                    Some(id) if writable >= IO_STATUS_SIZE => self.pcm.queue(id, direction, io, now, &mut done),
                    _ => done.push(Done { handle: io.handle, status: S_BAD_MSG, latency: 0 }),
                }
            }
        }
        let next = self.pcm.tick(now, &mut done);
        let (mut tx_used, mut rx_used) = (false, false);
        for d in done {
            let (queue, chain) = d.handle;
            // The status goes in the last 8 writable bytes; capture data before it is silence.
            let writable: u32 = chain.writable().map(|d| d.len).sum();
            let mut out = vec![0u8; writable.saturating_sub(IO_STATUS_SIZE) as usize];
            out.extend_from_slice(&d.status.to_le_bytes());
            out.extend_from_slice(&d.latency.to_le_bytes());
            let n = chain.write_all(&self.mem, &out).unwrap_or(0);
            let q = if queue == QUEUE_TX { self.txq.as_mut() } else { self.rxq.as_mut() };
            if let Some(q) = q {
                let _ = q.add_used(&self.mem, chain.head, n);
                if queue == QUEUE_TX { tx_used = true } else { rx_used = true }
            }
        }
        let notify = |q: Option<&mut Queue>, used: bool, mem: &GuestRam| used && q.is_some_and(|q| q.should_notify(mem));
        let control = control_used && self.control.should_notify(&self.mem);
        if control | notify(self.txq.as_mut(), tx_used, &self.mem) | notify(self.rxq.as_mut(), rx_used, &self.mem) {
            self.irq.raise(INT_USED_RING);
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    fn set_params(stream: u32, buffer: u32, period: u32, channels: u8, format: u8, rate: u8) -> Vec<u8> {
        let mut r = req(&[R_PCM_SET_PARAMS, stream, buffer, period, 0]);
        r.extend_from_slice(&[channels, format, rate, 0]);
        r
    }

    fn status(resp: &[u8]) -> u32 {
        le32(resp, 0).expect("status")
    }

    #[test]
    fn describes_one_output_and_one_input() {
        let mut pcm = Pcm::<u32>::new(Instant::now());
        assert_eq!(pcm.config(), *b"\0\0\0\0\x02\0\0\0\x02\0\0\0");
        let now = Instant::now();
        let mut done = Vec::new();
        let info = pcm.control(&req(&[R_PCM_INFO, 0, 2, 32]), now, &mut done);
        assert_eq!((status(&info), info.len()), (S_OK, 4 + 64));
        let out = &info[4..36];
        let formats = u64::from_le_bytes(out[8..16].try_into().expect("8"));
        let rates = u64::from_le_bytes(out[16..24].try_into().expect("8"));
        assert_eq!((formats & 1 << 5 != 0, rates & 1 << 7 != 0), (true, true), "S16 at 48 kHz");
        assert_eq!((out[24], out[25], out[26]), (D_OUTPUT, 1, 2));
        assert_eq!(info[36 + 24], D_INPUT);
        // Items take the driver's size.
        assert_eq!(pcm.control(&req(&[R_PCM_INFO, 1, 1, 40]), now, &mut done).len(), 4 + 40);
        let chmap = pcm.control(&req(&[R_CHMAP_INFO, 0, 1, 24]), now, &mut done);
        assert_eq!((status(&chmap), &chmap[8..12]), (S_OK, &[D_OUTPUT, 2, CHMAP_FL, CHMAP_FR][..]));
        assert_eq!(status(&pcm.control(&req(&[R_PCM_INFO, 1, 2, 32]), now, &mut done)), S_BAD_MSG, "past the last stream");
        assert_eq!(status(&pcm.control(&req(&[R_JACK_INFO, 0, 1, 24]), now, &mut done)), S_BAD_MSG, "no jacks");
        assert_eq!(status(&pcm.control(&req(&[R_JACK_INFO, 0, 0, 24]), now, &mut done)), S_OK);
        assert_eq!(status(&pcm.control(&req(&[0x0300]), now, &mut done)), S_NOT_SUPP, "no controls");
        assert_eq!(status(&pcm.control(&[1, 0], now, &mut done)), S_BAD_MSG, "short header");
        assert!(done.is_empty());
    }

    #[test]
    fn follows_the_stream_state_machine() {
        let mut pcm = Pcm::<u32>::new(Instant::now());
        let now = Instant::now();
        let mut done = Vec::new();
        let mut call = |r: Vec<u8>| status(&pcm.control(&r, now, &mut done));
        assert_eq!(call(req(&[R_PCM_PREPARE, 0])), S_BAD_MSG, "no parameters yet");
        assert_eq!(call(set_params(0, 3840, 960, 2, 5, 7)), S_OK);
        assert_eq!(call(set_params(0, 3840, 960, 6, 5, 7)), S_NOT_SUPP, "six channels");
        assert_eq!(call(set_params(0, 3840, 960, 2, 4, 7)), S_NOT_SUPP, "U8");
        assert_eq!(call(set_params(0, 3840, 960, 2, 5, 13)), S_NOT_SUPP, "384 kHz");
        assert_eq!(call(set_params(0, 100, 960, 2, 5, 7)), S_NOT_SUPP, "buffer smaller than a period");
        assert_eq!(call(set_params(2, 3840, 960, 2, 5, 7)), S_BAD_MSG, "no stream 2");
        assert_eq!(call(req(&[R_PCM_START, 0])), S_BAD_MSG, "not prepared");
        assert_eq!(call(req(&[R_PCM_PREPARE, 0])), S_OK);
        assert_eq!(call(req(&[R_PCM_START, 0])), S_OK);
        assert_eq!(call(set_params(0, 3840, 960, 2, 5, 7)), S_BAD_MSG, "running");
        assert_eq!(call(req(&[R_PCM_RELEASE, 0])), S_BAD_MSG, "running");
        assert_eq!(call(req(&[R_PCM_STOP, 0])), S_OK);
        assert_eq!(call(req(&[R_PCM_STOP, 0])), S_BAD_MSG, "already stopped");
        assert_eq!(call(req(&[R_PCM_START, 0])), S_OK, "stopped streams start again");
        assert_eq!(call(req(&[R_PCM_STOP, 0])), S_OK);
        assert_eq!(call(req(&[R_PCM_RELEASE, 0])), S_OK);
        assert_eq!(call(req(&[R_PCM_START, 0])), S_BAD_MSG, "released");
    }

    #[test]
    fn paces_playback_and_returns_silent_capture() {
        let t0 = Instant::now();
        let mut pcm = Pcm::<u32>::new(t0);
        let mut done = Vec::new();
        // 48 kHz stereo S16: 192 000 bytes/s, so 1920 bytes last 10 ms.
        for id in [0, 1] {
            assert_eq!(status(&pcm.control(&set_params(id, 7680, 1920, 2, 5, 7), t0, &mut done)), S_OK);
            assert_eq!(status(&pcm.control(&req(&[R_PCM_PREPARE, id]), t0, &mut done)), S_OK);
        }
        // Prefilled before START, as ALSA does.
        pcm.queue(0, D_OUTPUT, Io { handle: 1, bytes: 1920 }, t0, &mut done);
        pcm.queue(0, D_OUTPUT, Io { handle: 2, bytes: 1920 }, t0, &mut done);
        assert_eq!(pcm.tick(t0 + Duration::from_secs(1), &mut done), None, "not started: nothing moves");
        assert!(done.is_empty());
        pcm.control(&req(&[R_PCM_START, 0]), t0, &mut done);
        assert_eq!(pcm.tick(t0 + Duration::from_millis(5), &mut done), Some(t0 + Duration::from_millis(10)));
        assert!(done.is_empty());
        assert_eq!(pcm.tick(t0 + Duration::from_millis(10), &mut done), Some(t0 + Duration::from_millis(20)));
        assert_eq!(done, vec![Done { handle: 1, status: S_OK, latency: 1920 }]);
        done.clear();

        // Capture runs on its own clock: 960 bytes are 5 ms.
        pcm.control(&req(&[R_PCM_START, 1]), t0, &mut done);
        pcm.queue(1, D_INPUT, Io { handle: 7, bytes: 960 }, t0, &mut done);
        assert_eq!(pcm.tick(t0 + Duration::from_millis(5), &mut done), Some(t0 + Duration::from_millis(20)));
        assert_eq!(done, vec![Done { handle: 7, status: S_OK, latency: 0 }]);
        done.clear();
        pcm.tick(t0 + Duration::from_millis(20), &mut done);
        assert_eq!(done, vec![Done { handle: 2, status: S_OK, latency: 0 }]);
        done.clear();

        // Wrong direction or unknown stream: rejected at once. STOP flushes what's queued.
        pcm.queue(1, D_OUTPUT, Io { handle: 8, bytes: 4 }, t0, &mut done);
        pcm.queue(9, D_OUTPUT, Io { handle: 9, bytes: 4 }, t0, &mut done);
        assert_eq!(done.iter().map(|d| (d.handle, d.status)).collect::<Vec<_>>(), [(8, S_BAD_MSG), (9, S_BAD_MSG)]);
        done.clear();
        pcm.queue(0, D_OUTPUT, Io { handle: 10, bytes: 1920 }, t0, &mut done);
        pcm.control(&req(&[R_PCM_STOP, 0]), t0, &mut done);
        assert_eq!(done, vec![Done { handle: 10, status: S_OK, latency: 0 }]);

        // After an underrun, time restarts when audio arrives again.
        done.clear();
        pcm.control(&req(&[R_PCM_START, 0]), t0, &mut done);
        let late = t0 + Duration::from_secs(5);
        pcm.queue(0, D_OUTPUT, Io { handle: 11, bytes: 1920 }, late, &mut done);
        assert_eq!(pcm.tick(late, &mut done), Some(late + Duration::from_millis(10)));
        assert!(done.is_empty(), "not completed instantly");
    }

    #[test]
    fn serves_the_virtqueues() {
        use crate::irq::TestLine;
        use crate::virtio::queue::tests::{Driver, RAM_BASE};
        let mem = Arc::new(GuestRam::new(RAM_BASE, 0x10_0000).expect("ram"));
        let line = Arc::new(TestLine::default());
        // Three queues, each laid out in its own 16 KiB.
        let driver = |n: u64| {
            let mut d = Driver::new(8);
            for a in [&mut d.cfg.desc, &mut d.cfg.avail, &mut d.cfg.used] {
                *a += n * 0x4000;
            }
            d
        };
        let (mut ctl, mut txd, mut rxd) = (driver(0), driver(1), driver(2));
        let mut w = Worker {
            mem: mem.clone(),
            irq: Arc::new(Interrupt::new(line.clone())),
            control: Queue::new(ctl.cfg, false),
            txq: Some(Queue::new(txd.cfg, false)),
            rxq: Some(Queue::new(rxd.cfg, false)),
            pcm: Pcm::new(Instant::now()),
        };
        fn buf(i: u64) -> u64 {
            RAM_BASE + 0x8_0000 + i * 0x1000
        }
        fn control(w: &mut Worker, ctl: &mut Driver, i: u64, r: Vec<u8>, resp: u32) {
            w.mem.write(buf(i), &r).expect("request");
            ctl.push(&w.mem, &[(buf(i), r.len() as u32, false), (buf(i) + 0x800, resp, true)]);
            w.run();
        }
        control(&mut w, &mut ctl, 0, req(&[R_PCM_INFO, 0, 2, 32]), 4 + 64);
        assert_eq!((ctl.used(&mem, 0).1, mem.read_u32(buf(0) + 0x800).expect("status")), (68, S_OK));
        assert!(line.level(), "used ring interrupt");
        control(&mut w, &mut ctl, 1, set_params(0, 3840, 1920, 2, 5, 7), 4);
        control(&mut w, &mut ctl, 2, req(&[R_PCM_PREPARE, 0]), 4);
        control(&mut w, &mut ctl, 3, req(&[R_PCM_START, 0]), 4);
        assert_eq!((ctl.used_idx(&mem), mem.read_u32(buf(3) + 0x800).expect("status")), (4, S_OK));

        // 1920 bytes of playback: back after 10 ms, with only the status written.
        mem.write(buf(4), &0u32.to_le_bytes()).expect("xfer header");
        txd.push(&mem, &[(buf(4), 4, false), (buf(5), 1920, false), (buf(4) + 0x800, 8, true)]);
        assert!(w.run().is_some(), "a buffer is due later");
        assert_eq!(txd.used_idx(&mem), 0);
        std::thread::sleep(Duration::from_millis(11));
        w.run();
        assert_eq!((txd.used(&mem, 0).1, mem.read_u32(buf(4) + 0x800).expect("status")), (8, S_OK));

        // Capture on a stream with no parameters: rejected, the status still at the end.
        mem.write(buf(6), &1u32.to_le_bytes()).expect("xfer header");
        mem.write(buf(6) + 0x800, &[0xaa; 24]).expect("data");
        rxd.push(&mem, &[(buf(6), 4, false), (buf(6) + 0x800, 16, true), (buf(6) + 0x810, 8, true)]);
        w.run();
        let mut back = [0u8; 24];
        mem.read(buf(6) + 0x800, &mut back).expect("read");
        assert_eq!((rxd.used(&mem, 0).1, &back[..16], le32(&back, 16)), (24, &[0u8; 16][..], Some(S_BAD_MSG)));
    }
}
