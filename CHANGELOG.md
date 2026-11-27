# Changelog

Notable changes to simee. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and versions follow [Semantic Versioning](https://semver.org/). Add entries under
`## [Unreleased]`; `scripts/release.sh` moves them under the new version.

## [Unreleased]

### Added

- simee's mark, a cut emerald, drawn from one description in `sim_core::mark` (its cut, light,
  colours and motion states): the macOS app icon (`.icns`), favicon, menu bar template, PNGs and
  SVGs in `assets/brand/` (`scripts/brand.sh` redraws them), and in the desktop window the
  activity indicator beside a running job, each device's state in the list and the empty stage.
  `scripts/desktop-app.sh` bundles the window as `simee.app` with its icon.

## [0.1.0] - 2026-11-27

The first release: iOS simulator and Android emulator devices from one Rust core, published to
crates.io as `simee` (the CLI), `simee-emu` (the Android emulator) and the `simee-*` library
crates they are built from.

### Added

- `simee`, one CLI, JSON-lines socket API (`simee serve`) and MCP server (`simee mcp`) for both
  platforms: create, boot, shut down and delete devices, install and launch apps, screenshots and
  frame streams, touch, keys, buttons and text input, rotation and form factors (phones, tablets,
  foldables, watches, TVs, cars, XR), network shaping without root, per-device audio capture,
  processes, sockets, app containers and preferences, and memory tools.
- iOS: devices driven through the host's simulator service directly, headless, with no
  Simulator.app window; Xcode 26 and 27.
- Android: `emu`, simee's own emulator on Hypervisor.framework (Apple silicon) that boots
  unmodified SDK system images with the stock board, ports, console and `adb`, provisions images
  without Java (`emu -provision`), and runs on Linux in software mode (QEMU's TCG with emu's
  devices) where there is no KVM.
- A desktop window (Rune) for live, interactive devices of both platforms (built from `desktop/`,
  not published to crates.io).
