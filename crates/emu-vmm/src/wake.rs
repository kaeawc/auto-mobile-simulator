//! Which vCPUs a raised interrupt must rouse, and how: the wake/park state machine.
//!
//! With the framework's in-kernel GIC an idle vCPU waits for interrupts inside `hv_vcpu_run`.
//! simee measured that wait not noticing an SPI raised by a device thread until the vCPU's own
//! timer fired, so raising one kicks vCPUs out of the run (`hv_vcpus_exit`) to see it (first-boot
//! `/data` encryption: 22 s without, 8 s with). Other VMMs on the same API (QEMU's and libkrun's
//! in-kernel GIC, arcbox on macOS 26.4) only raise the SPI and measured kicks as pure cost, so
//! the framework may wake the target itself on newer macOS; [`Mode::NoKick`] tries that.
//! Kicking every vCPU for every SPI costs one forced exit per vCPU per interrupt, busy vCPUs
//! included. Here each vCPU publishes where it is, and an SPI rouses only its target (the vCPU
//! `GICD_IROUTER` names), and only if it needs it:
//!
//! - **Guest** (inside `hv_vcpu_run`): kick it, at most once per entry into the guest.
//! - **Parked** (in a host-side WFI wait): wake it.
//! - **Host** (handling an exit): nothing; it sees the pending interrupt when it next enters
//!   the guest, and a later park sees the notification and doesn't sleep.
//! - **Off** (PSCI CPU_OFF, or not started): nothing; it can't take interrupts.
//!
//! SGIs (IPIs between vCPUs) are handled by the in-kernel GIC and never reach the VMM. A vCPU in
//! the guest gets them from the framework, but one parked on the host can't see them: its wait
//! is bounded by [`SGI_POLL`] while any other vCPU is online (only an online vCPU can send one)
//! and unbounded (timer deadline or an explicit wake) otherwise.
//!
//! Each vCPU's state is one atomic word. Only the vCPU's own thread changes the state (and the
//! PSCI caller bringing an off vCPU online); interrupt sources only OR in the `NOTIFY` and
//! `KICKED` flags, so a wake never races a state change into a lost interrupt:
//!
//! - an SPI set before the vCPU's `enter` is seen by that entry (the swap reads the source's flag
//!   update, so the source's `hv_gic_set_spi` happens before `hv_vcpu_run`);
//! - one set after `enter` finds `Guest` and kicks (the current run returns, or the next one if
//!   the vCPU is just leaving);
//! - `leave` keeps `NOTIFY`, so an SPI that kicked a vCPU already trapping on WFI still stops the
//!   park that follows;
//! - a parked vCPU checks `NOTIFY` and starts waiting under its wait lock, and a source sets
//!   `NOTIFY` before it takes that lock to notify, so the notification can't land in between.
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Bound on a host-side WFI wait while another vCPU could send this one an SGI.
pub const SGI_POLL: Duration = Duration::from_millis(1);

/// At most this many vCPUs (kick sets are a bitmask).
pub const MAX_CPUS: usize = 64;

const STATE: u32 = 0b11;
const OFF: u32 = 0;
const HOST: u32 = 1;
const GUEST: u32 = 2;
const PARKED: u32 = 3;
/// An interrupt for this vCPU was raised since it last entered the guest.
const NOTIFY: u32 = 1 << 2;
/// It has been kicked since it last entered the guest.
const KICKED: u32 = 1 << 3;

/// What the VMM can do to vCPUs from any thread.
pub trait VcpuControl: Send + Sync {
    /// Forces the vCPUs in the bitmask (bit n = vCPU n) out of the guest: each one's current
    /// `hv_vcpu_run` returns, or its next one if it isn't in the guest. One call per batch.
    fn kick(&self, cpus: u64);
    /// `GICD_IROUTER<intid>`, where the guest routes the SPI, if it can be read.
    fn spi_router(&self, intid: u32) -> Option<u64>;
}

