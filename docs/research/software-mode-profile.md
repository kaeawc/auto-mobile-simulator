# Where software mode spends its time (4 vCPUs, no KVM)

Research for [#55](https://github.com/kaeawc/simee/issues/55): `emu -avd` on a Linux host with no
hardware virtualization and no GPU, profiled phase by phase so the next performance work goes to
the biggest cost. Status: in progress; sections are added as each step lands (1 environment, 2 reproduction done).

## 1. Environment

One host for every number below, 2026-11-27.

| | |
| --- | --- |
| Host | Firecracker VM, Ubuntu 22.04, kernel 6.1.189, `/dev/kvm` absent, no GPU (Mesa llvmpipe/lavapipe only) |
| CPU | 4 vCPUs of an Intel Xeon at 2.2 GHz (`hypervisor`, `sse4_2`, `avx2`, `avx512f`; no `vmx`/`svm`) |
| Memory / disk | 7.8 GiB RAM, no swap; 9.8 GB root disk, 1.6 GB free after everything below |
| Toolchain | rustc 1.99.1 (rustup), release build of `emu` and `simee` from main at 5bcc3a8 |
| QEMU | 11.1.3 built from the release tarball (`--target-list=x86_64-softmmu --enable-slirp`, no docs, no GUI), `-j4` in 191 s, installed to a private prefix and passed as `EMU_QEMU`. Ubuntu 22.04's 6.2 is too old (docs/android.md) |
| Image | API 36 `default` x86_64, r2: `system.img` 1.9 GB, `vendor.img` 104 MB, kernel 20 MB (1.7 GB on disk) |
| Renderer | the SDK's Linux `libgfxstream_backend.so` 37.3.10 from `emu -provision-renderer` (255 MB on disk) |
| adb | platform-tools, 22 MB |
| AVD | an AVD named `prof` on that image: `hw.cpu.ncore=4`, `hw.ramSize=1536` (1.5 GB, to leave the 7.8 GiB host room for QEMU, emu, perf and the sampler), 720×1280 at 320 dpi, 2 GB data |
| Profiling | `perf record -a -g` (the 5.15 `perf` binary runs on this 6.1 kernel; `perf_event_paranoid` is -1) and a 2 s per-thread `/proc` sampler (`utime`+`stime` per QEMU/emu thread) |

Fetched, and only that: the system image zip, the renderer archive, platform-tools, the QEMU
source tarball (sha256 5a8289c7…84b22), the Rust toolchain. The documented route was used as
written, except that QEMU is built from source: no QEMU 11 package exists for Ubuntu 22.04.

## 2. Reproducing the published numbers

`docs/benchmarks.md` ("Software mode through `emu -avd`") publishes: warm boot 161.0 s, launcher
swipe frame time p50 / p90 / p99 150 / 300 / 400 ms with 96.7% janky frames, 2.46 host cores while
swiping, 2.14 cores 60–120 s after boot and 0.75 cores 240–300 s after. Here: one uncounted first
boot (fresh userdata), then 3 warm boots of the same AVD, each followed by 2 runs of
`scripts/android-jank.sh emulator-5554` (10 launcher swipes up and down, then the launcher's
`dumpsys gfxinfo`; 6 runs, where the published table has 9). Boot is emu's `boot completed` line,
"launcher" is the first time `dumpsys activity` reports the launcher resumed, host cores are
emu's plus QEMU's `utime+stime` from a 2 s per-thread sampler. Medians; the AVD has 1.5 GB of
RAM, not the published run's 2 GB (the 7.8 GiB host also holds `perf` and the sampler).

| Measure | This host, median (runs) | Published |
| --- | --- | --- |
| First boot of fresh userdata, to `sys.boot_completed` / launcher | 360.2 s / 386.8 s (one run) | 381.4 s |
| Warm boot to `sys.boot_completed` | 172.6 s (179.3, 161.0, 172.6) | 161.0 s |
| Warm boot to the launcher resumed | 187.7 s (197.2, 176.2, 187.7) | not published |
| Host CPU 60–120 s after boot | 2.37 cores (3.78, 2.37, 1.19) | 2.14 |
| Host CPU 180–300 s after boot (the 2 min idle window) | 0.58 cores (0.57, 0.58, 0.58) | 0.73 at 240–300 s |
| Launcher swipes, frame time p50 / p90 / p99 | 150 / 300 / 450 ms | 150 / 300 / 400 |
| Janky frames | 95.5% (93.9, 95.8, 94.9, 96.7, 95.2, 96.7) | 96.7% |
| Frames rendered per run of 20 swipes | 128 first run, 184 second (131, 189 / 118, 182 / 124, 180) | not published |
| Host CPU while swiping | 2.56 cores (3.04, 2.11, 2.95, 2.17, 2.97, 2.19) | 2.46 |

Everything reproduces within the run-to-run spread except idle, which is 0.15 cores lower here
(0.57–0.60 in all 15 idle windows measured in this research, against 0.73 published). The first
swipe run of each boot draws about a third fewer frames than the second (the guest's ART and
SwiftShader are still warming) and burns about 0.9 cores more host CPU for them. The 60–120 s
window is the noisiest: 1.19 to 3.78 cores across three boots of the same AVD, because it
catches the guest's post-boot work (package scanning, dex optimization) at different points.
