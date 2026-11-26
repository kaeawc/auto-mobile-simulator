//! `opengles`: the GLES encoder's stream, on images whose graphics go over the goldfish pipe
//! (API 29 and 30, `qemu.gltransport=pipe`).
//!
//! Each guest GL connection (one per guest thread that uses GL) opens its own pipe, and the host
//! gives it its own render thread: a [`GlChannel`] from the [`GlBackend`] (gfxstream's
//! `RenderChannel` on the host). Guest bytes go to the channel as they arrive; a reader thread per
//! pipe waits on the channel and forwards the render thread's replies to the guest. That is the
//! stock emulator's model (one render thread per connection, all of them running at once), with
//! one more host thread per connection, which sleeps on the channel's queue while the guest
//! isn't waiting for a reply.
//!
//! `GLProcessPipe` ([`crate::devices::goldfish_pipe::GlProcess`]) names the process behind the
//! connections; with a backend it also tells the renderer when a process comes and goes.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use super::{HostMsg, PipeCtx, PipeHandler, PipeService};

/// What a [`GlChannel::read`] got.
#[derive(Debug, PartialEq, Eq)]
pub enum GlRead {
    /// Bytes for the guest were appended.
    Data,
    /// Nothing yet.
    Timeout,
    /// The channel is stopped and drained.
    Closed,
}

/// What a [`GlChannel::try_write`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum GlWrite {
    /// The render thread has the bytes.
    Done,
    /// Its queue is full (the render thread is far behind); nothing was taken.
    Full,
    /// The channel is stopped.
    Stopped,
}

/// One guest GL connection's host end.
pub trait GlChannel: Send + Sync {
    /// Guest bytes for the render thread, if its queue has room. Never waits.
    fn try_write(&self, data: &[u8]) -> GlWrite;
    /// Waits until the queue has room or the channel stops.
    fn wait_writable(&self);
    /// Appends the render thread's next reply to `out`, waiting up to `timeout`.
    fn read(&self, timeout: Duration, out: &mut Vec<u8>) -> GlRead;
    /// Ends the connection: a blocked read returns [`GlRead::Closed`].
    fn stop(&self);
}

/// The host renderer, as the GL pipes need it.
pub trait GlBackend: Send + Sync {
    fn open(&self) -> Option<Arc<dyn GlChannel>>;
    /// A guest process got `puid` from its `GLProcessPipe`.
    fn process_created(&self, puid: u64);
    /// That process closed its `GLProcessPipe` (it exited).
    fn process_dead(&self, puid: u64);
}

/// How long a reader waits on the channel before it looks again (the channel's stop wakes it at
/// once; this only bounds a missed wakeup).
const READ_WAIT: Duration = Duration::from_millis(500);

pub struct OpenglesService {
    pub backend: Arc<dyn GlBackend>,
}

impl PipeService for OpenglesService {
    fn open(&self, name: &str, ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        if name != "opengles" {
            return None;
        }
        let Some(channel) = self.backend.open() else {
            eprintln!("emu: no render channel for the guest's opengles pipe");
            return None;
        };
        let (reader, reader_ctx) = (channel.clone(), ctx.clone());
        let spawned = std::thread::Builder::new().name(format!("gl-pipe-{}", ctx.conn as u32)).spawn(move || {
            crate::device_thread();
            forward_replies(&*reader, &reader_ctx);
        });
        if spawned.is_err() {
            channel.stop();
            return None;
        }
        Some(Box::new(OpenglesPipe { channel, ctx, held: Arc::default() }))
    }
}

/// The render thread's replies → the guest, until the channel closes.
fn forward_replies(channel: &dyn GlChannel, ctx: &PipeCtx) {
    loop {
        let mut out = Vec::new();
        match channel.read(READ_WAIT, &mut out) {
            GlRead::Data if out.is_empty() => {}
            GlRead::Data => {
                if !ctx.send(HostMsg::Data(out)) {
                    channel.stop();
                    return;
                }
            }
            GlRead::Timeout => {}
            GlRead::Closed => {
                ctx.send(HostMsg::Close);
                return;
            }
        }
    }
}

/// Guest writes the render thread had no room for, in order.
type Held = Arc<Mutex<VecDeque<Vec<u8>>>>;

struct OpenglesPipe {
    channel: Arc<dyn GlChannel>,
    ctx: PipeCtx,
    /// Writes waiting for room. While any wait the pipe is [`write_blocked`](PipeHandler::write_blocked):
    /// the goldfish pipe answers the guest's next writes with "try again" (stock's
    /// PIPE_ERROR_AGAIN) instead of a vCPU waiting under the device's lock, and a thread of
    /// this pipe's waits for room, hands them over and reports [`PipeCtx::drained`].
    held: Held,
}

