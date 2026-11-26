# simee

iOS simulator and Android emulator devices from one Rust core. The iOS backend drives the host's
simulator service directly (device lifecycle, apps, framebuffer, HID input) with no stock simulator
app; the Android backend is `emu`, simee's own Android emulator on Hypervisor.framework that boots
unmodified SDK system images and matches the stock emulator's board, ports and console. The goal
for both is to match the stock tools exactly and then exceed them.

## Working rules

- New dependencies go in root `[workspace.dependencies]` with `default-features = false` and only
  the features used; crates reference them with `{ workspace = true }`.
- `unsafe` lives only in `crates/sim-coresim` (the Objective-C / IOSurface boundary),
  `crates/emu-hvf` (Hypervisor.framework and guest RAM), `crates/emu-gfxstream` (the renderer),
  `apps/emu`'s `shutdown_signals` (signal masks) and `desktop/src/surface.rs` (the zero-copy
  bridge). Every `unsafe` block carries a `// SAFETY:` comment
  (`clippy::undocumented_unsafe_blocks` is denied or warned there). Everything else forbids it.
- Private service APIs are reached by selector/symbol lookup at runtime and checked with
  `respondsToSelector:` / `dlsym` first, so a missing API is an `Error::Unavailable`, never a crash.
  Objects returned by I/O ports are remote proxies: call them through `rt::send_*` helpers, not
  `msg_send!` (whose debug signature check rejects proxied methods).
- In-device programs (`Device::exec`/`spawn`) run in the device's launchd namespace; their path
  must be prefixed with the runtime root (`Device::runtime_root`), or the host's binary runs.
- The injected shaper runs inside every process of a boot-injected device (SpringBoard included):
  keep it allocation-free on hot paths, never block in `close`/`read`/`write` for non-socket fds,
  and guard nested libSystem calls with `g_depth`. Rebuilding it changes its cached path, and
  `network_shape {inject:"boot"}` reboots devices still running an older build.
- Android: the guest-visible layout follows the stock board (`crates/emu-vmm/src/layout.rs`,
  `docs/reference/stock-board.md`): images hard-code device addresses (fstab, boot_devices).
  Change it only with evidence from a running stock emulator.
- Android: measure before optimizing. `emu -verbose` prints per-vCPU exit counts; `sample <pid> 5`
  shows host time per thread. Record results that change a design decision in docs/android.md.
- Android: never modify the stock emulator's files in an AVD directory; simee state lives in
  `<avd>/simee/` (console.log, emu.log, display.sock, `frames-<pid>-<n>.ring` frame rings).
- Android: one emulator process per device (Hypervisor.framework allows one VM per process). Start
  guests with a recorded PID and stop them with `kill -INT <pid>`, `adb -s emulator-<port> emu kill`
  or `simee shutdown <avd>`; don't kill by name (other emulators may be running).
- Front ends drive Android devices the way any tool can: the console port (stock text protocol)
  and `<avd>/simee/display.sock` (`sim_core::stream`). `sim-android` is that client; it never
  links the VMM.
- Pure logic (device selection, gestures, key maps, pixel conversion, frames and PNG encoding,
  geometry) goes in `crates/sim-core` with unit tests; FFI crates stay thin.
