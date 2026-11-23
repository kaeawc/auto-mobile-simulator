# sim-rs API reference

Generated from `sim-rs call help`. Transport: unix socket, one JSON request per line, one JSON response per line.

```text
→ {"id": 1, "method": "tap", "params": {"device": "booted", "x": 600, "y": 1300}}
← {"id": 1, "ok": true, "result": null}
```

`device` is a UDID (an Android device's UDID is its AVD name), an exact name, or `booted` (the default: the only running device). It is resolved across both platforms; pass `platform` (`ios` or `android`) to look at one. Parameters ending in `?` are optional.

**Platforms** says whose devices a method works on. Calling a method on a device of another platform fails with the platforms it does work on, e.g. `` `status_bar` isn't available on Android devices (it works on iOS) ``. Every device in a result carries `platform`, `form_factor` and `sub_platform`.

| Method | Platforms | Parameters | Description |
| --- | --- | --- | --- |
| `help` | Android, iOS | — | List methods with the platforms each supports |
| `form_factors` | Android, iOS | platform?, form_factor? | Every form factor on every platform: sub-platform, inputs and reference profile, or why it's unsupported |
| `devices` | Android, iOS | booted?, platform?(android\|ios), form_factor? | List devices with their platform, form factor and sub-platform |
| `runtimes` | Android, iOS | platform? | List installed runtimes (Android: system images) |
| `device_types` | iOS | — | List device types |
| `create` | Android, iOS | name, platform?(ios\|android), form_factor?; iOS: device_type, runtime; Android: system_image | Create a device. form_factor is checked against the device type (iOS) or the system image (Android) and shapes a new AVD |
| `delete` | Android, iOS | device | Delete a device |
| `boot` | Android, iOS | device, timeout_s?, env?, shaped? | Boot and wait until finished (iOS: env is the device launchd's environment, shaped injects the network shaper; Android: starts emu) |
| `shutdown` | Android, iOS | device | Shut down |
| `erase` | iOS | device | Erase content and settings (shut down first) |
| `screen` | Android, iOS | device, display?(main\|carplay) | Screen size in pixels |
| `screenshot` | Android, iOS | device, path?, display?(main\|carplay) | PNG to path, else base64 in `png` |
| `tap` | Android, iOS | device, x, y, hold_ms?, display?(main\|carplay) | Tap at pixels (display: an iPhone's CarPlay screen, iOS only) |
| `swipe` | Android, iOS | device, x1, y1, x2, y2, duration_ms?, settle?, display? | Swipe between pixels |
| `touch` | Android, iOS | device, x, y, phase(down\|move\|up), display? | One raw touch sample |
| `button` | Android, iOS | device, button(home\|lock\|side\|siri\|apple-pay\|action \| back\|volume-up\|volume-down\|recents\|menu\|up\|down\|left\|right\|select\|play-pause), hold_ms? | Press a hardware button, as the device's form factor has it: the Siri Remote on Apple TV (menu/back, home, select, play-pause, arrows) and the D-pad on Android TV; on Apple Watch home is the crown's press |
| `type` | Android, iOS | device, text | Type on the hardware keyboard |
| `key` | Android, iOS | device, usage | Press a key by HID usage |
| `open_url` | iOS | device, url | Open a URL |
| `install` | iOS | device, path | Install an .app |
| `uninstall` | iOS | device, bundle_id | Uninstall an app |
| `launch` | iOS | device, bundle_id, args?, env?, shaped? | Launch with arguments and environment (shaped: inject the network shaper); returns pid |
| `terminate` | iOS | device, bundle_id | Terminate an app |
| `apps` | iOS | device | Installed bundle ids |
| `appearance` | iOS | device, mode(light\|dark) | Light or dark UI |
| `location` | Android, iOS | device, latitude, longitude | Simulated location |
| `privacy` | iOS | device, action(grant\|revoke), service, bundle_id | Privacy permission |
| `push` | iOS | device, bundle_id, payload | Deliver an APNs payload (object) |
| `status_bar` | iOS | device, time?, data_network?, wifi_mode?, wifi_bars?, cellular_mode?, cellular_bars?, operator?, battery_state?, battery_level? | Override status bar |
| `clear_status_bar` | iOS | device | Remove status bar overrides |
| `content_size` | iOS | device, size | Dynamic Type size (name or 0-11) |
| `increase_contrast` | iOS | device, on | Increase Contrast |
| `dynamic_island` | iOS | device, suppressed | Hide the Dynamic Island |
| `backlight` | iOS | device, on | Display backlight |
| `keyboard_language` | iOS | device, language | Software keyboard language |
| `hardware_keyboard` | iOS | device, on | Connect the hardware keyboard |
| `ca_debug` | iOS | device, option, on | Render overlay (see ca_debug_state for names) |
| `ca_debug_state` | iOS | device | Every render overlay and whether it is on |
| `reset_keychain` | iOS | device | Delete every keychain item |
| `memory_warning` | iOS | device | Simulate a memory warning |
| `notify` | iOS | device, name, state? | Post a Darwin notification (optionally setting its state) |
| `biometric` | iOS | device, action(match\|nomatch\|enroll\|unenroll), face? | Touch ID / Face ID |
| `exec` | iOS | device, program, args?, env? | Run a program inside the device; returns stdout/stderr |
| `logs` | iOS | device, last?, predicate? | Unified log lines (default last 1m) |
| `processes` | iOS | device | Device processes with CPU and memory |
| `kill` | iOS | device, pid, signal? | Signal a device process |
| `connections` | iOS | device | Open sockets of device processes |
| `traffic` | iOS | device | Bytes in/out per device process |
| `containers` | iOS | device, sizes? | App data and bundle containers |
| `ls` | iOS | device, path? | List a directory under the device data dir |
| `read` | iOS | device, path | Read a file (base64) |
| `write` | iOS | device, path, text? \| base64? | Write a file |
| `rm` | iOS | device, path | Remove a file or directory |
| `du` | iOS | device, path? | Size in bytes |
| `clear_container` | iOS | device, path | Empty a container (an app's data) without deleting it |
| `defaults_read` | iOS | device, domain | Read a preferences domain |
| `defaults_write` | iOS | device, domain, key, value, type? | Write a preference (type: string\|int\|float\|bool) |
| `crash_reports` | iOS | device | Host crash reports for this device |
| `info` | Android, iOS | device | Paths and identity |
| `network_shape` | iOS | device, preset?(offline\|3g\|edge\|lte\|lossy\|very-bad\|none), offline?, latency_ms?, down_kbps?, up_kbps?, loss_pct?, port_base?, port_count?, rules?([{pattern, offline?, latency_ms?, down_kbps?, up_kbps?, loss_pct?}]), proxy?(default true), inject?(false\|true\|"launchd"\|"boot") | Shape the device's network without root. proxy: a shaping HTTP/SOCKS proxy set in the device's configd_sim. inject: an in-process socket shaper; "boot" reboots the device with it in launchd's environment so every process, WebKit included, is shaped live by the profile |
| `network_state` | iOS | device | Current profile, proxy port and injection state |
| `network_flows` | iOS | device | Connections through the device's shaping proxy (host, bytes, duration, outcome) |
| `network_clear` | iOS | device | Remove shaping: clear the device proxy, stop the proxy, remove injection |
| `audio_state` | iOS | device | Device audio settings (routed host UIDs, volume) and every host audio device |
| `audio_volume` | iOS | device, level(0-100) | Device output volume (100 = unity); applies to audio clients started afterwards |
| `audio_route` | iOS | device, scope(input\|output), uid \| "default" | Pin the device's input or output to a host device (any device, including loopback); applies to clients started afterwards |
| `audio_capture_start` | iOS | device, path?(.wav), mute? | Capture this device's audio output with a process tap (no driver); mute silences it on the host meanwhile; returns capture_id |
| `audio_capture_stop` | iOS | capture_id | Stop a capture: frames, duration_ms, rms, peak, dbfs, path |
| `audio_mute` | iOS | device, on | Silence (or restore) this device's audio on the host: a per-device null sink |
| `network_env` | iOS | device | Environment that makes one app launch shaped (for simctl: prefix each key with SIMCTL_CHILD_) |
| `console` | Android | device, command \| commands | Emulator console commands (`adb emu`'s protocol: sensor, geo, sms, gsm, finger, power, ...); reply lines |
| `rotate` | Android, iOS | device, orientation?(portrait\|landscape-right\|portrait-upside-down\|landscape-left; iOS) | Rotate the device 90° clockwise, or to an orientation (iOS) |
| `fold` | Android | device, angle(0-180) \| posture(closed\|half-opened\|opened) | Set a foldable's hinge |
| `xr` | Android | device, action(recenter \| rotate\|move: x, y, z \| options: environment(day\|night), passthrough(0-1) \| input: mode(mouse\|hand\|eye) \| viewport: mode(pan\|zoom\|rotate) \| pointer: x, y, phase?(move\|down\|up\|click)) | Android XR: turn (radians) or move (metres) the head, recenter, set the room and passthrough, the input mode, or point the dual-mode mouse at pixels |
| `rotary` | Android, iOS | device, ticks \| delta (Digital Crown units; iOS) | Turn a watch's rotary input: the Wear OS crown or bezel, the Apple Watch Digital Crown |
| `vehicle` | Android | device, property?(VHAL name or id: GEAR_SELECTION, gear, speed, 0x11600207, ...), area_id?, value? | Android Automotive's vehicle HAL: list every property's value, read one, or set one when `value` is given (typed by the property: drive, 12.5, true); through the emulator console's `car` command (emu-rs) |
| `watch_gesture` | iOS | device, gesture(double-tap\|wrist-flick) | An Apple Watch hand gesture |
| `remote_swipe` | iOS | device, direction(up\|down\|left\|right), duration_ms? | Swipe on the Siri Remote's touch surface (Apple TV) |
| `carplay` | iOS | device, on | Attach (or detach) the CarPlay display to a booted iPhone (iOS 26 runtimes); then tap/swipe/screenshot with display=carplay |