impl OpenglesPipe {
    /// Hands `held` to the render thread as it makes room, then tells the device.
    fn deliver_held(channel: &dyn GlChannel, held: &Mutex<VecDeque<Vec<u8>>>, ctx: &PipeCtx) {
        loop {
            channel.wait_writable();
            let mut q = held.lock().unwrap_or_else(PoisonError::into_inner);
            while let Some(front) = q.front() {
                match channel.try_write(front) {
                    GlWrite::Done => drop(q.pop_front()),
                    GlWrite::Full => break,
                    // The pipe is closing; its next write fails.
                    GlWrite::Stopped => return,
                }
            }
            if q.is_empty() {
                drop(q);
                ctx.drained();
                return;
            }
        }
    }
}

impl PipeHandler for OpenglesPipe {
    fn guest_data(&mut self, data: &[u8], _to_guest: &mut Vec<u8>) -> bool {
        let mut q = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        // Behind earlier held writes (a transport that never asks `write_blocked`).
        if !q.is_empty() {
            q.push_back(data.to_vec());
            return true;
        }
        match self.channel.try_write(data) {
            GlWrite::Done => true,
            GlWrite::Stopped => false,
            GlWrite::Full => {
                q.push_back(data.to_vec());
                drop(q);
                let (channel, held, ctx) = (self.channel.clone(), self.held.clone(), self.ctx.clone());
                let spawned = std::thread::Builder::new().name(format!("gl-pipe-w{}", self.ctx.conn as u32)).spawn(move || {
                    crate::device_thread();
                    OpenglesPipe::deliver_held(&*channel, &held, &ctx);
                });
                spawned.is_ok()
            }
        }
    }

    fn write_blocked(&self) -> bool {
        !self.held.lock().unwrap_or_else(PoisonError::into_inner).is_empty()
    }

    fn host_msg(&mut self, msg: HostMsg, to_guest: &mut Vec<u8>) -> bool {
        match msg {
            HostMsg::Data(d) if to_guest.is_empty() => {
                *to_guest = d;
                true
            }
            HostMsg::Data(d) => {
                to_guest.extend_from_slice(&d);
                true
            }
            HostMsg::Close | HostMsg::Attach(_) => false,
        }
    }

    fn closed(&mut self) {
        self.channel.stop();
    }
}

