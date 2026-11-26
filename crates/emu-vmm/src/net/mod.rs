//! A user-mode NAT behind virtio-net: the guest's internet, as stock's slirp gives it.
//!
//! The guest sits on 10.0.2.0/24 at 10.0.2.15 (the radio HAL's `+CGCONTRDP` config). The NAT is
//! its gateway, 10.0.2.2, and its resolver, 10.0.2.3, and answers ARP for those two only. Every
//! flow the guest opens becomes a host socket of the emu process, so it needs no root, no
//! entitlement and no host interface:
//!
//! - TCP: a guest SYN starts a non-blocking connect to the real destination; once that
//!   connects, a smoltcp socket answers the SYN (a refused or failed connect answers with RST).
//!   Data moves between smoltcp's buffers and the host socket with no copy in between.
//! - UDP: one connected host socket per guest (source, destination) pair, dropped when idle.
//! - ICMP echo: one unprivileged ICMP datagram socket per (destination, identifier); echoes to
//!   the NAT's own addresses are answered here.
//! - 10.0.2.2 is the host's loopback, as on stock; 10.0.2.3 is the host's resolver
//!   (`/etc/resolv.conf`).
//!
//! One thread per device runs it, blocked in `poll(2)` over the host sockets it is waiting on
//! and a wakeup the device rings when the guest sends; its timeout is smoltcp's next timer or
//! the next idle flow's expiry. An idle device costs no CPU.
mod engine;
mod host;
mod packet;
mod shape;
#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

pub use host::Dns;

use crate::virtio::net::{Backend, RxReady};

/// slirp's MAC for its virtual hosts (`52:55:0a:00:02:02`), which the guest learns for 10.0.2.2.
pub const GATEWAY_MAC: [u8; 6] = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];

/// Frames held for the guest. Past this smoltcp holds its segments (and retries) instead.
const TO_GUEST: usize = 256;
/// Frames from the guest waiting for the net thread; past this they drop, as on a full link.
const TO_HOST: usize = 1024;

/// The guest's network, as stock lays it out.
#[derive(Clone, Debug)]
pub struct Config {
    pub gateway: Ipv4Addr,
    pub resolver: Ipv4Addr,
    pub prefix_len: u8,
    pub gateway_mac: [u8; 6],
    /// What the gateway's address reaches on the host.
    pub loopback: IpAddr,
    pub dns: Dns,
    /// How many flows (each a host socket) the guest may hold open.
    pub limits: Limits,
    /// The console's `network speed|delay`: read live, so a change applies to the next frame.
    pub network: Arc<Mutex<sim_core::network::Conditions>>,
}

/// Caps on the guest's flows. The guest is untrusted and every flow is a host file descriptor
/// of the emu process: past the process's limit (`RLIMIT_NOFILE`, 256 by default on macOS) new
/// flows fail and so does every `accept` emu makes (the console, the adb bridge).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// TCP connections, connecting or open; a SYN past this is reset.
    pub tcp: usize,
    /// Of those, host connects still in progress.
    pub tcp_connecting: usize,
    /// UDP flows; past this the one closest to expiring is dropped.
    pub udp: usize,
    /// ICMP echo flows, likewise.
    pub ping: usize,
}

/// File descriptors left for the rest of emu: disks, the console and its clients, the adb
/// bridge, the display socket and frame rings, the renderer, the NAT's own wakeup.
const RESERVED_FDS: u64 = 128;

impl Limits {
    /// The caps for a process allowed `fds` descriptors: at most 1024 TCP (256 connecting),
    /// 512 UDP and 64 ICMP flows, fewer when the limit leaves less than that after
    /// [`RESERVED_FDS`].
    pub fn for_fds(fds: u64) -> Limits {
        let budget = usize::try_from(fds.saturating_sub(RESERVED_FDS)).unwrap_or(usize::MAX);
        let udp = (budget / 4).min(512);
        let ping = (budget / 16).min(64);
        let tcp = (budget - udp - ping).clamp(1, 1024);
        Limits { tcp, tcp_connecting: tcp.min(256), udp: udp.max(1), ping: ping.max(1) }
    }

    /// The caps for this process's current `RLIMIT_NOFILE` (emu raises it at start).
    pub fn from_rlimit() -> Limits {
        let fds = rustix::process::getrlimit(rustix::process::Resource::Nofile).current.unwrap_or(u64::MAX);
        Limits::for_fds(fds)
    }
}

