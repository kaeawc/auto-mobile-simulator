//! A stand-in for a running simee, for tests here and in crates that drive Android devices
//! (feature `testing`): an advertisement naming this process, a console that records commands
//! and answers like simee, and a display socket that records input lines.
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

use sim_core::frame::Frame;

use crate::Android;

/// What a fake emulator received.
pub struct FakeEmulator {
    /// Console commands, in order.
    pub console: Arc<Mutex<Vec<String>>>,
    /// Lines written to the display socket (touches, keys, wheel), in order.
    pub display: Arc<Mutex<Vec<String>>>,
    /// What `fold status` answers before its `OK` (simee's hinge report); None: a bare `OK`.
    pub fold_status: Arc<Mutex<Option<String>>>,
    /// What simee's `rotation` answers: degrees the guest rotated its display (0 at first).
    pub rotation: Arc<Mutex<u32>>,
}

/// Starts a fake emulator for AVD `avd` and advertises it in `android`'s running dir, so the AVD
/// lists as booted. `screenshot <path>` writes a 2x1 PNG; `car get`/`car list` answer with
/// vehicle HAL readings.
pub fn fake_emulator(android: &Android, avd: &str) -> FakeEmulator {
    fake(android, avd, false)
}

/// [`fake_emulator`] as the stock emulator: advertised without `emulator=simee`, and simee's own
/// console commands (`fold status`) get stock's refusal.
pub fn fake_stock_emulator(android: &Android, avd: &str) -> FakeEmulator {
    fake(android, avd, true)
}

fn fake(android: &Android, avd: &str, stock: bool) -> FakeEmulator {
    let console = Arc::new(Mutex::new(Vec::new()));
    let fold_status: Arc<Mutex<Option<String>>> = Arc::default();
    let status = fold_status.clone();
    let rotation = Arc::new(Mutex::new(0));
    let turned = rotation.clone();
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("console");
    let port = listener.local_addr().expect("addr").port();
    let log = console.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut out = stream.try_clone().expect("clone");
            let _ = out.write_all(b"Android Console: type 'help' for a list of commands\r\nOK\r\n");
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                let line = line.trim().to_owned();
                if line == "quit" {
                    break;
                }
                let reply = match line.split_whitespace().collect::<Vec<_>>().as_slice() {
                    // Authentication is the session's, not a command: answered, never recorded
                    // (clients send it whenever `~/.emulator_console_auth_token` exists).
                    ["auth", ..] => {
                        let _ = out.write_all(b"Android Console: type 'help' for a list of commands\r\nOK\r\n");
                        continue;
                    }
                    ["ping"] => "I am alive!\r\nOK".to_owned(),
                    ["frames"] => "3\r\nOK".to_owned(),
                    ["rotation"] if stock => "KO: unknown command, try 'help'".to_owned(),
                    ["rotation"] => format!("rotation: {}\r\nOK", turned.lock().expect("rotation")),
                    ["fold", "status"] if stock => "KO: Usage: \"fold\"".to_owned(),
                    ["fold", "status"] => match status.lock().expect("status").as_deref() {
                        Some(lines) => format!("{lines}\r\nOK"),
                        None => "OK".to_owned(),
                    },
                    ["car", "get", ..] => "property=GEAR_SELECTION id=0x11400400 area=0 status=available value=8\r\nOK".to_owned(),
                    ["car", "list"] => "property=GEAR_SELECTION id=0x11400400 area=0 status=available value=8\r\n\
                                        property=INFO_MAKE id=0x11100101 area=0 status=available value=Toy Car\r\nOK"
                        .to_owned(),
                    ["screenshot", path] => {
                        let frame = Frame { width: 2, height: 1, rgba: vec![9, 9, 9, 255, 8, 8, 8, 255] };
                        std::fs::write(path, sim_core::frame::encode_png(&frame).expect("png")).expect("write png");
                        format!("{path} (2x1, frame 3)\r\nOK")
                    }
                    _ => "OK".to_owned(),
                };
                log.lock().expect("log").push(line);
                let _ = out.write_all(format!("{reply}\r\n").as_bytes());
            }
        }
    });
    let display = Arc::new(Mutex::new(Vec::new()));
    let dir = android.env.avd_home.join(format!("{avd}.avd/simee"));
    std::fs::create_dir_all(&dir).expect("simee dir");
    let socket = std::os::unix::net::UnixListener::bind(dir.join("display.sock")).expect("display");
    let lines = display.clone();
    std::thread::spawn(move || {
        for conn in socket.incoming().flatten() {
            let lines = lines.clone();
            std::thread::spawn(move || {
                for l in BufReader::new(conn).lines().map_while(Result::ok) {
                    lines.lock().expect("lines").push(l);
                }
            });
        }
    });
    let running = android.running_dir.clone().expect("running dir");
    std::fs::create_dir_all(&running).expect("running");
    let emulator = if stock { "" } else { "emulator=simee\n" };
    let ad = format!("port.serial={port}\nport.adb={}\navd.name={avd}\n{emulator}", port + 1);
    std::fs::write(running.join(format!("pid_{}.ini", std::process::id())), ad).expect("advertise");
    FakeEmulator { console, display, fold_status, rotation }
}

/// `lines` once it holds `n` entries (or after 5 s, whatever it holds).
pub fn wait_for(lines: &Mutex<Vec<String>>, n: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while lines.lock().expect("lines").len() < n && Instant::now() < deadline {
        sleep(Duration::from_millis(10));
    }
    lines.lock().expect("lines").clone()
}