/// Which vCPUs an SPI can be delivered to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Cpu(usize),
    Any,
}

impl Target {
    /// Decodes `GICD_IROUTER`: Aff3 in [39:32], the 1-of-N mode bit (IRM) in 31, Aff2..Aff0 in
    /// [23:0]. vCPU n has MPIDR Aff0 = n; anything else (1-of-N, other affinities) is `Any`.
    pub fn from_irouter(value: u64) -> Target {
        const IRM: u64 = 1 << 31;
        const AFF1_TO_AFF3: u64 = 0xff_00ff_ff00;
        if value & IRM != 0 || value & AFF1_TO_AFF3 != 0 { Target::Any } else { Target::Cpu((value & 0xff) as usize) }
    }
}

/// How interrupts rouse vCPUs (`EMU_VCPU_WAKE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Only the SPI's target, only when it needs it (the default).
    Targeted,
    /// Never kick for an interrupt, only wake vCPUs parked on the host: for when the framework
    /// wakes a vCPU waiting in the guest itself (`EMU_VCPU_WAKE=none`).
    NoKick,
    /// Every vCPU for every SPI, host-side WFI waits bounded by [`SGI_POLL`]: the behaviour
    /// before targeted wakes, for A/B measurements (`EMU_VCPU_WAKE=all`).
    All,
}

impl Mode {
    pub fn parse(value: Option<&str>) -> Mode {
        match value {
            Some("all") => Mode::All,
            Some("none") => Mode::NoKick,
            _ => Mode::Targeted,
        }
    }

    pub fn from_env() -> Mode {
        Mode::parse(std::env::var("EMU_VCPU_WAKE").ok().as_deref())
    }
}

/// How a host-side WFI wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Woke {
    /// An interrupt source (or a stop) asked for this vCPU.
    Notified,
    /// The timer deadline or the SGI bound passed.
    Timeout,
}

/// What a source does for a vCPU, given its word before the source's flags went in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Kick,
    Unpark,
    Nothing,
}

fn action(old: u32) -> Action {
    match old & STATE {
        GUEST if old & KICKED == 0 => Action::Kick,
        // A source that found NOTIFY already set leaves the unpark to the one that set it.
        PARKED if old & NOTIFY == 0 => Action::Unpark,
        _ => Action::Nothing,
    }
}

/// One vCPU's word and wait, alone in its cache line (128 bytes on Apple silicon): its thread
/// writes the word twice per exit, and neighbours sharing the line would bounce it between
/// cores on every exit of every vCPU.
#[derive(Default)]
#[repr(align(128))]
struct Slot {
    word: AtomicU32,
    wait: Mutex<()>,
    cv: Condvar,
}

/// Every vCPU's wake state, and the means to rouse them.
pub struct Wakers<C> {
    slots: Vec<Slot>,
    control: C,
    mode: Mode,
}

