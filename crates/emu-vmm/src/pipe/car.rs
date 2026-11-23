//! `qemud:car`: the Android Automotive vehicle HAL's emulator channel.
//!
//! The emulator VHAL in AAOS images opens this pipe at boot (only when `ro.boot.qemu` is set,
//! and it carries on without it), then sends `SET_PROPERTY_ASYNC` whenever a property changes
//! and answers every host command in order. Stock registers the service only for car images
//! (build.prop names a `car` product or flavor); `MachineConfig::car` follows that.
//!
//! The protocol is `sim_core::vhal`; this is the pipe and the host API the console drives
//! ([`Car::set`], [`Car::get`], [`Car::all`]). Guest frames are always drained, whether or not a
//! host command is waiting.
use std::collections::HashMap;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use sim_core::vhal::{self, Message, PropValue, Tracker};

use super::{HostMsg, PipeCtx, PipeHandler, PipeService};

/// How long a command waits for the guest's reply.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(3);

const NOT_CONNECTED: &str = "the vehicle HAL isn't connected (the guest is still booting, or its VHAL has no qemud channel)";

/// The vehicle HAL connection, shared by the pipe and the console.
#[derive(Default)]
pub struct Car {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    conn: Option<PipeCtx>,
    tracker: Tracker,
    waiters: HashMap<u64, Sender<Message>>,
}

impl Car {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn connected(&self) -> bool {
        self.lock().conn.is_some()
    }

    /// Sends a command and waits up to `timeout` for its reply.
    pub fn request(&self, msg: &Message, timeout: Duration) -> Result<Message, String> {
        let frame = msg.frame()?;
        let (tx, rx) = channel();
        let id = {
            let mut s = self.lock();
            let conn = s.conn.clone().ok_or(NOT_CONNECTED)?;
            let id = s.tracker.expect(msg).ok_or("not a vehicle HAL command")?;
            s.waiters.insert(id, tx);
            if !conn.send(HostMsg::Data(frame)) {
                s.tracker.abandon(id);
                s.waiters.remove(&id);
                return Err(NOT_CONNECTED.into());
            }
            id
        };
        match rx.recv_timeout(timeout) {
            Ok(reply) => Ok(reply),
            Err(e) => {
                let mut s = self.lock();
                s.tracker.abandon(id);
                s.waiters.remove(&id);
                Err(match e {
                    RecvTimeoutError::Timeout => format!("the vehicle HAL didn't answer within {} ms", timeout.as_millis()),
                    RecvTimeoutError::Disconnected => "the vehicle HAL disconnected".into(),
                })
            }
        }
    }

    fn checked(reply: Message) -> Result<Message, String> {
        if reply.ok() { Ok(reply) } else { Err(format!("the vehicle HAL answered {}", vhal::status_name(reply.status.unwrap_or(0)))) }
    }

    /// Sets a property (`SET_PROPERTY_CMD`, as stock's car controls do).
    pub fn set(&self, value: PropValue) -> Result<(), String> {
        Self::checked(self.request(&Message::set_property(value), REPLY_TIMEOUT)?).map(drop)
    }

    /// Reads a property's current value from the guest (`GET_PROPERTY_CMD`).
    pub fn get(&self, prop: i32, area_id: i32) -> Result<PropValue, String> {
        let reply = Self::checked(self.request(&Message::get_property(prop, area_id), REPLY_TIMEOUT)?)?;
        reply.values.into_iter().next().ok_or_else(|| format!("the vehicle HAL returned no value for {}", vhal::label(prop)))
    }

    /// Every property's current value (`GET_PROPERTY_ALL_CMD`).
    pub fn all(&self) -> Result<Vec<PropValue>, String> {
        Ok(Self::checked(self.request(&Message::get_property_all(), REPLY_TIMEOUT)?)?.values)
    }

    /// The latest values the guest reported on its own or in replies, without asking it.
    pub fn known(&self) -> Vec<PropValue> {
        self.lock().tracker.values().cloned().collect()
    }

    fn attach(&self, ctx: PipeCtx) {
        let mut s = self.lock();
        // A new connection is a restarted VHAL: earlier commands will never be answered.
        s.conn = Some(ctx);
        s.tracker.reset();
        s.waiters.clear();
    }

    fn guest_data(&self, conn: u64, data: &[u8]) {
        let mut s = self.lock();
        if s.conn.as_ref().map(|c| c.conn) != Some(conn) {
            return; // a superseded connection: drained and dropped
        }
        for (id, reply) in s.tracker.feed(data) {
            if let Some(tx) = s.waiters.remove(&id) {
                let _ = tx.send(reply);
            }
        }
    }

    fn detach(&self, conn: u64) {
        let mut s = self.lock();
        if s.conn.as_ref().map(|c| c.conn) == Some(conn) {
            s.conn = None;
            s.waiters.clear();
        }
    }
}

