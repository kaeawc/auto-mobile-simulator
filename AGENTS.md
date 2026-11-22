# sim-rs

An iOS simulator front end written from scratch in Rust: it drives the host's simulator service
directly (device lifecycle, apps, framebuffer, HID input) with no dependency on the stock simulator
app. The goal is to match its behavior exactly and then exceed it.

## Working rules

- New dependencies go in root `[workspace.dependencies]` with `default-features = false` and only
  the features used; crates reference them with `{ workspace = true }`.
- `unsafe` lives only in `crates/sim-coresim` (the Objective-C / IOSurface boundary) and
  `desktop/src/surface.rs` (the zero-copy bridge). Every `unsafe` block carries a `// SAFETY:`
  comment (`clippy::undocumented_unsafe_blocks` is denied there). Everything else forbids it.
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
- Pure logic (device selection, gestures, key maps, pixel conversion, geometry) goes in
  `crates/sim-core` with unit tests; FFI crates stay thin.
- Doctests are disabled; don't rely on them for coverage.
- `desktop/` is a separate workspace (Rune is a large git dependency). Bump the Rune `rev` and every
  `[patch.crates-io]` entry together, and commit `desktop/Cargo.lock`.

## Build & validate

```bash
cargo c                      # check --all-targets
cargo t                      # nextest run (or: cargo test)
cargo lint                   # clippy --all-targets -D warnings
cargo fmt --all
cd desktop && cargo test     # Rune headless view tests
```

After any change: `cargo c && cargo lint && cargo t`.

Against a real device (see "Harness" in README.md):

```bash
SIM_RS_E2E_UDID=<udid> cargo nextest run -p sim-rs --test e2e
scripts/parity.sh <udid>                 # sim-rs vs simctl
scripts/automobile-harness.sh <udid>     # auto-mobile drives a sim-rs device
```

## Map

- `crates/sim-core` — device/runtime models and selection, privacy service names, geometry,
  gesture planning, HID usages and buttons, BGRA→RGBA conversion. No FFI.
- `crates/sim-coresim` — the service boundary: `Context` (service + device set), `Device`
  (lifecycle, apps, settings), `Display` (framebuffer surface, capture), `Hid` (touch, buttons,
  keyboard). `rt.rs` holds the Objective-C runtime helpers.
- `crates/sim-debug` — host-side introspection (no FFI): device process tree (`ps`), sockets
  (`lsof`), traffic (`nettop`), containers and files (confined to the device data dir), crash reports.
- `crates/sim-netshape` — no-root network shaping: `native/netshape.c` (the injected socket/DNS
  shaper, built on demand with Xcode's simulator clang and cached by source hash),
  `native/scproxy.m` (sets the device proxy in its `configd_sim`), `proxy.rs` (shaping HTTP/SOCKS
  proxy), `Profile`/`Rule` (the live profile file format).
- `crates/sim-audio` — driverless per-device audio: `native/simaudio.swift` (process-tap helper,
  built on demand), capture/mute sessions, route and volume via the device's `audiosettings.plist`.
- `crates/sim-api` — the JSON-lines unix socket API: `METHODS` table and dispatcher
  (`methods.rs`), server and client (`lib.rs`). The CLI, the socket and the desktop panel all go
  through it; add a capability here once and every surface gets it.
- `apps/sim-rs` — CLI; `tests/e2e.rs` runs against a booted device when `SIM_RS_E2E_UDID` is set.
- `desktop/` — Rune app: `device.rs` (live device, capture thread), `surface.rs` (zero-copy
  IOSurface bridge), `debug.rs` (debug panel: tabs, API worker thread, views), `icons.rs` (SVG
  icons), `keys.rs` (host keys → HID), `view.rs`, `main.rs` (state, actions, poll, tests).
  Headless `--snap` needs a debug build (release builds lack Rune's tooling feature).
- `scripts/` — `parity.sh`, `automobile-harness.sh`, `pf-shape.sh` (pf/dummynet, needs sudo).
- `docs/research/` — `network-shaping.md`, `audio-camera.md`; `docs/automobile-tool-matrix.md`.
