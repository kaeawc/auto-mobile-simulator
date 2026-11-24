# Running a device farm

How to start many devices on one Mac, get them quiet fast, and keep them cheap while they idle.
Measured with `scripts/farm-bench-ios.sh` (iOS) on a 16-core (12 P + 4 E) Apple-silicon MacBook
with 128 GB, iOS 27.2 runtime, iPhone 17 devices, while CI jobs shared the host (1-minute load
averages of 10-500 during the runs: treat single numbers as ±30 %).

## iOS simulators

### Quick start

```bash
# Once: a template that has done its first boot (data migration, indexing) and is shut down.
T=$(simee create farm-template "iPhone 17" com.apple.CoreSimulator.SimRuntime.iOS-27-2)
simee boot "$T" --settle --timeout 600 && simee shutdown "$T"

# Per farm: clones (APFS: ~15 MB of disk each), booted four at a time, each waited for until quiet,
# optionally with the slim profile (applied per device: a clone doesn't inherit it).
for i in 1 2 3 4 5 6 7 8; do simee clone "$T" farm-$i; done
simee boot-many farm-1 farm-2 farm-3 farm-4 farm-5 farm-6 farm-7 farm-8 --profile slim
```

`boot-many` waits for each device to settle by default (`--no-settle` doesn't); `--settle` is
still accepted. It exits non-zero only when a device failed to boot: one that booted but didn't
settle in time is printed as `settled=false` (with a `booted, not settled after N s` note on
stderr) and counted in `unsettled=`. `boot --settle` likewise exits 0 for a device that booted
but didn't settle, and prints the note.

Name runtimes by identifier (`com.apple.CoreSimulator.SimRuntime.iOS-27-2`): a host with an
Xcode beta can have two runtimes called "iOS 27.2".

### What "settled" means

`boot` returns when the service reports the device booted (terminal boot status) and its display
has a surface. That is 2-20 s after the call, while the device is still at 300-2400 % of a core:
a first boot migrates data and indexes for one to two minutes, every boot starts 190-300 daemons,
and poster, widget and Siri extensions keep starting for minutes after.

`boot --settle` (and `boot_many {settle}`, and `settle` for a device already booted) also waits
until the device's whole process tree (its `launchd_sim` and every descendant) uses under 10 % of
one core over 10 s with SpringBoard running (`settle_cpu_pct`, `settle_window_s` change the rule).
CPU is each process's accumulated CPU time, so it is exact over the window and a process
that exits doesn't make the tree look quieter. The threshold comes from the measurements: an idle
device runs at 1-6 % (no client attached), so 10 % separates "storm over" from "idle" with room
for one busy daemon; 5 % (the research brief's guess) never fired on some idle devices.

The wait reads the tree through libproc (`sim_coresim::procs`) once a second: the host's process
table (pid, parent, name), then the CPU time of the device's own processes. It used to spawn a
host-wide `ps -Ao pid=,ppid=,time=,args=` instead, 6.5-8.4 % of a core during a `boot_many` on a
host with ~2000 processes. Measured on this host with ~1200 processes and no device booted
(`cargo test -p sim-coresim --release poll_cost -- --ignored --nocapture --test-threads=1`, median
of 3): the `ps` listing cost 38.2 ms of CPU per poll; the table costs 0.24 ms, and reading CPU
costs 1.1 µs per process, so a poll of a 100-300 process device costs about 0.35-0.6 ms. Each wait
reads the table itself, so 8 waits at once cost about 4 ms a second, where one shared `ps` listing
cost 38 ms.

Each process's CPU is split by QoS: the `background_cpu_pct` of a result is what ran at the
maintenance or background QoS (launchd jobs of `ProcessType` Background, darwinbg tasks:
priority 4, on the efficiency cores), `foreground_cpu_pct` the rest, `cpu_pct` all of it. Once a
device is past its first-boot storm most of what it still runs is background work on the 4 E-cores
while the P-cores idle. `settle_foreground: true` (`foreground: true` for `settle`) judges the
tree on its foreground CPU only, so a device with only background work left counts as settled.
The default is unchanged (all CPU). Not yet measured on a device: how much sooner it fires. The
QoS split itself was checked on the host (`taskpolicy -b yes` lands 99.9 % in background CPU, plain
`yes` 0 %: `procs::tests::background_cpu_is_attributed_to_background_qos`).

The settle wait has its own budget, `settle_timeout_s` (`--settle-timeout`), counted from the end
of the boot, separate from `timeout_s` (the boot call). Its default is 600 s, and in `boot_many`
150 s per device in flight above K = 4 (K = 8: 1200 s): settle times measured 250-537 s per device
under load, and K storms at once make each one longer. A wait that runs out is not a failed boot:
the result says `settled: false` with `settle_error` (the CPU over the last window), `waited_s`,
`procs`, `cpu_pct`, `background_cpu_pct` and `foreground_cpu_pct`, and the device stays booted.
The standalone `settle` method still fails when it runs out of time (waiting is all it does), with
the same 600 s default.

Settled is not forever. Deferred work starts two to four minutes after boot (process count
190 → 240 on a clone, with 100-700 % bursts), so a device that settled at 60 s can be busy again
at 200 s. For a farm that is fine: settle says when the boot storm is over and the device can take
work; the later bursts are background work the device does anyway.

### Settle times

| Boot | N | `boot` returned (all) | settled (all, from outside) | simee `--settle` |
| --- | --- | --- | --- | --- |
| first boot, fresh device | 1 | 19 s | 97-107 s | — |
| first boot, fresh device | 2 | 30 s | 68-80 s | — |
| first boot, fresh device | 4 | 47 s | 421-460 s | — |
| first boot, fresh device | 8 | 373 s | 448-508 s | — |
| clone of a booted-once template | 1 | 1.5-20 s | 50-84 s | 50-79 s |
| clones, `boot-many -j 4` | 8 | 125 s | 65-245 s | 45-79 s each, all 124 s |
| clones, `boot-many -j 4`, quieter host | 8 | — | — | 44-85 s each, all 128 s |
| clones, second boot of the same devices | 4 | 16-25 s each | 251-263 s | 250-263 s (round 1: 152-291 s) |

Later boots were not measurably faster than a clone's first boot here (the host was at load 60+
during that run); what removes most of the work is the clone of a booted-once template.

"From outside" is the benchmark's own judge (same rule, sampled once a second from a separate
process); under load its samples are seconds apart, so it lags simee's own wait.

