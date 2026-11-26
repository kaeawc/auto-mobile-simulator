# Benchmarks

Every number the README and [comparison.md](comparison.md) quote comes from this page. Each row
says what was run, on what, and when. The rules for keeping it current are in
[AGENTS.md](../AGENTS.md#benchmarks-and-public-docs).

## Android: emu against the SDK emulator

One API 36 `google_apis` arm64 AVD (4 vCPU, 2 GB) on an M3 Max (macOS 26.6.2), 2026-11-24, SDK
emulator 37.2.13.0. Medians of 3 boots of each kind; host load average 3–5 during the boots.
These runs predate emu's Vulkan-only renderer (2026-11-25), which shortened a phone's warm boot
and lowered its peak memory on another AVD ([internals](#emulator-internals)); this table is due
a rerun with it.

- **Cold:** a new AVD's first boot (no userdata yet: emu formats and encrypts `/data`).
- **Warm:** a later boot of the same AVD. The SDK emulator runs with `-no-snapshot-load`; emu
  has no snapshots, so both do a full boot.
- **Snapshot:** the SDK emulator restoring its quickboot snapshot, for reference.

```bash
scripts/android-bench.sh <label> <avd> emu|stock [-no-snapshot-load]   # one boot, one result line
```

| Measure | emu | SDK emulator | Ratio |
| --- | --- | --- | --- |
| Cold: launch until `sys.boot_completed` | 9.5 s | 22.9 s | 2.4× faster |
| Cold: launch until `adb` lists the device | 5.2 s | 14.9 s | 2.9× faster |
| Cold: peak RSS, process tree | 3764 MB | 4988 MB | 25% less |
| Warm: launch until `sys.boot_completed` | 3.5 s | 17.1 s | 4.9× faster |
| Warm: launch until `adb` lists the device | 1.6 s | 8.5 s | 5.3× faster |
| Warm: peak RSS, process tree | 2090 MB | 3780 MB | 45% less |
| Snapshot restore until `sys.boot_completed` | none | 2.8 s | |
| `adb shell echo`, warm | 0.02 s | 0.03 s | equal |
| Guest CPU, 100k shell loop, warm | 0.22 s | 0.36 s | |
| Guest CPU, 4× md5, warm | 0.43 s | 0.59 s | |

These replace the 2026-11-22 run, whose 1.67 GB peak memory for emu (against 5.09 GB) did not
hold up when remeasured.

### API 29 (GLES over the goldfish pipe)

One API 29 `default` arm64 AVD (2 GB, 720x1280), same host (Apple silicon Mac, 16 CPUs, macOS
26.6), 2026-11-26, SDK emulator 37.2.13.0. Warm boots (the AVD had booted before under both),
medians of 3, emu and stock alternating; host load average 21–30 during the runs.

```bash
PORT=5610 scripts/android-bench.sh a29-emu-N <avd> emu
PORT=5612 scripts/android-bench.sh a29-stock-N <avd> stock
```

| Measure | emu | SDK emulator | Ratio |
| --- | --- | --- | --- |
| Warm: launch until `sys.boot_completed` | 4.8 s | 6.4 s | 1.3× faster |
| Warm: launch until `adb` lists the device | 2.4 s | 3.5 s | 1.5× faster |
| Warm: peak RSS, process tree | 1596 MB | 1692 MB | 6% less |
| Guest CPU, 100k shell loop, warm | 0.29 s | 0.59 s | |
| Guest CPU, 4× md5, warm | 0.43 s | 0.94 s | |

### Idle host CPU

The same API 36 AVD, warm-booted under emu built without and with its network device (#29),
alternating, 3 runs each, M3 Max, 2026-11-24. Each run waits 60 s after `sys.boot_completed`,
then takes emu's CPU time over 240 s (`ps -o time`) and counts `SETUP_DATA_CALL` in the radio log
over the same window. Other work on the host put the load average between 4 and 34 across the
runs.

| emu | Idle host CPU, median (runs) | Data calls in 240 s |
| --- | --- | --- |
| No network device | 65.9% of a core (61.6, 65.9, 66.3) | 276–294 |
| Link-only virtio-net | 6.0% of a core (5.7, 6.0, 6.6) | 0 |

Without an `eth0`, the guest's telephony retries the data call without a delay; with one, the
call succeeds once and the guest goes idle.

With the user-mode NAT behind it (#36): one fresh API 36 google_apis AVD and one emu build, the
NAT on and off (`EMU_NET=link`), 3 warm boots each, alternating after one uncounted first boot,
M3 Max (16 CPUs), 2026-11-25. Each waits 60 s after `sys.boot_completed`, then takes emu's CPU
time over 60 s (`ps -o time`). `scripts/net-bench.sh idle <avd>`. Other lanes' work on the host
put the 1-minute load between 4.5 and 77 across the runs (each run's load at its start and end
below); the first link-only run had 65–77, the rest 4.5–15.6.

| emu | Idle host CPU, median (runs, 1-minute load) | Guest internet |
| --- | --- | --- |
| Link-only virtio-net | 5.6% of a core (7.5 at 77–65, 5.6 at 9.3–8.9, 4.9 at 5.4–4.5) | none |
| virtio-net with the NAT | 5.9% of a core (5.9 at 11.9–8.9, 5.0 at 9.6–7.4, 7.3 at 10.8–15.6) | validated |

The difference is inside the run-to-run spread. `sample <pid> 5` on the idle NAT device found its
net thread blocked in `poll` in all 3939 samples.

Throughput through the NAT on the same AVD, build and day, after the boot above: 200 MiB between
the guest's toybox `nc` and a Python socket server on the host's loopback (the guest's
10.0.2.2), one uncounted warm-up transfer then 3 runs each way, all timed on the host by the
server (from accept to the guest's last byte, or to the guest's close after the host's last
byte); the 1-minute load was 6.5–7.5. `scripts/net-bench.sh throughput <avd>`.

| Direction | Time for 200 MiB, median (runs) | Rate |
| --- | --- | --- |
| Guest to host | 0.342 s (0.321, 0.349, 0.342) | about 610 MB/s |
| Host to guest | 0.102 s (0.109, 0.102, 0.098) | about 2.1 GB/s |

The SDK emulator's slirp was not measured on this host.

### Frame pacing

How smoothly the guest draws under steady motion: 10 app-drawer swipes up and down on the
launcher, then the launcher's own frame stats (`dumpsys gfxinfo`). Janky frames missed their
deadline. One API 36 `google_apis` AVD of each size (4 vCPU, 2 GB) on the M3 Max, 2026-11-25,
SDK emulator 37.2.13.0, each warm-booted and given 60 s to settle, 3 runs of each alternating;
host load average 4–7. emu with GL is the same build with `EMU_RENDERER_GL=1`, as emu ran before
the Vulkan-only renderer. Host CPU is emu's process over the run; the stock emulator's runs in a
child process the script doesn't track.

```bash
scripts/android-jank.sh emulator-<port> [emulator pid]   # one key=value line
```

| Device | Emulator | Janky frames, median (runs) | Frame time p90 / p99 | Host CPU |
| --- | --- | --- | --- | --- |
| Pixel Tablet, 2560×1600 | emu | 0% (0, 0.56, 0) | 5 / 6 ms | 49% of a core |
| | emu with GL | 9.5% (9.5, 9.9, 8.7) | 22 / 48 ms | 96% |
| | SDK emulator | 5.6% (5.9, 5.5, 5.6) | 18 / 23 ms | |
| Phone, 1080×2400 | emu | 0.25% (0.25, 0.13, 0.25) | 5 / 5 ms | 33% |
| | emu with GL | 5.0% (6.1, 5.0, 5.0) | 14 / 31 ms | 63% |
| | SDK emulator | 6.2% (8.4, 5.7, 6.2) | 18 / 30 ms | |

Both display at 60 Hz. The stock emulator's tablet rendered about 1350 frames over the swipes and
emu's about 530; emu with the guest on stock's GLES driver (`EMU_GUEST_EGL=emulation`) rendered
1254, so the count follows the guest's driver. On the phone both render about 790.

## Android farm

Clones of one settled golden AVD booted together, each until it is settled (boot complete and
idle), same host and day. `scripts/farm-bench-android.sh`.

| Devices | All settled | Failures | Host memory used per device |
| --- | --- | --- | --- |
| 1 | 72 s | 0 | 3.1 GB |
| 4 | 223 s | 0 | 2.8 GB |
| 8 | 266 s | 0 | 2.8 GB |

Most of the settle time is the guest's telephony retrying mobile data, since emu has no network
device yet ([#29](https://github.com/kaeawc/simee/issues/29)).

## iOS: simee against `simctl`

One new iPhone per run under each tool, same host, 2026-11-24, 3 runs each; the
boot returns once the device has finished booting (`simctl bootstatus` for `simctl`). Load on
this host varied during these runs, so treat them as a rough comparison.

| Measure | simee | `simctl` |
| --- | --- | --- |
| First boot of a new device, median | 15.5 s | 18.9 s |
| Later boot, median | 2.3 s | 2.3 s |

`scripts/farm-bench-ios.sh`, 8 clones booted 4 at a time, same day:

| Profile | Settled | Footprint per device | Processes per device |
| --- | --- | --- | --- |
| default | 5 of 8 within 600 s | 2176 MB | 235 |
| `slim` | 8 of 8 in 392 s | 1158 MB | 148 |

Time to usable devices: fresh clones of one booted-once iPhone 17 (iOS 27.2) template, first boot
through `simee boot-many <clones> -j K --profile slim --json`, 2026-11-26, M3 Max, 16 CPUs, macOS
26.6, 3 interleaved runs of each with other work on the host (1-minute load 25-550: read the
ranges). "Before" is the same build with the slim disables applied after the boot and settle
judged on all CPU (`--settle-cpu all`), as until this change; the mechanism is in
[farm.md](farm.md#the-slim-profile).

| Measure | before | after |
| --- | --- | --- |
| 1 clone: booted until settled, median (runs) | 95.0 s (97.2, 95.0, 52.7) | 33.3 s (44.2, 33.3, 28.3) |
| 4 clones at once (K=4): all settled, median (runs) | 229 s (229, 170, 238) | 43 s (35, 43, 71) |
| 4 clones at once: CPU-s each device used until settled, median (not like for like: "before" waited on all CPU, "after" on foreground CPU only, so its wait ends earlier and background work keeps running after it; docs/farm.md) | 167 | 76 |
| The slim profile's own time in that, 4 at once, median of 12 | 42.9 s | 5.4 s |

Time per automation step on one booted iPhone 17 (iOS 27.2) through `simee serve`, one persistent
connection, `python3 scripts/ios-step-bench.py <socket> <udid> 20` (medians of 20 calls; the
median of 3 interleaved runs), 2026-11-26, same host, 48 AVDs and 100 simulators on it, 1-minute
load 21-32. "Before" routed every call by listing both platforms' devices:

| Measure | before | after |
| --- | --- | --- |
| tap (holds 60 ms) | 137.5 ms | 63.4 ms |
| screenshot, PNG written to a file | 99.7 ms | 24.5 ms |
| screenshot, base64 in the reply | 107.8 ms | 34.8 ms |
| tap, then screenshot to a file | 241.2 ms | 88.9 ms |

One screenshot as a command (a process per call), same device and hour, median of 7 interleaved
(load 13-69): `simee screenshot` forwarded to `simee serve` 36 ms, `simee screenshot` in its own
process (`SIMEE_IN_PROCESS=1`) 173 ms, `xcrun simctl io <udid> screenshot` 601 ms.

## Emulator internals

These explain the table above. Each is a before and after of emu itself, with the mechanism
in [android.md](android.md#performance-notes).

| Change | Before | After | Measured on |
| --- | --- | --- | --- |
| Kick the target vCPU on an interrupt: first-boot `/data` encryption | 22 s | 8 s | M-series Mac |
| Plain `fsync` for guest flushes: first-boot APEX decompression | over 270 s | about 70 s | M-series Mac |
| Screenshot of a 2480×1740 screen (convert and encode) | 86 ms | 19 ms | `cargo run --release -p sim-core --example frame_bench` |
| Display stream: emulator CPU per frame besides the readback, tablet / phone | 12.8 / 7.1 ms | 2.0 / 0 ms | 4-core Linux VM |
| vsock host to guest | 474 MiB/s | 597 MiB/s | 4-core Linux VM, `vsock_throughput` |
| adb client to guest, end to end | 498 MiB/s | 870 MiB/s | 4-core Linux VM, `adb_throughput_to_guest` |
| Whole-disk FITRIM | | 90 ms | M-series Mac |
| Vulkan-only renderer: warm boot of a 1080×2400 phone, median (runs) | 3.7 s (3.7, 3.5, 3.7) | 3.3 s (3.3, 3.3, 3.3) | M3 Max, 2026-11-25, `EMU_RENDERER_GL=1 scripts/android-bench.sh` against without |
| Vulkan-only renderer: peak RSS of that boot | 3262 MB (3279, 1939, 3262) | 2014 MB (2044, 2014, 1945) | same runs |
| GPU state released on reset: hard guest reset of a 720×1280 API 36 phone, trigger to the next boot's first host frame, median (runs) | never (boot not complete in 120 s) | 3.26 s (3.36, 3.12, 3.26) | M3 Max, 2026-11-25, host load 7 to 9, `scripts/android-reset-bench.sh <avd> hard 3` |
| Same, trigger to `sys.boot_completed` | never | 2.96 s (3.13, 2.96, 2.94) | same runs |
| Same change, `adb reboot`: trigger to first host frame / to `sys.boot_completed`, median of 8 (two processes, 4 reboots each) | 3.92 / 3.74 s (3.72 to 4.12 / 3.61 to 3.99) | 4.06 / 3.88 s (3.65 to 4.18 / 3.61 to 4.08) | same AVD and hour, builds interleaved, host load 6 to 10, `scripts/android-reset-bench.sh <avd> reboot 4` |
| Same, a second interleaved set at host load 13 to 66 | 4.07 / 3.97 s (3.78 to 4.74 / 3.78 to 4.58) | 4.07 / 3.91 s (3.84 to 4.23 / 3.78 to 4.06) | same AVD and hour |
| Same change, `kill -INT` to process exit (exit status) | 0.13 s (0.14, 0.12, 0.13; all 0) | 0.13 s (0.13, 0.14, 0.13; all 0) | same AVD and hour, host load 6 to 8, `scripts/android-reset-bench.sh <avd> exit 3` |

The reset rows come from `scripts/android-reset-bench.sh` with `EMU` set to each build (the
change's parent and the change, release builds, 16 cores). A hard reset is
`echo b > /proc/sysrq-trigger` under `adb root`. Every time is from the trigger, so it includes
what emu does between the old boot's last instruction and the new boot's first (with this change,
the GPU teardown and the black frame); for `adb reboot` it also includes the guest's orderly
shutdown, about 1.0 s on both builds. The first host frame is the first `adb emu screenshot`
after emu's `-verbose` log prints `stopped after …: Reset` (polled every 0.1 s) that is neither
the frame taken just before the trigger nor all black: the next boot's first content, by the
same test on both builds (before the change the screen kept the last boot's final frame until
then, after it black). `sys.boot_completed` is polled beside it. Before the change a hard reset
never booted; a frame that differed did show (0.60 s), but not the next boot's. For
`adb reboot` the two sets disagree in sign (+0.14 / +0.13 s, then 0.00 / -0.06 s), within
the spread between rounds of one build (0.18 s): no cost is measured. These rows replace runs
that timed from the Reset line, which this change prints after its own teardown, ran the
0.3 s all-black test on every poll of a black screen (so only the build that shows black
polled slowly), and timed `sys.boot_completed` only after the first frame; those runs had
`adb reboot` about 0.3 s slower with the change.

The `--ignored` benches run on any host:

```bash
cargo test --release -p emu-vmm vsock_throughput -- --ignored --nocapture
cargo test --release -p emu-vmm present_cost -- --ignored --nocapture
CASE=2480,1080,true cargo test --release -p emu frame_path_latency -- --ignored --nocapture
```

## iOS

The injected network shaper's cost per call is measured in
[research/netshape-overhead.md](research/netshape-overhead.md).

## Build times (local)

Apple silicon Mac, 16 CPUs, macOS 26.6, 2026-11-26. The host was heavily loaded by other builds
the whole time (load average 50 to 700), so wall-clock time is not comparable between runs; the
rows give CPU seconds (user plus system of the build and its children, from `getrusage`), which
load does not inflate. Each is a clean build into an empty target directory; "before" and
"after" were run alternately, twice, and the range is both runs. Wall-clock times under that load
were 20 to 34 s before and 10 to 14 s after for the workspace, 114 s before and 37 to 42 s after for the desktop.

| Build | Before | After | Command |
|---|---|---|---|
| Workspace test build, clean | 131 to 136 CPU s | 74 to 77 CPU s | `cargo test --no-run` |
| Desktop test build, clean | 614 CPU s | 147 CPU s | `cd desktop && cargo test --no-run` |
| Workspace test run, warm | 12.5 to 13.7 s | 13.0 to 13.9 s | `cargo test --workspace` |
| Desktop test run, warm | 0.55 to 0.99 s | 1.06 to 1.41 s | `cd desktop && cargo test` |

What changed: the workspace's `dev` profile built every dependency at opt-level 2; it now builds
them at 0 and keeps 2 or 3 only for the ones on a runtime path (zlib-rs and flate2 for the kernel's
gunzip, smoltcp, crc32fast, adler2, base64, cpal, and the PNG crates that already had it). The
desktop's `test` profile builds its dependencies (Rune's GPU stack) at 0: its view tests run in
about a second either way. `cargo build` and `cargo run` in the desktop keep opt-level 2, since a
debug window has to stay smooth.

Measured and left alone:

| Build | Result | Command |
|---|---|---|
| Release `emu`, clean | 13 s wall (57 CPU s) at load 17; release settings unchanged | `cargo build --release -p emu` |
| Incremental after a one-line edit in `sim-core`, tests | 3.0 to 4.9 s; unchanged by `debug = 0` (3.1 to 3.6 s) or `-ld_classic` (3.6 to 4.1 s) | `cargo test --no-run` |
| Incremental after a one-line edit in `emu-vmm`, `cargo c` / `cargo lint` | about 1 to 2 s each | `cargo c`, `cargo lint` |
| `scripts/check.sh --all`, warm | 15 to 26 s, of which the test run is 13 to 22 s | `scripts/check.sh --all` |

The incremental loop is already a few seconds, so no profile change helps it; the clean-build wins
are what a fresh checkout, a CI cache miss or a Rune bump pays.

## Not yet measured

Claims that would belong in the README once someone runs them:

- `simee screenshot` against `xcrun simctl io screenshot` on a quiet host (the run above had load
  13-69).
- iOS boot and settle on a quiet host (the runs above had other load).
- Time per automation step (tap, then screenshot) through the socket API against `adb` on the SDK
  emulator (`simctl` has no tap).
- Host Spotlight indexing of simulator data: `mdfind` lists 13,858 items under one booted-once
  device's directory (6,562 files), so the host's `mds_stores` indexes what devices write. What
  excluding `~/Library/Developer/CoreSimulator` from Spotlight saves a farm needs a change to the
  host's Spotlight privacy settings to measure.
- Host CPU of an idle booted device, emu against the SDK emulator.
- API 29 and 30 cold boots (a new AVD's first), API 30 warm boots, and their idle host CPU: an
  API 29 or 30 device under emu sat at 150–170% of a core after the AutoMobile harness, with
  `com.android.phone`, `system_server` and `logd` busy in the guest; not yet compared with stock.
- Network throughput and latency through emu's NAT against the SDK emulator's slirp.
- Host CPU of an idle booted device with and without the virtio-snd card (`EMU_VIRTIO_SND=0`),
  median of 3 boots each on the same host and day (an earlier sample on API 36 showed no
  difference, but its boots and command weren't recorded).
- A farm of SDK emulators on the same host, to compare with the emu farm above.
