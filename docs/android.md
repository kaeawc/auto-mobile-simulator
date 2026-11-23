# The Android backend: emu-rs

sim-rs's Android devices run on emu-rs, imported from
[kaeawc/auto-mobile-emulator](https://github.com/kaeawc/auto-mobile-emulator) at `f0ccf83`. This
page is that project's README, kept for its status, provisioning, benchmark and performance notes;
paths are this repository's. How it fits the combined project: [architecture.md](architecture.md);
which Android form factors work: [form-factors.md](form-factors.md).

emu-rs is an Android emulator written from scratch in Rust for Apple silicon. It boots the SDK's
unmodified arm64 system images on Hypervisor.framework and presents the same board, ports and
control surfaces as the official emulator, so existing tooling (`adb`, `adb emu`, test
harnesses such as [AutoMobile](https://github.com/kaeawc/auto-mobile)) can drive it unchanged.
The goals are to be able to run in the official emulator's place, to be measurably faster, and
to use fewer resources.

## Status

| Area | State |
| --- | --- |
| VM core | vCPUs, the framework's in-kernel GICv3, PSCI (SMP, power off and reset), arch timer, PL011 console, PL031 RTC |
| Board | Stock memory map and virtio-mmio slots, so the unmodified fstab and first-stage init find `vda`–`vde` and their by-name paths |
| Storage | virtio-blk with indirect descriptors, event-idx, and discard/write-zeroes (hole punching). System and vendor are copy-on-write in memory; userdata, cache and metadata persist per AVD. Userdata is created by a native ext4 formatter (no `mkfs.ext4`) |
| Boot | Full Android boot to `sys.boot_completed` in about 11–20 s on a warm boot (stock, cold: 51 s); first boot formats and encrypts `/data` |
| adb | virtio-vsock with a host bridge on `console port + 1`, announced to the adb server. `adb shell`/`root`/`push` work, and the device shows up about 8 s after launch on a warm boot |
| Console | `adb emu` / telnet console with token auth: `avd name`, `ping`, `kill`, `sensor set\|get\|status`, `event send\|text`, `geo fix`, `finger touch\|remove`, `power`, `rotate`, `sms send`, `gsm call\|accept\|busy\|cancel\|hold\|list\|voice\|data\|status\|signal`, `screenshot`, plus emu-rs's `touch`, `clipboard`, `frames`, `sms list`, `xr` (Android XR) |
| Discovery | `-list-avds`, `pid_<pid>.ini` running-instance file, `emulator-<port>` serial on stdout |
| Graphics | Home screen and apps render. virtio-gpu with gfxstream contexts on the host GPU: GLES through ANGLE on Metal, Vulkan on MoltenVK (lavapipe fallback), Vulkan composition. Host-visible memory via blob mapping; goldfish address space (PCIe, shared-slot allocator) and goldfish pipe (refcounted color buffers) for the ranchu gralloc; guest `screencap` and host screenshots both work |
| Input & sensors | virtio-input touchscreen, keyboard and rotary, plus stock's dual-mode mouse on XR images. The keyboard is stock's `qwerty2` (0627:0001, stock's key set), so goldfish's `qwerty2.kl` applies: HOME, D-pad centre (`KEY_CENTER`), Recents (580), media and stem keys arrive. The console takes every stock `KEY_*` name and numeric codes. `qemud:sensors` lists the AVD's sensors by stock's rules (`hw.sensor*` flags; heart rate and wrist tilt on Wear OS API 28+; the hinge on foldables) with live values; GNSS over `qemud:gps`; goldfish battery; fingerprint; clipboard |
| Telephony | AT modem on a virtio-console `modem` port: the radio HAL registers on LTE with a loaded SIM; incoming SMS and calls from the console |
| Images | Phone API 35, 36, 37 and Wear boot (5–8 s warm; Wear gets stock's +512 MB below 1 GB). Automotive needs the vehicle HAL (`qemud:car`) and its audio HAL; Bluetooth needs a controller (in progress). Android XR (`google-xr`, `android-xr-*`, `ai-glasses`; provisioned from the `google_xr` site) gets the dual-mode mouse and `qemud:xr_service` (console `xr`), not yet booted: it also needs virtio-snd (in progress) |
| Wear OS | New AVDs: 512 MB (booted with stock's +512 MB), `hw.mainKeys=yes`, no D-pad, and for round screens `boot.prop`'s `ro.emulator.circular=true`, which reaches the guest as a `qemu-props` property and bootconfig `androidboot.emulator.circular=1`. The rotary turns 15 units (degrees) per click, like stock; the console's `EV_REL:REL_WHEEL:<n>` stays raw |
| TV | New AVDs: 2 GB, `hw.mainKeys=yes`, `hw.dPad=yes`. The D-pad keys, centre included, reach the guest through `qwerty2.kl`; not yet checked on a running TV image |
| Hardware keys | `qemu.hw.mainkeys` follows the AVD's `hw.mainKeys` (stock's default yes hides the navigation bar); phone, tablet, foldable and automotive AVDs created by emu-rs write `no` |
| AutoMobile | API 37: 38 of 44 harness tools pass; open: network shaping, gRPC audio, VM snapshots |
| Desktop app | sim-rs's window (`desktop/`), shared with iOS devices: AVD list, start/stop, live guest console, live screen with touch and form-factor controls (Back/Home/Recents/Power, rotate, hinge, rotary, D-pad) over `<avd>/emu-rs/display.sock` |

## Quick start

```bash
cargo run -p emu -- -list-avds
cargo run -p emu -- -avd <name> -show-kernel -verbose   # first boot formats and encrypts /data
adb -s emulator-5554 shell                              # once adbd is up
adb -s emulator-5554 emu kill
make shim && export ANDROID_HOME=~/.emu-rs/sdk          # make tools launch emu-rs as "emulator"
cd desktop && cargo run                                 # the desktop window (iOS and Android)
sim-rs list --platform android                          # the same AVDs through sim-rs
```

`cargo run` signs binaries with the `com.apple.security.hypervisor` entitlement
(`scripts/sign-run.sh` is the cargo runner); Hypervisor.framework refuses unsigned callers.

Flags: `-avd`/`@name`, `-port`, `-cores`, `-memory`, `-read-only` (all disk writes kept in
memory, several instances per AVD), `-wipe-data`, `-show-kernel`, `-verbose` (prints vCPU exit
counts at shutdown), `-no-boot-anim`. The rest of the SDK emulator's flags are accepted and
ignored for now.

## Layout

| Path | What |
| --- | --- |
| `crates/emu-hvf` | Safe wrappers over Hypervisor.framework: VM, vCPUs, GIC, guest RAM with zero-copy file I/O. One of the few crates allowed `unsafe` (see AGENTS.md). |
| `crates/emu-vmm` | The machine: memory map (`layout.rs`), device tree (`board.rs`), vCPU loop and PSCI (`vcpu.rs`), MMIO bus, PL011/PL031, virtio-mmio transport, blk, rng, vsock, host pipe services (adb bridge, `qemud:boot-properties`) |
| `crates/emu-avd` | AVD discovery and config (`~/.android/avd`, `config.ini`, system image), form factor (`Avd::profile`), running-instance advertisements, bootconfig encoding |
| `crates/emu-sdk` | Java-free SDK provisioning: repository manifests, verified downloads, unpacking, the on-demand renderer (`emu -provision`, `-provision-renderer`) |
| `crates/emu-ext4` | Native ext4 formatter for first-boot userdata: writes only metadata, so a fresh 10 GiB image takes under 1 MB on disk |
| `crates/emu-fdt` | Flattened device tree writer |
| `apps/emu` | The `emu` CLI: SDK-emulator-compatible flags, per-AVD instance state, console server, display server |
| `crates/sim-android` | The client front ends use: AVDs as devices, starting `emu`, the console and display-socket clients |
| `desktop/` | sim-rs's Rune window (both platforms), its own workspace |
| `docs/` | [Stock board reference](docs/reference/stock-board.md), [desktop and display design](docs/design/desktop-and-display.md), [roadmap](docs/roadmap-android.md) |
| `scripts/` | Cargo runner that signs with the hypervisor entitlement, SDK overlay (`make shim`), AutoMobile harness (`make harness-android DEVICE=emulator-5554`) |

## Benchmarks

`scripts/android-bench.sh` boots an AVD the same way under either emulator and measures it. Results
below are from one API 36 `google_apis` AVD (4 vCPU, 2 GB) on an M-series Mac, 2026-11-22. Stock
was cold-booted (`-no-snapshot-load`).

| | emu-rs | stock |
| --- | --- | --- |
| launch → `adb device` | **14.5 s** | 25.8 s |
| peak RSS (process tree) | **1.67 GB** | 5.09 GB |
| `adb shell echo` (median of 3) | **0.41 s** | 0.80 s |
| guest CPU: shell loop / 4× md5 | 0.74 s / 0.96 s | 0.72 s / 0.89 s |
| `sys.boot_completed` | not yet (needs graphics) | 51 s |

Guest CPU speed is equal, as expected: both run guest code natively. The differences are in
the VMM: no userspace GIC, fewer device threads, and RAM that is only committed when touched.

## Provisioning without Java

emu-rs needs a system image, platform-tools (adb) and, for graphics, the gfxstream renderer.
Java, sdkmanager, avdmanager, Python, e2fsprogs and the full emulator package aren't required
(userdata is formatted natively by `crates/emu-ext4`). The `emu` binary provisions the rest
itself:

```bash
emu -list-images -tag google_apis                       # stable arm64 images for this host
emu -provision 36 -create-avd small36                   # platform-tools + google_apis image + renderer + AVD
emu -provision 35 -tag google_apis_playstore -create-avd play35 -lcd 480x800@240 -cores 2 -memory 1536
emu -provision 36 -dry-run                              # resolve only: URL, size, sha1, install dir
emu -provision 36 -no-renderer                          # skip the renderer (boots without graphics)
emu -provision-renderer                                 # install or update just the renderer
emu -create-avd NAME -sysdir system-images/android-36/google_apis/arm64-v8a -lcd 720x1280@320
```

`crates/emu-sdk` reads Google's repository manifests (`repository2-3.xml`, and
`sys-img/<tag>/sys-img2-3.xml` per tag), picks the newest stable archive for macOS/arm64,
downloads it with the system's `/usr/bin/curl` (resumable, into `$ANDROID_HOME/.downloads`),
checks size and sha1, unpacks it with `/usr/bin/unzip` and moves it into the standard SDK layout.
Installed packages are skipped, manifests are cached for a day, and the archive is deleted once
unpacked. The SDK is `$ANDROID_SDK_ROOT`/`$ANDROID_HOME`, else `~/Library/Android/sdk`.

**The renderer** comes out of the SDK's `emulator` package, fetched on demand: the archive
(420.6 MB for 37.3.10) is verified like any other, but only `emulator/lib64/**` minus
`lib64/qt/**` (the stock window's Qt libraries) is unpacked, plus `LICENSE` and `NOTICE.txt`:
498 MB on disk instead of the package's 1.32 GB. That keeps everything the renderer loads: the
backend and its `@rpath` libraries, the shader translator it opens by name, `gles_angle`, and
the Vulkan loader, ICDs (MoltenVK, lavapipe with its LLVM, SwiftShader) and validation layers.
It goes to `$ANDROID_HOME/emu-rs/renderer/<revision>/` (with a `renderer.properties` marker
recording revision and sha1, and a `current` symlink to the newest), never `emulator/`, so
sdkmanager's own package tracking is never confused. `-provision` installs it unless the SDK
already has a renderer; `-provision-renderer` updates to the newest stable revision and removes
older ones.

At boot emu uses the SDK's own `emulator/lib64` when present, else the newest
`emu-rs/renderer/*/lib64`, and points gfxstream's launcher directory at its parent (where it
finds `lib64/vulkan` and `lib64/gles_*`). `emu-gfxstream` preloads the backend's `@rpath`
dependencies leaves-first from that directory, so they resolve without any rpath; libraries
gfxstream `dlopen`s by bare name resolve through the binary's rpaths (`@executable_path/lib64`,
which emu links to the chosen `lib64`, then the build machine's `emulator/lib64` and
`emu-rs/renderer/current/lib64`). To check an install without a VM:
`cargo run -p emu-gfxstream --example renderer_caps -- <lib64> moltenvk` prints capsets 3, 8, 9.

AVD defaults favour cheap, fast boots: 720×1280 screen, 2 GB RAM, 4 vCPU, 4 GB data.
Benchmarks of the stock emulator found that smaller screens boot markedly faster and that RAM
above 2 GB slows cold boots down.

## Performance notes

What has mattered so far, measured with `sample` and the `-verbose` exit counters:

- **In-kernel GICv3.** The stock board emulates a GICv2 in userspace. Here interrupt
  acknowledge, EOI and the virtual timer never leave the guest, and an idle vCPU parks inside
  the framework (zero WFI exits in practice).
- **Kick on interrupt.** A vCPU idling inside the framework doesn't notice a newly raised SPI
  until its own timer fires. Kicking vCPUs out of `hv_vcpu_run` after raising one cut
  first-boot `/data` encryption from 22 s to 8 s.
- **Plain `fsync` for guest flushes.** `std`'s `sync_data` is `F_FULLFSYNC` on macOS, which
  drains the drive cache. It was 75% of the userdata disk thread's time, and APEX
  decompression on first boot took more than 270 s; with plain `fsync` it takes about 70 s.
  Writes already sit in the host page cache, so an emulator crash loses nothing either way.
- **Zero-copy disk I/O.** `pread`/`pwrite` go straight between the image and guest RAM, one
  worker thread per disk, so the vCPU that kicked a queue goes back to the guest at once.
- **Sparse, lazily committed memory.** Guest RAM is an untouched anonymous mapping and the
  userdata image is a sparse file.
- **Discard punches holes.** virtio-blk `DISCARD`/`WRITE_ZEROES` become `F_PUNCHHOLE` on
  persistent images (an in-memory zeroed-range set on ephemeral ones), so space the guest frees
  goes back to the host: with `/data` mounted `-o discard`, writing then deleting 512 MiB grew
  the image by 525 MB and shrank it by 524 MB. A whole-disk FITRIM takes 90 ms. `/data` sits on
  dm-default-key metadata encryption, which passes discard through but not write-zeroes, and
  zeroes it writes land as ciphertext. So discard is the only way to free space, and a fresh
  first boot (about 610 MB allocated) has only about 2 MB left to trim.

## Development

```bash
cargo c && cargo lint && cargo t    # check, clippy -D warnings, tests (no hypervisor needed)
make run AVD=<name>                 # boot with the console on stdout
cd desktop && cargo test            # headless Rune view tests
```
