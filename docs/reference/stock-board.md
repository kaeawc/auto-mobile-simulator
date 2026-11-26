# The stock emulator's arm64 board (API 36, emulator 37.2)

Captured from a running stock instance (`/sys/firmware/fdt`, `/proc/iomem`, `/proc/interrupts`,
`/sys/block`, `getprop`). simee reproduces the parts the system image depends on.

## Memory map

| Address | Device | Interrupt (SPI → INTID) | simee |
| --- | --- | --- | --- |
| `0x0800_0000` | GICv2 distributor (+ CPU iface `0x0801_0000`, v2m MSI `0x0802_0000`) | — | GICv3 in-kernel: dist `0x0800_0000`, redist `0x080a_0000` |
| `0x0900_0000` | PL011 UART (`ttyAMA0`) | 1 → 33, level | same |
| `0x0901_0000` | PL031 RTC | 2 → 34, level | same |
| `0x0902_0000` | fw-cfg | — | — |
| `0x0903_0000` | PL061 GPIO (power key via gpio-keys) | 7 → 39 | todo |
| `0x0a00_0000 + 0x200·n` | virtio-mmio slot n (32 slots) | 32+n → 64+n, edge | same slots, level |
| `0x0a02_0000` | goldfish_fb | 16 | — |
| `0x0a03_0000` | goldfish_audio | 18 | — |
| `0x0a04_0000` | goldfish_battery | 17 | todo |
| `0x0a05_0000` | goldfish-events keypad | 19 | todo |
| `0x0a06_0000` | goldfish_pipe | 20 | todo (graphics transport) |
| `0x0a07_0000` | goldfish_sync | 21 | todo (graphics fences) |
| `0x1000_0000` | PCIe MMIO window; ECAM at `0x3f00_0000`; 64-bit window `0x8_0000_0000` | INTx map | todo |
| `0x4000_0000` | RAM (`hw.ramSize`; the stock launcher pads 2048 MB to 2560 MB) | — | exactly `hw.ramSize` |

## virtio-mmio slots in use

| Slot | Address | Device | Block name |
| --- | --- | --- | --- |
| 25 | `a003200` | rng | |
| 26 | `a003400` | net (simee: virtio-net, link only so far) | |
| 27 | `a003600` | blk: `system.img` (GPT: vbmeta, super) — `androidboot.boot_devices` | vda |
| 28 | `a003800` | blk: cache | vdb |
| 29 | `a003a00` | blk: userdata (fstab mounts `/dev/block/vdc` on `/data`) | vdc |
| 30 | `a003c00` | blk: encryptionkey.img (GPT: metadata) mounted by path on `/metadata` | vdd |
| 31 | `a003e00` | blk: vendor.img (GPT: vendor) | vde |

simee adds vsock at slot 24 and, for images that enable `VirtioSndCard` and for Automotive, virtio-snd at slot 18
(stock puts both on PCI; the guest drivers bind on either transport).

## PCI devices (stock)

virtio-snd, 2× virtio-console (logcat on `hvc1`, modem/bluetooth/uwb vports), 11× virtio-input,
goldfish address space (`607d:f153`, host-visible memory for graphics), virtio-wifi
(mac80211_hwsim), virtio-vsock (guest CID 77).

The virtio-console ports (`/sys/class/virtio-ports/*/name`, emulator 37.2.13, 2026-11-25):

| Stock port | Name | Guest device | simee (one device, slot 19) |
| --- | --- | --- | --- |
| `vport8p0` | (console) | `hvc0` | port 0 |
| `vport8p1` | (console) | `hvc1`, goldfish-logcat | port 2 |
| `vport8p2` | `uwb`, a named console port | `hvc2`: the UWB HAL opens the tty | port 3 |
| `vport8p3` | `bluetooth` | `/dev/bluetooth0`, `bt_vhci_forwarder` | port 4 |
| `vport9p1` | `modem` | the radio HAL | port 1 |

The guest finds ports by name and numbers console ports in port order, so only the console
ports' relative order matters (uwb must be the third). Stock serves bluetooth and uwb from
netsimd; simee from a built-in controller with no peers (docs/research/bluetooth.md).

