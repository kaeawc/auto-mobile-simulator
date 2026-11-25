# Benchmarks

Every number the README and [comparison.md](comparison.md) quote comes from this page. Each row
says what was run, on what, and when. The rules for keeping it current are in
[AGENTS.md](../AGENTS.md#benchmarks-and-public-docs).

## Android: emu against the SDK emulator

One API 36 `google_apis` arm64 AVD (4 vCPU, 2 GB) on an M3 Max (macOS 26.6.2), 2026-11-24, SDK
emulator 37.2.13.0. Medians of 3 boots of each kind; host load average 3–5 during the boots.

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

The `--ignored` benches run on any host:

```bash
cargo test --release -p emu-vmm vsock_throughput -- --ignored --nocapture
cargo test --release -p emu-vmm present_cost -- --ignored --nocapture
CASE=2480,1080,true cargo test --release -p emu frame_path_latency -- --ignored --nocapture
```

## iOS

The injected network shaper's cost per call is measured in
[research/netshape-overhead.md](research/netshape-overhead.md).

## Not yet measured

Claims that would belong in the README once someone runs them:

- `simee screenshot` against `xcrun simctl io screenshot` on a quiet host.
- iOS boot and settle on a quiet host (the runs above had other load).
- Time per automation step (tap, then screenshot) through the socket API against `simctl` and
  against `adb` on the SDK emulator.
- Host CPU of an idle booted device, emu against the SDK emulator.
- Network throughput and latency through emu's NAT against the SDK emulator's slirp.
- A farm of SDK emulators on the same host, to compare with the emu farm above.
