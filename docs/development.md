# Development

## Requirements

- macOS on Apple silicon to run devices: Xcode (`xcode-select -p`, or `DEVELOPER_DIR`) with at
  least one simulator runtime for iOS; for Android, an SDK system image (`emu -provision` fetches
  one, no Java needed).
- Rust 1.99.1 (pinned in `rust-toolchain.toml`).
- On Linux the shared core, the API, the CLI and the desktop window build and test (CI runs them
  there); there are no devices to run, since both runtimes need macOS.


Build and check commands, the working rules and the code map are in [AGENTS.md](../AGENTS.md).

## Harness

[auto-mobile](https://github.com/kaeawc/auto-mobile) maps both the public and private Apple
simulator APIs and is the reference for behavior. Three layers check simee against it and against
`simctl`:

| Check | What it proves |
| --- | --- |
| `cargo t` | pure logic: selection, gestures, key maps, pixel conversion |
| `SIMEE_E2E_UDID=<udid> cargo nextest run -p simee --test e2e` | simee commands on a real device; identity matches `simctl` |
| `scripts/parity.sh <udid>` | identity, screenshot size and pixels, and app list match `simctl` |
| `scripts/automobile-harness.sh <udid>` | auto-mobile acquires, observes and drives a simee-booted device (private daemon; touches no other daemon) |

Android checks (Apple silicon): `cargo run -p emu -- -avd <name> -show-kernel -verbose`, then
`adb -s emulator-5554 shell getprop sys.boot_completed`;
`SIMEE_E2E_AVD=<avd> cargo nextest run -p simee --test e2e_android`;
`scripts/automobile-harness-android.sh emulator-5554` (AutoMobile's tools against emu).

Many devices of both platforms at once: `scripts/automobile-farm.sh <udid> emulator-5554 emulator-5556`
runs each harness through one private AutoMobile daemon, one session per device.

