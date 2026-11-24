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
simee boot-many farm-1 farm-2 farm-3 farm-4 farm-5 farm-6 farm-7 farm-8 --settle --profile slim
```

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
CPU is each process's accumulated CPU time from `ps`, so it is exact over the window and a process
that exits doesn't make the tree look quieter. The threshold comes from the measurements: an idle
device runs at 1-6 % (no client attached), so 10 % separates "storm over" from "idle" with room
for one busy daemon; 5 % (the research brief's guess) never fired on some idle devices.

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

### Clones: no first boot per device

`clone` (CoreSimulator's `-[SimDeviceSet cloneDevice:name:error:]`, what `simctl clone` does)
copies a shut-down device: its data, settings and apps, as APFS clones. 4 s per clone; `du` says
2.2 GB per device but the volume's used space grows ~10-30 MB per clone. A clone of a template
that has done its first boot skips the data migration and indexing storm.

### The `slim` profile

`boot --profile slim` disables (in the device's own launchd database) and stops 61 daemons no AutoMobile tool needs: PosterBoard and the
~15 poster extensions it keeps alive (~30-45 MB footprint each), widgets (chronod), Siri and
suggestions, photo and media analysis, Health, Home, fitness, the Store, iCloud sync, Mail, News,
Weather, Wallet, Game Center, Screen Time, watch and Find My companions, experiments and ML hosts.
The list and what was kept on purpose is `SLIM` in `crates/sim-api/src/ios.rs`.

| per device | fresh | clone | slim clone |
| --- | --- | --- | --- |
| processes | 253-297 | 217-244 | 113-153 |
| footprint (dirty memory, shared once) | 2.6-2.8 GB | 2.0-2.2 GB | 1.07-1.2 GB |
| CPU of 8 devices together, 60 s after settle | 869 % | 25 % | 7 % |

Applying the profile takes 24-66 s per device (one in-device shell running `launchctl disable`
and `bootout` for each job; longer under load), counted in `settle_s`. A clone doesn't inherit the
disables (`clone` resets them: a clone of a slimmed template came up with 221 processes), so the
profile is applied on each boot; applying it again is harmless.

Kept: SpringBoard, backboardd, InputUI (keyboard), the accessibility server and daemons
(AutoMobile reads the screen through accessibility), installd, lsd, runningboardd, tccd,
usernotificationsd, locationd, sharingd, webbookmarksd (Safari), assetsd, logging, configd_sim,
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
