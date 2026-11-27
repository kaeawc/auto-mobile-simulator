# Changelog

Notable changes to simee. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and versions follow [Semantic Versioning](https://semver.org/). Add entries under
`## [Unreleased]`; `scripts/release.sh` moves them under the new version.

## [Unreleased]

### Changed

- The iPhone Duo is iOS's foldable: `form_factors` lists (iOS, foldable) as supported with the
  Duo's reference profile (2853×2007 unfolded, 1398×2034 folded, @3x), its devices and device type
  report `form_factor: foldable`, and `create --form-factor foldable` takes an iPhone Duo. Rotation,
  CarPlay's form factor check and the desktop's edge gestures treat it as the iPhone it is.

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
