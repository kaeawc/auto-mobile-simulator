# Bluetooth and UWB

How the stock emulator gives an Android guest a Bluetooth controller and a UWB chip, what the
guest needs from them, and what emu does. Evidence: the API 36 `google_apis` arm64 image, booted
on emu and on the stock emulator 37.2.13 (build 16740729) on 2026-11-25, plus the AOSP sources
named below. Issue [#31](https://github.com/kaeawc/simee/issues/31).

## What the guest does

Both radios hang off virtio-console ports. `qemu-props` publishes each named port as
`vendor.qemu.vport.<name>=/dev/vportNpM`, and `/vendor/etc/init/hw/init.ranchu.rc` reacts:

```
on property:vendor.qemu.vport.bluetooth=*
    symlink ${vendor.qemu.vport.bluetooth} /dev/bluetooth0
service bt_vhci_forwarder /vendor/bin/bt_vhci_forwarder -virtio_console_dev=/dev/bluetooth0
    class main
on property:vendor.qemu.vport.uwb=*
    symlink ${vendor.qemu.vport.uwb} /dev/hvc2
    start vendor.uwb_hal
service vendor.uwb_hal /vendor/bin/hw/android.hardware.uwb-service /dev/hvc2
```

### Bluetooth

- `bt_vhci_forwarder` (cuttlefish's `guest/commands/bt_vhci_forwarder`) opens `/dev/vhci` and the
  port once, at start. vhci registers a kernel `hci0`; the forwarder copies H4 packets (a type
  byte, then the HCI packet) between vhci and the port, and drops whatever the port sends before
  the first command from vhci (leftovers from a previous boot). With no port the forwarder runs
  but forwards nothing.
- When `hci0` registers, the Linux kernel runs its own controller init through vhci (Reset, Read
  Local Version, Read BD_ADDR, Read Local Supported Commands and Features, buffer sizes, event
  masks, the accept list, ...). It sends optional commands only when the controller's Supported
  Commands bitmap lists them.
- The HAL is AOSP's default AIDL HAL (`android.hardware.bluetooth-service.default`,
  `hardware/interfaces/bluetooth/aidl/default`). It opens `hci0`'s user channel through the
  management socket (falling back to `vendor.ser.bt-uart`). On boards whose `ro.product.board`
  starts with `goldfish` or `cutf` it sends its own HCI Reset first and waits one second for
  exactly `0e 04 01 03 0c 00`.
- The stack (`com.android.bt`, `system/gd`) then resets the controller and reads it
  (`hci/controller.cc`): Set Event Mask (and Page 2), Write LE Host Support, Read Local Name,
  Version, Supported Commands, LE features and states, every extended features page, Read Buffer
  Size, LE Read Buffer Size, the accept list size, Write Simple Pairing Mode, the vendor
  capabilities (0xFD53, optional) and Read BD_ADDR. Every mandatory one must succeed, and each
  must come back as the event type it expects (Command Complete or Command Status), or the stack
  aborts. It only sends optional commands the bitmap lists.

Without a controller (emu before this change) the kernel's Reset timed out (`hci0: Opcode 0x0c03
failed: -110`), then the HAL's and the stack's, and `com.google.android.bluetooth` aborted with
`StartEverything: Can't start stack, last instance: starting HciHal` six times after boot.

### UWB

- `/dev/hvc2` always exists (the hvc driver creates all 64 nodes), so the `symlink` fails with
  `File exists` on stock too, and the HAL opens the tty `hvc2` itself. The `uwb` port must
  therefore be a *console* port that is also named, and the guest's third console port (stock:
  `vport8p2`, after hvc0 and logcat's hvc1).
- The HAL (AOSP's Rust `uwb_default_hal`) reads UCI frames from the tty and hands them to the
  stack. Opening and `coreInit` need nothing from the chip. Android's `uwb_core` injects the
  device-ready notification itself (a workaround for b/243140882 in `uci_hal_android.rs`), then
  sends CORE_GET_DEVICE_INFO and ANDROID_SET_COUNTRY_CODE. The HAL sends CORE_DEVICE_RESET when
  the stack closes it.
- Without the property init never starts the HAL, but the VINTF manifest declares `IUwb`, so
  `UwbService` asked servicemanager for it about once a second for the whole run
  (servicemanager → init `ctl.interface_start` → `Could not find`).

## What stock does

The stock guest's ports (`/sys/class/virtio-ports/*/name`, same image):

| Port | Name | Guest device |
| --- | --- | --- |
| `vport8p0` | (console) | `hvc0` |
| `vport8p1` | (console) | `hvc1`, goldfish-logcat |
| `vport8p2` | `uwb` (named console) | `hvc2`, the UWB HAL |
| `vport8p3` | `bluetooth` | `/dev/bluetooth0`, the forwarder |
| `vport9p1` | `modem` | the radio HAL |

The host end of both is `netsimd` (the SDK's `emulator/netsimd`), one per host and shared by
every stock emulator on it. It embeds rootcanal (a full BR/EDR + LE controller model) and pica
(a UWB model) and links the devices it serves, so emulators on one host discover and pair with
each other. The AVD's `netsim.ini` records the address netsim gave it (`BB:BB:BB:00:00:09` for
this AVD). netsimd also offers a raw H4 TCP socket (`--hci-port`, `NETSIM_HCI_PORT`), a packet
stream TCP port (`--tcp-port`), gRPC (`--grpc-port`, `--grpc-uds-path`) and test beacons
(`--test-beacons`).

What stock's controller tells the stack (`dumpsys bluetooth_manager`): HCI and LMP 5.3,
manufacturer 224, ACL 1023 × 10, SCO 255 × 10, LE ACL 27 × 20, ISO 1021 × 12, 16 extended
advertising sets of up to 512 bytes, accept and resolving lists of 16, Android vendor
capabilities v1.06. Bluetooth is on after boot (`bluetooth_on=1`). On the UWB side the whole
exchange at boot was CORE_GET_DEVICE_INFO and ANDROID_SET_COUNTRY_CODE (and, logged after the
HAL opened, a reset response and a ready notification). `IUwb` was looked up once.

## Options for emu

1. **A built-in controller with no peers (done).** `crates/emu-vmm/src/bt/`: an H4 codec, a
   Bluetooth 4.2 dual-mode controller and a UCI responder, behind a `bluetooth` port and a
   named-console `uwb` port in stock's order. Costs: about 1,300 lines of Rust with their tests, no
   dependency, no host process, nothing per device beyond the console worker it already has.
   Gives: the kernel's init, the HAL and the stack start and stay up; Bluetooth turns on and off;
   discovery and LE scans finish with no results; pages time out; UWB is enabled and rangeless.
   Doesn't give: any peer, pairing, GATT, audio or ranging.
2. **Bridge the ports to netsimd.** The `bluetooth` port's bytes are already H4, so connecting it
   to netsimd's raw HCI TCP port would replace option 1's controller with rootcanal: emu devices
   and stock emulators on one host would see and pair with each other, with the same controller
   features stock reports. UWB would need netsim's packet stream (or gRPC) for pica. Costs: emu
   starts or finds a netsimd (its lifetime, its version, its idle shutdown), one TCP connection
   per device, and netsimd's CPU, which is not measured. Not verified yet: that a raw HCI
   connection becomes a netsim device that other devices see, and how netsimd assigns its
   address.
3. **Pairing between emu devices without netsim.** A host-side "air" shared by emu processes,
   with inquiry results, paging, ACL relay and LE advertising reports in emu's controller: in
   effect a second rootcanal. Large, and option 2 gives the same thing with stock's model.

## Recommendation

Ship option 1 now: it ends #31's crash loop and the IUwb lookups at no host cost, and keeps
every device's Bluetooth usable for apps that only check the adapter. Do option 2 next, opt-in
first (a flag, or when a netsimd is already running), behind the same port, so the default stays
cheap for farms; measure netsimd's idle CPU and memory per device before making it the default.
Don't build option 3.

## Result

API 36 `google_apis`, release emu, 2026-11-25 (`logcat -b all` from 38 s after launch for 300 s):

| | emu before | emu now | stock 37.2.13 |
| --- | --- | --- | --- |
| Bluetooth stack aborts (`Can't start stack`) | 6 within 30 s, about 35 s after boot | 0; one process from boot until the disable | 0 |
| `IUwb` lines in logcat | 1042 in the 2 minutes captured | 1 | 1 |
| Bluetooth after boot | crash-looped | ON; `cmd bluetooth_manager disable` → OFF, `enable` → ON | ON |
| UWB | HAL never started | HAL running, `cmd uwb status`: enabled, READY | enabled |

The only command either host sent that emu's controller doesn't know was the vendor
capabilities probe (0xFD53), which the stack treats as "none". Not checked on a device yet: a
discovery from Settings (unit-tested: Command Status, then Inquiry Complete with no results).

## emu's controller

- HCI and LMP 4.2, manufacturer 0xFFFF (the spec's test ID), legacy advertising and scanning,
  Secure Simple Pairing, EIR; no SCO, LE encryption, privacy, data length extension or extended
  advertising, so neither host sends commands for them. The Supported Commands bitmap is built
  from the table of commands the controller answers (`controller::COMMANDS`), so the two can't
  disagree; anything else gets Unknown HCI Command (Command Status for vendor commands, which the
  stack treats as "not supported"), and emu prints each such opcode once on stderr.
- The public address is `BB:BB:BB` plus three bytes hashed from the AVD's disk paths: stable per
  AVD, different between AVDs.
- Inquiry Complete comes right after the inquiry's Command Status rather than after the inquiry
  length.
- UWB answers CORE_GET_DEVICE_INFO with pica's values (UCI 2.0, MAC and PHY 1.3, test 1.1), caps
  and config with nothing, the country code with OK, a reset with OK and a ready notification,
  and refuses session and ranging commands.