impl<C: VcpuControl> Wakers<C> {
    /// All vCPUs start off; each comes online with [`Wakers::online`].
    pub fn new(cpus: usize, control: C, mode: Mode) -> Wakers<C> {
        assert!(cpus <= MAX_CPUS, "at most {MAX_CPUS} vCPUs");
        Wakers { slots: (0..cpus).map(|_| Slot::default()).collect(), control, mode }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn control(&self) -> &C {
        &self.control
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    fn all(&self) -> u64 {
        if self.slots.len() == 64 { u64::MAX } else { (1u64 << self.slots.len()) - 1 }
    }

    /// Rouses whoever SPI `intid` (just raised) can be delivered to.
    pub fn wake_spi(&self, intid: u32) {
        let target = match self.mode {
            Mode::Targeted | Mode::NoKick => self.control.spi_router(intid).map_or(Target::Any, Target::from_irouter),
            Mode::All => Target::Any,
        };
        self.wake(target);
    }

    /// Rouses `target` for an interrupt that is already pending in the GIC.
    pub fn wake(&self, target: Target) {
        if self.mode == Mode::All {
            self.wake_all();
            return;
        }
        let cpus = match target {
            Target::Cpu(i) if i < self.slots.len() => 1u64 << i,
            _ => self.all(),
        };
        let mut kick = 0u64;
        for (i, slot) in self.slots.iter().enumerate().filter(|(i, _)| cpus & (1 << i) != 0) {
            match action(slot.word.fetch_or(NOTIFY | KICKED, Ordering::SeqCst)) {
                Action::Kick => kick |= 1 << i,
                Action::Unpark => Self::unpark(slot),
                Action::Nothing => {}
            }
        }
        if kick != 0 && self.mode != Mode::NoKick {
            self.control.kick(kick);
        }
    }

    /// Forces every vCPU out of the guest and out of any host-side wait, whatever its state
    /// (a stop: vCPUs in the host may be about to enter the guest).
    pub fn wake_all(&self) {
        for slot in &self.slots {
            if slot.word.fetch_or(NOTIFY | KICKED, Ordering::SeqCst) & STATE == PARKED {
                Self::unpark(slot);
            }
        }
        self.control.kick(self.all());
    }

    fn unpark(slot: &Slot) {
        let _g = slot.wait.lock().unwrap_or_else(|e| e.into_inner());
        slot.cv.notify_one();
    }

    fn set_state(&self, idx: usize, keep: u32, state: u32) {
        self.slots[idx].word.update(Ordering::SeqCst, Ordering::SeqCst, |w| (w & keep) | state);
    }

    /// vCPU `idx` is powered on (boot, or PSCI CPU_ON from another vCPU) and in the host.
    pub fn online(&self, idx: usize) {
        self.set_state(idx, NOTIFY, HOST);
    }

    /// vCPU `idx` powered off.
    pub fn offline(&self, idx: usize) {
        self.set_state(idx, 0, OFF);
    }

    /// vCPU `idx` is about to enter the guest. Interrupts raised before this are seen by the
    /// entry, so the flags start over.
    pub fn enter(&self, idx: usize) {
        self.slots[idx].word.swap(GUEST, Ordering::SeqCst);
    }

    /// vCPU `idx` left the guest. A notification stays for the park that may follow.
    pub fn leave(&self, idx: usize) {
        self.set_state(idx, NOTIFY, HOST);
    }

    /// How long a host-side WFI wait of vCPU `idx` may last, given its timer deadline.
    pub fn bound(&self, idx: usize, timer: Option<Duration>) -> Option<Duration> {
        let sgi_possible = match self.mode {
            Mode::All => true,
            Mode::Targeted | Mode::NoKick => {
                self.slots.iter().enumerate().any(|(i, s)| i != idx && s.word.load(Ordering::SeqCst) & STATE != OFF)
            }
        };
        match (timer, sgi_possible) {
            (Some(t), true) => Some(t.min(SGI_POLL)),
            (None, true) => Some(SGI_POLL),
            (t, false) => t,
        }
    }

    /// Host-side WFI for vCPU `idx` (in the host): waits until an interrupt source notifies it,
    /// its timer deadline (`None`: no timer armed) or the SGI bound passes.
    pub fn park(&self, idx: usize, timer: Option<Duration>) -> Woke {
        let slot = &self.slots[idx];
        let deadline = self.bound(idx, timer).map(|d| Instant::now() + d);
        let mut g = slot.wait.lock().unwrap_or_else(|e| e.into_inner());
        if !self.park_begin(idx) {
            return Woke::Notified;
        }
        let woke = loop {
            if slot.word.load(Ordering::SeqCst) & NOTIFY != 0 {
                break Woke::Notified;
            }
            match deadline {
                None => g = slot.cv.wait(g).unwrap_or_else(|e| e.into_inner()),
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        break Woke::Timeout;
                    }
                    g = slot.cv.wait_timeout(g, d - now).unwrap_or_else(|e| e.into_inner()).0;
                }
            }
        };
        drop(g);
        self.park_end(idx);
        woke
    }

