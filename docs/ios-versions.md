# Supported iOS and tvOS versions

What works on each installed simulator runtime, run feature by feature against a fresh device of
each. Measured 2026-11-26 on an Apple silicon Mac (M3 Max, macOS 26.6.2) with Xcode 26.6
(17F113, CoreSimulator 1174.21). The host was heavily loaded by other work that day, so no timings
are given here; see [benchmarks.md](benchmarks.md) for those.

How: `simee create` an iPhone 16 (iOS 18), iPhone 17 (iOS 26 and 27) or iPhone Duo (the only
device type of the iOS 27.1 and `iOS-27-2-iPhone19-4` runtimes), `simee boot`, wait a minute, then
`scripts/ios-matrix.sh <udid>`, `scripts/parity.sh <udid>` and
`SIMEE_E2E_UDID=<udid> cargo test -p simee --test e2e`; frame callbacks with
`cargo run -p sim-coresim --example frame_watch -- <udid>`. The AutoMobile harness
(`scripts/automobile-harness.sh`) ran on the oldest and newest runtimes.

## iPhone

| Feature | iOS 18.0 | iOS 18.6 | iOS 26.5 | iOS 27.0 | iOS 27.1 (Duo) | iOS 27.2 | iOS 27.2 (Duo) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `parity.sh` (identity, screenshot size and pixels, apps) | pass | pass | pass | pass | pass | pass | pass |
| e2e suite | pass | pass | pass | pass | pass | pass | pass |
| screenshot | pass | pass | pass | pass | pass | pass | pass |
| tap, swipe, button, key, type | pass | pass | pass | pass | pass | pass | pass |
| rotate (orientation and 90°) | pass | pass | pass | pass | pass | pass | pass |
| frame callbacks (`Display::watch_frames`) | pass | pass | pass | pass | pass | pass | pass |
| install, launch, terminate, uninstall, apps | pass | pass | pass | pass | pass | pass | pass |
| privacy grant/revoke | pass | pass | pass | pass | pass | pass | pass |
| location, appearance, open URL, memory warning | pass | pass | pass | pass | pass | pass | pass |
| status bar, content size, contrast, biometric, keychain | pass | pass | pass | pass | pass | pass | pass |
| push | pass | pass | pass | pass¹ | pass¹ | pass¹ | pass¹ |
| exec, processes, logs, connections, containers | pass | pass | pass | pass | pass | pass | pass |
| network shaping: proxy | pass | pass | pass | pass | pass | pass | pass |
| network shaping: injected (`inject: true`) | pass | pass | pass | pass | pass | pass | pass |
| audio: state, volume, mute, capture start/stop | pass | pass | pass | pass | pass | pass | pass |
| CarPlay attach/detach | pass² | pass² | pass² | unsupported³ | unsupported³ | unsupported³ | unsupported³ |
| AutoMobile harness (getApple, observe, pressButton) | pass | — | — | — | — | pass | — |

1. In the first minute after boot `push` fails with "Simulator device failed to complete the
   requested operation" while the device's `apsd` is still starting; the same call passes once it
   has. Wait for the device to settle (`simee settle`).
2. The guest connects the car display (SpringBoard logs `Car[2-3]` at 800×480) and taps and
   screenshots reach it, but on this host it stays black on every runtime. Not yet compared with
   Simulator.app's CarPlay window on the same host.
3. `carplay` refuses iOS 27 runtimes: Apple renders CarPlay black there.

## Apple TV

| Feature | tvOS 26.5 | tvOS 27.0 |
| --- | --- | --- |
| `parity.sh` | pass | pass |
| screenshot, frame callbacks | pass | pass |
| buttons (home, arrows, select, menu, play/pause), `remote-swipe`, type, key | pass | pass |
| launch, terminate, apps, location, memory warning, processes | pass | pass |
| network shaping: proxy and injected | pass | pass |
| audio: state, capture start/stop | pass | pass |
| appearance | unsupported⁴ | unsupported⁴ |
| open URL (`https://`) | unsupported⁵ | unsupported⁵ |
| rotate | unsupported (an Apple TV doesn't rotate) | unsupported |

4. The tvOS runtime has no user interface style: `simctl ui appearance` fails the same way, and
   simee now says why ("Runtime does not support userInterfaceStyle").
5. tvOS has no web browser to open the URL; `simctl openurl` fails the same way.

## What changed in simee for these runtimes

- **Blocks carry signatures.** The screen adapter and framebuffer descriptors are ROCKit remote
  proxies under this CoreSimulator, and ROCKit throws an Objective-C exception (which aborts the
  process: Rust can't catch it) for a block without a signature. `carplay` aborted the API server
  on iOS 18.0, and `Display::watch_frames` (the desktop's frame clock) aborted on iOS 26.5. Every
  block simee hands the service now carries its signature (`rt::ObjBlock` and friends).
- **The shaper loads on iOS 18.** It linked `libdns_services`, which iOS 18 runtimes don't have:
  with `inject: true`, every process the device started afterwards, `launchctl` included, died in
  dyld, so the injection couldn't be undone without a reboot. The library is now weakly linked
  (the Network.framework resolver hook is skipped where it doesn't exist), and injection first runs
  one program with the shaper and refuses if it doesn't load.
- **The iPhone Duo's main screen.** It has two built-in framebuffers, and the inner one (black when
  folded) comes first; simee captured and touched that one. The main display is now the built-in
  framebuffer the size of the device type's main screen (`sim_core::framebuffer::main_port`).
  The inner screen isn't reachable yet. `parity.sh` asks `simctl` for the `primary` display.
- **Network shaping on Apple TV.** The shaper and the proxy tool were built for the iOS simulator
  only, and dyld refused them in a tvOS device. They are built per platform now
  (`sim_netshape::Sdk`). watchOS and visionOS devices get a clear error.
- `audio_capture_stop` no longer resolves a default device (it failed with several booted).
- `rotate` refuses devices other than iPhone and iPad instead of reporting success.
