# sim-rs

iOS simulator and Android emulator devices from one Rust core: one CLI, one JSON socket API and one
desktop window for both, with every device, method and control labelled with its platform and form
factor.

- **iOS** devices are driven through the host's simulator service directly (lifecycle, apps,
  framebuffer, HID input). No stock Simulator app runs: devices boot headless.
- **Android** devices run on **emu-rs**, an Android emulator written from scratch on
  Hypervisor.framework. It boots the SDK's unmodified arm64 system images and presents the stock
  emulator's board, ports and console, so `adb`, `adb emu` and harnesses such as
  [AutoMobile](https://github.com/kaeawc/auto-mobile) drive it unchanged
  ([docs/android.md](docs/android.md)).

The aim is to behave exactly like the stock tools first, then go beyond them: scriptable
everything, headless by default, fast, and easy to debug.

![The desktop window with iOS and Android devices](docs/desktop.png)

## Architecture

```text
            sim-rs CLI        desktop window (Rune)        scripts, AutoMobile
                 \                    |                        /
                  +----------- sim-api: one method table ------+   (JSON lines on a unix socket)
                  |                                            |
        iOS backend (macOS)                          Android backend
   sim-coresim: CoreSimulator, HID,          sim-android: AVDs, `emu` processes,
   IOSurface framebuffer; sim-debug,         console client, display-socket client
   sim-netshape, sim-audio                              |
                  |                           emu (one process per device): emu-vmm,
          launchd_sim per device              emu-hvf, emu-gfxstream, emu-avd, emu-sdk
                  \                                    /
                   +-------- sim-core: shared, pure ----+
     devices and selection, platforms and form factors, frames and PNG, pixel formats,
     gestures, keys and buttons (HID and Linux codes), the display stream protocol
```

Shared logic lives once, in `sim-core` and `sim-api`; each backend keeps only what is specific to
its platform. Devices of both platforms run outside the window and API processes (iOS under its own
`launchd_sim`, Android in its own `emu`), so a crashed device never takes a front end down.
Details, and the decisions behind them: [docs/architecture.md](docs/architecture.md).

## Platforms and form factors

| Form factor | Android | iOS (Apple) | Inputs |
| --- | --- | --- | --- |
| phone | Android | iOS | touch, keyboard, buttons |
| tablet | Android | iPadOS | touch, keyboard, buttons |
| foldable | Android | unsupported: no Apple device folds | touch, keyboard, buttons, hinge |
| watch | Wear OS | watchOS | touch, rotary input / Digital Crown, buttons |
| tv | Android TV | tvOS | D-pad remote / Siri Remote, keyboard |
| auto | Android Automotive | CarPlay | touch, head unit |
| vision | Android XR | visionOS | eye tracking / gaze, hand tracking / pinch, keyboard |

`sim-rs form-factors` prints this matrix (and `call form_factors` returns it with reference
screen sizes, densities, shapes and fold states). Unsupported combinations are rejected with the
reason, wherever they come up: `create`, `devices` filters, the desktop's controls. What each form
factor can do today, and what is still a gap: [docs/form-factors.md](docs/form-factors.md).

## Requirements

- macOS on Apple silicon to run devices: Xcode (`xcode-select -p`, or `DEVELOPER_DIR`) with at
  least one simulator runtime for iOS; for Android, an SDK system image (`emu -provision` fetches
  one, no Java needed).
- Rust 1.99.1 (pinned in `rust-toolchain.toml`).
- On Linux the shared core, the API, the CLI and the desktop window build and test (CI runs them
  there); there are no devices to run, since both runtimes need macOS.

## Quick start

```bash
cargo build
alias sim-rs=target/debug/sim-rs

sim-rs list                                   # every device, grouped by runtime, labelled
sim-rs list --platform android --form-factor watch
sim-rs form-factors
```

### iOS

```bash
sim-rs runtimes | device-types                # what can be created
sim-rs create "My Phone" "iPhone 17" "iOS 26.5"                                   # phone (iOS)
sim-rs create "Pad" "iPad Pro 11-inch (M4)" "iOS 26.5" --form-factor tablet       # iPadOS
sim-rs create "Watch" "Apple Watch Series 10 (46mm)" "watchOS 26.0" --form-factor watch
sim-rs create "TV" "Apple TV 4K (3rd generation)" "tvOS 26.0" --form-factor tv
sim-rs create "Vision" "Apple Vision Pro" "visionOS 26.0" --form-factor vision
sim-rs boot <device>                          # waits until the device finishes booting
sim-rs rotate Phone --orientation landscape-left   # or 90° clockwise without --orientation
sim-rs carplay Phone on                       # CarPlay display on a booted iPhone (iOS 26 runtimes)
sim-rs tap Phone 400 240 --display carplay    # ...then touch and screenshot it
sim-rs rotary Watch 3                         # the Digital Crown (--delta for raw crown units)
sim-rs watch-gesture Watch double-tap         # or wrist-flick
sim-rs button TV select                       # the Siri Remote: select, menu, home, play-pause, arrows
sim-rs remote-swipe TV left                   # its touch surface
```

`--form-factor` is checked against the device type (an iPhone is not a tablet). CarPlay is an
external display of an iPhone simulator rather than a device type: `--form-factor auto` says so,
and `carplay on` attaches it. See [docs/form-factors.md](docs/form-factors.md) for how each
Apple input is sent.

### Android

```bash
emu -provision 36 -tag google_apis            # platform-tools, the image and the renderer
emu -list-images -tag android-wear            # other image families: android-tv, google-tv,
                                              # android-automotive, ...
sim-rs create Pixel   --platform android --system-image "system-images;android-36;google_apis;arm64-v8a"
sim-rs create Tablet  --platform android --system-image "system-images;android-36;google_apis;arm64-v8a" --form-factor tablet
sim-rs create Fold    --platform android --system-image "system-images;android-36;google_apis;arm64-v8a" --form-factor foldable
sim-rs create Wear    --platform android --system-image "system-images;android-34;android-wear;arm64-v8a"
sim-rs create TV      --platform android --system-image "system-images;android-34;google-tv;arm64-v8a"
sim-rs create Car     --platform android --system-image "system-images;android-34-ext9;android-automotive;arm64-v8a"
sim-rs create XR      --platform android --system-image "system-images;android-34;google-xr;arm64-v8a"
sim-rs boot Pixel                             # starts emu; waits for sys.boot_completed
adb -s emulator-5554 shell                    # it is an ordinary emulator to every tool
sim-rs fold Fold half-opened                  # the hinge (foldables only)
sim-rs rotary Wear 3                          # the rotary input (watches only)
sim-rs xr XR recenter                         # Android XR head pose, options, pointer
sim-rs vehicle Car gear drive                 # a vehicle HAL property (Automotive only)
sim-rs console Pixel sensor get acceleration  # anything the emulator console speaks
sim-rs shutdown Pixel
```

The form factor comes from the image (Wear OS, TV and Automotive images) or `--form-factor`
(phone, tablet or foldable from a mobile image, which also sizes the screen and adds a foldable's
hinge). An image can't boot another family's form factor, and the error says which it boots.
`emu` itself stays a drop-in for the SDK's `emulator` (`emu -avd <name>`, `make shim`); see
[docs/android.md](docs/android.md).

### Both

```bash
sim-rs screenshot <device> out.png            # full-resolution PNG
sim-rs tap <device> 600 1300                  # pixels from the top-left of the screen
sim-rs swipe <device> 600 2000 600 900 --duration 300 [--settle]
sim-rs button <device> home|lock              # iOS: side|siri|apple-pay|action; Android: back|recents|
                                              # volume-up|volume-down|menu; TVs: up|down|left|right|
                                              # select|play-pause (menu|back on the Siri Remote)
sim-rs rotate <device>                        # 90° clockwise (iOS: --orientation too)
sim-rs rotary <device> 3                      # a watch's crown (Wear OS or Apple Watch)
sim-rs type <device> 'Hello, World!'          # hardware keyboard, US layout, on both platforms
sim-rs key <device> 40                        # HID usage (40 = Return)
sim-rs location <device> 37.33 -122.03
sim-rs shutdown|delete <device>
```

`<device>` is a UDID (an Android device's is its AVD name), an exact name, or `booted`.
iOS-only commands: `install|uninstall|launch|terminate|apps`, `open-url`, `appearance`, `privacy`,
`memory-warning`, `erase`, `exec`, `carplay`, `watch-gesture`, `remote-swipe`; Android-only:
`fold`, `console`, `xr`. Using one
on the other platform's device fails with the platforms it works on.

## Socket API

Everything above is a method in `sim-api`'s table, served as JSON lines on a unix socket; the CLI
and the desktop window call the same table.

```bash
sim-rs serve                                   # $SIM_RS_SOCKET or $TMPDIR/sim-rs.sock
sim-rs call help                               # every method, with the platforms it supports
sim-rs call devices '{"platform":"android"}'
sim-rs call fold '{"device":"Fold","posture":"half-opened"}'
sim-rs call ca_debug '{"device":"booted","option":"blended-layers","on":true}'
```

The desktop window serves the same API on `$TMPDIR/sim-rs-desktop.sock`. Full reference:
[docs/api.md](docs/api.md).

## iOS debug surface

Because a simulated iOS device's processes run on the host under their own `launchd_sim`, and its
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
sim-rs call processes '{"device":"booted"}'
sim-rs call push '{"device":"booted","bundle_id":"com.example.app","payload":{"aps":{"alert":"hi"}}}'
sim-rs exec booted launchctl list              # run a program inside the device
```

Android devices' equivalents go through adb (`adb shell`, logcat) and the emulator console
(`sim-rs console`); the desktop shows their guest console.

## iOS network shaping (no root)

sim-rs shapes an iOS device's network without root, MDM or a network extension:

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

## iOS audio (no driver)

An iOS device's processes are ordinary clients of the host's Core Audio server, so sim-rs captures or
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
cargo run -- --snap shot.png --scene demo   # headless render; scenes: demo, android, foldable,
                                            # watch, tv, apple-watch, apple-tv, empty
```

Devices are listed under iOS and Android, each with its sub-platform. Pick one (boot it if
needed): click and drag on the screen for touches; type to use the device keyboard. ⌘⇧H is Home,
⌘L Lock/Power, ⌘S saves a screenshot to the Desktop. The toolbar follows the form factor:

| Device | Controls |
| --- | --- |
| iPhone / iPad | Home, Lock, Rotate, Appearance |
| Apple Watch | Crown press, Side button, Crown up / down |
| Apple TV | Menu, TV, arrows and Select (the Siri Remote), Appearance |
| Apple Vision Pro | Home, Appearance |
| Android phone / tablet | Back, Home, Recents, Power, Rotate |
| Android foldable | the above, plus Fold closed / Half open / Open flat |
| Wear OS watch (round screen) | Back, Home, Power, Rotary up / down |
| Android TV | Back, Home, Power, D-pad and Select |
| Android Automotive | Back, Home |
| Android XR | Back, Home, Power, Look left / Recenter / Look right |

iOS frames go to the GPU without copying (the framebuffer IOSurface is sampled directly) and the
right-hand panel has five debug tabs (Device, Network, Storage, Processes, Logs). Android frames
stream from the emulator's display socket, and the panel shows the device and its live guest
console.

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

Android checks (Apple silicon): `cargo run -p emu -- -avd <name> -show-kernel -verbose`, then
`adb -s emulator-5554 shell getprop sys.boot_completed`;
`SIM_RS_E2E_AVD=<avd> cargo nextest run -p sim-rs --test e2e_android`;
`scripts/automobile-harness-android.sh emulator-5554` (AutoMobile's tools against emu-rs).

## Layout

See [AGENTS.md](AGENTS.md#map).

## License

MIT