The touch screen, `adb shell getevent -p` and `dumpsys input` on the same AVDs under both
(emulator 37.2.13, 2026-11-26; API 36 phone 720×1280, API 36 tablet 2560×1600, API 37 Wear 454×454):

| Device | Stock (identical on all three) | simee (identical on all three) |
| --- | --- | --- |
| `virtio_input_multi_touch_1` | keys `BTN_TOOL_RUBBER`, `BTN_STYLUS`; `ABS_X`/`ABS_Y`/`ABS_MT_POSITION_*` 0..32767 whatever the display, `ABS_Z` 0..1, `ABS_MT_TOUCH_MAJOR`/`MINOR` 0..2147483647, `ABS_MT_ORIENTATION` 0..90, `ABS_MT_SLOT` 0..10, `ABS_MT_TOOL_TYPE` 0..15, `ABS_MT_TRACKING_ID` 0..65535, `ABS_MT_PRESSURE` 0..1024; `SW_LID`; no input props; ids 0000:0000 v0 | the same (`getevent -p` and the InputReader block of `dumpsys input` identical on the tablet, 2026-11-26) |
| guest's view of it | classes `KEYBOARD \| TOUCH \| TOUCH_MT \| SWITCH`; `DIRECT`, `TOUCH_SCREEN` (no input properties: the image's `virtio_input_multi_touch_1.idc` sets `touch.deviceType = touchScreen`, found in every local vendor image from API 31 to 37; without that file, or under another device name, Android would treat it as a pointer); size calibration `GEOMETRIC`, orientation `interpolated`; `RawToDisplay` scale 2560/32768 | the same |
| a finger (stock: its mouse, gRPC `sendMouse`; simee: console `touch`, `display.sock`, the desktop) | down: tracking id (= slot), position, pressure 1024, touch major and minor 0x500; a move: the position that changed; the lift: pressure 0, tracking id -1. Cooked (`dumpsys input`): pressure 1.0, touch major 81.25 on the tablet | the same events and cooked values |
| pixel → axis | `p × 32767 / size`, truncated: a tap at (100, 200) reaches apps at (99.922, 199.951), left of and above the pixel asked for | the smallest value at or past `p × 32768 / size` (Android's mapping back): (100, 200) arrives as (100.000, 200.000), and every pixel of a 2560, 1600, 1280, 720 or 454 axis comes back as itself plus under a pixel. Deliberate: stock's lands outside a view whose edge is the pixel tapped |
| a pen | pen slot 10, tool type pen or `MT_TOOL_MAX` + `BTN_TOOL_RUBBER`, `BTN_STYLUS`; gRPC pressure as sent (0..1024). Hovering sends no tracking id, so the guest sees no hover (`dumpsys input`: no pointer) | the same codes; a hovering pen takes the slot's tracking id at zero pressure, so apps get hover events (deliberate: more than stock). The console's `pen ... pressure N` is 1..255, scaled to 0..1024; a press without one is stock's 0x81 |
| how many | 11 (`virtio_input_multi_touch_1`..`_11`), one per display: the guest's `com.android.emulator.multidisplay` associates `_2`..`_11` with its virtual displays (`AssociatedDisplay: displayId='virtual:com.android.emulator.multidisplay:…'`), which a single-display device doesn't have, so they take no input | 1. Deliberate until simee has multiple displays: the extra ten would sit on no display |

## Kernel command line and bootconfig

Kernel, stock's `-show-kernel` command line (its `console=ttyAMA0,38400` and `printk.devkmsg=on`
appear only in that mode, so that's the mode this capture was taken in): `8250.nr_uarts=1
no_timer_check console=ttyAMA0,38400 keep_bootcon earlyprintk=ttyAMA0 ndns=2 loop.max_part=7
printk.devkmsg=on bootconfig mac80211_hwsim.radios=0` plus the image's `kernel_cmdline.txt`. The
console parameters depend on the mode (`SetupParameters.cpp` `setupVirtualSerialPorts`, QEMU2
on ARM): `keep_bootcon earlyprintk=ttyAMA0` always; `console=ttyAMA0,38400` only with
`-show-kernel`, else `console=0` and the serial port backed by `null`; `printk.devkmsg=on`
(`main-kernel-parameters.cpp`) only with `-show-kernel`, `-shell` or `-shell-serial`. simee
always passes the `-show-kernel` set and, without `-show-kernel`, adds `loglevel=3` (critical
messages and panics only; docs/farm-android.md).

Bootconfig is appended to the ramdisk. Keys: `androidboot.hardware=ranchu`,
`boot_devices=a003600.virtio_mmio`, `qemu=1`, `qemu.avd_name`, `serialno`, `veritymode`,
`vbmeta.{digest,hash_alg,size}` (from `VerifiedBootParams.textproto`),
`dalvik.vm.heapsize=576m`, `qemu.skin=WxH`, `qemu.vsync`, `logcat`, `opengles.version`,
`hardware.gltransport=pipe`, `hardware.vulkan=ranchu`, `qemu.adb.pubkey`,
`qemu.settings.system.screen_off_timeout`, `qemu.virtiowifi`, `qemu.camera_*`, `qemu.hwcodec.*`.
A round screen adds `emulator.circular=1`.

## Boot properties, keys and sensors (from the emulator source, emu-master-dev)

- `qemud:boot-properties` serves `boot.prop`'s lines (the AVD directory's, else the system
  image's) and the host's own: `qemu.hw.mainkeys` is `hw.mainKeys` (hardware-properties.ini
  default yes: no navigation bar), `qemu.sf.lcd_density`, `qemu.timezone`. A truthy
  `ro.emulator.circular` there sets `hw.lcd.circular`, which becomes bootconfig
  `androidboot.emulator.circular=1` (android-qemu2-glue/main.cpp, userspace-boot-properties.cpp).
- The virtio keyboard is `qwerty2`, ids 0627:0001 (hw/input/virtio-input-hid.c), so the guest
  uses goldfish's `qwerty2.kl`. It reports QEMU's qcode keys plus HOME, BACK, SEND, END, SOFT1/2,
  VOLUMEUP/DOWN, POWER, SEARCH, SLEEP, CENTER (232, DPAD_CENTER), GREEN (399), MOVE_HOME/END
  (452/453), APP_SWITCH (580) and STEM_PRIMARY..ASSIST (581–585); it also advertises EV_REP and
  three LEDs. The console's `KEY_*` names are hw-events.h's; codes may also be numbers.
- The rotary's unit is a degree (`virtio_input_rotary.idc`: `device.res = 57.295`); gRPC turns
  15 per click.
- `list-sensors` is built from the `hw.accelerometer`/`hw.gyroscope`/`hw.sensors.*` flags (on by
  default except RGBC light, heart rate, wrist tilt), `hw.sensor.hinge` (angles 1 and 2 by
  `hw.sensor.hinge.count`), and heart rate and wrist tilt forced on for Wear at API 28+
  (android/hw-sensors.cpp).

## Facts that cost time to find

- Fresh userdata is a qcow2 image that already contains an **empty ext4 filesystem**. The
  `/data` fstab entry isn't `formattable`, so a blank disk sends vold down `mountFstab` with no
  metadata key and init reboots into recovery. With a filesystem present, init mounts it, sees
  metadata encryption, and calls `encryptFstab`, which encrypts it in place.
- Current adbd has no qemu-pipe transport. It listens on guest `vsock:5555` (and `tcp:5555`),
  so the host opens the vsock connection when an adb client connects to `console + 1`.
- Other non-graphics guest pipes connect over vsock to host CID 2, port 5000, and send
  `pipe:<name>\0`. Names seen: `qemud:boot-properties`, `qemud:sensors`, `qemud:camera`,
  `QemuMiscPipe`. Graphics pipes (`opengles`, `GLProcessPipe`, `refcount`) never use vsock:
  they need `goldfish_pipe` or virtio-gpu.
- The image's EGL is `emulation` (gfxstream). `libEGL_angle` is present but sits on
  `vulkan.ranchu`, which also forwards to the host. There is no guest-only software renderer.