    /// Publishes `Parked`, unless a notification is already pending (then consumes it and
    /// returns false: don't sleep).
    fn park_begin(&self, idx: usize) -> bool {
        let word = &self.slots[idx].word;
        let old = word.update(Ordering::SeqCst, Ordering::SeqCst, |w| if w & NOTIFY != 0 { HOST } else { PARKED });
        old & NOTIFY == 0
    }

    fn park_end(&self, idx: usize) {
        self.set_state(idx, 0, HOST);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64};

    use super::*;

    /// Records kicks; routes every SPI to `route` (`None`: the router can't be read).
    #[derive(Default)]
    struct Fake {
        kicks: Mutex<Vec<u64>>,
        route: Mutex<Option<u64>>,
    }

    impl VcpuControl for Fake {
        fn kick(&self, cpus: u64) {
            self.kicks.lock().expect("lock").push(cpus);
        }

        fn spi_router(&self, _intid: u32) -> Option<u64> {
            *self.route.lock().expect("lock")
        }
    }

    /// A timer far enough out that a park ending with it means the wake was lost.
    const LONG: Duration = Duration::from_secs(10);

    fn wakers(cpus: usize) -> Wakers<Fake> {
        Wakers::new(cpus, Fake::default(), Mode::Targeted)
    }

    fn kicks(w: &Wakers<Fake>) -> Vec<u64> {
        std::mem::take(&mut *w.control().kicks.lock().expect("lock"))
    }

    fn word(w: &Wakers<Fake>, idx: usize) -> u32 {
        w.slots[idx].word.load(Ordering::SeqCst)
    }

    #[test]
    fn irouter_decodes_to_a_vcpu_or_any() {
        assert_eq!(Target::from_irouter(0), Target::Cpu(0));
        assert_eq!(Target::from_irouter(3), Target::Cpu(3));
        assert_eq!(Target::from_irouter(0xff), Target::Cpu(255));
        assert_eq!(Target::from_irouter(1 << 31), Target::Any, "1-of-N");
        assert_eq!(Target::from_irouter(1 << 31 | 2), Target::Any);
        assert_eq!(Target::from_irouter(1 << 8), Target::Any, "Aff1");
        assert_eq!(Target::from_irouter(1 << 16), Target::Any, "Aff2");
        assert_eq!(Target::from_irouter(1 << 32), Target::Any, "Aff3");
        // RES0 bits between IRM and Aff3 don't change the target.
        assert_eq!(Target::from_irouter(1 << 24 | 1), Target::Cpu(1));
    }

    #[test]
    fn mode_defaults_to_targeted() {
        assert_eq!(Mode::parse(None), Mode::Targeted);
        assert_eq!(Mode::parse(Some("")), Mode::Targeted);
        assert_eq!(Mode::parse(Some("targeted")), Mode::Targeted);
        assert_eq!(Mode::parse(Some("all")), Mode::All);
        assert_eq!(Mode::parse(Some("none")), Mode::NoKick);
    }

    #[test]
    fn every_word_has_one_action() {
        for w in 0..16u32 {
            let expected = match (w & STATE, w & NOTIFY != 0, w & KICKED != 0) {
                (GUEST, _, false) => Action::Kick,
                (PARKED, false, _) => Action::Unpark,
                _ => Action::Nothing,
            };
            assert_eq!(action(w), expected, "word {w:#06b}");
        }
    }

    #[test]
    fn an_spi_kicks_only_its_target_in_the_guest() {
        let w = wakers(4);
        for i in 0..4 {
            w.online(i);
            w.enter(i);
        }
        *w.control().route.lock().expect("lock") = Some(2);
        w.wake_spi(40);
        assert_eq!(kicks(&w), [0b0100]);
        // Until vCPU 2 enters the guest again, more SPIs don't kick it again.
        w.wake_spi(40);
        w.wake_spi(41);
        assert_eq!(kicks(&w), [] as [u64; 0]);
        w.leave(2);
        w.enter(2);
        w.wake_spi(40);
        assert_eq!(kicks(&w), [0b0100]);
    }

