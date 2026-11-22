# sim-rs

A from-scratch iOS simulator front end in Rust. It talks to the host's simulator service directly
to create, boot and drive simulated iOS devices, stream their screen, and inject touches, hardware
buttons and keyboard input. No stock simulator app runs: devices boot headless, and the optional
desktop window (built with [Rune](https://github.com/anomalyco/rune)) shows the live screen.

The aim is to behave exactly like Apple's simulator first, then go beyond it (scriptable everything,
headless by default, zero-copy display).

## Requirements

- macOS on Apple silicon with Xcode installed (`xcode-select -p` must point at it, or set
  `DEVELOPER_DIR`) and at least one iOS simulator runtime.
- Rust 1.99.1 (pinned in `rust-toolchain.toml`).

## CLI

```bash
cargo build
alias sim-rs=target/debug/sim-rs

sim-rs list [--booted] [--json]          # devices, grouped by runtime
sim-rs runtimes | device-types           # what can be created
sim-rs create "My Phone" "iPhone 17" "iOS 26.5"   # prints the UDID
sim-rs boot <device>                     # waits until the device finishes booting
sim-rs screenshot <device> out.png       # full-resolution PNG from the framebuffer
sim-rs tap <device> 600 1300             # pixels from the top-left of the screen
sim-rs swipe <device> 600 2000 600 900 --duration 300 [--settle]
sim-rs button <device> home|lock|side|siri|apple-pay
sim-rs type <device> 'Hello, World!'     # hardware keyboard, US layout
sim-rs key <device> 40                   # HID usage (40 = Return)
sim-rs install|uninstall|launch|terminate|apps ...
sim-rs open-url <device> https://example.com
sim-rs appearance <device> dark
sim-rs location <device> 37.33 -122.03
sim-rs privacy <device> grant photos com.example.app
sim-rs memory-warning <device>
sim-rs shutdown|erase|delete <device>
```

`<device>` is a UDID, an exact name, or `booted`.

## Debug surface and socket API

Because a simulated device's processes run on the host under their own `launchd_sim`, and its
filesystem is a host directory, sim-rs can expose far more than the stock simulator:

| Area | What you get |
| --- | --- |
| Processes | every device process with CPU, memory and uptime; signal any of them |
| Network | open sockets per process (TCP/UDP, peer, state) and bytes in/out per process |
| Storage | every app's data and bundle container with sizes; list, read, write, delete files; empty an app's data; reveal in Finder |
| Preferences | read and write any `defaults` domain inside the device |
| Logs | the device's unified log (`log show`, with predicates) |
| In-device exec | run the runtime's own tools inside the device: `defaults`, `launchctl`, `notifyutil`, `log`, `heap`, `leaks`, `vmmap`, `sample`, ... |
| Rendering | Core Animation overlays: flash updated regions, blended layers, off-screen rendering, copied and misaligned images, cache hits |
| Device state | status bar overrides, Dynamic Type size, Increase Contrast, hardware keyboard, keyboard language, Dynamic Island, backlight, keychain reset, memory warning |
| Events | push notifications, Darwin notifications (with state), Face ID / Touch ID enroll, match and no-match, location |
| Launch | launch apps with arguments and environment (e.g. `CFNETWORK_DIAGNOSTICS=3`) |

All of it is available three ways: the CLI, the desktop panel, and a JSON-lines unix socket API.

```bash
sim-rs serve                                   # $SIM_RS_SOCKET or $TMPDIR/sim-rs.sock
sim-rs call help                               # every method
sim-rs call processes '{"device":"booted"}'
sim-rs call ca_debug '{"device":"booted","option":"blended-layers","on":true}'
sim-rs call push '{"device":"booted","bundle_id":"com.example.app","payload":{"aps":{"alert":"hi"}}}'
sim-rs exec booted launchctl list              # run a program inside the device
```

The desktop window serves the same API on `$TMPDIR/sim-rs-desktop.sock`. Full reference:
[docs/api.md](docs/api.md).

## Network shaping (no root)

sim-rs shapes a device's network without root, MDM or a network extension:

```bash
sim-rs boot <device> --shaped            # or: sim-rs call network_shape '{"device":"…","inject":"boot"}'
sim-rs call network_shape '{"device":"…","preset":"3g"}'
sim-rs call network_shape '{"device":"…","latency_ms":300,"down_kbps":1500,"loss_pct":5,
  "rules":[{"pattern":"api.example.com","offline":true},{"pattern":"10.0.0.0/8","latency_ms":20}]}'
sim-rs call network_clear '{"device":"…"}'
```

- **Socket shaper (recommended):** a small dylib injected into every device process through the
  device's boot environment. It interposes socket and DNS calls and applies offline, latency,
  bandwidth and loss live from a profile file. Covers URLSession (HTTP/1.1, HTTP/2, HTTP/3/QUIC),
  WebSockets, WebKit, raw TCP/UDP and Network.framework, with per-host rules by hostname or CIDR.
- **Shaping proxy:** an HTTP/SOCKS proxy set in the device's own `configd_sim`; shapes proxy-aware
  HTTPS without decrypting it and logs every host (`network_flows`).
- **Kernel shaping:** `scripts/pf-shape.sh` (needs sudo) adds pf/dummynet pipes for the shaper's
  source-port range, for exact queueing and packet loss.

Measured coverage and limits: [docs/research/network-shaping.md](docs/research/network-shaping.md).

## Audio (no driver)

A device's processes are ordinary clients of the host's Core Audio server, so sim-rs captures or
silences exactly one device with a Core Audio process tap (macOS 14.2+), no loopback driver needed:

```bash
sim-rs call audio_capture_start '{"device":"…","path":"/tmp/device.wav","mute":true}'   # → capture_id
sim-rs call audio_capture_stop  '{"capture_id":"cap-1"}'                                 # → rms, peak, duration
sim-rs call audio_mute  '{"device":"…","on":true}'                                       # per-device null sink
sim-rs call audio_route '{"device":"…","scope":"output","uid":"BlackHole2ch_UID"}'       # any host device
```

Microphone injection still needs a loopback device. Details: [docs/research/audio-camera.md](docs/research/audio-camera.md).

## Desktop window

```bash
cd desktop
cargo run --release                     # SIM_RS_DEVICE=<udid|name|booted> preselects a device
cargo run -- --snap shot.png --scene demo   # headless render
```

Pick a device on the left (boot it if needed). Click and drag on the screen for touches; type to
use the device keyboard. ⌘⇧H is Home, ⌘L Lock, ⌘S saves a screenshot to the Desktop. Frames go to
the GPU without copying: the device's framebuffer IOSurface is sampled directly.

The panel on the right has five tabs (Device, Network, Storage, Processes, Logs) covering the
debug surface above; every control has an SVG icon (`desktop/src/icons.rs`).

## Portability

sim-rs itself is portable Rust, but the iOS runtime it drives is Apple's: iOS userland built for the
host's XNU kernel, running under a per-device `launchd_sim`, rendering through Metal into IOSurfaces.
That only runs on macOS, and Apple's license limits the runtimes to Apple hardware. What can be
portable: the desktop window (Rune runs on Linux and Windows) and the socket API, with a Mac
running `sim-rs serve` as the device host. The shared core, the API and the CLI already build and
test on Linux (CI runs them there); the macOS-only crates compile to nothing on other hosts, and
device methods report that iOS devices need a macOS host.

## Harness

[auto-mobile](https://github.com/kaeawc/auto-mobile) maps both the public and private Apple
simulator APIs and is the reference for behavior. Three layers check sim-rs against it and against
`simctl`:

| Check | What it proves |
| --- | --- |
| `cargo t` | pure logic: selection, gestures, key maps, pixel conversion |
| `SIM_RS_E2E_UDID=<udid> cargo nextest run -p sim-rs --test e2e` | sim-rs commands on a real device; identity matches `simctl` |
| `scripts/parity.sh <udid>` | identity, screenshot size and pixels, and app list match `simctl` |
| `scripts/automobile-harness.sh <udid>` | auto-mobile acquires, observes and drives a sim-rs-booted device (private daemon; touches no other daemon) |

## Layout

See [AGENTS.md](AGENTS.md#map).

## License

MIT
