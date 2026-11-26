//! The bezel: a band [`BAND`] points wide just outside the device's screen. A drag that starts
//! there comes in from that edge of the screen, as a drag from Simulator.app's bezel does. On iOS
//! it goes through the API's `touch` with `edge` (what `swipe --edge` sends), so home, the app
//! switcher, Notification Center and Control Center open from the window. Android has no edge
//! flag: its edge gestures (gesture navigation's back from the left or right, home from the
//! bottom, a watch's dismiss from the left) are ordinary touches that start at the screen's edge,
//! so there a bezel drag is a touch that starts on the edge pixel nearest the press. Devices
//! without edge gestures ([`crate::controls::bezel`]) have no bezel.

use std::cell::OnceCell;
use std::sync::mpsc;

use rune::{Drag, Phase as DragPhase};
use serde_json::{Value, json};
use sim_api::Session;
use sim_core::gesture::{Phase, ScreenEdge};

use crate::{Input, State, work};

/// How far outside the screen, in points, a press still lands on the bezel.
pub const BAND: f32 = 24.0;

/// How a device's bezel drags reach it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A touch flagged with the edge it comes in from (the API's `touch` with `edge`).
    Ios,
    /// An ordinary touch that starts on the edge.
    Android,
}

/// The API, as the window calls it (a stand-in in tests).
pub type Call = fn(&mut Session, &str, &Value) -> Result<Value, String>;

/// What the window knows of the bezel: where the stage and the screen are, the edge of the drag
/// in progress, and the line to the API that iOS edge touches go out on.
pub struct Bezel {
    /// The stage around the screen, `[x, y, w, h]` in window points (drags arrive local to it).
    pub stage: [f32; 4],
    /// The screen, in window points.
    pub screen: [f32; 4],
    /// The edge the drag in progress came in from.
    pub edge: Option<ScreenEdge>,
    call: Call,
    line: OnceCell<Option<mpsc::Sender<Value>>>,
    outbox: work::Outbox,
}

impl Default for Bezel {
    fn default() -> Self {
        Self::with_call(sim_api::call_method)
    }
}

impl Bezel {
    pub fn with_call(call: Call) -> Self {
        Self { stage: [0.0; 4], screen: [0.0; 4], edge: None, call, line: OnceCell::new(), outbox: work::Outbox::default() }
    }

    /// Where failed edge touches are reported.
    pub fn set_outbox(&mut self, outbox: work::Outbox) {
        self.outbox = outbox;
    }

    /// Sends one sample of an iOS edge touch through the API, in order, on its own thread (the
    /// first sample starts it); a failure goes to the status line.
    pub fn send(&self, udid: &str, at: (f32, f32), phase: Phase, edge: ScreenEdge) {
        let line = self.line.get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Value>();
            let (call, outbox) = (self.call, self.outbox.clone());
            let spawned = std::thread::Builder::new().name("simee-bezel".into()).spawn(move || {
                let mut session = Session::default();
                for params in rx {
                    if let Err(e) = call(&mut session, "touch", &params) {
                        outbox.post(format!("edge swipe: {e}"));
                    }
                }
            });
            spawned.ok().map(|_| tx)
        });
        if let Some(line) = line {
            line.send(touch_params(udid, at, phase, edge)).ok();
        }
    }
}

/// The edge of `screen` (`[x, y, w, h]`) whose bezel a press at `p` (same coordinates) lands
/// on: within [`BAND`] outside the screen. A press on the screen, or farther out, is not on the
/// bezel. Outside a corner the side it is farther beyond wins, and a tie goes to the top or
/// bottom (home and Control Center are the edge gestures people reach for).
pub fn hit(screen: [f32; 4], p: (f32, f32)) -> Option<ScreenEdge> {
    let [x, y, w, h] = screen;
    let (left, right, top, bottom) = (x - p.0, p.0 - (x + w), y - p.1, p.1 - (y + h));
    let out_x = left.max(right).max(0.0);
    let out_y = top.max(bottom).max(0.0);
    if (out_x == 0.0 && out_y == 0.0) || out_x > BAND || out_y > BAND {
        return None;
    }
    Some(if out_y >= out_x {
        if top > 0.0 { ScreenEdge::Top } else { ScreenEdge::Bottom }
    } else if left > 0.0 {
        ScreenEdge::Left
    } else {
        ScreenEdge::Right
    })
}

/// `p` (window points) in the device pixels of a screen of `size` drawn at `screen`, pulled onto
/// the screen: a press on the bezel lands on the screen's edge.
pub fn to_device(screen: [f32; 4], p: (f32, f32), size: (u32, u32)) -> (f32, f32) {
    let [x, y, w, h] = screen;
    let along = |v: f32, from: f32, len: f32, px: u32| ((v - from) / len.max(1.0) * px as f32).clamp(0.0, px.saturating_sub(1) as f32);
    (along(p.0, x, w, size.0), along(p.1, y, h, size.1))
}

/// The API's `touch` for one sample of an edge touch at `at` (device pixels).
pub fn touch_params(udid: &str, at: (f32, f32), phase: Phase, edge: ScreenEdge) -> Value {
    let phase = match phase {
        Phase::Down => "down",
        Phase::Move => "move",
        Phase::Up => "up",
    };
    json!({"device": udid, "x": at.0.round(), "y": at.1.round(), "phase": phase, "edge": edge.as_str()})
}

