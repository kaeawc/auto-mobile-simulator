# Where software mode spends its time (4 vCPUs, no KVM)

Research for [#55](https://github.com/kaeawc/simee/issues/55): `emu -avd` on a Linux host with no
hardware virtualization and no GPU, profiled phase by phase so the next performance work goes to
the biggest cost. Every run is one AVD on one host over about 6 hours; the tables say how many boots each number has.

## 1. Environment

One host for every number below, 2026-11-27.

| | |
| --- | --- |
| Host | Firecracker VM, Ubuntu 22.04, kernel 6.1.189, `/dev/kvm` absent, no GPU (Mesa llvmpipe/lavapipe only) |
| CPU | 4 vCPUs of an Intel Xeon at 2.2 GHz (`hypervisor`, `sse4_2`, `avx2`, `avx512f`; no `vmx`/`svm`) |
| Memory / disk | 7.8 GiB RAM, no swap; root disk 9.8 GB at the start (1.6 GB free once the image and renderer were fetched), grown to 20 GB part-way through |
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
(0.55–0.62 in every idle window of every run in this research except one, against 0.73 published). The first
swipe run of each boot draws about a third fewer frames than the second (the guest's ART and
SwiftShader are still warming) and burns about 0.9 cores more host CPU for them. The 60–120 s
window is the noisiest: 1.19 to 3.78 cores across three boots of the same AVD, because it
catches the guest's post-boot work at different points (system_server, the composer HAL and
Settings were the busiest guest processes there, section 3).

## 3. Where the host CPU goes, by phase

Method. Phases are the ones the published rows use: boot (emu start to `sys.boot_completed`),
60–120 s after, 240–300 s after (idle), and the two swipe sets (10 up and 10 down launcher swipes,
sent either over emu's display socket with `simee swipe`, which starts nothing in the guest, or
over adb with `scripts/android-jank.sh`, which starts one `input` process in the guest per swipe).

- **By thread**: the 2 s per-thread sampler (`utime+stime` of every thread of emu and of its QEMU
  child), on a boot without `perf` running. The vCPU threads are QEMU's `CPU n/TCG`; `Thread<0n>`
  in emu are SwiftShader's four rasterizer workers.
- **By symbol**: `perf record -a -g -F 299` over each phase, `perf report --no-children --sort
  comm,dso,sym`, on a second boot. Samples in the TCG code cache (an anonymous mapping that holds
  the code QEMU generated from the guest's) are one symbol, "translated guest code"; QEMU's own
  functions are grouped by what they do. `perf` didn't change the boot time (170.8 s with it,
  169.1 to 177.7 s without).
- Host CPU "cores" are `samples / total samples × 4`, idle included, so each row sums to 4.

### Threads, cores of the 4 (one boot, no perf)

| Phase | vCPU threads (4× `CPU n/TCG`) | SwiftShader workers (emu `Thread<0n>`) | emu's devices, console, display, GPU decoder | QEMU main loop and helpers | Total |
| --- | --- | --- | --- | --- | --- |
| Boot (168 s) | 2.16 | 0.56 | 0.03 | 0.08 | 2.83 |
| 60–120 s after (60 s) | 1.21 | 0.03 | 0.01 | 0.06 | 1.31 |
| Idle, 240–300 s (60 s) | 0.50 | 0.01 | 0.01 | 0.06 | 0.57 |
| Idle, 180–300 s (the 2 min window) | 0.51 | 0.01 | 0.01 | 0.06 | 0.58 |
| Swipes over the display socket (32 s) | 2.61 | 0.35 | 0.03 | 0.06 | 3.05 |
| Swipes over adb (44 s) | 1.62 | 0.55 | 0.03 | 0.06 | 2.26 |

Top threads by phase (cores): boot: each vCPU 0.53–0.56, each SwiftShader worker 0.14, QEMU main
0.06; idle: vCPUs 0.10–0.14 each, QEMU main 0.06, everything of emu's under 0.01; swipes over the
display socket: each vCPU 0.64–0.66, SwiftShader workers 0.34 in total.