/// A pipe the device drops without closing (the guest reset) stops its channel too, so its
/// reader and render thread end.
impl Drop for OpenglesPipe {
    fn drop(&mut self) {
        self.channel.stop();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::VecDeque;
    use std::sync::mpsc::{Receiver, channel};
    use std::sync::{Condvar, Mutex};

    use super::*;
    use crate::pipe::WorkerEvent;

    /// A render thread that answers every write with the bytes reversed. A test can fill its
    /// queue (writes find no room) or hold its replies (reads wait, even after a stop).
    #[derive(Default)]
    pub(crate) struct Echo {
        state: Mutex<EchoState>,
        more: Condvar,
        pub(crate) written: Mutex<Vec<u8>>,
    }

    #[derive(Default)]
    struct EchoState {
        replies: VecDeque<Vec<u8>>,
        stopped: bool,
        full: bool,
        held: bool,
    }

    impl Echo {
        pub(crate) fn set_full(&self, full: bool) {
            self.state.lock().expect("lock").full = full;
            self.more.notify_all();
        }
        pub(crate) fn set_held(&self, held: bool) {
            self.state.lock().expect("lock").held = held;
            self.more.notify_all();
        }
    }

    impl GlChannel for Echo {
        fn try_write(&self, data: &[u8]) -> GlWrite {
            let mut st = self.state.lock().expect("lock");
            if st.stopped {
                return GlWrite::Stopped;
            }
            if st.full {
                return GlWrite::Full;
            }
            self.written.lock().expect("lock").extend_from_slice(data);
            st.replies.push_back(data.iter().rev().copied().collect());
            self.more.notify_all();
            GlWrite::Done
        }
        fn wait_writable(&self) {
            let st = self.state.lock().expect("lock");
            drop(self.more.wait_while(st, |s| s.full && !s.stopped).expect("wait"));
        }
        fn read(&self, timeout: Duration, out: &mut Vec<u8>) -> GlRead {
            let st = self.state.lock().expect("lock");
            let (mut st, _) = self.more.wait_timeout_while(st, timeout, |s| s.held || (s.replies.is_empty() && !s.stopped)).expect("wait");
            if st.held {
                return GlRead::Timeout;
            }
            match st.replies.pop_front() {
                Some(d) => {
                    out.extend_from_slice(&d);
                    GlRead::Data
                }
                None if st.stopped => GlRead::Closed,
                None => GlRead::Timeout,
            }
        }
        fn stop(&self) {
            self.state.lock().expect("lock").stopped = true;
            self.more.notify_all();
        }
    }

    #[derive(Default)]
    pub(crate) struct Backend {
        pub(crate) channels: Mutex<Vec<Arc<Echo>>>,
        pub(crate) processes: Mutex<Vec<(u64, bool)>>,
        pub(crate) refuse: bool,
    }

    impl GlBackend for Backend {
        fn open(&self) -> Option<Arc<dyn GlChannel>> {
            if self.refuse {
                return None;
            }
            let c = Arc::new(Echo::default());
            self.channels.lock().expect("lock").push(c.clone());
            Some(c)
        }
        fn process_created(&self, puid: u64) {
            self.processes.lock().expect("lock").push((puid, true));
        }
        fn process_dead(&self, puid: u64) {
            self.processes.lock().expect("lock").push((puid, false));
        }
    }

    fn open(backend: &Arc<Backend>) -> (Box<dyn PipeHandler>, Receiver<WorkerEvent>) {
        let (tx, rx) = channel();
        let svc = OpenglesService { backend: backend.clone() };
        let h = svc.open("opengles", PipeCtx { conn: 7, events: tx }).expect("served");
        (h, rx)
    }

    fn host_msg(rx: &Receiver<WorkerEvent>) -> HostMsg {
        match rx.recv_timeout(Duration::from_secs(5)).expect("an event") {
            WorkerEvent::Host { conn: 7, msg } => msg,
            _ => panic!("expected a host message for the pipe"),
        }
    }

    #[test]
    fn guest_bytes_reach_the_render_thread_and_replies_come_back() {
        let backend = Arc::new(Backend::default());
        let (mut h, rx) = open(&backend);
        let mut to_guest = Vec::new();
        assert!(h.guest_data(b"\x00\x00\x00\x00rcGetRendererVersion", &mut to_guest));
        assert!(to_guest.is_empty(), "replies come from the reader thread, not inline");
        let HostMsg::Data(reply) = host_msg(&rx) else { panic!("expected data") };
        let sent = b"\x00\x00\x00\x00rcGetRendererVersion";
        assert_eq!(reply, sent.iter().rev().copied().collect::<Vec<u8>>(), "the render thread's answer");
        assert!(h.host_msg(HostMsg::Data(reply.clone()), &mut to_guest));
        assert!(h.host_msg(HostMsg::Data(b"!".to_vec()), &mut to_guest));
        assert_eq!(to_guest, [reply, b"!".to_vec()].concat());
        assert_eq!(&*backend.channels.lock().expect("lock")[0].written.lock().expect("lock"), b"\x00\x00\x00\x00rcGetRendererVersion");
    }

    #[test]
    fn each_pipe_gets_its_own_channel() {
        let backend = Arc::new(Backend::default());
        let (_a, _ra) = open(&backend);
        let (_b, _rb) = open(&backend);
        assert_eq!(backend.channels.lock().expect("lock").len(), 2);
    }

    #[test]
    fn a_guest_close_stops_the_channel_and_its_reader() {
        let backend = Arc::new(Backend::default());
        let (mut h, rx) = open(&backend);
        h.closed();
        assert!(matches!(host_msg(&rx), HostMsg::Close), "the reader saw the stop and ended");
        let c = backend.channels.lock().expect("lock")[0].clone();
        assert_eq!(c.try_write(b"late"), GlWrite::Stopped);
        assert!(!h.guest_data(b"late", &mut Vec::new()), "writes after the stop close the pipe");
    }

    #[test]
    fn a_host_stop_closes_the_pipe() {
        let backend = Arc::new(Backend::default());
        let (mut h, rx) = open(&backend);
        backend.channels.lock().expect("lock")[0].stop();
        let msg = host_msg(&rx);
        assert!(matches!(msg, HostMsg::Close));
        assert!(!h.host_msg(msg, &mut Vec::new()), "the pipe hangs up");
    }

    #[test]
    fn other_names_and_a_renderer_without_channels_are_not_served() {
        let (tx, _rx) = channel();
        let backend = Arc::new(Backend::default());
        let svc = OpenglesService { backend };
        assert!(svc.open("opengles2", PipeCtx { conn: 1, events: tx.clone() }).is_none());
        let svc = OpenglesService { backend: Arc::new(Backend { refuse: true, ..Backend::default() }) };
        assert!(svc.open("opengles", PipeCtx { conn: 1, events: tx }).is_none());
    }
}