/// The stage's drag handler: a drag that starts on the bezel queues edge touches (in device
/// pixels) until it ends; any other press on the stage does nothing.
pub fn on_drag(s: &mut State, d: &Drag) {
    let [sx, sy, ..] = s.bezel.stage;
    let p = (sx + d.x, sy + d.y);
    let edge = match d.phase {
        DragPhase::Down => {
            s.bezel.edge = hit(s.bezel.screen, p);
            s.bezel.edge
        }
        DragPhase::Move => s.bezel.edge,
        DragPhase::Up => s.bezel.edge.take(),
    };
    let Some(edge) = edge else { return };
    let (x, y) = to_device(s.bezel.screen, p, s.screen_size());
    let phase = match d.phase {
        DragPhase::Down => Phase::Down,
        DragPhase::Move => Phase::Move,
        DragPhase::Up => Phase::Up,
    };
    s.inputs.push(Input::Edge(x, y, phase, edge));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    const SCREEN: [f32; 4] = [100.0, 50.0, 400.0, 600.0];

    #[test]
    fn presses_just_outside_the_screen_are_on_its_bezel() {
        assert_eq!(hit(SCREEN, (300.0, 660.0)), Some(ScreenEdge::Bottom));
        assert_eq!(hit(SCREEN, (480.0, 40.0)), Some(ScreenEdge::Top), "Control Center's corner");
        assert_eq!(hit(SCREEN, (90.0, 300.0)), Some(ScreenEdge::Left));
        assert_eq!(hit(SCREEN, (510.0, 300.0)), Some(ScreenEdge::Right));
        assert_eq!(hit(SCREEN, (300.0, 300.0)), None, "on the screen");
        assert_eq!(hit(SCREEN, (100.0, 50.0)), None, "the screen's own corner pixel");
        assert_eq!(hit(SCREEN, (300.0, 650.0 + BAND + 1.0)), None, "beyond the bezel");
        assert_eq!(hit(SCREEN, (300.0 + 300.0, 300.0)), None, "far to the side");
        // Outside a corner: the side it is farther beyond, the top or bottom on a tie.
        assert_eq!(hit(SCREEN, (95.0, 40.0)), Some(ScreenEdge::Top));
        assert_eq!(hit(SCREEN, (80.0, 45.0)), Some(ScreenEdge::Left));
        assert_eq!(hit(SCREEN, (90.0, 660.0)), Some(ScreenEdge::Bottom));
    }

    #[test]
    fn bezel_presses_land_on_the_screens_edge_in_device_pixels() {
        let size = (2064, 2752);
        assert_eq!(to_device(SCREEN, (300.0, 670.0), size), (1032.0, 2751.0));
        assert_eq!(to_device(SCREEN, (300.0, 20.0), size), (1032.0, 0.0));
        assert_eq!(to_device(SCREEN, (90.0, 350.0), size), (0.0, 1376.0));
        assert_eq!(to_device(SCREEN, (300.0, 350.0), size), (1032.0, 1376.0));
    }

    #[test]
    fn edge_touches_are_the_apis_touch_with_edge() {
        let p = touch_params("U1", (1032.4, 2751.0), Phase::Down, ScreenEdge::Bottom);
        assert_eq!(p, json!({"device": "U1", "x": 1032.0, "y": 2751.0, "phase": "down", "edge": "bottom"}));
        assert_eq!(touch_params("U1", (5.0, 6.0), Phase::Move, ScreenEdge::Top)["phase"], "move");
        assert_eq!(touch_params("U1", (5.0, 6.0), Phase::Up, ScreenEdge::Left)["edge"], "left");
        // The method and its parameters are the API's own.
        let touch = sim_api::METHODS.iter().find(|m| m.0 == "touch").expect("touch method");
        for param in ["x", "y", "phase", "edge?"] {
            assert!(touch.1.contains(param), "{param} in {}", touch.1);
        }
    }

    static CALLS: Mutex<Vec<(String, Value)>> = Mutex::new(Vec::new());

    fn recording_call(_: &mut Session, method: &str, params: &Value) -> Result<Value, String> {
        CALLS.lock().map_err(|e| e.to_string())?.push((method.to_owned(), params.clone()));
        if params["phase"] == "up" { Err("device went away".into()) } else { Ok(Value::Null) }
    }

    #[test]
    fn samples_reach_the_api_in_order_and_failures_reach_the_status_line() {
        let mut bezel = Bezel::with_call(recording_call);
        let outbox = work::Outbox::default();
        bezel.set_outbox(outbox.clone());
        bezel.send("U1", (10.0, 2751.0), Phase::Down, ScreenEdge::Bottom);
        bezel.send("U1", (10.0, 2000.0), Phase::Move, ScreenEdge::Bottom);
        bezel.send("U1", (10.0, 1200.0), Phase::Up, ScreenEdge::Bottom);
        let deadline = Instant::now() + Duration::from_secs(5);
        while CALLS.lock().expect("calls").len() < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let calls = CALLS.lock().expect("calls").clone();
        let phases: Vec<_> = calls.iter().map(|(m, p)| (m.as_str(), p["phase"].as_str().unwrap_or_default(), p["y"].as_f64())).collect();
        assert_eq!(phases, [("touch", "down", Some(2751.0)), ("touch", "move", Some(2000.0)), ("touch", "up", Some(1200.0))]);
        assert!(calls.iter().all(|(_, p)| p["edge"] == "bottom" && p["device"] == "U1"));
        let mut lines = Vec::new();
        while lines.is_empty() && Instant::now() < deadline {
            lines = outbox.take();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(lines, ["edge swipe: device went away"]);
    }
}
