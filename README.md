# simee

iOS simulators and Android emulators without Simulator.app or the SDK emulator: one Rust core, one
CLI, one JSON socket API and one desktop window for both platforms. Your existing tools keep
working, because each device looks like the stock one: `adb`, `adb emu`, `simctl` and test
harnesses drive it unchanged.

![The desktop window with iOS and Android devices](docs/desktop.png)

## Compared with the stock tools

### Android: emu against the SDK emulator

The same API 36 `google_apis` AVD (4 vCPU, 2 GB) under both, on one M3 Max, 2026-11-24, median
of 3 boots each. A cold boot is a new AVD's first; a warm boot is any later one, without a snapshot.

| | emu | SDK emulator |
| --- | --- | --- |
| Cold boot, until `sys.boot_completed` | **9.5 s** | 22.9 s |
| Warm boot, until `sys.boot_completed` | **3.5 s** | 17.1 s |
| Warm boot, until `adb` sees the device | **1.6 s** | 8.5 s |
| Peak memory, whole process tree, warm | **2.09 GB** | 3.78 GB |
| Java needed to provision an image | no | yes (`sdkmanager`) |

The SDK emulator restores a snapshot in 2.8 s; emu has no snapshots yet, and its warm boot is
within a second of that restore.

Guest code runs natively under both. The gap is the emulator itself: an
in-kernel interrupt controller, zero-copy disk and adb paths, and memory that is committed only
when the guest touches it.

### iOS: simee against Simulator.app and `simctl`

| | simee | Simulator.app and `simctl` |
| --- | --- | --- |
| Booting a device | headless, no app window | opens Simulator.app, or `simctl boot` with no screen |
| Touch, keys, buttons, Digital Crown, Siri Remote | CLI and API | by hand in the window |
| Network conditions | per device, per host, live, no root | Network Link Conditioner: whole Mac, needs install |
| Audio | capture or mute one device, no driver | host output only |
| Processes, sockets, traffic per process | listed per device, signalable | Activity Monitor, across the whole Mac |
| App containers and preferences | browse, read, write, clear | find the path yourself |
| Core Animation overlays, memory warning, Face ID | scriptable | Debug menu, by hand |
| CarPlay display | attach, touch, screenshot | window only |

Every row on the left is a CLI command, an API method and a control in the desktop window.

How each number was measured, and what has not been measured yet:
[docs/benchmarks.md](docs/benchmarks.md). The full comparison:
[docs/comparison.md](docs/comparison.md).

## Quick start

Needs macOS on Apple silicon, Rust (pinned in `rust-toolchain.toml`) and, for iOS, Xcode with a
simulator runtime.

```bash
cargo build && alias simee=target/debug/simee

simee list                                   # every iOS and Android device
simee boot "iPhone 17"                       # headless; waits until it has booted
simee screenshot booted out.png
simee tap booted 600 1300
```

Android, with no Android Studio and no Java:

```bash
emu -provision 36 -tag google_apis            # platform-tools, the system image, the renderer
simee create Pixel --platform android --system-image "system-images;android-36;google_apis;arm64-v8a"
simee boot Pixel
adb -s emulator-5554 shell                    # an ordinary emulator to every tool
```

To put emu behind tools that launch `emulator` themselves: `make shim`, then
`export ANDROID_HOME=~/.simee/sdk`.

## What it covers

Phones, tablets, foldables, watches, TVs, cars and headsets on both platforms, each with its own
inputs (hinge, rotary, D-pad, Digital Crown, Siri Remote, vehicle properties, XR head pose).
Unsupported combinations are refused with the reason. The matrix and its gaps:
[docs/form-factors.md](docs/form-factors.md).

## Not yet

- Apple silicon Macs only. Linux builds and tests everything but cannot run devices.
- No Android snapshots, so every boot is a full boot.
- AutoMobile's Android harness: 38 of 41 tools pass (2026-11-24). Open: network shaping and VM
  snapshots.
- Automotive and Android XR images have their devices implemented but have not been booted.
- iOS microphone injection still needs a loopback audio device.
- Android guests reach the internet over IPv4 only (TCP, UDP, ping); no IPv6 or inbound port
  forwarding yet. IP fragments the guest sends are dropped, and DNS goes to the host's first
  resolver, not to a VPN's scoped one.

## Documentation

| | |
| --- | --- |
| [Comparison](docs/comparison.md) | what differs from the stock tools, and why |
| [Benchmarks](docs/benchmarks.md) | every published number and the command behind it |
| [CLI](docs/cli.md) and [API](docs/api.md) | commands and methods for both platforms |
| [Debugging](docs/debugging.md) | the iOS debug surface, network shaping, audio, Android counters |
| [Desktop window](docs/desktop.md) | the window, its shortcuts and per-device controls |
| [Android backend](docs/android.md) | emu status, provisioning, performance notes |
| [Architecture](docs/architecture.md) | layers, process model, design decisions |
| [Development](docs/development.md) | requirements, checks against real devices |

## License

MIT
