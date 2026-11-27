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

/// A stand-in `adb` in `android`'s SDK (`platform-tools/adb`, a shell script): it logs each
/// invocation and answers the app commands the way a device does, with the texts a real
/// API 36 guest prints. Package `com.example.app` is installed with a launcher activity (and
/// third-party; `com.example.svc` launches but never has a process), `com.missing.app` isn't, `broken.apk` fails to install and `com.example.crash`
/// has an activity that doesn't start.
pub fn fake_adb(android: &Android) -> FakeAdb {
    use std::os::unix::fs::PermissionsExt;
    let dir = android.env.sdk_root.as_ref().expect("sdk root").join("platform-tools");
    std::fs::create_dir_all(&dir).expect("platform-tools");
    let adb = dir.join("adb");
    std::fs::write(&adb, FAKE_ADB).expect("adb");
    std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    FakeAdb { log: dir.join("adb.log") }
}

/// What a [`fake_adb`] was asked.
pub struct FakeAdb {
    log: std::path::PathBuf,
}

impl FakeAdb {
    /// Each invocation's arguments, in order, without the leading `-s <serial>`.
    pub fn calls(&self) -> Vec<String> {
        let text = std::fs::read_to_string(&self.log).unwrap_or_default();
        text.lines().map(|l| l.split_once(' ').and_then(|(_, rest)| rest.split_once(' ')).map_or(l, |(_, r)| r).to_owned()).collect()
    }
}

const FAKE_ADB: &str = r#"#!/bin/sh
echo "$*" >> "$(dirname "$0")/adb.log"
[ "$1" = "-s" ] && shift 2
case "$1" in
install)
  case "$*" in
    *broken.apk*) echo "Performing Streamed Install" >&2; echo "adb: failed to install broken.apk: Failure [INSTALL_PARSE_FAILED_NOT_APK: Failed to parse /data/app/vmdl1.tmp/base.apk]" >&2; exit 1;;
  esac
  echo "Performing Streamed Install"
  echo "Success";;
shell)
  case "$2" in
    "pm uninstall com.missing.app") echo "Failure [DELETE_FAILED_INTERNAL_ERROR]";;
    "pm uninstall "*) echo "Success";;
    "pm list packages -3") printf 'package:com.example.app\npackage:com.example.zeta\n';;
    "pm list packages") printf 'package:com.android.settings\npackage:com.example.zeta\npackage:android\npackage:com.example.app\n';;
    "cmd package resolve-activity --brief -c android.intent.category.LAUNCHER com.example.app")
      printf 'priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=false\ncom.example.app/.MainActivity\n';;
    "cmd package resolve-activity --brief -c android.intent.category.LAUNCHER com.example.crash")
      printf 'priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=false\ncom.example.crash/.Gone\n';;
    "cmd package resolve-activity --brief -c android.intent.category.LAUNCHER com.example.svc")
      printf 'priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=false\ncom.example.svc/.Main\n';;
    "cmd package resolve-activity"*) echo "No activity found";;
    "am start"*"com.example.crash/.Gone"*)
      printf 'Starting: Intent { act=android.intent.action.MAIN cat=[android.intent.category.LAUNCHER] cmp=com.example.crash/.Gone }\nError type 3\nError: Activity class {com.example.crash/com.example.crash.Gone} does not exist.\n';;
    "am start"*"android.intent.action.VIEW"*"nowhere://"*)
      printf 'Starting: Intent { act=android.intent.action.VIEW dat=nowhere://x }\nError: Activity not started, unable to resolve Intent { act=android.intent.action.VIEW dat=nowhere://x flg=0x10000000 }\n';;
    "am start"*"android.intent.action.VIEW"*) printf 'Starting: Intent { act=android.intent.action.VIEW dat=https://example.com/ }\n';;
    "am start"*) printf 'Starting: Intent { act=android.intent.action.MAIN cat=[android.intent.category.LAUNCHER] cmp=com.example.app/.MainActivity }\nStatus: ok\nLaunchState: COLD\nActivity: com.example.app/.MainActivity\nTotalTime: 812\nWaitTime: 820\nComplete\n';;
    "pidof -s com.example.app") echo "4321";;
    "pidof -s "*) ;;
    "am force-stop "*) ;;
    *) echo "fake adb: unhandled: $2" >&2; exit 2;;
  esac;;
*) echo "fake adb: unhandled: $*" >&2; exit 2;;
esac
"#;