pub struct CarService {
    pub car: Arc<Car>,
}

impl PipeService for CarService {
    fn open(&self, name: &str, ctx: PipeCtx) -> Option<Box<dyn PipeHandler>> {
        if name != "qemud:car" {
            return None;
        }
        let conn = ctx.conn;
        self.car.attach(ctx);
        Some(Box::new(CarPipe { car: self.car.clone(), conn }))
    }
}

struct CarPipe {
    car: Arc<Car>,
    conn: u64,
}

impl PipeHandler for CarPipe {
    fn guest_data(&mut self, data: &[u8], _to_guest: &mut Vec<u8>) -> bool {
        self.car.guest_data(self.conn, data);
        true
    }

    fn host_msg(&mut self, msg: HostMsg, to_guest: &mut Vec<u8>) -> bool {
        match msg {
            HostMsg::Data(d) => {
                to_guest.extend_from_slice(&d);
                true
            }
            HostMsg::Close | HostMsg::Attach(_) => false,
        }
    }

    fn closed(&mut self) {
        self.car.detach(self.conn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipe::WorkerEvent;
    use sim_core::vhal::{GEAR_SELECTION, MsgType, parse_value};

    /// A guest → host frame, as the VHAL's `qemud_channel_send` writes it.
    fn guest_frame(msg: &Message) -> Vec<u8> {
        let payload = msg.encode();
        let mut out = format!("{:04x}", payload.len()).into_bytes();
        out.extend_from_slice(&payload);
        out
    }

    #[test]
    fn commands_reach_the_guest_and_replies_come_back() {
        let car = Arc::new(Car::default());
        assert_eq!(car.set(parse_value(GEAR_SELECTION, 0, "D").expect("D")), Err(NOT_CONNECTED.into()));
        assert!(CarService { car: car.clone() }.open("qemud:sensors", PipeCtx { conn: 1, events: channel().0 }).is_none());
        let (events, host) = channel();
        let mut pipe = CarService { car: car.clone() }.open("qemud:car", PipeCtx { conn: 4, events }).expect("open");
        assert!(car.connected());

        // Unsolicited values during boot are drained and remembered.
        let speed = PropValue { prop: 0x1160_0207, area_id: Some(0), float_values: vec![0.0], ..Default::default() };
        let boot = Message { msg_type: MsgType::SetPropertyAsync as i32, values: vec![speed.clone()], ..Default::default() };
        let mut out = Vec::new();
        assert!(pipe.guest_data(&guest_frame(&boot), &mut out));
        assert!(out.is_empty());
        assert_eq!(car.known(), vec![speed]);

        let host_car = car.clone();
        let setter = std::thread::spawn(move || host_car.set(parse_value(GEAR_SELECTION, 0, "drive").expect("drive")));
        let WorkerEvent::Host { conn: 4, msg: HostMsg::Data(frame) } = host.recv_timeout(Duration::from_secs(5)).expect("frame") else {
            panic!("expected data for conn 4")
        };
        assert!(pipe.host_msg(HostMsg::Data(frame.clone()), &mut out));
        assert_eq!(out, frame);
        let sent = Message::decode(&frame[4..]).expect("decode");
        assert!(sent.is(MsgType::SetPropertyCmd) && sent.values[0].int32_values == [8]);
        let reply = Message { msg_type: MsgType::SetPropertyResp as i32, status: Some(0), ..Default::default() };
        assert!(pipe.guest_data(&guest_frame(&reply), &mut Vec::new()));
        assert_eq!(setter.join().expect("join"), Ok(()));

        // Refusals come back as errors.
        let host_car = car.clone();
        let getter = std::thread::spawn(move || host_car.get(0x2140_0001, 0));
        let _ = host.recv_timeout(Duration::from_secs(5)).expect("frame");
        let refused = Message { msg_type: MsgType::GetPropertyResp as i32, status: Some(3), ..Default::default() };
        assert!(pipe.guest_data(&guest_frame(&refused), &mut Vec::new()));
        assert_eq!(getter.join().expect("join"), Err("the vehicle HAL answered ERROR_INVALID_PROPERTY".into()));

        pipe.closed();
        assert!(!car.connected());
        assert_eq!(car.request(&Message::get_property_all(), Duration::from_millis(10)), Err(NOT_CONNECTED.into()));
    }

    #[test]
    fn unanswered_commands_time_out() {
        let car = Arc::new(Car::default());
        let (events, _host) = channel();
        let _pipe = CarService { car: car.clone() }.open("qemud:car", PipeCtx { conn: 1, events }).expect("open");
        let err = car.request(&Message::get_property(GEAR_SELECTION, 0), Duration::from_millis(20)).expect_err("timeout");
        assert_eq!(err, "the vehicle HAL didn't answer within 20 ms");
    }
}
