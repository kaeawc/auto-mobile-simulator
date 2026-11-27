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

First boots (`-wipe-data`) and warm boots of one API 29 `default` AVD under emu by host GLES
backend (#94), same host, 2026-11-26, rounds of first boot, warm boot, warm boot on SwiftShader,
3 rounds; host load average 10–14. Seconds from launch until `sys.boot_completed` and until the
launcher has focus (`dumpsys window`), polled once a second, so whole seconds. On Metal a first
boot never reaches the launcher (1 of 1 that day, 4 of 4 earlier: the renderer fault in
[android.md](android.md#performance-notes)); emu now runs a first boot on SwiftShader. Not yet
compared with stock's first boot.

```bash
target/release/emu -avd <avd> -port 5620 -wipe-data   # first boot; without -wipe-data: warm
ANGLE_DEFAULT_PLATFORM=swiftshader target/release/emu -avd <avd> -port 5620   # warm, SwiftShader
# then once a second: adb -s emulator-5620 shell getprop sys.boot_completed, and
# adb -s emulator-5620 shell dumpsys window | grep mCurrentFocus (Launcher)
```

| Boot | Host GLES | `sys.boot_completed`, median (runs) | Launcher, median (runs) |
| --- | --- | --- | --- |
| First | ANGLE on SwiftShader (emu's default) | 4 s (4, 3, 4) | 4 s (4, 4, 5) |
| First | ANGLE on Metal | 9 s (1 run) | never (watchdog restarts) |
| Warm | ANGLE on Metal (emu's default) | 5 s (4, 5, 7) | 6 s (6, 5, 7) |
| Warm | ANGLE on SwiftShader | 4 s (4, 3, 4) | 4 s (4, 4, 5) |

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

## Android: software mode (Linux without KVM)

The CPU decision for hosts with no hardware virtualization
([#55](https://github.com/kaeawc/simee/issues/55)), measured on its motivating host: a
Firecracker x86_64 guest (Ubuntu 22.04, 4 vCPUs of a Xeon at 2.2 GHz with AVX-512, 3.8 GiB RAM,
no `/dev/kvm`, no GPU), 2026-11-26. One API 36 `default` image of each architecture (r2; the
google_apis x86_64 image's 4.2 GB `system.img` didn't fit this host's 10 GB disk beside the
toolchain), 4 vCPUs and 2 GB, booted by plain QEMU 11.1.3 TCG with stock's disk layout and no GPU,
so these time the translator alone, before any of emu's devices. Without a GPU SurfaceFlinger
can't start, so the milestones are a root shell on the console and zygote's first start
(`boot_progress_start`, in guest uptime). Then the framework is stopped and android-bench.sh's
guest CPU microbenchmarks run. Medians of 3 warm boots after one uncounted first boot; each boot's
microbenchmarks are the median of 3 rounds.

```bash
scripts/tcg-bench.py <sysdir> <workdir> --qemu <qemu-system-x86_64|aarch64> [--cpu MODEL] [--boots 3]
```

| Guest | CPU model | Shell | Zygote starts | 100k shell loop | 4× md5 of 64 MiB | QEMU peak RSS |
| --- | --- | --- | --- | --- | --- | --- |
| x86_64 | `max,-erms,-fsrm,-fzrm,-fsrs,-fsrc` | 16.5 s | 49.4 s | 9.8 s | 1.82 s | 1138 MB |
| x86_64 | `max` | 18.9 s | 54.5 s | 10.1 s | 2.75 s | 1126 MB |
| arm64 | `max,pauth=off,sve=off` | 13.8 s | 55.6 s | 17.3 s | 3.67 s | 1113 MB |

The same microbenchmarks under emu on Hypervisor.framework take 0.22 s and 0.43 s (M3 Max,
[above](#android-emu-against-the-sdk-emulator)). Single boots of other configurations on the same host and day, each with the median of 3
microbenchmark rounds (`--cpu`, `--tb-size`, `--threads`; the x86_64 ones with the guest's
`mitigations=off`, which made no difference, see below), which decided the defaults:

| Configuration (single boot) | Zygote starts | 100k shell loop | 4× md5 |
| --- | --- | --- | --- |
| x86_64, `max` without the fast-string features (the best row's model) | 50.1 s | 11.0 s | 1.98 s |
| x86_64, Haswell model (AVX2) without ERMS | 56.3 s | 10.7 s | 2.23 s |
| x86_64, Haswell model with ERMS | not recorded | 10.2 s | 3.03 s |
| x86_64, SSE4.2-only model (`qemu64,+ssse3,+sse4.1,+sse4.2,+popcnt,+cx16`) | 66.0 s | 10.4 s | 2.84 s |
| x86_64, Haswell without ERMS, translation cache 256 MiB instead of QEMU's default 1 GiB | 54.5 s | 13.9 s | 3.43 s |
| x86_64, Haswell without ERMS, one translator thread for all vCPUs (`thread=single`) | 115.0 s | 17.7 s | 11.7 s |
| arm64, `max,pauth=off,sve=off`, booted in the same sequence as the next three | 72.4 s | 19.3 s | 4.53 s |
| arm64, pointer authentication emulated (`pauth-impdef=on`) | 78.3 s | 19.9 s | 4.85 s |
| arm64, `max` (QARMA pointer authentication) | 77.1 s | 20.2 s | 4.55 s |
| arm64, `cortex-a76` | 97.0 s | 19.4 s | 6.33 s |

The arm64 single boots ran second to fifth after a first boot and reached zygote later than the
three counted boots above; compare them with each other.

What decided it:

- **x86_64 images on x86_64 hosts.** Same-ISA translation runs guest code 1.8 (shell loop) to
  2.0 (md5) times faster than the best arm64 configuration, and arm64 takes 13% longer to reach
  zygote, though it reaches the shell sooner (its kernel and first-stage init are lighter). What
  comes after zygote (the framework, ART) is guest code, so the guest-code ratio decides.
- **QEMU 11 or newer.** Ubuntu 22.04's QEMU 6.2 panics this guest within seconds under
  multi-threaded TCG (`int3` in `kmem_cache_alloc`: a CPU running the kernel's live-patching
  breakpoint after the patch finished); 11.1.3, which has QEMU's fix for it
  ([LP #2051965](https://bugs.launchpad.net/ubuntu/+source/qemu/+bug/2051965)), booted these
  20-odd boots without it. The window isn't closed: one of about a dozen full boots on emu's
  devices, before the boot rows below, hit the same panic.
- **Hide the fast-string features.** With ERMS/FSRM advertised the guest kernel's memcpy is
  `rep movsb`, which TCG runs as a byte loop: md5 of `/dev/zero` takes 51% longer and zygote
  starts 10% later.
- **Keep the 1 GiB translation cache and one translator thread per vCPU.** A single translator
  thread, the model a translator library such as Unicorn offers, takes twice as long to reach
  zygote (115 s against 56.3 s for the same CPU model) and five times as long for the parallel
  md5s, so option 2 of #55 is not pursued.
- `mitigations=off` in the guest made no difference: a single boot with it (zygote 50.1 s, md5
  1.98 s) falls inside the spread of an earlier set of 3 boots without it (48.8 to 50.1 s, 1.84 to
  2.05 s).

adb into the same guest under QEMU 11 TCG on the same host, through emu's virtio-vsock served
over vhost-user (`crates/emu-vmm/examples/vhost_user_vsock.rs`, emu's adb bridge on the host
side) and, for comparison on the same boot, through QEMU's own user-mode network to adbd's TCP
listener (`-netdev user,hostfwd`). Framework stopped; 21 `adb shell echo` round trips and 3
transfers of 64 MiB each way per round, two rounds alternating, every transfer byte-checked.

| Transport | `adb shell echo`, median (2 rounds) | 64 MiB push | 64 MiB pull |
| --- | --- | --- | --- |
| emu's vsock over vhost-user | 149, 131 ms | 42 to 54 MiB/s | 62 to 99 MiB/s |
| QEMU's user network to adbd's TCP port | 134, 125 ms | 38 to 45 MiB/s | 13 to 18 MiB/s |
| emu's vsock, every queue called on each raise (before) | 157, 171 ms | 38 to 48 MiB/s | 47 to 66 MiB/s |

Calling every queue on each raise cost the guest a spurious interrupt per completion, because
vsock's queues share one MSI vector: 14 vsock interrupts per round trip before, 9 after
(`/proc/interrupts`, 20 round trips). emu's vsock server used 0.53 s of CPU for a whole boot plus
the 384 MiB of transfers, 21 MB peak RSS: what's left per round trip is the guest under
translation. The SDK emulator's own adb path has no row: it doesn't start on this host (below).

The SDK emulator (37.3.10) does not start on this host as first set up: it requires 7.37 GB of
free disk to create userdata whatever the AVD's data size (2.2 GB was free), and warns that
3939 MiB is under its suggested 4096 MiB of RAM. Its `-accel-check` reports `/dev/kvm is not
found`. With the host grown to 8 GiB and a 20 GB disk it starts, and the boot rows below compare
against it.

The renderer on the same host, no VM: the SDK's Linux `libgfxstream_backend.so` (37.3.10, from
`emu -provision-renderer`) on lavapipe, initialized Vulkan-only as emu runs it, then a 720×1280
RGBA color buffer uploaded and read back the way frames are (median of 9 reads per run), 5 runs
each. Before this, `-provision-renderer` fetched Apple silicon's archive on every host, so Linux
had no renderer it could load.

```bash
cargo run --release -p emu-gfxstream --example renderer_caps -- -vulkan-only <lib64>
```

| Vulkan driver | Renderer init, median (runs) | 720×1280 readback, median of the runs' medians |
| --- | --- | --- |
| The SDK's lavapipe (`lib64/vulkan`) | 108 ms (108, 112, 140, 104, 91) | 1.26 ms |
| The host's Mesa lavapipe (Ubuntu 22.04, LLVM 15) | 143 ms (105, 155, 143, 140, 146) | 1.21 ms |

emu uses the SDK's: the two are equal here, and the SDK's is the same on every host. Rendering
throughput, where they could differ, needs a booted guest. Booted, the SDK's lavapipe crashed in a
render thread on every boot, so boots render on its SwiftShader (below).

### Boot to `sys.boot_completed` with emu's devices

The same host, grown to 8 GiB of RAM and a 20 GB disk, 2026-11-26. The API 36 `default` x86_64
image, 4 vCPUs and 2 GB, under QEMU 11.1.3 TCG with emu's devices
(`crates/emu-vmm/examples/soft_devices.rs`: vsock, console, net and the GPU over vhost-user, the
goldfish address space and pipe over vfio-user). The renderer is the SDK's gfxstream on its
SwiftShader. Five warm boots after one uncounted first boot. Each boot's time runs from QEMU's
start to `getprop sys.boot_completed` reading 1 on the console. Host CPU is QEMU's plus emu's,
over 60–120 s after boot complete (the window the Mac rows use) and over 240–300 s. Peak RSS is
each process's `VmHWM`.

```bash
scripts/tcg-bench.py <sysdir> <workdir> --qemu <qemu-system-x86_64> --emu-devices target/release/examples/soft_devices \
  --lib64 <renderer lib64> --icd swiftshader --boots 5
```

| Run | Failed boots | Boot to `sys.boot_completed`, median (runs) | Host CPU 60–120 s after | 240–300 s after | Peak RSS, QEMU + emu |
| --- | --- | --- | --- | --- | --- |
| emu's devices | 0 of 5 | 168 s (165.6, 150.5, 168.0, 183.7, 181.0) | 2.14 cores | 0.75 cores | 3058 + 233 MB |
| Before: goldfish writes posted | 2 of 5 (a guest panic, a hang) | 170 s (180.9, 170.0, 160.8) | 3.94 cores | not measured | 2992 + 173 MB |

The first boot of a fresh userdata took 375.5 s. With QEMU's default posted writes on the
goldfish devices, one boot panicked (`zap_huge_pmd`) and one never completed (SurfaceFlinger
crashed in `init`, zygote restarted). `x-no-posted-writes` fixed both (docs/android.md, "Software
mode"). The "before" CPU column was measured from 60 s after boot complete only; most of that
load is the guest's post-boot work, and host CPU settles at about a third of it by 240 s.
On one interactive boot, about 4 minutes after boot complete, `top -H` showed QEMU's four vCPU
threads at about 16% of a core each, its main loop at 7%, and emu under 1%. The guest's own
`top` showed it 84% idle; the rest was system_server, telephony and logd.

Under emulation, apps can exceed Android's startup timeouts. One interactive boot showed "System
UI isn't responding" after com.android.phone overran its startup (its main thread runnable, not
blocked).

The SDK emulator's software mode on the same host and day: 37.3.10 with the same image in an AVD
of the same shape (4 vCPUs; stock raised its 2 GB to 2560 MB). It runs with `-accel off`, which
it refuses unless `ANDROID_I_WANT_MY_TCG=yes` is set, plus SwiftShader and `-no-snapshot`. One
uncounted first boot, then three, each with a 40-minute budget.

```bash
scripts/tcg-bench.py <sysdir> <workdir> --stock <sdk>/emulator/emulator --avd <x86_64 avd> --boots 3 --budget 2400
```

| Run | Boot to `sys.boot_completed`, median of 3 (runs) | Host CPU 60–120 s after | 240–300 s after | Peak RSS |
| --- | --- | --- | --- | --- |
| emu's devices (above) | 168 s | 2.14 cores | 0.75 cores | 3058 + 233 MB (3.29 GB) |
| The SDK emulator, `-accel off` | 2302 s (over 2400, 1603.3, 2301.9) | 1.08 cores | 1.09 cores | 3116 MB (3.12 GB) |

The SDK emulator's software mode runs every vCPU on one translator thread (`top -H`: one QEMU thread
at 93% of a core, the rest near idle). Its first boot and first counted boot didn't reach
`sys.boot_completed` in 40 minutes. In the guest, `system_server` was killed by its watchdog 4
times, and zygote started 5 times in the first. The two that completed took 27 and 38 minutes,
13.7 times emu's median. Stock idles at about one core because its single translator thread
never goes idle. In this mode it also gives the guest GLES over the pipe (`gltransport=pipe`,
`skiagl`) rather than ANGLE. Builds of emu ran at the lowest priority on 2 of the 4 cores during
some of the counted boots.

### Software mode through `emu -avd`

`emu -avd` booting the same AVD itself (#106), same host and day, three rounds. Each round is a
warm boot, a 60 s settle, three runs of `scripts/android-jank.sh emulator-5554 <emu pid>` (10
launcher swipes up and down, then the launcher's `dumpsys gfxinfo`; host CPU covers emu and its
QEMU), and `scripts/ios-step-bench.py <socket> x86_36 20 360 600` through `simee serve` (20 calls
of each step on an empty spot of the workspace, after a warm-up). Boot times are emu's own `boot
completed` line. Medians of the 3 boots, the 9 jank runs, and the 3 step runs' medians.

| Measure | emu, software mode | The SDK emulator, `-accel off` (one boot) |
| --- | --- | --- |
| Warm boot to `sys.boot_completed` | 161.0 s (156.2, 165.5, 161.0) | 850 s, then unusable (below) |
| Launcher swipes: frame time p50 / p90 / p99 | 150 / 300 / 400 ms | not measurable |
| Launcher swipes: janky frames | 96.7% | not measurable |
| Host CPU while swiping | 2.46 cores | 1.06 cores |
| `tap` (holds 60 ms) | 60.8 ms | no display socket (adb only) |
| `screenshot` to a file / as base64 | 36.9 / 61.1 ms | no display socket |
| `tap`, then `screenshot` to a file | 101.4 ms | no display socket |

The stock emulator's boot set `sys.boot_completed` after 850 s. In the minutes after it, the guest
had no `package`, `window`, `input` or `gfxinfo` service (`cmd: Can't find service`), so every
swipe and frame query failed. It then crashed with SIGSEGV when told `emu kill`. simee's step
bench drives input and frames over emu's display socket, which the stock emulator doesn't have.

The launcher draws at about 7 frames a second in software mode: every frame's guest-side work runs
translated, and SwiftShader renders it on the same 4 cores. Taps and screenshots don't depend on
the guest's speed. A tap is the API's 60 ms hold plus under 1 ms, and a screenshot reads the last
frame emu already holds.

`kfence.sample_interval=0` (#106, the `int3` panics) against the image's KFENCE sampling (every
500 ms), through `scripts/tcg-bench.py --emu-devices ... --append`, four alternating rounds, one
boot of each per round:

| Arm | Boot, median (runs) | Host CPU 60–120 s after | 240–300 s after | Panics |
| --- | --- | --- | --- | --- |
| KFENCE sampling off | 165.4 s (175.9, 160.7, 161.1, 169.6) | 1.82 cores | 0.73 cores | 0 of 4 |
| KFENCE every 500 ms | 167.3 s (167.3, 164.3, 184.6, 167.2) | 1.67 cores | 0.74 cores | 0 of 4 |

Turning sampling off costs nothing measurable. The fix rests on the mechanism, not on a panic
rate, which these 8 boots are too few to show. A warm `emu -avd` boot had panicked with `int3`
in `__kmem_cache_alloc_node` 101 s in, on three CPUs at once. Lowest-priority builds of emu ran
on 2 cores during the 184.6 s run.

The guest display's refresh rate, 30 Hz against 60 (`androidboot.qemu.vsync`, which emu takes
from the AVD's `hw.lcd.vsync`), through `scripts/tcg-bench.py --emu-devices ... --vsync N`, three
alternating rounds, one boot of each per round. The ranchu composer marks its present fences
unreliable, so SurfaceFlinger keeps hardware vsync on even when nothing draws. On an idle
software-mode guest, Perfetto showed 60 composer callbacks a second, each a binder call that
SurfaceFlinger handles: 3.2 s of guest CPU per 14.5 s.

| Refresh rate | Boot, median (runs) | Host CPU 60–120 s after | 240–300 s after |
| --- | --- | --- | --- |
| 30 Hz | 169.0 s (154.4, 175.8, 169.0) | 1.94 cores | 0.61 cores (0.61, 0.69, 0.61) |
| 60 Hz (the default) | 175.7 s (169.2, 175.7, 177.6) | 2.12 cores | 0.73 cores (0.72, 0.74, 0.73) |

30 Hz settles 0.12 cores lower, and it was lower in every round. Software mode draws about 7
frames a second, so a 30 Hz display costs no frames. But apps see a 30 Hz display, so 60 stays the
default, and an AVD opts in with `hw.lcd.vsync=30`.

On GitHub's `ubuntu-latest` (4 vCPUs, `.github/workflows/software-mode.yml`), the first boot of
a fresh userdata reached `sys.boot_completed` in 243.9 s (one run). This host's first boot took
381.4 s.

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
| Kick the target vCPU on an interrupt: first-boot `/data` encryption | 22 s | 8 s | M-series Mac, an earlier macOS |
| No kicks on an interrupt (`EMU_VCPU_WAKE` default `targeted` → `none` on macOS 26 and later): first boot (`-wipe-data`) of a 720×1280 API 36 phone, median (runs). A tie: the arms differ by less than the run-to-run spread | 11.4 s (11.4, 11.5, 11.4) | 11.6 s (11.8, 11.4; a third low-load run is owed) | M3 Max, macOS 26.6.2, 2026-11-26, runs at 1-minute load 7.7 to 8.1 (16 CPUs), arms interleaved, `EMU_VCPU_WAKE=targeted\|none scripts/android-bench.sh <label> <avd> emu -wipe-data` |
| Same runs, vCPU0 guest entries / emu CPU-seconds to `sys.boot_completed`, first boot, median | 348k / 21.7 | 257k / 22.1 | same runs (`vcpu0_runs`, `cpu_s`) |
| Same change, warm boot, median of 6 | 4.2 s (3.5 to 4.7) | 3.7 s (3.7 to 5.2) | same AVD and hour, host load 9.6 to 13.2 |
| Same change, vCPU0 guest entries to `sys.boot_completed`, first / warm boot, median | 338k / 59.7k | 249k / 41.4k | same runs (`vcpu0_runs`) |
| Same change, emu CPU-seconds to `sys.boot_completed`, first / warm boot, median | 25.0 / 9.3 | 23.4 / 8.7 | same runs (`cpu_s`) |
| Same change, launcher swipes a minute after boot: janky frames, median of 6 (2 boots × 3 runs) / emu CPU | 0.40% / 28% of a core | 0.20% / 25% | same AVD, host load 5 to 11, `scripts/android-jank.sh emulator-5600 <pid>` |
| Renderer init with Metal's shader cache cold / warm (mostly ANGLE's Metal shader library) | 4.5 and 5.1 s (2 of 20 launches) | 0.33 s (median of the other 18) | M3 Max, 2026-11-26, `emu -verbose` (`renderer initialized in`) |
| Same init without a guest, as an API 36 boot makes it: cold (an empty cache) / warm, median of 3 | 4.36 s (4.31, 4.36, 4.39) | 0.08 s (70 to 96 ms, 6 runs) | M3 Max, 2026-11-26, host load 23 to 27, `cargo run --release -p emu-gfxstream --example renderer_caps -- -emu <lib64>`, each cold run under a new bundle id |
| emu with its own Metal cache (bundle `dev.simee.emu`): repeat boots of one binary at one path, renderer init / launch to `sys.boot_completed`, median (runs) | 1.01 s (4.69, 1.01, 0.35) / 11.7 s (20.0, 11.7, 8.3): the shared cache missed in 2 of 3 boots | 0.34 s (0.33 to 0.36) / 7.8 s (7.8, 7.8, 7.8, 9.8, then 7.8, 8.4, 8.0 in a third interleaved round), 7 boots (2 with the ring-yield experiment of android.md "Ring polling", since dropped); the third round's before arm, run the same minute, was warm too (init 0.32 to 0.36 s, boot 7.8, 7.8, 9.4 s, median 7.8), so that round did not reproduce the shared cache's misses | API 36 phone 720×1280, 4 vCPUs, M3 Max, 2026-11-26, arms interleaved, both binaries warmed first; host load 8 to 33 (before) and 9 to 22 (after), 15 to 17 in the third round (2026-11-26, 08:57 to 08:59); `emu -verbose` (`renderer initialized in`), `adb shell getprop sys.boot_completed` polled every 0.25 s from launch |
| Same boots, launcher swipes 30 s after `sys.boot_completed`: janky frames / emu CPU, median (runs). Not a difference: both arms swung run to run on a host other work was loading (the 1-minute load reached 114 during the run) | 3.1% (3.1, 1.1, 27.0) / 30% (28, 30, 55) | 1.1% (0, 20.4, 0, 2.3) / 34% (28, 38, 30, 38) | same runs, `scripts/android-jank.sh emulator-5600 <pid>` |
| A fresh install path: the boot-shaped renderer init (`emu -warm-renderer <lib64>`, timed) without / after `emu -provision-renderer` from that path, 3 interleaved rounds | 5.42, 0.83, 0.73 s (cold once: later fresh copies were warm) | 0.78, 1.00, 0.78 s | M3 Max, 2026-11-26, host load 180 to 210 (every number slow), a new copy of one emu build per run |
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

### `launch`, with and without `restart`

iPhone 17 Pro, iOS 27.2 (24B8117), `com.apple.Preferences`, through a release `simee serve`,
median of 10, host load 9–18 (2026-11-27, M3 Max).

| Case | ms |
| --- | --- |
| Not running (1 s after `terminate`), `restart` true or false | 32 |
| Right after `terminate` | 122 |
| Right after `terminate`, after waiting for the old PID to exit | 121 |
| Running, `restart: true` (killed and cold-started, new PID) | 136 |
| Running in the background, `restart: false` (same PID, brought forward) | 29 |
| Running in front and settled, `restart: false` | 17 |
| 1 s after a cold launch, `restart: false` | 2120 |

The old process has already exited when `terminate` returns, so waiting on its PID saves
nothing. The service instead holds a launch for about 100 ms after a terminate (129 ms at a 0 ms
gap, 73 ms at 50 ms, 32 ms from 100 ms on). `restart: false` waits for a launch or activation
still in progress: about 3.15 s after a cold launch began, up to ~0.8 s after one brought the app
forward.

### Per-call device lookup on a crowded host

iPhone 17 Pro, iOS 27.2 (24B8117), booted, on a host with 90 simulators and 112 AVDs, through a
release `simee serve`, one persistent connection, median of 20 round trips over the socket (the
median of 3 runs, the builds before and after #120 interleaved against the same device), 1-minute
load 10-23 (2026-11-26, M3 Max, macOS 26.6). "Before" sorted every simulator
(`-[SimDeviceSet devices]`) to find one by UDID, listed the AVD home to route each call, and read
the framebuffer size through a remote proxy on every `screen` and tap; "after" looks the UDID up
in the set's dictionary (`devicesByUDID`), stats one `<name>.ini`, and keeps the size per boot
until a rotation or fold.

| Method | before (ms) | after (ms) |
| --- | --- | --- |
| `location` | 1.54 | 0.66 |
| `screen` | 0.34 | 0.04 |

What `location` has left is the service's own round trip (`host_support_mig_set_location_lat_lon`,
~95% of its samples).

### Introspection: `processes`, `connections`, `kill`, `network_state`

iPhone 17 Pro, iOS 27.2 (24B8117), freshly booted (~205 device processes, ~1200 on the host),
through a release `simee serve`, median of 10 round trips over the socket, the build before and
after #118 against the same device back to back, host load 19–34 (2026-11-27, M3 Max, macOS
26.6.2). `kill` is of a just-launched `com.apple.Preferences`. Host children counted by polling
the server's `proc_listchildpids` every 0.2 ms over 5 calls.

| Method | `ps`/`lsof`/`kill` (ms) | libproc (ms) | Host spawns per call, before → after |
| --- | --- | --- | --- |
| `processes` | 141.6 | 3.5 | 2 → 0 |
| `connections` | 190.7 | 2.5 | 3 → 0 |
| `kill` | 126.3 | 2.5 | 2 → 0 |
| `network_state` | 385.4 | 198.6 | 3 → 0 |

For scale, `crash_reports` (a device lookup and a directory listing) takes 1.5 ms on the same
server; the libproc reads themselves take 0.9 ms (`processes`) and 0.5 ms (`connections`) in
process. `network_state`'s remaining
time is its in-device `launchctl getenv` (for `injected`), which runs under the device's
`launchd_sim`, not the server.

### iPad `pointer` click

iPad Pro 13-inch (M5), iOS 27.2 (24B8117), the InputProbe test app (it logs where each click
lands), `pointer` `action: click` through a release `simee serve`, one persistent connection,
20 targets spread over the screen; a fresh server for each of 10 runs (its first click homes),
the first run clicking all 20 targets and the other 9 three. Before and after #122 built from the
same tree into separate target dirs, against the same device back to back (2026-11-27, M3 Max,
macOS 26.6.2). Quiet is host load 9-37 from other work; loaded adds 32 `yes` processes (load
14-96). Before #122 every call homed the pointer; now a server moves on from where its last call
left it, in steps of up to 120 pt instead of 10 pt, and homes on its first call, after a failure
or rotation, and every 100 calls.

| Case | before (ms) | after (ms) |
| --- | --- | --- |
| First click (homes), quiet, median of 10 | 3493 | 1839 |
| Later click, quiet, median of 37 | 3641 | 408 |
| First click (homes), loaded, median of 10 | 3638 | 1849 |
| Later click, loaded, median of 37 | 3803 | 412 |

All 20 clicks landed on their targets in every case: within 0.36 pt (median 0.28) before and
0.10 pt (median 0.00) after, quiet and loaded, read back from the app's log. 120 later clicks to
random points after one homing stayed within 0.32 pt. A later click is now the move (23
reports at most, 16 ms apart, sent against deadlines), 64 ms for the move to land and the 80 ms
press.

## MCP server

What the MCP server's protocol layer costs to build, against the official Rust SDK. Linux VM,
4 vCPUs, 2026-11-26, `CARGO_BUILD_JOBS=2`, rustc 1.99.1.

| Measure | `sim-mcp` (hand-written, on `serde_json`) | `rmcp` 3.11 |
| --- | --- | --- |
| Crates added to `Cargo.lock` | 1 (`sim-mcp`) | 75 in the normal dependency tree |
| Clean dev build of a program that depends on it, empty `main` | not measured alone: 0 new dependencies to build | 41 s wall (62 s user, 11 s system) |

`rmcp` was built from a scratch package with `rmcp = { version = "3.11", default-features = false,
features = ["server", "transport-io"] }` and `tokio` with `rt`, `macros` and `io-std`; the crate
count is `cargo tree --prefix none -e normal | sort -u | wc -l` in it (145 with its dev
dependencies). `Cargo.lock` has 140 packages in all. Why this decides the protocol layer:
[mcp.md](mcp.md#why-the-protocol-layer-is-hand-written).

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

- Whether a farm's guest shaders, which share emu's per-bundle Metal cache (`dev.simee.emu`), push ANGLE's shader library out of it, so renderer init goes cold again on a busy farm.
- First boot of a fresh install with and without `emu -provision-renderer`'s renderer warm-up,
  launch to `sys.boot_completed` on a quiet host (only the renderer init itself is measured,
  at load 180 to 210). Each round needs a temporary `ANDROID_HOME` and so a 420.6 MB download of
  the emulator package (`-provision-renderer -dry-run`), which wasn't run without the owner's
  go-ahead; the host load was also 15 to 17, not under 16.

- First boot with and without interrupt kicks at a 1-minute load under 8: three more runs per arm (two `none` runs and three `targeted` runs are published), and the `targeted` default below macOS 26 has no measurement on that macOS.
- `simee screenshot` against `xcrun simctl io screenshot` on a quiet host (the run above had load
  13-69).
- iOS boot and settle on a quiet host (the runs above had other load).
- Time per automation step (tap, then screenshot) through the socket API against `adb` on the SDK
  emulator (`simctl` has no tap).
- Host Spotlight indexing of simulator data: `mdfind` lists 13,858 items under one booted-once
  device's directory (6,562 files), so the host's `mds_stores` indexes what devices write. What
  excluding `~/Library/Developer/CoreSimulator` from Spotlight saves a farm needs a change to the
  host's Spotlight privacy settings to measure.
- Time per automation step through `simee mcp` (a `tools/call` of `tap`, then `screenshot`) against
  the socket API and the CLI on the same device.
- iPad `pointer … --to` drag latency on a quiet host, and `pointer` timing and placement on
  iOS 26.5 since #122 (clicks on 27.2 are under "iPad `pointer` click").
- Host CPU of an idle booted device, emu against the SDK emulator.
- API 29 first boots against stock, API 30 first and warm boots, and their idle host CPU: an
  API 29 or 30 device under emu sat at 150–170% of a core after the AutoMobile harness, with
  `com.android.phone`, `system_server` and `logd` busy in the guest; not yet compared with stock.
- Network throughput and latency through emu's NAT against the SDK emulator's slirp.
- Host CPU of an idle booted device with and without the virtio-snd card (`EMU_VIRTIO_SND=0`),
  median of 3 boots each on the same host and day (an earlier sample on API 36 showed no
  difference, but its boots and command weren't recorded).
- A farm of SDK emulators on the same host, to compare with the emu farm above.