### Booting many at once: `boot_many` and K

`boot_many` boots a list with at most K boots (and settle waits) in flight; each worker thread has
its own service connection, so nothing in simee serialises the boots (the service's `boot` call
itself returns in ~1-20 s). K sweep, 8 clones, time until all settled:

| K | all settled (outside) | simee's own `all_s` |
| --- | --- | --- |
| 2 | 537 s | 537 s |
| 4 | 245 s | 124 s |
| 8 | 528 s | 297 s |

K = 8 starts eight storms at once and every device takes longer than four would in sequence; K = 2
leaves the host idle. The default is a quarter of the logical CPUs, at least 2 (4 here).

A slot is held through the profile and the settle wait, not just the boot call: what K bounds is
the number of boot storms in flight, which is what the sweep measured. Without the settle wait a
slot would be freed when the service's `boot` returns (1-20 s), long before the storm is over, so
K would bound only those calls and the storms of all N devices would overlap. That is why
`boot_many` settles by default (`settle: false`, `--no-settle`, opts out for a caller that only
wants the service's "booted"); `boot` still settles only when asked, since a single boot has no
slot to hold.

The shaper (`shaped`) is resolved once per `boot_many` call, on the caller's thread, before any
worker starts: K workers building it at once would race on the same cache files.

### Clones: no first boot per device

`clone` (CoreSimulator's `-[SimDeviceSet cloneDevice:name:error:]`, what `simctl clone` does)
copies a shut-down device: its data, settings and apps, as APFS clones. 4 s per clone; `du` says
2.2 GB per device but the volume's used space grows ~10-30 MB per clone. A clone of a template
that has done its first boot skips the data migration and indexing storm.

### The `slim` profile

`boot --profile slim` (or `simee profile <device> slim` on a booted one) disables (in the
device's own launchd database) and stops 58 daemons no AutoMobile tool and no simee method needs: PosterBoard and the
~15 poster extensions it keeps alive (~30-45 MB footprint each), widgets (chronod), Siri and
suggestions, photo and media analysis, Health, Home, fitness, the Store, iCloud sync, Mail, News,
Weather, Wallet, Game Center, Screen Time, watch and Find My companions, experiments and ML hosts.
The list and what was kept on purpose is `SLIM` in `crates/sim-api/src/ios.rs`.

| per device | fresh | clone | slim clone |
| --- | --- | --- | --- |
| processes | 253-297 | 217-244 | 113-153 |
| footprint (dirty memory, shared once) | 2.6-2.8 GB | 2.0-2.2 GB | 1.07-1.2 GB |
| CPU of 8 devices together, 60 s after settle | 869 % | 25 % | 7 % |

Applying the profile takes ~20 s on a quiet host, 24-66 s under load (the runtime has no shell, so
`launchctl disable` and `bootout` are a spawn each per job), counted in `settle_s`. The result
reports what launchd did, not what was asked: `disabled` counts the labels `launchctl
print-disabled system` lists as disabled afterwards, `failed` names the others (and any whose
`disable` printed an error; the "Please switch to user/foreground/…" warning a running job's
`disable` prints is not one), `not_loaded` the labels `launchctl list` didn't show before (already
stopped, or not a job in this runtime: `launchctl disable` accepts any label, so a mistyped one
shows up here).

Most disables survive a plain reboot (measured, iOS 27.2: a slimmed device shut down and booted
without `--profile` came up with 133 processes, its sampled jobs still listed disabled and none
loaded). Three don't: companionappd, NPKCompanionAgent and brookcompaniond come back listed
`enabled` and running after every reboot (measured on iOS 27.2: `launchctl print-disabled system`
and `launchctl list` after a reboot of a slimmed device). A clone doesn't inherit any of them
(`clone` resets them: a clone of a slimmed template came up with 221 processes). So pass
`--profile slim` on every boot, not only a clone's first. Applying it again is cheap: it reads
`launchctl list` and `print-disabled` first and leaves alone each job that is still disabled and
not loaded (the result's `already` counts them, and they aren't listed in `not_loaded`), so a
reboot costs those two reads, a disable and a bootout per job that came back, and the final
`print-disabled`. `simee profile <device> slim --undo` (`profile {name, undo: true}`) enables the
same jobs again; they run from the next boot (measured: 59 enabled in 13 s; after a reboot the
sampled jobs were loaded again and none listed disabled, 209 processes at settle against 133 slim).

`com.apple.sleepd` is kept. With it disabled, every later boot of a slimmed device spins:
SpringBoard and mobiletimerd retry the `com.apple.sleepd.sleepserver` mach service in a tight loop
(138k log lines in 3 s, about 350 % of a core), and the device never settles. Measured on iOS 27.2,
3 warm reboots each, judged from outside by a 2 s sampler (same 10 % / 10 s rule), load 4-60:

| warm reboot | settled | CPU-s to settle | processes | footprint |
| --- | --- | --- | --- | --- |
| stock | 48-110 s | 347-383 | 198-208 | 1.79-1.92 GB |
| slim without sleepd | 29-83 s | 69-142 | 94-113 | 733-922 MB |
| slim with sleepd disabled | never | ~350 % of a core, spinning | | |

`com.apple.carkitd` is not in the profile: `carplay` needs it. The kept jobs are pinned by a test
per simee method that reaches into the device (`carplay`, `location`, `privacy`, `push`,
`appearance`, the keyboard, `open_url`, `biometric`, `status_bar`, `notify`, `reset_keychain`,
`defaults_*`, install and launch, `logs`, network shaping, audio).

Kept: SpringBoard, backboardd, InputUI (keyboard), the accessibility server and daemons
(AutoMobile reads the screen through accessibility), installd, lsd, runningboardd, tccd,
usernotificationsd, locationd, sharingd, webbookmarksd (Safari), assetsd, sleepd, logging, configd_sim,
nsurlsessiond. AutoMobile's tools pass on a slim device (getApple, observe, pressButton, listApps,
launchApp, tapOn, swipeOn, terminateApp, openLink, homeScreen, getLogs).

### Forwarding to `simee serve`

A CLI call that runs in its own process costs several times more than one forwarded to a running
`simee serve` (measured: screenshot 164 vs 37 ms, tap 321 vs 83 ms; 38 ms with 8 devices
driven through one server). The server itself is cheap: 7 MB idle, 73 MB, 16 threads and 32 file
descriptors with one device driven, 124 MB, 14 threads and 32 descriptors with eight (each
device's input worker lets go after five idle minutes). On a farm run every caller with the
same `DEVELOPER_DIR`, `HOME`, `TMPDIR` and `ANDROID_*`/`EMU_*`/`SIMEE_*` settings as the server
(`PATH` doesn't matter), and set `SIMEE_REQUIRE_SERVER=1` so a caller that can't use the server
fails with the reason instead of silently running slower; `--verbose` prints where each command
ran. See docs/api.md.

### Benchmark

```bash
cargo build --release -p simee
scripts/farm-bench-ios.sh 1 2 4 8                                  # fresh devices, one boot each
MODE=clone TEMPLATE=<udid> BOOT=many K=4 BOOT_ARGS=--settle scripts/farm-bench-ios.sh 8
MODE=clone TEMPLATE=<slim udid> BOOT=many BOOT_ARGS=--settle ROUNDS=2 scripts/farm-bench-ios.sh 4
```

One `key=value` line per N and round in `bench/`: wall time until `boot` returned and until all
were settled, each device's settle time, memory (system-wide `vm_stat` delta and `footprint` of the
devices' processes), processes per device, the devices' CPU at settle and 60 s later, disk per
device, failures. Devices are named `perffarm-<label>-<i>`, created for the run and deleted after.

## Android emulators

The same methods drive an AVD farm, with the same names and result shapes:

```bash
# Once: a golden AVD booted under simee and stopped (its settled disks are what clones copy).
simee boot golden && simee shutdown golden
for i in 1 2 3 4; do simee clone golden farm-$i; done   # copy-on-write disks, no first boot
simee boot-many farm-1 farm-2 farm-3 farm-4 -j 2 --first-port 5554
```

`boot_many` on AVDs (`sim_android::Android::boot_many`) starts at most `concurrency` emulators at
once, each on its own console/adb port pair handed out from `first_port` (default 5554) by one
allocator, and waits for each to report `sys.boot_completed`. Each device's result is `device`,
`boot_s`, `console_port` and `serial` (`emulator-<port>`), or `error`; `failed` counts errors and
`unsettled` is always 0. It runs in the calling process (`IN_CALLER`): the emulators are its
children. One call boots one platform's devices.

There is no Android `settle`: sim-android has no settle rule in Rust (the Android bench script,
`scripts/farm-bench-android.sh`, judges it from the host: `sys.boot_completed`, then the emu
process under 60 % of a core for 5 s). `boot` and `boot_many` refuse
the iOS-only `settle`, `settle_*`, `profile`, `shaped` and `env` on an AVD with the reason,
instead of ignoring them.

Per settled device at idle the bench also records `footprint_mb` (`footprint` of the emu process:
its dirty memory, the guest pages it has touched included) and `idle_wakeups_s` (`top -l 2 -s
IDLE_S -stats pid,idlew`: how often the process woke a core out of idle), with their medians, beside
the host CPU and vCPU exits. On a loaded host few cores are idle, so the wakeup count reads low.