    #[test]
    fn an_unroutable_spi_kicks_every_vcpu_in_the_guest_in_one_batch() {
        let w = wakers(4);
        for i in 0..4 {
            w.online(i);
        }
        w.enter(0);
        w.enter(3);
        w.wake_spi(40);
        assert_eq!(kicks(&w), [0b1001], "router unreadable");
        for route in [1 << 31, 1 << 8, 9] {
            w.enter(0);
            w.enter(3);
            *w.control().route.lock().expect("lock") = Some(route);
            w.wake_spi(40);
            assert_eq!(kicks(&w), [0b1001], "route {route:#x}");
        }
    }

    #[test]
    fn vcpus_in_the_host_or_off_are_not_kicked() {
        let w = wakers(3);
        w.online(1);
        w.enter(1);
        w.leave(1);
        w.wake(Target::Cpu(0));
        w.wake(Target::Cpu(1));
        w.wake(Target::Any);
        assert_eq!(kicks(&w), [] as [u64; 0]);
        // The vCPU in the host keeps the notification for a park; the off one is untouched
        // in state.
        assert_eq!(word(&w, 1) & (STATE | NOTIFY), HOST | NOTIFY);
        assert_eq!(word(&w, 0) & STATE, OFF);
        // Entering the guest starts over: the entry sees what was pending.
        w.enter(1);
        assert_eq!(word(&w, 1), GUEST);
    }

    #[test]
    fn a_notification_while_in_the_host_stops_the_next_park() {
        let w = wakers(1);
        w.online(0);
        w.wake(Target::Cpu(0));
        let t = Instant::now();
        assert_eq!(w.park(0, Some(LONG)), Woke::Notified);
        assert!(t.elapsed() < Duration::from_secs(1));
        assert_eq!(word(&w, 0), HOST, "consumed");
    }

    /// The SPI arrives while the vCPU is in the guest but already trapping on WFI: the kick lands
    /// on its next run, and the park in between must not sleep through the interrupt.
    #[test]
    fn a_kick_racing_a_wfi_trap_stops_the_park() {
        let w = wakers(1);
        w.online(0);
        w.enter(0);
        w.wake(Target::Cpu(0));
        assert_eq!(kicks(&w), [1]);
        w.leave(0);
        assert_eq!(w.park(0, Some(LONG)), Woke::Notified);
    }

    #[test]
    fn the_wait_is_unbounded_only_when_no_other_vcpu_can_send_an_sgi() {
        let ms = Duration::from_millis;
        let w = wakers(2);
        w.online(0);
        assert_eq!(w.bound(0, None), None);
        assert_eq!(w.bound(0, Some(ms(50))), Some(ms(50)));
        // CPU_ON marks the target online before its thread runs.
        w.online(1);
        assert_eq!(w.bound(0, None), Some(SGI_POLL));
        assert_eq!(w.bound(0, Some(ms(50))), Some(SGI_POLL));
        assert_eq!(w.bound(0, Some(Duration::from_micros(200))), Some(Duration::from_micros(200)));
        // Parked or in the guest, vCPU 1 still counts; off, it doesn't.
        w.enter(1);
        assert_eq!(w.bound(0, None), Some(SGI_POLL));
        w.leave(1);
        w.offline(1);
        assert_eq!(w.bound(0, None), None);
        // Legacy mode always bounds.
        let all = Wakers::new(1, Fake::default(), Mode::All);
        assert_eq!(all.bound(0, None), Some(SGI_POLL));
    }

    #[test]
    fn a_park_ends_at_the_timer_deadline() {
        let w = wakers(1);
        w.online(0);
        let t = Instant::now();
        assert_eq!(w.park(0, Some(Duration::from_millis(20))), Woke::Timeout);
        assert!(t.elapsed() >= Duration::from_millis(20));
        assert_eq!(word(&w, 0), HOST);
    }

