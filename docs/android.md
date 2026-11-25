# The Android backend: emu

simee's Android devices run on `emu`, its own Android emulator (called emu-rs before the project
was renamed simee), imported from
[kaeawc/auto-mobile-emulator](https://github.com/kaeawc/auto-mobile-emulator) at `f0ccf83`. This
page is that project's README, kept for its status, provisioning, benchmark and performance notes;
paths are this repository's. How it fits the combined project: [architecture.md](architecture.md);
which Android form factors work: [form-factors.md](form-factors.md).

simee is an Android emulator written from scratch in Rust for Apple silicon. It boots the SDK's
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
| Boot | Full Android boot to `sys.boot_completed`: 9.5 s cold, 3.5 s warm (stock: 22.9 s and 17.1 s; [benchmarks](benchmarks.md)); first boot formats and encrypts `/data` |
| adb | virtio-vsock with a host bridge on `console port + 1`, announced to the adb server. `adb shell`/`root`/`push` work, and the device shows up about 8 s after launch on a warm boot |
| Console | `adb emu` / telnet console with token auth: `avd name`, `ping`, `kill`, `sensor set\|get\|status`, `event send\|text`, `geo fix`, `finger touch\|remove`, `power`, `rotate`, `sms send`, `gsm call\|accept\|busy\|cancel\|hold\|list\|voice\|data\|status\|signal`, `screenshot`, plus simee's `touch`, `clipboard`, `frames`, `sms list`, `xr` (Android XR), `car` (Automotive) |
| Discovery | `-list-avds`, `pid_<pid>.ini` running-instance file, `emulator-<port>` serial on stdout |
| Graphics | Home screen and apps render. virtio-gpu with gfxstream contexts on the host GPU: GLES through ANGLE on Metal, Vulkan on MoltenVK (lavapipe fallback), Vulkan composition. Host-visible memory via blob mapping; goldfish address space (PCIe, shared-slot allocator) and goldfish pipe (refcounted color buffers) for the ranchu gralloc; guest `screencap` and host screenshots both work |
| Input & sensors | virtio-input touchscreen, keyboard and rotary, plus stock's dual-mode mouse on XR images. The keyboard is stock's `qwerty2` (0627:0001, stock's key set), so goldfish's `qwerty2.kl` applies: HOME, D-pad centre (`KEY_CENTER`), Recents (580), media and stem keys arrive. The console takes every stock `KEY_*` name and numeric codes. `qemud:sensors` lists the AVD's sensors by stock's rules (`hw.sensor*` flags; heart rate and wrist tilt on Wear OS API 28+; the hinge on foldables) with live values; GNSS over `qemud:gps`; goldfish battery; fingerprint; clipboard |
| Telephony | AT modem on a virtio-console `modem` port: the radio HAL registers on LTE with a loaded SIM; incoming SMS and calls from the console |
| Network | virtio-net at stock's net slot (26, MAC `52:54:00:12:34:56`), so the guest has the `eth0` the radio HAL configures from the modem's `+CGCONTRDP` reply (10.0.2.15/24, gateway 10.0.2.2, DNS 10.0.2.3). Behind it, a user-mode NAT in the emu process (`crates/emu-vmm/src/net`, smoltcp for TCP; no root, entitlement or host interface): TCP, UDP and ping (unprivileged ICMP sockets) to anywhere the host reaches; 10.0.2.2 is the host's loopback and 10.0.2.3 forwards to the host's resolver (`/etc/resolv.conf`), as on stock. The cellular network validates (NetworkMonitor's HTTPS probe returns 204). The guest is untrusted, so its flows are capped by emu's open-file limit (raised at start to 10240 on macOS): 1024 TCP connections (256 still connecting; a SYN past that is reset), 512 UDP and 64 ICMP flows, with 128 descriptors kept for the console, adb bridge and display; a DNS flow closes at its last answer. `EMU_NET=link` keeps the link and drops the NAT (for `scripts/net-bench.sh`). Not yet: IPv6, inbound port forwarding (`redir`), IP fragments from the guest, VPN-scoped resolvers |
| Images | Phone API 35, 36, 37 and Wear boot (5–8 s warm; Wear gets stock's +512 MB below 1 GB). Automotive: the vehicle HAL channel (`qemud:car`) and a silent virtio-snd card are implemented but not yet booted ([Automotive](#automotive)); Bluetooth needs a controller (in progress). Android XR (`google-xr`, `android-xr-*`, `ai-glasses`; provisioned from the `google_xr` site) gets the dual-mode mouse and `qemud:xr_service` (console `xr`), not yet booted: it gets the virtio-snd card too |
| Wear OS | New AVDs: 512 MB (booted with stock's +512 MB), `hw.mainKeys=yes`, no D-pad, and for round screens `boot.prop`'s `ro.emulator.circular=true`, which reaches the guest as a `qemu-props` property and bootconfig `androidboot.emulator.circular=1`. The rotary turns 15 units (degrees) per click, like stock; the console's `EV_REL:REL_WHEEL:<n>` stays raw |
| TV | New AVDs: 2 GB, `hw.mainKeys=yes`, `hw.dPad=yes`. The D-pad keys, centre included, reach the guest through `qwerty2.kl`; not yet checked on a running TV image |
| Hardware keys | `qemu.hw.mainkeys` follows the AVD's `hw.mainKeys` (stock's default yes hides the navigation bar); phone, tablet, foldable and automotive AVDs created by simee write `no` |
| AutoMobile | API 37: 38 of 44 harness tools pass; open: network shaping, gRPC audio, VM snapshots |
| Desktop app | simee's window (`desktop/`), shared with iOS devices: AVD list, start/stop, live guest console, live screen with touch and form-factor controls (Back/Home/Recents/Power, rotate, hinge, rotary, D-pad) over `<avd>/simee/display.sock` |

## Quick start

```bash
cargo run -p emu -- -list-avds
cargo run -p emu -- -avd <name> -show-kernel -verbose   # first boot formats and encrypts /data
adb -s emulator-5554 shell                              # once adbd is up
adb -s emulator-5554 emu kill
make shim && export ANDROID_HOME=~/.simee/sdk          # make tools launch simee as "emulator"
cd desktop && cargo run                                 # the desktop window (iOS and Android)
simee list --platform android                          # the same AVDs through simee
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
| `crates/emu-vmm` | The machine: memory map (`layout.rs`), device tree (`board.rs`), vCPU loop and PSCI (`vcpu.rs`), MMIO bus, PL011/PL031, virtio-mmio transport, blk, rng, vsock, snd, host pipe services (adb bridge, `qemud:boot-properties`, `qemud:car`, ...) |
| `crates/emu-avd` | AVD discovery and config (`~/.android/avd`, `config.ini`, system image), form factor (`Avd::profile`), running-instance advertisements, bootconfig encoding |
| `crates/emu-sdk` | Java-free SDK provisioning: repository manifests, verified downloads, unpacking, the on-demand renderer (`emu -provision`, `-provision-renderer`) |
| `crates/emu-ext4` | Native ext4 formatter for first-boot userdata: writes only metadata, so a fresh 10 GiB image takes under 1 MB on disk |
| `crates/emu-fdt` | Flattened device tree writer |
| `apps/emu` | The `emu` CLI: SDK-emulator-compatible flags, per-AVD instance state, console server, display server |
| `crates/sim-android` | The client front ends use: AVDs as devices, starting `emu`, the console and display-socket clients |
| `desktop/` | simee's Rune window (both platforms), its own workspace |
| `docs/` | [Stock board reference](reference/stock-board.md), [desktop and display design](design/desktop-and-display.md), [roadmap](roadmap-android.md) |
| `scripts/` | Cargo runner that signs with the hypervisor entitlement, SDK overlay (`make shim`), AutoMobile harness (`make harness-android DEVICE=emulator-5554`) |

## Benchmarks

`scripts/android-bench.sh` boots an AVD the same way under either emulator and measures it. The
current numbers, cold and warm, and how they were taken are in [benchmarks.md](benchmarks.md).

Both run guest code natively on the CPU. The differences are in the VMM: no
userspace GIC, fewer device threads, and RAM that is only committed when touched.

## Provisioning without Java

simee needs a system image, platform-tools (adb) and, for graphics, the gfxstream renderer.
Java, sdkmanager, avdmanager, Python, e2fsprogs and the full emulator package aren't required
(userdata is formatted natively by `crates/emu-ext4`). The `emu` binary provisions the rest
itself:

```bash
emu -list-images -tag google_apis                       # stable arm64 images for this host
emu -provision 36 -create-avd small36                   # platform-tools + google_apis image + renderer + AVD
emu -provision 35 -tag google_apis_playstore -create-avd play35 -lcd 480x800@240 -cores 2 -memory 1536
emu -provision 36 -dry-run                              # resolve only: URL, size, sha1, install dir
emu -provision 36 -no-renderer                          # skip the renderer (boots without graphics)
emu -provision 34 -tag android-automotive               # no plain 34 image: picks the newest, 34-ext9
emu -provision-renderer                                 # install or update just the renderer
emu -create-avd NAME -sysdir system-images/android-36/google_apis/arm64-v8a -lcd 720x1280@320
```

`crates/emu-sdk` reads Google's repository manifests (`repository2-3.xml`, and
`sys-img/<tag>/sys-img2-3.xml` per tag), picks the newest stable archive for the host
(platform-tools for the machine it runs on; system images are host-independent; the renderer is
always macOS/arm64's), downloads it with the system's `/usr/bin/curl` (resumable, into `$ANDROID_HOME/.downloads`),
checks size and sha1, unpacks it with `/usr/bin/unzip` and moves it into the standard SDK layout.
Installed packages are skipped, manifests are cached for a day, and the archive is deleted once
unpacked. Disk images (`*.img`) don't go through a plain unzip: each streams out of the archive
(`unzip -p`) into a file where only non-zero 4 KiB blocks are written, so zero runs stay holes.
Most of a system image is zeros: TV 34's 8.6 GB `system.img` takes 1.4 GB and reads back
byte-identical. Unpacking TV 34 took 64 s instead of 4 min 48 s (unzip writing 8.6 GB, then a pass
reading it back to punch holes) on a Linux VM, against a 40 s floor for inflating it at all. The SDK is `$ANDROID_SDK_ROOT`/`$ANDROID_HOME`, else Android Studio's default
(`~/Library/Android/sdk` on macOS, `~/Android/Sdk` on Linux). `SDK_TEST_BASE_URL` (the variable sdkmanager honours) points provisioning at another repository,
a mirror or a `file://` copy. An API level a tag publishes only
at extension levels (Automotive's API 34 and 35) resolves to the newest of them. Provisioning
works on Linux too, for preparing images and AVDs; there `-provision` skips the renderer, since
only Apple silicon boots guests.

**The renderer** comes out of the SDK's `emulator` package, fetched on demand: the archive
(420.6 MB for 37.3.10) is verified like any other, but only `emulator/lib64/**` minus
`lib64/qt/**` (the stock window's Qt libraries) is unpacked, plus `LICENSE` and `NOTICE.txt`:
498 MB on disk instead of the package's 1.32 GB. That keeps everything the renderer loads: the
backend and its `@rpath` libraries, the shader translator it opens by name, `gles_angle`, and
the Vulkan loader, ICDs (MoltenVK, lavapipe with its LLVM, SwiftShader) and validation layers.
It goes to `$ANDROID_HOME/simee/renderer/<revision>/` (with a `renderer.properties` marker
recording revision and sha1, and a `current` symlink to the newest), never `emulator/`, so
sdkmanager's own package tracking is never confused. `-provision` installs it unless the SDK
already has a renderer; `-provision-renderer` updates to the newest stable revision and removes
older ones.

At boot emu uses the SDK's own `emulator/lib64` when present, else the newest
`simee/renderer/*/lib64`, and points gfxstream's launcher directory at its parent (where it
finds `lib64/vulkan` and `lib64/gles_*`). `emu-gfxstream` preloads the backend's `@rpath`
dependencies leaves-first from that directory, so they resolve without any rpath; libraries
gfxstream `dlopen`s by bare name resolve through the binary's rpaths (`@executable_path/lib64`,
which emu links to the chosen `lib64`, then the build machine's `emulator/lib64` and
`simee/renderer/current/lib64`). To check an install without a VM:
`cargo run -p emu-gfxstream --example renderer_caps -- <lib64> moltenvk` prints capsets 3, 8, 9.

AVD defaults favour cheap, fast boots: 720×1280 screen, 2 GB RAM, 4 vCPU, 4 GB data.
Benchmarks of the stock emulator found that smaller screens boot markedly faster and that RAM
above 2 GB slows cold boots down.

## Automotive

What the AAOS sources show (`device/generic/car/emulator`, `hardware/interfaces/automotive/vehicle`,
the emulator's `qemu-setup.cpp`), what simee does with it, and what still needs a real boot.

**Vehicle HAL.** The emulator VHAL (`vhal_aidl/VehicleEmulator`, API 33–36) opens the qemud
`car` pipe only when `ro.boot.qemu` is set; if the open fails it logs and carries on, and its
property values come from the image's own JSON config. So the pipe doesn't gate boot: it is how
the host drives the car. Stock registers the service only for car images (`build.prop`'s
`ro.product.name`, `ro.product.system.name` or `ro.build.flavor` contains `car`); simee does
the same (`Avd::is_car`, falling back to an `android-automotive*` tag when an image has no
`build.prop`) and refuses the pipe on every other image, as before.

The channel is `libqemud` framing (4 hex digits of length; the guest sends 64 KiB and larger
payloads with a 4-byte big-endian `len | 0x8000_0000` header) around proto2
`vhal_proto.EmulatorMessage`. The guest reads into a 2048-byte buffer and ends its read loop on
a payload that fills it, so host frames carry at most 2047 bytes. It answers every command in
order and sends `SET_PROPERTY_ASYNC` on its own from boot, so simee drains the pipe whether or
not anyone is waiting. `sim_core::vhal` is the protocol (a hand-written codec, no protobuf
dependency, checked against hand-encoded stock commands), `emu_vmm::pipe::car` the pipe.

Stock has no console command for the car (its controls live in the UI and in gRPC's
`sendCarEvent`), so simee adds one, as an extension:

```
car set <property> [<area>] <value>    # SET_PROPERTY_CMD, as stock's car page sends
car get <property> [<area>]            # GET_PROPERTY_CMD
car list                               # GET_PROPERTY_ALL_CMD
car names                              # the property names the console knows
```

Properties are VHAL names (`GEAR_SELECTION`, `PERF_VEHICLE_SPEED`, `IGNITION_STATE`, ...; short
`gear`, `speed`, `ignition`, `night`, `parking-brake`) or numeric ids; values are typed by the
id's type bits (`drive`/`P`, `on`/`acc`, `12.5`, `true`, `2,0` for vectors, hex for bytes).
Value lines read `property=<name> id=<hex> area=<n> status=<s> value=<text>`. simee reaches it
as the `vehicle` API method and CLI command (`simee vehicle <avd> gear drive`), for devices
whose form factor is auto.

**Audio.** From API 35 the car audio HAL (`audio.primary.caremu`) calls `mixer_open(0)` and
then `mixer_get_name()` without a NULL check, so without ALSA card 0 it most likely crash-loops.
simee adds a silent virtio-snd card (one output and one input stream, S16/S24/S32/float,
8–48 kHz, mono or stereo, no jacks) on virtio-mmio slot 18 for car images and for XR images
(whose image enables `VirtioSndCard`). Playback buffers are consumed and capture buffers come
back as silence at the stream's real rate, so the HAL's blocking writes and reads keep their
timing. `EMU_VIRTIO_SND=1` (or `0`) forces the card on (or off) for any image; `-no-audio`
doesn't remove it, since it never plays on the host anyway. Stock puts virtio-snd on PCI, phone
images included; simee leaves phones without it until it has been tried on one.

**Needs a real boot.** Whether the car image reaches the launcher with these two pieces; that
the VHAL connects and `car get gear` answers; that the kernel's `virtio_snd` driver binds on
virtio-mmio and `caremu` opens card 0 (its mixer has no controls, which its init loop
tolerates); and the rest of the car stack (EVS, cluster display, rotary controller), which
simee doesn't model.

**Checked offline** against `system-images;android-34-ext9;android-automotive;arm64-v8a` r5
(`scripts/inspect-image.py`, `emu -avd Car34 -dry-run`): the image ships `build.prop` beside it,
so simee recognizes it as a car and adds `qemud:car` and the sound card; its first-stage fstab
and modules match the board; its VHAL (`android.hardware.automotive.vehicle@V1-emulator-service`)
opens `pipe:qemud:car`, over vsock first; and every property and gear/ignition value in
`sim_core::vhal` exists in the image's own `vehicle-types-meta.json` (a trimmed copy is a test
fixture). Its vendor also runs EVS, CAN, broadcast radio and remote-access HALs, none of which
simee serves.

## Performance notes

What has mattered so far, measured with `sample` and the `-verbose` exit counters:

- **In-kernel GICv3.** The stock board emulates a GICv2 in userspace. Here interrupt
  acknowledge, EOI and the virtual timer never leave the guest, and an idle vCPU parks inside
  the framework (zero WFI exits in practice).
- **Kick on interrupt.** A vCPU idling inside the framework doesn't notice a newly raised SPI
  until its own timer fires. Kicking vCPUs out of `hv_vcpu_run` after raising one cut
  first-boot `/data` encryption from 22 s to 8 s. Only the SPI's target (`GICD_IROUTER`) is
  kicked, and only while it is in the guest, once per entry (`crates/emu-vmm/src/wake.rs`).
  Other VMMs on macOS 26 measured `hv_gic_set_spi` alone waking the target; A/B with
  `EMU_VCPU_WAKE=none` (no kicks) and `EMU_VCPU_WAKE=all` (the old kick-everyone).
- **Plain `fsync` for guest flushes.** `std`'s `sync_data` is `F_FULLFSYNC` on macOS, which
  drains the drive cache. It was 75% of the userdata disk thread's time, and APEX
  decompression on first boot took more than 270 s; with plain `fsync` it takes about 70 s.
  Writes already sit in the host page cache, so an emulator crash loses nothing either way.
- **Zero-copy disk I/O.** `pread`/`pwrite` go straight between the image and guest RAM, one
  worker thread per disk, so the vCPU that kicked a queue goes back to the guest at once.
- **One syscall per block request, no kicks while draining.** Linux sends a 128 KiB request as
  one indirect table of up to 32 scattered pages; virtio-blk issued a `pread`/`pwrite` per page.
  Now a request is one `preadv`/`pwritev` (guest-adjacent pages merged, at most 1024 iovecs a
  call), an ephemeral disk checks its overlay once per request, and a whole-chunk write to an
  ephemeral disk no longer reads the base chunk it replaces. In-process A/B medians on a loaded
  Mac (`cargo test --release -p emu-vmm disk_vectored_vs_per_segment -- --ignored`), per
  request: persistent read 20 → 7.2 µs, persistent write 62 → 9.9 µs, ephemeral read 20 → 7.5
  µs; the skipped base read takes ephemeral writes to fresh chunks from 16 to 10.5 µs. The
  worker turns notifications off while it drains and back on with a recheck (QEMU's
  `virtio_queue_set_notification`; publish `avail_event`, full barrier, reread `avail.idx`), so
  a guest that publishes while a request is being served doesn't kick. With a driver thread
  racing the worker (`blk_kicks_while_draining`), kicks per request at queue depth 2 went from
  1.0 to 0.001 with event-idx, and at every depth ≥ 2 from 1.0 to under 0.005 without it; at
  depth ≥ 4 with event-idx the old code already rarely kicked. Each saved kick is an MMIO exit
  on a real guest. Popping a 34-entry indirect chain into a reused `Chain`, with the table read
  in one copy, takes 82 ns instead of about 400 (`queue_pop_indirect`).
- **Sparse, lazily committed memory.** Guest RAM is an untouched anonymous mapping and the
  userdata image is a sparse file.
- **Discard punches holes.** virtio-blk `DISCARD`/`WRITE_ZEROES` become `F_PUNCHHOLE` on
  persistent images (an in-memory zeroed-range set on ephemeral ones), so space the guest frees
  goes back to the host: with `/data` mounted `-o discard`, writing then deleting 512 MiB grew
  the image by 525 MB and shrank it by 524 MB. A whole-disk FITRIM takes 90 ms. `/data` sits on
  dm-default-key metadata encryption, which passes discard through but not write-zeroes, and
  zeroes it writes land as ciphertext. So discard is the only way to free space, and a fresh
  first boot (about 610 MB allocated) has only about 2 MB left to trim.
- **Screenshots: fast PNG, no re-encode, no needless copies.** Automation takes a screenshot
  after nearly every step. On a 2480×1740 screen (`cargo run --release -p sim-core --example
  frame_bench`), PNG encoding at the `png` crate's default level took 74 ms and BGRA→RGBA
  conversion 11 ms; `Fast` (fdeflate) encodes in 11.5 ms for a file 1.7× larger, and a conversion
  loop per pixel format (no per-pixel `match`) takes 7.8 ms: 86 → 19 ms. The API passes the
  emulator's PNG straight through (with `path`, emu writes the file itself) instead of decoding
  and re-encoding it, and the display socket's frame loop no longer clones the whole frame just
  to read its size. The display socket's per-frame downscale (a box filter) went from 22.7 to
  7.2 ms at factor 2 (16.4 to 7.5 ms at 3): const-generic kernels for factors 2–4 and division by
  a reciprocal, identical output.
- **The frame path: no copy per post, one readback per frame, a shared-memory ring.** The path
  is described in [the display design](design/desktop-and-display.md#frame-path-built). Stage by
  stage for the desktop's stream (`hello 1080`) on a 4-core Linux VM (memcpy 5.4 GB/s), medians,
  tablet 2480×1740 / phone 1080×2400:

  | Stage | Before | After | Benchmark (`--ignored --nocapture`) |
  | --- | --- | --- | --- |
  | Post callback (renderer thread) | 3.9 / 1.6 ms copy | 0.0002 ms (size only) | `cargo test --release -p emu-vmm present_cost` |
  | Readback | per stream and per screenshot: Vulkan → CPU → GL → CPU under the FrameBuffer lock | once per frame, shared by streams and screenshots (a full-size ring stream reads straight into its slot): Vulkan → CPU | needs a Mac (below) |
  | Scale (/3 / /1) | 8.0 ms / 1.8 ms copy | 2.0 ms (2.8 on one thread) / none | `cargo test --release -p sim-core downscale_cost` |
  | Emulator → socket (1.9 / 10.4 MB) | 0.86 / 3.7 ms of writes | a 32-byte notice | `cargo test --release -p emu transport_cost` |
  | Emulator CPU per frame besides the readback | ≈12.8 / ≈7.1 ms | ≈2.0 / ≈0 ms | sum of the above |
  | Post → client holds the pixels | — | 8.1–9.0 / 8.0–8.8 ms (ring), 7.8–10.3 / 8.5–9.3 ms (socket), across runs | `CASE=2480,1080,true cargo test --release -p emu frame_path_latency` |

  End to end the ring and the socket tie on this VM: the latency is mostly two memory-bound
  copies, the stand-in readback (a memcpy of the frame) and the client's copy into the buffer it
  hands to `Pixels` (its `pread` costs what a socket read did). What the ring removes is the
  emulator's work: no payload writes, and frames are scaled (or read back) straight into shared
  memory. A new ring is written out in full when it's made (so a full disk is an error rather than a
  `SIGBUS`), which costs its first frames 20–60 ms here, once per ring. Frames scale on one thread below 16 MiB of input and in up to 4
  bands above: two threads were slower than one on this VM (likely SMT siblings), four faster.
  Rings live in `<avd>/simee/`, a disk-backed file on macOS whose dirty pages the system writes
  back periodically (benchmark rigs on Linux use `/dev/shm`). The direct Vulkan readback is checked
  against the GL one on its first 3 frames and turned off if they differ (`EMU_READBACK=gl` turns
  it off outright).
- **vsock without intermediate copies.** adb (and every qemud pipe) rides vsock. Host bytes were
  copied into the connection's queue, out of it into a packet `Vec`, and then into the guest's
  buffer; now the adb forwarder hands its buffer over, the worker takes it over when nothing is
  queued, and packets are written straight from the queue into the guest's descriptors
  (`Chain::write_parts`). Host→guest through one pipe with the guest posting Linux's 4 KiB
  receive buffers (`cargo test --release -p emu-vmm vsock_throughput -- --ignored`): median of 5,
  284 → 512 MiB/s on a 4-core Linux VM. Guest→host (`vsock_throughput_to_host`, 64 KiB
  packets) runs at about 4 GiB/s, so host→guest's limit is the guest's small receive buffers.
- **vsock backpressure: a stalled client stalls only itself.** One worker thread serves every
  pipe, and the adb forwarder used to `write_all` to its client on it, so an adb client that
  stopped reading (`kill -STOP`) froze sensors, GPS, the vehicle HAL and every other adb session
  once its socket filled. Now the worker advertises `fwd_cnt` only for bytes a pipe has
  delivered (`PipeHandler::backlog`), as Firecracker and Cloud Hypervisor do, and sends the
  credit update itself, because Linux guests never send CREDIT_REQUEST. The forwarder sends
  without waiting and queues the rest for a writer thread, at most the 256 KiB the guest was
  allowed. A guest that sends past its credit is reset, as Linux resets one. The other
  direction is bounded too: a client is read only while less than 1 MiB waits for the guest
  (`guest_took`). The stock emulator's vsock path buffers without limit both ways. Connections
  take turns for rx buffers (a ready queue rather than a scan per buffer), and tx waits while
  a queue's worth of replies waits for rx buffers, as vhost-vsock does. Medians of 10
  interleaved runs on a 4-core Linux VM, before → after: `vsock_throughput` 474 → 597 MiB/s,
  `vsock_throughput_to_host` 5.9 → 6.4 GiB/s (its guest now honours credit),
  `adb_throughput_to_guest` (TCP client → guest, end to end) 498 → 870 MiB/s,
  `adb_throughput_to_host` 1382 → 1433 MiB/s.
- **Launch to first guest instruction.** Most images ship a gzip `kernel-ranchu` (an API 37
  kernel is 16 MiB that inflates to about 40 MiB). It used to be read whole, inflated into a
  `Vec` with miniz_oxide and copied into guest RAM on every launch. QEMU, and so the stock
  emulator, inflates on every launch too, while Firecracker and crosvm take only raw images. Now
  the first launch inflates it in 1 MiB chunks straight into guest RAM with zlib-rs and writes
  the image to `~/Library/Caches/simee/kernel-<path>-<stamp>`, keyed by path, size, mtime,
  inode and the gzip trailer's CRC32 and ISIZE (one 8-byte `pread`, so a kernel rewritten in
  place with its mtime kept is a miss), through a temp file that is synced before the rename. A
  hit must be exactly ISIZE bytes long: a short entry would read as zeros past its end. Older
  versions of the same path, and temp files over a minute old (a launch killed mid-write), are
  removed. Later launches `pread` that file into guest RAM, as they do a raw `Image`. Medians of
  7 interleaved runs on an M3 Max under CI load (`cargo test --release -p emu-vmm kernel_load --
  --ignored --nocapture`): 115 ms before (miniz_oxide), 71–76 ms to inflate with zlib-rs, 86–95
  ms on a miss (the synced cache write adds 15–20 ms), 3.5–3.9 ms on a hit. The gfxstream
  renderer's load and init (about 400 ms, `renderer_init_off_thread`) now starts on its own
  thread once the console port is bound and the boot files are found, and `run` waits for it
  just before it creates the GPU device; a launch that fails before that point waits for the
  init to finish before it exits, so the process never exits mid-dlopen. The thread stays
  parked afterwards, as the main thread did. Reading the 1.8 MiB ramdisk straight into guest RAM instead of through a `Vec` saved
  0.06 ms, so it was left as it was. `-verbose` prints the kernel source and time, how long the
  boot waited for the renderer, and when the vCPUs started relative to launch.
- **Quiet console.** The PL011 traps twice per byte, and a boot printed ~230 KB on it: 88% of
  all vCPU exits. Without `-show-kernel` the cmdline now adds `loglevel=3`: only emergencies,
  alerts and criticals print, and dmesg still has everything. Stock passes `console=0` then and
  prints nothing. Measured with `EMU_EXIT_PROFILE` (`scripts/farm-bench-android.sh`, details in
  docs/farm-android.md): exits to boot_completed 516k → 77–109k, idle 4.5k → 122–154 exits/s.
- **goldfish-logcat.** `init.ranchu.rc` runs `logcat -f /dev/hvc1` all the time. hvc1 had no
  backend, so logcat restarted every ~5 s. The virtio-console's third port (unnamed, so a console
  port, which makes it hvc1) now backs it. Its output is discarded with filter `*:S` unless
  `-logcat TAGS` or `-logcat-output FILE` is given (through a bounded queue that drops rather
  than stall the console worker; FILE rolls over at 4 MiB). sim-android starts emu with
  `-logcat '*:I' -logcat-output <avd>/simee/logcat.txt`, which the desktop's Android panel shows.
- **Giving guest memory back to the host.** `GuestRam::discard` uses `hv_vm_unmap`, then
  `mmap(MAP_FIXED)`, then `hv_vm_map` on whole 16 KiB pages. It is the only method measured that
  both lowers `phys_footprint` and reads back as zeroes. Measured with a VM in a test (64 MiB,
  half written by the host, half by the guest; Apple M3 Max, macOS 26.6.2, 2026-11-24; run each
  with `cargo test -p emu-hvf --test discard_probe -- --ignored --nocapture --exact <name>`):

  | Method | Footprint | Guest reads after | Host reads after | Host and guest share pages after |
  |---|---|---|---|---|
  | `madvise(MADV_DONTNEED)` | 66.1 → 66.1 MiB | old data | old data | yes |
  | `madvise(MADV_FREE)` | 66.1 → 66.1 MiB | old data | old data | yes |
  | `madvise(MADV_FREE_REUSABLE)` | 66.1 → 34.1 MiB | old data | old data | yes |
  | `mmap(MAP_FIXED)` alone | 66.1 → 34.1 MiB | old data | zeroes | **no** |
  | `hv_vm_unmap` + `mmap(MAP_FIXED)` + `hv_vm_map` | 66.1 → 2.0 MiB | zeroes | zeroes | yes |

  - The shipped method costs 30–80 µs per MiB and 0.6–1.9 ms for 32 MiB (`guest_ram_discard_in_a_vm`).
  - A page that both the host and the guest touched counts twice in `phys_footprint`: the host
    reading the guest-written half raised it from 66 to 98 MiB, with nothing discarded.
  - The goldfish address space's clearing thread now discards freed runs of 1 MiB or more. In a
    model of the device (no guest), freeing 64 MiB of buffers left 64 MiB resident before this
    change and 0 MiB after (`cargo test -p emu-vmm --lib clear_cost -- --ignored --nocapture`).
    Not yet measured with real gralloc traffic.
  - A virtio-balloon or free-page reporting would have to use the same unmap and remap path.
    `madvise` alone either keeps the old data or doesn't lower the footprint of pages the guest
    touched.

- **Thread QoS.** A thread doesn't inherit its spawner's QoS class on Darwin, so emu sets one on
  each thread: user-initiated on vCPUs and device workers, utility on housekeeping (signal
  handling, adb announcements, the logcat writer, the address-space clearer). Lowering works as
  documented: utility threads run at priority 20. Raising doesn't always: launched from an agent's
  shell on this Mac (M3 Max, macOS 26.6.2, 2026-11-24), threads set to user-interactive,
  user-initiated and default all ran at priority 31 (`ps -M`), so the process's QoS ceiling was
  default there. Whether vCPUs reach a higher tier when emu is started from Terminal or a
  launchd job, and whether that changes boot or settle time, is not yet measured.

## Development

```bash
cargo c && cargo lint && cargo t    # check, clippy -D warnings, tests (no hypervisor needed)
make run AVD=<name>                 # boot with the console on stdout
cd desktop && cargo test            # headless Rune view tests
```