- macOS-only crates gate themselves with a crate-level `#![cfg(target_os = "macos")]` and
  target-specific dependencies, so the shared core, the API and the CLI build and test on Linux.
  Callers gate their use the same way (`sim-api`'s `ios.rs` vs `ios_unavailable.rs`).
- The CLI is a thin front over `sim-api`'s method table: a new device command is an API method
  first, then a CLI subcommand that calls it.
- Every device, API method and UI element says which platform it belongs to: devices carry
  `platform`, `form_factor` and `sub_platform`; `help` lists each method's platforms. Validate a
  (platform, form factor) pair with `Kind::new`, which explains unsupported combinations.
- Doctests are disabled; don't rely on them for coverage.
- `desktop/` is a separate workspace (Rune is a large git dependency). Bump the Rune `rev` and every
  `[patch.crates-io]` entry together, and commit `desktop/Cargo.lock`.

## Benchmarks and public docs

The README and `docs/comparison.md` are how people decide whether to adopt this project. They
argue with measured numbers against the stock tools, so keep the numbers current:

- `docs/benchmarks.md` is the one source of numbers. Every row has the value for this project,
  the value for the stock tool where one exists, the date, the machine, and the command that
  reproduces it. The README and `docs/comparison.md` quote rows from it and never carry a number
  it doesn't have.
- A change that moves a published number updates its row in the same PR. A performance PR with
  no row to update adds one, or says in its description why the win isn't user-visible.
- Compare like with like: the same AVD or device type, host and day, both tools cold or both
  warm, median of at least 3 runs. Say what was run (`scripts/android-bench.sh`, `hyperfine`, a
  `--ignored` bench). A number measured on a different machine (a Linux VM bench) is labelled so.
- Never publish an estimate, a number from memory, or a comparison that wasn't run. An
  unmeasured claim goes under "Not yet measured" in `docs/benchmarks.md` until it is.
- Re-run the stock comparison when the README's headline rows are more than about a month old,
  or when the stock tools ship a new release, and update the date.
- Lead with what a user feels (time to a usable device, memory per device, time per automation
  step, what can be inspected), then link to the mechanism (`docs/android.md` "Performance
  notes"). State limits plainly in "Not yet": a reader who hits an undisclosed gap leaves.
- Keep the README short: what it is, the comparison, a quick start, links. Reference material
  lives in `docs/`. Write specific claims in plain sentences; no slogans, no superlatives
  without a number beside them.

## Build & validate

```bash
cargo c                      # check --all-targets
cargo t                      # nextest run (or: cargo test)
cargo lint                   # clippy --all-targets -D warnings
cargo fmt --all
cd desktop && cargo test     # Rune headless view tests
```

After any change: `cargo c && cargo lint && cargo t`. Before pushing: `scripts/check.sh` (CI's
checks, fastest first: fmt, clippy for this host and for the other platform's target, the tests,
the desktop when it changed; a few seconds warm). `scripts/install-hooks.sh` makes it the
pre-push hook. Coverage: `cargo llvm-cov nextest --workspace --summary-only` (CI's Linux job
fails under its `--fail-under-lines` floor; raise the floor with coverage, never lower it). On a non-macOS host, also lint the macOS
code (no linking needed): `rustup target add aarch64-apple-darwin` once, then
`cargo clippy --target aarch64-apple-darwin --all-targets -- -D warnings`.

Unit tests never need a device or the hypervisor. Against a real device (see
docs/development.md):

```bash
SIMEE_E2E_UDID=<udid> cargo nextest run -p simee --test e2e
scripts/parity.sh <udid>                 # simee vs simctl
scripts/automobile-harness.sh <udid>     # auto-mobile drives a simee device
# Android (Apple silicon; cargo signs emu with the hypervisor entitlement via scripts/sign-run.sh)
cargo run -p emu -- -avd <name> -show-kernel -verbose
adb -s emulator-5554 shell getprop sys.boot_completed      # after VMM changes, boot an AVD and check
SIMEE_E2E_AVD=<avd> cargo nextest run -p simee --test e2e_android
scripts/automobile-harness-android.sh emulator-5554
# Many devices, both platforms: one private AutoMobile daemon, one session per device
scripts/automobile-farm.sh <udid> emulator-5554 emulator-5556
```

## Map

- `crates/sim-core` — `platform.rs` (Android vs iOS, Apple and Android sub-platforms),
  `form_factor.rs` (phone…vision, the support matrix and its reasons, reference profiles with
  screen, density, shape, fold and inputs, inference from Apple device types and Android AVD
  configs), device/runtime models (labelled with platform and form factor) and selection,
  privacy service names, geometry,
  gesture planning, HID usages and buttons (per form factor: the Siri Remote, crown steps, watch
  gestures, orientations and the Purple orientation message), `carplay.rs` (StarkConfig, runtime
  gate), `goldfish.rs` (stock emulator key names, the `qwerty2` key set, sensor masks), `xr.rs`
  (Android XR commands and the `xr_service` messages), `vhal.rs` (the vehicle HAL's
  `EmulatorMessage` codec, property names and values), `network.rs` (the stock console's
  `network speed|delay` presets, parsing and `network status` report), BGRA→RGBA conversion,
  `Frame` and PNG encoding. No FFI.
- `crates/sim-coresim` — the service boundary: `Context` (service + device set), `Device`
  (lifecycle, apps, settings, orientation, CarPlay), `Display` (framebuffer surface, capture; main
  or external), `Hid` (touch, buttons, keyboard, the Digital Crown, watch gestures, the Siri
  Remote's touch surface, CarPlay touches), `carplay.rs` (attaching the CarPlay display). `rt.rs`
  holds the Objective-C runtime helpers.
- `crates/sim-debug` — host-side introspection (no FFI): device process tree (`ps`), sockets
  (`lsof`), traffic (`nettop`), containers and files (confined to the device data dir), crash reports.
- `crates/sim-netshape` — no-root network shaping: `native/netshape.c` (the injected socket/DNS
  shaper, built on demand with Xcode's simulator clang and cached by source hash; its portable
  core is tested on the host by `native/netshape_test.c` through `tests/native.rs`),
  `native/scproxy.m` (sets the device proxy in its `configd_sim`), `proxy.rs` (shaping HTTP/SOCKS
  proxy), `Profile`/`Rule` (the live profile file format).
- `crates/sim-audio` — driverless per-device audio: `native/simaudio.swift` (process-tap helper,
  built on demand), capture/mute sessions, route and volume via the device's `audiosettings.plist`.
- `crates/sim-android` — the Android backend's client: AVDs as labelled devices, system images as
  runtimes, AVD creation shaped by form factor, starting `emu` and waiting for boot, the console
  client (`console.rs`) and the display socket client (`display.rs`). No FFI.
- `crates/emu-hvf` — `Vm` (GIC, RAM mapping, SPIs), `Vcpu` (run/regs/vtimer), `GuestRam`,
  `SharedMap` (a file mapped shared: the display server's frame ring). `GuestRam`, `SharedMap` and
  `host` (hole punching) build on any unix host; the hypervisor (`hv.rs`) is Apple silicon only.
- `crates/emu-vmm` — `machine.rs` (`run`), `layout.rs`, `board.rs` (DT), `vcpu.rs` (exits, PSCI,
  wakers), `bus.rs`, `devices/` (PL011, PL031, goldfish), `virtio/` (mmio transport, queue, blk,
  rng, vsock, gpu with `legacy_asg.rs` (API 31–33's address-space graphics on the renderer's
  protocol), input incl. XR's dual-mode mouse, console, snd), `pipe/` (adb bridge, qemud
  services incl. `car.rs`, the vehicle HAL, and `xr.rs`, Android XR's `xr_service`), `modem/`,
  `disk.rs`, `boot.rs`, `display.rs` (`FrameStore`). Everything but `machine.rs`, `vcpu.rs` and
  `virtio/gpu.rs` builds and tests on Linux: put device logic there, not behind the hypervisor.
- `crates/emu-avd` — `Env` (SDK/AVD paths), `Avd` (config, `profile()`), `NewAvd`
  (`for_profile`), running-instance advertisements (`Running`), `BootConfig`, ini parsing.
- `crates/emu-sdk` — Java-free provisioning: repository manifests, verified downloads, the
  on-demand gfxstream renderer.
- `crates/emu-gfxstream` — `stream_renderer_*` FFI; `examples/renderer_caps.rs` checks a renderer.
- `crates/emu-ext4` — the native ext4 formatter for first-boot userdata. `crates/emu-fdt` — the
  device tree writer.
- `crates/sim-api` — the JSON-lines unix socket API: `METHODS` table and dispatcher
  (`methods.rs`), parameter parsing (`params.rs`), the iOS backend's methods (`ios.rs`; on
  non-macOS hosts `ios_unavailable.rs`), the Android backend's methods (`android.rs`), server
  and client (`lib.rs`). The CLI, the socket and the
  desktop panel all go through it; add a capability here once and every surface gets it.
- `apps/simee` — CLI: argument parsing and output formatting over `sim-api`'s methods;
  `tests/e2e.rs` runs against a booted iOS device when `SIMEE_E2E_UDID` is set,
  `tests/e2e_android.rs` against a running AVD when `SIMEE_E2E_AVD` is.
- `apps/emu` — the Android emulator binary, a drop-in for the SDK's `emulator`: `args.rs` (SDK
  flags), `instance.rs` (disks, bootconfig, boot props), `console.rs` (console server,
  running-instance file; its commands build and test on any host), `display_server.rs` (frames
  and input on `display.sock`, frames through a shared ring for clients that ask; builds and
  tests on any host),
  `provision.rs` (`-provision`, `-list-images`). Boots only on Apple silicon; elsewhere it lists,
  creates and provisions AVDs.
- `desktop/` — Rune app, one window for both platforms: `device.rs` (the live iOS device facade; a
  stub where there's no simulator service, so the window builds and tests on Linux), `ios.rs`
  (live iOS device, capture thread; macOS), `surface.rs` (zero-copy IOSurface bridge; macOS),
  `android.rs` (live Android device: frames and input over `display.sock`), `controls.rs` (the
  toolbar each platform and form factor gets), `debug.rs` (debug panel: tabs, API worker thread, views), `icons.rs` (SVG
  icons), `keys.rs` (host keys → HID), `view.rs`, `main.rs` (state, actions, poll, tests).
  Headless `--snap` works in debug builds, and in release with the `tooling` feature (Rune's
  headless flags; off in the shipped build, +1.3 MB stripped):
  `cargo run --release --features tooling -- --snap /tmp/x.png --scene demo`. Scenes: demo,
  android, foldable, watch, tv, apple-watch, apple-tv, empty. On Linux it renders with Mesa's
  lavapipe.
- `scripts/` — `parity.sh`, `automobile-harness.sh`, `automobile-farm.sh` (every device through
  one private AutoMobile daemon; `automobile-daemon.sh` starts, joins and stops it, and the
  harnesses join it through `AM_DAEMON_ENV`), `pf-shape.sh` (pf/dummynet, needs sudo);
  Android: `sign-run.sh` (the cargo runner: hypervisor entitlement), `sdk-shim.sh` (SDK overlay
  whose `emulator` is simee), `automobile-harness-android.sh`, `android-bench.sh`, `android-jank.sh` (frame pacing under
  launcher swipes), `net-bench.sh` (idle CPU with the NAT and link only, throughput through the NAT),
  `inspect-image.py` (what a system image expects of the board, unpacked without booting: fstab,
  modules, input names, vendor HALs).
- `docs/research/` — `network-shaping.md`, `audio-camera.md`; `docs/automobile-tool-matrix.md`.
  `docs/architecture.md` (layers, process model, shared logic, the conflict decisions),
  `docs/form-factors.md` (support matrix, profiles, inference, launching each kind, known gaps).
  Android: `docs/android.md` (status, provisioning, benchmarks, performance notes),
  `docs/reference/stock-board.md`, `docs/design/desktop-and-display.md`, `docs/roadmap-android.md`.