    #[test]
    fn a_park_bounded_for_sgis_times_out_quickly() {
        let w = wakers(2);
        w.online(0);
        w.online(1);
        let t = Instant::now();
        assert_eq!(w.park(0, Some(LONG)), Woke::Timeout);
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn an_spi_wakes_a_parked_vcpu_without_a_kick() {
        let w = Arc::new(wakers(2));
        w.online(0);
        let parker = {
            let w = w.clone();
            std::thread::spawn(move || w.park(0, Some(LONG)))
        };
        while word(&w, 0) & STATE != PARKED {
            std::thread::yield_now();
        }
        w.wake(Target::Cpu(0));
        assert_eq!(parker.join().expect("thread"), Woke::Notified);
        assert_eq!(kicks(&w), [] as [u64; 0]);
    }

    #[test]
    fn spurious_condvar_wakeups_keep_waiting() {
        let w = Arc::new(wakers(1));
        w.online(0);
        let parker = {
            let w = w.clone();
            std::thread::spawn(move || w.park(0, Some(LONG)))
        };
        while word(&w, 0) & STATE != PARKED {
            std::thread::yield_now();
        }
        for _ in 0..10 {
            Wakers::<Fake>::unpark(&w.slots[0]);
        }
        std::thread::sleep(Duration::from_millis(20));
        assert!(!parker.is_finished(), "woke without a notification");
        w.wake(Target::Any);
        assert_eq!(parker.join().expect("thread"), Woke::Notified);
    }

    #[test]
    fn a_stop_kicks_every_vcpu_and_wakes_parked_ones() {
        let w = Arc::new(wakers(3));
        w.online(0);
        let parker = {
            let w = w.clone();
            std::thread::spawn(move || w.park(0, Some(LONG)))
        };
        while word(&w, 0) & STATE != PARKED {
            std::thread::yield_now();
        }
        w.online(1);
        w.enter(2);
        w.wake_all();
        assert_eq!(parker.join().expect("thread"), Woke::Notified);
        // Including the ones in the host (they may be about to enter) and already kicked ones.
        assert_eq!(kicks(&w), [0b111]);
        w.wake_all();
        assert_eq!(kicks(&w), [0b111]);
    }

    #[test]
    fn legacy_mode_kicks_everyone_for_every_spi() {
        let w = Wakers::new(3, Fake::default(), Mode::All);
        *w.control().route.lock().expect("lock") = Some(1);
        w.wake_spi(40);
        w.wake(Target::Cpu(2));
        assert_eq!(kicks(&w), [0b111, 0b111]);
    }

    #[test]
    fn no_kick_mode_only_wakes_parked_vcpus() {
        // One vCPU, so its park is unbounded and only the wake can end it.
        let w = Arc::new(Wakers::new(1, Fake::default(), Mode::NoKick));
        *w.control().route.lock().expect("lock") = Some(0);
        w.online(0);
        w.enter(0);
        w.wake(Target::Any);
        w.wake_spi(40);
        assert_eq!(kicks(&w), [] as [u64; 0]);
        w.enter(0);
        w.leave(0);
        let parker = {
            let w = w.clone();
            std::thread::spawn(move || w.park(0, Some(LONG)))
        };
        while word(&w, 0) & STATE != PARKED {
            std::thread::yield_now();
        }
        w.wake_spi(40);
        assert_eq!(parker.join().expect("thread"), Woke::Notified);
        assert_eq!(kicks(&w), [] as [u64; 0]);
        // A stop still kicks.
        w.wake_all();
        assert_eq!(kicks(&w), [1]);
    }

    #[test]
    fn out_of_range_targets_and_full_masks() {
        let w = wakers(2);
        w.enter(0);
        w.enter(1);
        w.wake(Target::Cpu(7));
        assert_eq!(kicks(&w), [0b11]);
        let w = Wakers::new(64, Fake::default(), Mode::Targeted);
        assert_eq!((w.len(), w.is_empty()), (64, false));
        w.wake_all();
        assert_eq!(kicks(&w), [u64::MAX]);
    }

    /// Every interleaving of a device raising an SPI (pend in the GIC, then `wake`) with one
    /// vCPU cycle (enter, run, leave, park): the interrupt is never left pending with the vCPU
    /// asleep or stuck in the guest without a kick.
    #[test]
    fn no_interleaving_loses_an_interrupt() {
        // vCPU steps, in order; the device's two steps go between any of them.
        #[derive(Clone, Copy, Debug)]
        enum V {
            Enter,
            Leave,
            ParkBegin,
            Sleep,
        }
        const VCPU: [V; 4] = [V::Enter, V::Leave, V::ParkBegin, V::Sleep];
        for pend_at in 0..=VCPU.len() {
            for wake_at in pend_at..=VCPU.len() {
                let w = wakers(1);
                w.online(0);
                let mut pending = false;
                // Seen: taken at a guest entry, or the run was kicked, or the park didn't
                // sleep / was notified.
                let mut seen = false;
                let mut parked = false;
                for (pos, step) in VCPU.iter().enumerate().map(|(i, s)| (i, Some(*s))).chain([(VCPU.len(), None)]) {
                    if pos == pend_at {
                        pending = true;
                    }
                    if pos == wake_at {
                        w.wake(Target::Cpu(0));
                        if !kicks(&w).is_empty() {
                            seen = true;
                        }
                    }
                    match step {
                        Some(V::Enter) => {
                            w.enter(0);
                            seen |= pending;
                        }
                        Some(V::Leave) => w.leave(0),
                        Some(V::ParkBegin) => parked = w.park_begin(0),
                        // What the wait loop checks under the lock.
                        Some(V::Sleep) if parked && word(&w, 0) & NOTIFY != 0 => seen = true,
                        Some(V::Sleep) if !parked => seen |= pending,
                        Some(V::Sleep) | None => {}
                    }
                }
                // A wake that comes after the vCPU went to sleep is the condvar case: it must
                // see Parked and set NOTIFY.
                if parked && !seen {
                    seen = word(&w, 0) & NOTIFY != 0;
                }
                assert!(seen, "lost: pend before step {pend_at}, wake before step {wake_at}");
            }
        }
    }

    /// Device threads raise SPIs at vCPU threads that run, trap and park in a loop, against a
    /// model of the framework: a run takes what is pending at entry, otherwise stays in the guest
    /// (never noticing an SPI raised meanwhile) until kicked; a kick while it isn't running makes
    /// the next run return. Every interrupt must be taken promptly: a lost one stalls past the
    /// deadline. With one vCPU its parks are unbounded, so a lost unpark hangs too.
    fn stress(cpus: usize, devices_per_cpu: usize, per_device: u64) {
        #[derive(Default)]
        struct Gic {
            pending: Vec<AtomicU64>,
            cancel: Vec<AtomicBool>,
            taken: Vec<AtomicU64>,
            lock: Mutex<()>,
            cv: Condvar,
        }

        struct Hv(Arc<Gic>);

        impl VcpuControl for Hv {
            fn kick(&self, cpus: u64) {
                let _g = self.0.lock.lock().expect("lock");
                for (i, c) in self.0.cancel.iter().enumerate() {
                    if cpus & (1 << i) != 0 {
                        c.store(true, Ordering::SeqCst);
                    }
                }
                self.0.cv.notify_all();
            }

            fn spi_router(&self, intid: u32) -> Option<u64> {
                Some(u64::from(intid))
            }
        }

        let gic = Arc::new(Gic {
            pending: (0..cpus).map(|_| AtomicU64::new(0)).collect(),
            cancel: (0..cpus).map(|_| AtomicBool::new(false)).collect(),
            taken: (0..cpus).map(|_| AtomicU64::new(0)).collect(),
            ..Gic::default()
        });
        let w = Arc::new(Wakers::new(cpus, Hv(gic.clone()), Mode::Targeted));
        for i in 0..cpus {
            w.online(i);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let vcpus: Vec<_> = (0..cpus)
            .map(|i| {
                let (w, gic, stop) = (w.clone(), gic.clone(), stop.clone());
                std::thread::spawn(move || {
                    let mut n = 0u64;
                    while !stop.load(Ordering::SeqCst) {
                        w.enter(i);
                        let took = gic.pending[i].swap(0, Ordering::SeqCst);
                        if took > 0 {
                            gic.taken[i].fetch_add(took, Ordering::SeqCst);
                        } else {
                            let g = gic.lock.lock().expect("lock");
                            let _g = gic.cv.wait_while(g, |_| !gic.cancel[i].load(Ordering::SeqCst)).expect("wait");
                            gic.cancel[i].store(false, Ordering::SeqCst);
                        }
                        w.leave(i);
                        n += 1;
                        // Every other exit is a WFI trap with no timer armed.
                        if n.is_multiple_of(2) {
                            w.park(i, None);
                        }
                    }
                })
            })
            .collect();
        let devices: Vec<_> = (0..cpus * devices_per_cpu)
            .map(|d| {
                let (w, gic) = (w.clone(), gic.clone());
                let i = d % cpus;
                std::thread::spawn(move || {
                    for _ in 0..per_device {
                        let before = gic.taken[i].load(Ordering::SeqCst);
                        gic.pending[i].fetch_add(1, Ordering::SeqCst);
                        w.wake_spi(i as u32);
                        let t = Instant::now();
                        while gic.taken[i].load(Ordering::SeqCst) == before {
                            assert!(t.elapsed() < Duration::from_secs(5), "an interrupt for vCPU {i} was lost");
                            std::thread::yield_now();
                        }
                    }
                })
            })
            .collect();
        for d in devices {
            d.join().expect("thread");
        }
        // The last ones too, before the stop's wake_all could rescue a lost one.
        let total = per_device * devices_per_cpu as u64;
        let t = Instant::now();
        while gic.taken.iter().any(|n| n.load(Ordering::SeqCst) != total) {
            assert!(t.elapsed() < Duration::from_secs(5), "interrupts lost at the end");
            std::thread::yield_now();
        }
        stop.store(true, Ordering::SeqCst);
        w.wake_all();
        for v in vcpus {
            v.join().expect("thread");
        }
    }

    #[test]
    fn stress_one_vcpu_unbounded_parks() {
        stress(1, 3, 3000);
    }

    #[test]
    fn stress_targeted_kicks_between_vcpus() {
        stress(4, 2, 1500);
    }

    /// The per-exit cost of publishing where a vCPU is: N vCPU threads each looping
    /// `enter`/`leave` on their own slot, no interrupt sources. Neighbouring slots sharing a
    /// cache line show up as a per-exit cost that grows with N.
    /// `cargo test --release -p emu-vmm exit_publish_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark"]
    fn exit_publish_cost() {
        const ITERS: u32 = 2_000_000;
        for cpus in [1usize, 2, 4, 8] {
            let w = wakers(cpus);
            for i in 0..cpus {
                w.online(i);
            }
            let barrier = std::sync::Barrier::new(cpus);
            let per: Vec<f64> = std::thread::scope(|s| {
                let hs: Vec<_> = (0..cpus)
                    .map(|i| {
                        let (w, barrier) = (&w, &barrier);
                        s.spawn(move || {
                            barrier.wait();
                            let t = Instant::now();
                            for _ in 0..ITERS {
                                w.enter(i);
                                w.leave(i);
                            }
                            t.elapsed().as_nanos() as f64 / f64::from(ITERS)
                        })
                    })
                    .collect();
                hs.into_iter().map(|h| h.join().expect("thread")).collect()
            });
            let worst = per.iter().copied().fold(0.0, f64::max);
            println!("exit_publish {cpus} vCPUs: {worst:.1} ns per enter+leave (slowest thread)");
        }
    }
}
