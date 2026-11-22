# sim-rs API reference

Generated from `sim-rs call help`. Transport: unix socket, one JSON request per line, one JSON response per line.

```text
→ {"id": 1, "method": "tap", "params": {"device": "booted", "x": 600, "y": 1300}}
← {"id": 1, "ok": true, "result": null}
```

`device` is a UDID, an exact name, or `booted` (the default). Parameters ending in `?` are optional.

| Method | Parameters | Description |
| --- | --- | --- |
| `help` | — | List methods |
| `devices` | booted? | List devices |
| `runtimes` | — | List installed runtimes |
| `device_types` | — | List device types |
| `create` | name, device_type, runtime | Create a device |
| `delete` | device | Delete a device |
| `boot` | device, timeout_s? | Boot and wait until finished |
| `shutdown` | device | Shut down |
| `erase` | device | Erase content and settings (shut down first) |
| `screen` | device | Screen size in pixels |
| `screenshot` | device, path? | PNG to path, else base64 in `png` |
| `tap` | device, x, y, hold_ms? | Tap at pixels |
| `swipe` | device, x1, y1, x2, y2, duration_ms?, settle? | Swipe between pixels |
| `touch` | device, x, y, phase(down\|move\|up) | One raw touch sample |
| `button` | device, button(home\|lock\|side\|siri\|apple-pay), hold_ms? | Press a hardware button |
| `type` | device, text | Type on the hardware keyboard |
| `key` | device, usage | Press a key by HID usage |
| `open_url` | device, url | Open a URL |
| `install` | device, path | Install an .app |
| `uninstall` | device, bundle_id | Uninstall an app |
| `launch` | device, bundle_id, args?, env?, shaped? | Launch with arguments and environment (shaped: inject the network shaper); returns pid |
| `terminate` | device, bundle_id | Terminate an app |
| `apps` | device | Installed bundle ids |
| `appearance` | device, mode(light\|dark) | Light or dark UI |
| `location` | device, latitude, longitude | Simulated location |
| `privacy` | device, action(grant\|revoke), service, bundle_id | Privacy permission |
| `push` | device, bundle_id, payload | Deliver an APNs payload (object) |
| `status_bar` | device, time?, data_network?, wifi_mode?, wifi_bars?, cellular_mode?, cellular_bars?, operator?, battery_state?, battery_level? | Override status bar |
| `clear_status_bar` | device | Remove status bar overrides |
| `content_size` | device, size | Dynamic Type size (name or 0-11) |
| `increase_contrast` | device, on | Increase Contrast |
| `dynamic_island` | device, suppressed | Hide the Dynamic Island |
| `backlight` | device, on | Display backlight |
| `keyboard_language` | device, language | Software keyboard language |
| `hardware_keyboard` | device, on | Connect the hardware keyboard |
| `ca_debug` | device, option, on | Render overlay (see ca_debug_state for names) |
| `ca_debug_state` | device | Every render overlay and whether it is on |
| `reset_keychain` | device | Delete every keychain item |
| `memory_warning` | device | Simulate a memory warning |
| `notify` | device, name, state? | Post a Darwin notification (optionally setting its state) |
| `biometric` | device, action(match\|nomatch\|enroll\|unenroll), face? | Touch ID / Face ID |
| `exec` | device, program, args?, env? | Run a program inside the device; returns stdout/stderr |
| `logs` | device, last?, predicate? | Unified log lines (default last 1m) |
| `processes` | device | Device processes with CPU and memory |
| `kill` | device, pid, signal? | Signal a device process |
| `connections` | device | Open sockets of device processes |
| `traffic` | device | Bytes in/out per device process |
| `containers` | device, sizes? | App data and bundle containers |
| `ls` | device, path? | List a directory under the device data dir |
| `read` | device, path | Read a file (base64) |
| `write` | device, path, text? \| base64? | Write a file |
| `rm` | device, path | Remove a file or directory |
| `du` | device, path? | Size in bytes |
| `clear_container` | device, path | Empty a container (an app's data) without deleting it |
| `defaults_read` | device, domain | Read a preferences domain |
| `defaults_write` | device, domain, key, value, type? | Write a preference (type: string\|int\|float\|bool) |
| `crash_reports` | device | Host crash reports for this device |
| `info` | device | Paths and identity |
| `network_shape` | device, preset?(offline\|3g\|edge\|lte\|lossy\|very-bad\|none), offline?, latency_ms?, down_kbps?, up_kbps?, loss_pct?, port_base?, port_count?, rules?([{pattern, offline?, latency_ms?, down_kbps?, up_kbps?, loss_pct?}]), proxy?(default true), inject?(false\|true\|"launchd"\|"boot") | Shape the device's network without root. proxy: a shaping HTTP/SOCKS proxy set in the device's configd_sim. inject: an in-process socket shaper; "boot" reboots the device with it in launchd's environment so every process, WebKit included, is shaped live by the profile |
| `network_state` | device | Current profile, proxy port and injection state |
| `network_flows` | device | Connections through the device's shaping proxy (host, bytes, duration, outcome) |
| `network_clear` | device | Remove shaping: clear the device proxy, stop the proxy, remove injection |
| `audio_state` | device | Device audio settings (routed host UIDs, volume) and every host audio device |
| `audio_volume` | device, level(0-100) | Device output volume (100 = unity); applies to audio clients started afterwards |
| `audio_route` | device, scope(input\|output), uid \| "default" | Pin the device's input or output to a host device (any device, including loopback); applies to clients started afterwards |
| `audio_capture_start` | device, path?(.wav), mute? | Capture this device's audio output with a process tap (no driver); mute silences it on the host meanwhile; returns capture_id |
| `audio_capture_stop` | capture_id | Stop a capture: frames, duration_ms, rms, peak, dbfs, path |
| `audio_mute` | device, on | Silence (or restore) this device's audio on the host: a per-device null sink |
| `network_env` | device | Environment that makes one app launch shaped (for simctl: prefix each key with SIMCTL_CHILD_) |