impl Default for Config {
    fn default() -> Config {
        Config {
            gateway: Ipv4Addr::new(10, 0, 2, 2),
            resolver: Ipv4Addr::new(10, 0, 2, 3),
            prefix_len: 24,
            gateway_mac: GATEWAY_MAC,
            loopback: IpAddr::V4(Ipv4Addr::LOCALHOST),
            dns: Dns::System,
            limits: Limits::from_rlimit(),
            network: Arc::default(),
        }
    }
}

/// What the device's worker and the net thread share.
struct Shared {
    to_host: Mutex<VecDeque<Vec<u8>>>,
    to_guest: Mutex<VecDeque<Vec<u8>>>,
    ready: Mutex<Option<RxReady>>,
    /// The net thread stopped taking smoltcp's frames because `to_guest` was full: the worker
    /// wakes it once the guest has taken half.
    guest_full: AtomicBool,
    stop: AtomicBool,
    reset: AtomicBool,
    wake: UnixStream,
    /// Loop iterations and open flows, for tests and diagnostics.
    iterations: AtomicU64,
    flows: AtomicUsize,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Shared {
    fn wake(&self) {
        // A full socket buffer means a wakeup is already pending.
        let _ = (&self.wake).write(&[1]);
    }

    fn ring_guest(&self) {
        if let Some(r) = lock(&self.ready).as_ref() {
            r.ring();
        }
    }
}

/// The NAT, as the virtio-net device's [`Backend`]. Its thread starts on the device's first
/// activation; a later activation (the guest rebooted or reset the device) drops every flow.
pub struct Nat {
    shared: Arc<Shared>,
    /// The engine's wakeup end and config, until the thread takes them.
    start: Option<(UnixStream, Config)>,
    thread: Option<JoinHandle<()>>,
}

impl Nat {
    pub fn new(cfg: Config) -> io::Result<Nat> {
        let (wake, wait) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        wait.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            to_host: Mutex::new(VecDeque::new()),
            to_guest: Mutex::new(VecDeque::new()),
            ready: Mutex::new(None),
            guest_full: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            reset: AtomicBool::new(false),
            wake,
            iterations: AtomicU64::new(0),
            flows: AtomicUsize::new(0),
        });
        Ok(Nat { shared, start: Some((wait, cfg)), thread: None })
    }

    #[cfg(test)]
    fn iterations(&self) -> u64 {
        self.shared.iterations.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn flows(&self) -> usize {
        self.shared.flows.load(Ordering::Relaxed)
    }
}

impl Backend for Nat {
    fn attach(&mut self, ready: RxReady) {
        *lock(&self.shared.ready) = Some(ready);
        lock(&self.shared.to_guest).clear();
        lock(&self.shared.to_host).clear();
        if let Some((wait, cfg)) = self.start.take() {
            let shared = self.shared.clone();
            let spawned = std::thread::Builder::new().name("virtio-net-nat".into()).spawn(move || {
                crate::device_thread();
                engine::Engine::new(cfg, shared).run(wait);
            });
            match spawned {
                Ok(h) => self.thread = Some(h),
                Err(e) => eprintln!("emu: net: no NAT thread ({e}); the network is link only"),
            }
        } else {
            self.shared.reset.store(true, Ordering::Release);
            self.shared.wake();
        }
    }

    fn transmit(&mut self, frame: &[u8]) {
        if self.thread.is_none() {
            return;
        }
        let mut q = lock(&self.shared.to_host);
        if q.len() >= TO_HOST {
            return;
        }
        q.push_back(frame.to_vec());
        let first = q.len() == 1;
        drop(q);
        if first {
            self.shared.wake();
        }
    }

    fn receive(&mut self) -> Option<Vec<u8>> {
        let mut q = lock(&self.shared.to_guest);
        let f = q.pop_front();
        if q.len() <= TO_GUEST / 2 && self.shared.guest_full.swap(false, Ordering::AcqRel) {
            drop(q);
            self.shared.wake();
        }
        f
    }
}

impl Drop for Nat {
    fn drop(&mut self) {
        if let Some(h) = self.thread.take() {
            self.shared.stop.store(true, Ordering::Release);
            self.shared.wake();
            let _ = h.join();
        }
    }
}
