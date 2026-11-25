# Compared with the stock tools

What changes if you run devices on simee instead of Simulator.app and the Android SDK emulator.
Numbers are from [benchmarks.md](benchmarks.md).

## What stays the same

Nothing downstream has to change. An Android device on emu has the stock emulator's board,
ports, console and `emulator-<port>` serial, and boots Google's unmodified system images, so
`adb`, `adb emu`, Gradle and test harnesses treat it as the SDK emulator. An iOS device is the
same CoreSimulator device `simctl` and Xcode see; simee is another client of the same service.
You can mix the two: boot with simee, install with Xcode.

## Android

### Faster to a usable device, in less memory

| | emu | SDK emulator |
| --- | --- | --- |
| Cold boot, until `sys.boot_completed` | 9.5 s | 22.9 s |
| Warm boot, until `sys.boot_completed` | 3.5 s | 17.1 s |
| Peak memory, process tree, cold / warm | 3.76 / 2.09 GB | 4.99 / 3.78 GB |
| Janky frames swiping the launcher, Pixel Tablet / phone | 0% / 0.25% | 5.6% / 6.2% |

Medians of 3 boots of one API 36 AVD; the setup is in [benchmarks.md](benchmarks.md).

Guest code runs at the same speed under both, since both execute it natively on the CPU. The
difference is the work the emulator does around the guest:

- **Interrupts stay in the kernel.** The stock board emulates its interrupt controller in
  userspace. emu uses Hypervisor.framework's, so acknowledging an interrupt or reading the
  timer never leaves the guest, and an idle vCPU parks in the kernel.
- **Disk and adb bytes are not copied.** Disk reads and writes go straight between the image
  file and guest memory. adb traffic is written from the host socket's buffer into the guest's.
- **Memory is committed when touched.** Guest RAM and the userdata image are sparse, and space
  the guest frees is returned to the host.
- **A stalled client stalls only itself.** An adb client that stops reading cannot freeze
  sensors, GPS or other adb sessions. The stock emulator buffers without limit in both directions.

### Easier to set up and to run headless

- `emu -provision 36` downloads platform-tools, a system image and the renderer. No Java, no
  Android Studio, no `sdkmanager`.
- One process per device, no window required. State is plain files in `<avd>/simee/`.
- `-read-only` runs several instances of one AVD with all writes kept in memory.

### Easier to see inside

- `emu -verbose` prints per-vCPU exit counts at shutdown; `EMU_FRAME_STATS=1` prints frame
  timings per stream.
- The guest console is a log file and a live pane in the desktop window.
- The console adds `touch`, `clipboard`, `frames` and `sms list` to the stock commands.

## iOS

Simulator.app is a window you operate by hand. `simctl` scripts the lifecycle and little else.
simee talks to the simulator service directly, and because a simulated device's processes and
files live on the host, it can reach things neither stock tool exposes.

| You want to | simee | Stock |
| --- | --- | --- |
| Run devices on CI with no window | boots headless | `simctl boot`, then no way to see or touch the screen |
| Tap, swipe, type, press buttons from a script | `tap`, `swipe`, `type`, `button`, `rotary` | not in `simctl`; UI tests or AppleScript |
| Test on a slow or broken network | per device and per host, changed live, no root | Network Link Conditioner: the whole Mac |
| Take one host offline, leave the rest | a rule by hostname or CIDR | not available |
| Record or silence one device's audio | `audio_capture_start`, `audio_mute` | not available |
| See what a device is running and talking to | processes, sockets and bytes per process | Activity Monitor, across the whole Mac |
| Inspect or reset an app's data | list, read, write and clear containers and `defaults` | locate the container path, then Finder |
| Turn on rendering overlays | `ca_debug` from a script | Debug menu, by hand |
| Send a push, a memory warning, a Face ID match | one call each | `simctl push`; the rest by menu |
| Drive a CarPlay screen | attach, touch, screenshot | a window |
| Run a tool inside the device | `exec` with the runtime's own `heap`, `leaks`, `vmmap`, `sample` | `simctl spawn` |

Details and examples for each row: [debugging.md](debugging.md).

## One surface for both platforms

The CLI, the JSON socket API and the desktop window share one method table. A command that
exists on both platforms has the same name and arguments on both (`screenshot`, `tap`, `swipe`,
`type`, `button`, `location`), and every device and method says which platform and form factor
it belongs to. A script written against an iPhone runs against a Pixel with a different device
name and that screen's coordinates.

## Where the stock tools are still ahead

- Android snapshots: the SDK emulator restores a saved state in seconds. emu always boots.
- Hosts: the SDK emulator runs on Intel Macs, Linux and Windows. emu needs Apple silicon.
- Android coverage: 38 of 44 AutoMobile harness tools pass on API 37. Automotive and XR images
  have not been booted.
- iOS graphics debugging that lives in Xcode (view debugger, Instruments) is unchanged: use
  Xcode for it, against the same device.
