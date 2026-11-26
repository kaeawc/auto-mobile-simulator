//! `logcat`: the log stream of images without a logcat console (API 30 and older). Their
//! `goldfish-logcat` service runs `logcat -Q`, which reads `qemu.cmdline` for
//! `androidboot.consolepipe=qemu_pipe,pipe:logcat` and writes the log, as text, to that pipe.
//! The host never answers.
use super::{HostMsg, PipeCtx, PipeHandler, PipeService};
use crate::virtio::console::Spool;

/// Where the stream goes; None discards it, as stock does without `-logcat`.
pub struct LogcatService {
    pub out: Option<Spool>,
}

impl PipeService for LogcatService {
    fn open(&self, name: &str, _ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        (name == "logcat").then(|| Box::new(LogcatPipe { out: self.out.clone() }) as _)
    }
}

struct LogcatPipe {
    out: Option<Spool>,
}

impl PipeHandler for LogcatPipe {
    fn guest_data(&mut self, data: &[u8], _to_guest: &mut Vec<u8>) -> bool {
        if let Some(out) = &self.out {
            out.send(data);
        }
        true
    }

    fn host_msg(&mut self, msg: HostMsg, _to_guest: &mut Vec<u8>) -> bool {
        !matches!(msg, HostMsg::Close)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writes_the_guest_log_and_answers_nothing() {
        let got = Shared::default();
        let svc = LogcatService { out: Some(Spool::new("logcat-test", Box::new(got.clone()), 8).expect("spool")) };
        let (tx, _rx) = std::sync::mpsc::channel();
        assert!(svc.open("qemud:logcat", PipeCtx { conn: 1, events: tx.clone() }).is_none(), "only `logcat`");
        let mut p = svc.open("logcat", PipeCtx { conn: 1, events: tx }).expect("open");
        let mut out = Vec::new();
        assert!(p.guest_data(b"11-26 02:00:00.000  1  1 I init: hello\n", &mut out));
        assert!(out.is_empty());
        drop(p);
        drop(svc); // the spool's writer drains and exits
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while got.0.lock().expect("lock").is_empty() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(&*got.0.lock().expect("lock"), b"11-26 02:00:00.000  1  1 I init: hello\n");
    }

    #[test]
    fn without_a_destination_the_log_is_dropped() {
        let svc = LogcatService { out: None };
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut p = svc.open("logcat", PipeCtx { conn: 1, events: tx }).expect("open");
        let mut out = Vec::new();
        assert!(p.guest_data(b"x", &mut out) && out.is_empty());
        assert!(!p.host_msg(HostMsg::Close, &mut out));
    }
}