emu's own device threads (vsock, the vhost-user kicks, the GPU, the sensors, the console) are 0.03
cores at their busiest and 0.01 idle: nothing in emu's Rust is a cost. The host never runs out of
cores either: no phase passes 3.05 of 4, and the vCPUs are 65% busy in the display-socket swipes. What bounds
a frame is how fast one guest thread runs, because the guest thread that draws a frame (the
launcher's UI thread, SurfaceFlinger) is one translated thread.

### Symbols, cores of the 4 (second boot, `perf`)

| Phase | Host idle | Translated guest code | TB lookup (`helper_lookup_tb_ptr`, `qht_*`) | Soft MMU (TLB refill, loads and stores) | Translating guest code | Other QEMU | SwiftShader (JIT'd shaders and driver) | QEMU main loop | emu device threads | Other processes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Boot | 1.03 | 0.67 | 0.60 | 0.39 | 0.13 | 0.40 | 0.64 | 0.06 | 0.02 | 0.07 |
| 60–120 s after | 2.67 | 0.39 | 0.31 | 0.30 | 0.03 | 0.16 | 0.02 | 0.06 | 0.01 | 0.05 |
| Idle, 240–300 s | 3.37 | 0.15 | 0.14 | 0.13 | 0.00 | 0.08 | 0.00 | 0.06 | 0.01 | 0.05 |
| Swipes (display socket; this run drew only 7 frames) | 1.08 | 0.76 | 0.65 | 0.58 | 0.22 | 0.40 | 0.19 | 0.06 | 0.01 | 0.06 |
| Swipes (adb) | 1.66 | 0.51 | 0.50 | 0.35 | 0.03 | 0.25 | 0.56 | 0.05 | 0.02 | 0.06 |

Top 10 symbols per phase (cores; `qemu` is QEMU's own code, the rest are named):

| Phase | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Boot | translated guest code 0.67 | SwiftShader shader JIT 0.55 | `helper_lookup_tb_ptr` 0.31 | `qht_lookup_custom` 0.15 | `probe_access_internal` 0.05 | `mmu_translate` 0.05 | `x86_get_tb_cpu_state` 0.04 | `tb_lookup_cmp` 0.04 | `tlb_set_page_full` 0.04 | `tb_htable_lookup` 0.03 |
| 60–120 s after | translated guest code 0.39 | `helper_lookup_tb_ptr` 0.15 | `qht_lookup_custom` 0.09 | `mmu_translate` 0.04 | `probe_access_internal` 0.04 | `tlb_set_page_full` 0.03 | `do_ld_8` 0.02 | `tb_lookup_cmp` 0.02 | `tb_htable_lookup` 0.02 | `do_ld4_mmu` 0.02 |
| Idle | translated guest code 0.15 | `helper_lookup_tb_ptr` 0.07 | `qht_lookup_custom` 0.04 | `probe_access_internal` 0.02 | `mmu_translate` 0.02 | `tlb_set_page_full` 0.01 | `tb_htable_lookup` 0.01 | `_raw_spin_unlock_irqrestore` (kernel) 0.01 | `tb_lookup_cmp` 0.01 | `do_ld_8` 0.01 |
| Swipes (display socket) | translated guest code 0.76 | `helper_lookup_tb_ptr` 0.30 | `qht_lookup_custom` 0.18 | SwiftShader shader JIT 0.17 | `probe_access_internal` 0.08 | `mmu_translate` 0.08 | `tlb_set_page_full` 0.05 | `tb_lookup_cmp` 0.05 | `do_ld4_mmu` 0.05 | `do_ld_8` 0.05 |
| Swipes (adb) | translated guest code 0.51 | SwiftShader shader JIT 0.51 | `helper_lookup_tb_ptr` 0.25 | `qht_lookup_custom` 0.14 | `probe_access_internal` 0.05 | `mmu_translate` 0.05 | `tlb_set_page_full` 0.04 | `tb_lookup_cmp` 0.03 | `do_ld_8` 0.03 | `x86_get_tb_cpu_state` 0.03 |

What the vCPU threads (the biggest consumer in every phase) spend their time on, as a share of
their busy time (the five TCG groups of the table above):

| Phase | Translated guest code | Finding the next block (TB lookup) | Soft MMU | Translating new code | Other QEMU |
| --- | --- | --- | --- | --- | --- |
| Boot | 31% | 27% | 18% | 6% | 18% |
| 60–120 s after | 33% | 26% | 25% | 3% | 13% |
| Idle | 30% | 28% | 26% | 0% | 16% |
| Swipes (display socket) | 29% | 25% | 22% | 8% | 15% |
| Swipes (adb) | 31% | 30% | 21% | 2% | 15% |

In boot the lookup is `helper_lookup_tb_ptr` 14% of vCPU time, `qht_lookup_custom` 7%, then
`tb_lookup_cmp`, `x86_get_tb_cpu_state`, `tb_htable_lookup` and `curr_cflags`; the soft MMU is
`probe_access_internal`, `mmu_translate`, `tlb_set_page_full`, `do_ld_*` and `victim_tlb_hit`.
Only about 30% of the time the vCPUs are busy runs the guest's own code, in every phase; 50 to
55% goes to jumping between translated blocks and to the soft TLB. Translating new code is a
boot-time cost (6 to 8%, and 0 when idle); once the cache is warm the cost of software mode is
dispatch and address translation, per instruction. Section 4 tests why they're so high.

### What the guest is doing

Guest uptime milestones from `logcat -b events` on a boot (ms of uptime; emu's `boot completed`
came 166.0 s after the start, 10 s after the guest's last milestone):

| Milestone | Guest uptime | Step took |
| --- | --- | --- |
| zygote starts (`boot_progress_start`): firmware, kernel, first-stage init, second-stage init | 55.4 s | 55.4 s |
| preload of classes and resources ends | 77.6 s | 22.1 s |
| system_server runs (`system_run`) | 82.6 s | 5.1 s |
| PackageManager scans the system partition and data ready (`pms_ready`) | 100.5 s | 17.9 s |
| ActivityManager ready (`ams_ready`) | 119.2 s | 18.7 s |
| screen enabled | 132.1 s | 12.9 s |
| boot animation stops (`sf_stop_bootanim`) | 155.1 s | 23.0 s |

Busiest guest processes (`dumpsys cpuinfo`, percent of one guest CPU): the 14 s from 16 to 30 s
after boot completes (load average 17): system_server 86%, the ranchu composer HAL 69% (59% user),
Settings 47%, SurfaceFlinger 30%, SystemUI 24%, the launcher 20%. In the idle window: system_server
30%, SystemUI 12%, the composer HAL 10%, SurfaceFlinger 8.3%, the sensors HAL 6.2%, the launcher 4.3%.
One guest thread's speed is what it waits on: system_server's boot is serial.

## 4. Why the vCPUs are slow, and what was tried

### The cause: every guest context switch flushes QEMU's TLB

Section 3 puts about 50% of the vCPUs' busy time in finding the next translated block and in the
soft TLB, and 30% in the guest's own code. QEMU's monitor (`info jit`, through an `emu` run whose
QEMU is a wrapper script that adds `-monitor unix:...`) says why. Guest `/proc/stat` and QEMU's TLB
counters, read together over the same 60 s of an idle guest, 4 minutes after boot complete:

| Over 60 s, idle | Guest context switches (`ctxt`) | QEMU "TLB partial flushes" | "TLB elided flushes" |
| --- | --- | --- | --- |
| Count | 34,143 (564 a second) | 34,172 (570 a second) | 107,956 |

One QEMU TLB flush per guest context switch, to within 0.1%, while "TLB full flushes" stays 0. Over
31 s of launcher swipes (display socket): 64,319 context switches (2,111 a second) and about 136,000
partial flushes. An x86 guest reloads CR3 when it switches address space; QEMU's TCG doesn't
implement PCID (`-cpu max,+pcid` on QEMU 11.1.3: `warning: TCG doesn't support requested feature:
CPUID[eax=01h].ECX.pcid`; the guest's `/proc/cpuinfo` has no `pcid` or `invpcid`), so each switch
drops the vCPU's soft TLB, and with it the per-vCPU jump cache that `helper_lookup_tb_ptr` consults
first. The next few hundred guest instructions refill both: `tlb_set_page_full`, `mmu_translate`,
`probe_access_internal` and `qht_lookup_custom` are the symbols that fill the vCPU rows of section
3. The guest isn't to blame either: its kernel reports `meltdown: Not affected`, so no page-table
isolation adds CR3 writes on every syscall, and `mitigations=off` changed nothing in earlier
measurements (docs/benchmarks.md).

Two more `info jit` readings: the translation cache held 622 MB of its 1,053 MB 4 minutes after
boot complete (1.00 M translated blocks) and 695 MB (1.12 M) after 31 s of swipes, and its flush
count was 1 throughout: translating is a one-off cost after boot, and the cache has room for the
workload of a short session.

### What was tried

Every run: the same AVD and image as section 2 (4 vCPUs, 1.5 GB, 720×1280), one boot per row unless
said, the same swipe scripts. The baseline is the six boots of sections 2 and 3: boot 171.7 s median
(161.0 to 179.3), launcher 186.6 s (176.2 to 197.2), idle 0.57 to 0.60 cores. Boot-to-boot spread
is ±9 s, so a difference under about 10 s is not a result.

| Configuration | Boot / launcher | Idle, 240–300 s | Launcher swipes over adb: frames, p50 / p90, janky | Result |
| --- | --- | --- | --- | --- |
| 3 vCPUs (`hw.cpu.ncore=3`) | 199.3 s / 226.1 s | 0.59 | 174, 200 / 350 ms, 98.3% | boot 16% later |
| 2 vCPUs | 434.7 s / 518.5 s | 0.58 | 161, 150 / 400 ms, 98.1% | boot 2.5× longer |
| QEMU confined to 3 cores (`taskset -c 0-2`), the renderer and emu on the 4th | 180.5 s / 203.0 s | 0.60 | 181, 200 / 350 ms, 97.2% | no change (within noise) |
| `hw.lcd.vsync=30` | 180.8 s / 204.4 s | 0.55 | 175, 150 / 300 ms, 89.7% | idle 0.04 lower; fewer janky frames because there are fewer deadlines |
| 540×960 at 240 dpi | 166.2 s / 196.4 s | 0.59 | 194, 150 / 200 ms, 94.3% | boot 3% earlier (noise); 8% more frames and p90 200 ms against 300 |
| Animations off (window, transition, animator scales 0) | 166.0 s / 191.7 s | 0.59 | 110, 150 / 300 ms, 98.2% | no gain: fewer frames because nothing animates |
| QEMU rebuilt with a 16× larger jump cache (`TB_JMP_CACHE_BITS` 12 → 16), 2 boots | 187.8 / 178.2 s; launcher 220.0 / 199.3 s | 0.82, 0.57 | 173, 150 / 300 ms, 97.1% and 182, 150 / 300 ms, 98.9% | slower: `tcg_flush_jmp_cache` took 0.13 to 0.17 cores (each flush clears 512 KB), the lookup share didn't fall (0.79 against 0.60 cores in boot) |
| 6 vCPUs on the 4 host cores, against a baseline boot from the same state (below) | 167.1 s / 176.1 s, against 149.4 s / 156.1 s | 0.59, against 0.64 | 107, 200 / 300 ms, 99.1%, against 113, 150 / 300 ms, 98.2% | boot 12% later |
| QEMU's translation cache 2 GiB (`tb-size=2048`) instead of the default 1 GiB, against the same state | 144.9 s / 150.8 s, against 143.3 s / 149.2 s | not measured | 12 min of `monkey` instead (below) | no difference |
| `-gpu lavapipe` (`LP_NUM_THREADS` 0 and 2) | emu exited 138 s and 66 s in | | | no boot, as docs/benchmarks.md records for the SDK's lavapipe |

The 6 vCPU and `tb-size` rows, and the baseline they are compared with, ran after the animation
scales were set to 0 for the "animations off" row. They persist in the AVD's userdata, so those
runs, `mon1` above and the `monkey` runs below had the animations off (107 to 113 frames over adb,
against 173 to 194 with them on), until the next `-wipe-data` reset them. Each of these rows is
compared with a boot from its own state: 6 vCPUs against 149.4 s / 156.1 s (idle 0.64, 113 frames, 98.2%
janky), `tb-size` against its own 1 GiB run. A fresh baseline after the reset (a second boot of
the wiped AVD, animations on) took 156.4 s / 175.6 s, idle 0.59, 181 frames, 97.8% janky, which is
the pre-reset baseline again. Boot-to-boot spread across the day was 143 to 187 s.

The first two rows say the guest is bound by single-thread speed, not by cores: 4 vCPUs are 54–65%
busy, and still taking one away costs 16% and two cost 150%. The larger jump cache is the direct
test of the flush explanation: more entries make each flush dearer and don't reduce the misses,
because the flushes empty it either way.

### The translation cache: 1 GiB is full after the boot

`info jit` every 30 s through a 12.5 minute `monkey` run (25% app switches, 700 ms throttle)
that began 2 minutes after boot completed, with QEMU's default 1 GiB cache and with 2 GiB
(`tb-size=2048` in `-accel`, through the wrapper), the same AVD state.

| | 1 GiB (default) | 2 GiB |
| --- | --- | --- |
| Cache in use when `monkey` starts | 500 MB, after 1 flush during the boot | 1,015 MB, 0 flushes |
| Cache after 30 s / 60 s / 150 s of `monkey` | 726 / 894 / 984 MB | 1,183 / 1,288 / 1,367 MB |
| Flushes | 2: one in the boot, one about 160 s into `monkey` (984 MB to 262 MB) | 0 (1,469 MB at the end) |
| Boot / launcher | 143.3 s / 149.2 s | 144.9 s / 150.8 s |
| vCPU host cores per 30 s, `monkey` period | 2.07 to 2.78 | 1.93 to 2.79 |
| Events `monkey` injected, app switches | 99, 192 | 98, 191 |

Booting translates about 1 GB of code (1.65 M blocks), so the default cache fills and flushes once
before the launcher, and once more after about 2.5 minutes of app switching. Neither flush is
visible: the boot is the same length, the vCPU host cores have no dip or step at the flush, and
`monkey` is throttled so it can't show a slower guest. A flush throws away about 1 to 1.5 M
blocks; at the 58 µs of vCPU time a block cost in the swipes (8% of 2.6 vCPU cores over 31 s for
the 111,000 new blocks `info jit` counted) that is about 60 to 90 vCPU-seconds of translation,
15 to 25 s of a 4 vCPU guest (an estimate, spread over the minutes after the flush). No result here
justifies passing `tb-size`.

### The first boot: apexd decompresses 24 APEX files, one at a time

A fresh userdata took 360.2 s to `sys.boot_completed` and 385.8 s to the launcher (median of 4
first boots, 360.1 to 366.7 s and 380.3 to 386.8 s), against 156 to 179 s for a warm one. The
guest's own milestones (`logcat -b events`, uptime) on a first and a warm boot:

| Milestone | First boot | Warm boot |
| --- | --- | --- |
| zygote starts (`boot_progress_start`) | 177.5 s | 52.2 s |
| `system_run` | 203.1 s | 77.6 s |
| `pms_ready` | 233.6 s | 95.3 s |
| `ams_ready` | 301.2 s | 115.0 s |
| `enable_screen` | 324.5 s | 128.2 s |
| boot animation stops | 348.8 s | 144.7 s |

122 of the 126 extra seconds before zygote are one wait: init's `Wait for property
'apexd.status=activated' took 118366ms`. On a first boot `apexd` decompresses the image's 24
compressed APEX files (`.capex`, for example `com.android.art`, `conscrypt`, `adservices`) into
`/data/apex/decompressed`, one after the other (24 `Decompressing` lines from 11:02:48 to
11:04:40), and finds them already there on every later boot. The host ran one vCPU's worth for
those 180 s (1.25 to 1.45 cores, against 2.8 in a warm boot). The rest of the extra time is
`system_server` after `pms_ready` (67 s to `ams_ready` against 20 s; a 110 s `main` handler
delivery in `UserManagerService`, logcat's "Slow delivery") and the boot animation.

Cloning a settled AVD skips all of it. `emu -clone-avd prof2 -from prof` took 12.8 s here (a plain
file copy of 2.1 GB of mostly sparse files, since this host's file system has no reflinks) and
the clone's first boot reached `sys.boot_completed` in 161.1 s and the launcher in 190.4 s (one
boot), against 360 and 386 s for a fresh AVD. docs/farm-android.md already describes clones for
the farm.

### savevm and loadvm

On the software-mode VM (emu's devices attached, the guest at the launcher), the QEMU monitor's
`savevm snap1` answers at once:

```
Error: State blocked by non-migratable device '0000:00:02.0/vhost-user-vsock'
```

`info snapshots`: "There is no snapshot available." QEMU names the first blocker; emu serves the
vsock, the console, net, touch, keyboard, rotary and GPU devices over vhost-user and the goldfish
address space and pipe over vfio-user, and the host-side state behind them (the renderer's
contexts and resources, open pipe connections, adb sessions) lives in emu, not in QEMU. So a booted
snapshot does not restore, and making one restore is work in emu, not a flag. Its payoff would be
the largest of anything in this document: the 172 s boot would become the time to map and
restore 1.5 GB.
