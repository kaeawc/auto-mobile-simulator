# Running many Android devices (simee farms)

How to start many simee devices on one Mac quickly, settle them, and keep their idle cost low,
with the measurements behind each choice. Host for every number: Apple M3 Max (12 P + 4 E cores,
128 GB), shared with CI runners (1-minute load averages from 20 to 500 during these runs, so
wall times are noisy; exit counts, console bytes and disk sizes are not). Image: API 36
`google_apis` arm64, 4 vCPU, 2 GB, 720x1280.

## Tools

- `emu -clone-avd NAME -from GOLDEN` / `sim_android::Android::clone_avd`: a new AVD whose simee
  disks (`simee/userdata.img`, `cache.img`, `encryptionkey.img`, the metadata partition that holds
  user data's key) are APFS clones (`std::fs::copy` uses `fclonefileat` on macOS) of a stopped
  golden AVD's. The clone boots warm, persistent, with its own state from then on. The golden's
  stock emulator files are never read or written; its instance lock is held while copying, so a
  running golden is refused. `emu -delete-avd NAME` (`Env::delete_avd`) validates the name as
  `-create-avd` does, finds the directory through `<name>.ini`'s `path=` as booting does, and
  removes only that directory (it must hold a `config.ini`) and the pointer. It refuses while any
  emulator runs the AVD: one advertised for it in the running directory (stock writes the same
  file), or a held lock file in its directory (simee's `simee/instance.lock`, stock's
  `multiinstance.lock`, or `hardware-qemu.ini.lock` naming a live PID).
- `sim_android::Android::boot_many(avds, concurrency, timeout, first_port)`: boots up to K at a
  time, each on a console port handed out by one allocator (`allocate_ports`: even ports whose
  console and adb ports no running emulator advertises, whose claim no starting simee holds
  (`port_usable`: the lock file below) and that bind now), and returns one `BootOutcome` per AVD
  (port, launch-to-booted time or the reason it failed). A failure doesn't stop the others; the
  next device starts when any finishes. Another allocator (a second farm, a bare `emu` from
  AutoMobile or Studio) can still claim a pair between the probe and emu's own claim; emu then
  exits with "console port N is taken by another simee instance" and that device is retried on
  the next free pair (up to 3 times) instead of being reported failed. A device that doesn't boot
  within the timeout is stopped (`kill -INT`, 10 s, then SIGKILL) and reaped by `boot_with`
  (`Device::boot` too): a failed outcome leaves no emulator running, no port held, no zombie.
- emu claims its console port with a lock file (`~/Library/Caches/simee/ports/<port>.lock`, held
  for the process's life) before probing it, so instances started at the same moment can't pick
  the same pair (they used to: both probed a pair free, one then failed to bind). `-port N` is
  claimed too and fails at once if another simee holds it.
- `scripts/farm-bench-android.sh <label> <golden> <N>`: the benchmark below.

## Settled, and the benchmark

Settled = `sys.boot_completed=1`, then the emu process using under 60% of one host core over 5 s
(host-side, so a busy guest isn't loaded further by adb). Every boot shows a long tail after
`boot_completed`: 20–90 s of post-boot jobs at 100–200% CPU before the device quiets. That tail,
not the boot, dominates time-to-settled.

The script reports per run: time to all settled, median boot and settle (each from the device's
own launch, which is also where its `BOOT_TIMEOUT` starts: with K < N, later devices start
later; the runs below used K = N), failures, memory two
ways (sum of RSS, and the system-wide change in anonymous + wired + compressor pages from
`vm_stat`, at settle and 60 s later), host CPU % and vCPU exits/s per device at idle (from
`EMU_EXIT_PROFILE`), and `df`/`du` for the AVDs it made. RSS counts guest pages that host and
guest both touched twice under Hypervisor.framework (about 4.0 GB for a 2 GB guest); the
system-wide figure doesn't, but on this shared host CI moves it by gigabytes between samples, so
neither is a clean per-device number yet (see Results).

## The console (88% of all vCPU exits)

The PL011 traps twice per byte (FR read, DR write), synchronously in the writer. With
`console=ttyAMA0` at the default console level, a boot printed ~230 KB (kernel, init, ueventd,
SELinux) and idle printed ~2 KB/s (the IUwb lookups and goldfish-logcat restarts below).

What stock does (`SetupParameters.cpp` `setupVirtualSerialPorts`, `main-kernel-parameters.cpp`),
by mode, on QEMU2 for ARM:

| | stock without `-show-kernel` | stock with `-show-kernel` | simee default | simee `-show-kernel` |
|---|---|---|---|---|
| console | `console=0` (no kernel console), serial backed by `null` | `console=ttyAMA0,38400` | `console=ttyAMA0,38400 loglevel=3` | `console=ttyAMA0,38400` |
| always | `keep_bootcon earlyprintk=ttyAMA0` | same | same | same |
| `printk.devkmsg=on` | only with `-shell` | yes | yes | yes |

The cmdline in docs/reference/stock-board.md is the `-show-kernel` one.

simee now: without `-show-kernel`, `console=ttyAMA0` stays with `loglevel=3`: only emergencies,
alerts and criticals print, which includes init's FATAL lines and panics (`panic()` raises the
console level), so `console.log` is empty for a normal boot; dmesg keeps everything
(`printk.devkmsg=on` stays, a better debugging story than stock's rate-limited kmsg).
`-show-kernel` prints everything, as before; `EMU_CONSOLE=full` logs everything to console.log
without printing it. console.log is created once per emu process and appended to across guest
resets, so a panic's text survives the reboot it causes (each reset used to recreate it, which
is why one sysrq-c test found it empty; not yet re-tested on a device). Considered and not
taken: `console=hvc0` on the virtio-console port 0 (whole buffers per kick, full logs kept),
because it changes `/dev/console` for every image, and quieting already removes ~all of the cost.

The desktop's Android panel (LOG) shows the last 200 lines of the device's log
(`sim_android::device_log_tail`): console.log's critical lines (at most a quarter), then the
logcat stream. sim-android starts every emu (`Device::boot`, `boot_many`) with
`-logcat '*:I' -logcat-output <avd>/simee/logcat.txt`, so devices booted from the desktop, the
API or the CLI have that stream; one started some other way shows only console.log's lines, and
an empty panel says how to get the kernel log.

## goldfish-logcat and /dev/hvc1

`init.ranchu.rc` runs `service goldfish-logcat /system/bin/logcat -f /dev/hvc1 ${ro.boot.logcat}`
(class main, always). simee's virtio-console had one console port, so `/dev/hvc1` had no
backend: logcat exited every ~5 s, each restart forking `flags_health_check` and writing ~8
console lines. Stock backs hvc1 with a virtconsole (null unless `-logcat`).

Now the virtio-console's third port (after port 0 and `modem`) is an unnamed console port, which
the guest makes `/dev/hvc1` (unnamed ports are console ports; hvc numbers follow port order; no
board change: same device, same slot). Its output is discarded unless `-logcat TAGS` (stdout,
filter TAGS, stock's flag) or `-logcat-output FILE` (all of it unless `-logcat` gives a filter;
FILE rolls over to `FILE.1` at 4 MiB, so at most 8 MiB on disk). When it's discarded the filter
is `*:S` (`androidboot.logcat`; was `*:V`), so logcat formats and writes nothing. The
destination is opened once per process (a reset appends) and fed through a bounded queue
(1024 guest buffers) on its own thread: the virtio-console worker, which also serves the radio
HAL's `modem` port, only queues. When the destination falls behind (a stdout pipe nobody reads)
output is dropped, counted (`-verbose` prints the count at exit) and noted in the stream as
`[emu: N bytes dropped ...]`. hvc1 hands over a whole buffer per kick, so a logcat stream costs
the guest logcat's formatting, not vCPU exits per byte; sim-android launches with `*:I`.

## UWB (system_server asks for IUwb every second)

`init.ranchu.rc` starts the UWB HAL only `on property:vendor.qemu.vport.uwb=*`, linking that
virtio-console port to `/dev/hvc2` (stock serves it from netsim). simee has no `uwb` port, so the
HAL never starts and system_server's UWB service retries the lazy AIDL service every second
(servicemanager → init `ctl.interface_start` → "Could not find"), ~3 kmsg lines/s (56 in dmesg 40 s
after boot). Not changed: the fix stock makes is a working UWB device behind the port (a `uwb`
port with nothing answering the HAL's UCI protocol would trade the lookups for a HAL that times
out). With the console quiet these lines no longer print; the lookup itself is cheap.

## Port and adb scaling

adb servers scan adb ports 5555–5585 (16 emulators) unless started with
`ADB_LOCAL_TRANSPORT_MAX_PORT`; instances above that are known only because simee tells the
server (`host:emulator:<adb port>`) at boot. A server started later (after `adb kill-server`)
never heard of them. emu now repeats that announcement every 5 s (a no-op for a server that has
the device: one loopback connect and write). Tested with a private adb server
(`adb -P 5137 nodaemon server`, emu started with `ANDROID_ADB_SERVER_PORT=5137`, port 5600): the
device was listed, gone right after the server restarted, and listed again within 8 s.

## Not done here: balloon and snapshots

- virtio-balloon with free-page reporting would return idle guests' free pages (the API 36 kernel
  has page reporting built in, `virtio_balloon.ko` ships in system_dlkm). It needs a virtio-mmio
  slot the stock board doesn't define for it, so a board change with evidence from stock first,
  and on macOS returning a range means `hv_vm_unmap` + remap of 16 KiB-aligned runs.
- Snapshots (save RAM + vCPU + GIC + devices, restore as `MAP_PRIVATE` of the RAM file, which
  would also share clean guest pages between instances) need the renderer's state: the SDK's
  gfxstream refuses `stream_renderer_snapshot` (`docs/roadmap-android.md`). Golden userdata clones
  are the snapshot-free half of that: no first boot, but each device still boots and settles.

## Results

`scripts/farm-bench-android.sh`, clones of one settled golden, 1-minute load 20 at each start and
40–760 by the end (CI), so times and CPU % are noisy; base = emu before these changes.

| run | N | boot med s | settle med s | idle CPU % med | idle exits/s med | PL011 share |
|---|---|---|---|---|---|---|
| base | 1 | 7.6 | 74.5 | 88 | 6895 | – |
| new | 1 | 4.1 | 96.7 | 137 | 867 | 0% |
| base | 2 | 5.4 | 26.7 | 39 | 4274 | 94% |
| new | 2 | 5.1 | 125.3 | 38 | 144 | 0% |
| base | 4 | 5.0 | 49.9 | 144 | 6623 | 76% |
| new | 4 | 6.2 | 42.2 | 37 | 154 | 0% |
| new, fresh AVD (first boot) | 1 | 9.3 | 50.9 | 62 | 284 | 0% |

- Boot: 516k vCPU exits before (88% PL011) → 77–109k after (64 PL011 accesses); console.log
  1.4 MB → 0 bytes for a normal boot.
- Idle, quiet window on one device: 4.5k → 122 exits/s; host CPU 35–42% → 29%; guest busy
  7–8% → 5.7% of 4 vCPUs, tasks 2.7 → 2.3/s. What's left is the guest's own work (vCPU threads
  ~12.5% each in `hv_trap`; system_server ~11%), not exits.
- Clones: `df` grew 0–3 MB per clone while `du` counts 1.7 GB each.
- Memory: RSS ~4.0 GB per device (2 GB guest); the system-wide figure moved by gigabytes with CI
  between samples, so it isn't a per-device number here.
- N=8 and the K sweep weren't run: the host stayed over its load limit (1-min load 145–160).
