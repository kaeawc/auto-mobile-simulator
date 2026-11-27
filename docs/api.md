# simee API reference

Generated from `simee call help`. Transport: unix socket, one JSON request per line, one JSON response per line.

```text
→ {"id": 1, "method": "tap", "params": {"device": "booted", "x": 600, "y": 1300}}
← {"id": 1, "ok": true, "result": null}
```

A connection may send any number of requests, each answered before the next is read; keep one open
rather than connecting per call. Large results (a base64 `screenshot`) are written as one line
straight from the result, so read up to the newline before parsing.

**Binary replies.** A request may add `"binary": true` to take a result's bytes raw instead of as
base64 in the line (a local client that wants the PNG skips the base64 encode, the JSON escaping and
a third more bytes on the socket). When the result has a bytes field (today `screenshot`'s `png`
without `path`), the response line carries `"binary": <len>`, the field holds `{"binary": <len>}`,
and exactly `<len>` raw bytes follow the newline before the next response:

```text
→ {"id": 3, "method": "screenshot", "params": {"device": "booted"}, "binary": true}
← {"binary":4383251,"id":3,"ok":true,"result":{"height":2622,"png":{"binary":4383251},"width":1206}}\n
← <4383251 bytes of PNG>
```

- It is asked per request, so connections stay stateless, a client can mix both kinds, and every
  request without the field (every existing client, MCP) gets the same bytes as before.
- No bytes field (any other method, or `screenshot` with `path`), or an error: the normal response
  line, with no `binary` and nothing after it. A server from before binary replies ignores the
  field and answers with base64, so a client that asks should accept either.
- Read the line, then exactly `<len>` bytes: a stream that ends first is an error (the Rust client,
  `sim_api::Client::call_binary`, says `the reply ended after N of <len> bytes`).
- A reply carries at most one attachment. `read`'s `base64` is not a bytes field yet: it stays base64.
- `simee screenshot <device> -` writes the PNG to stdout this way (in its own process, the bytes are
  never base64 either). `simee screenshot <device> <path>` keeps sending `path`: whichever process
  runs the method writes the file, so the PNG doesn't cross the socket at all.
- The MCP server stays base64: its tool results are JSON text.

**The CLI and a server.** When a matching `simee serve` is listening on the default socket
(`$SIMEE_SOCKET`, else `$TMPDIR/simee.sock`), other `simee` commands run their method on that
server instead of in their own process: they skip connecting to the simulator service, reuse the
server's per-device HID and display connections, and drop the CLI's 100 ms post-input wait (the
server's HID client outlives the command). The output is byte for byte the same.

- **Handshake.** Before forwarding, the CLI calls `identity` with a 1 s deadline for connecting and
  answering. It forwards only when the answer equals its own: the same build (crate version and a
  hash of the method table), the same executable (on macOS its Mach-O `LC_UUID`, which the linker
  derives from the linked code: changed code never matches, a relink or copy of the same code
  does; elsewhere path, inode, size and mtime unchanged since the server started), and the same
  environment (a hash of every `ANDROID_*`, `EMU_*` and `SIMEE_*` variable, `DEVELOPER_DIR`,
  `HOME`, `TMPDIR`, and the `adb` that `PATH` finds, which the Android backend falls back to). `PATH`
  itself is not part of it (a pyenv shim or an extra directory changed it without changing any
  result), nor are the client's own switches (`SIMEE_SOCKET`, `SIMEE_IN_PROCESS`,
  `SIMEE_REQUIRE_SERVER`, `SIMEE_VERBOSE`, `SIMEE_E2E_*`).
- **Fallback.** No server, a stale socket file, another build or environment, a server from before
  the handshake (it answers "unknown method"), or one that doesn't answer in time (suspended,
  wedged): the command runs in the CLI's own process. A forwarded command has no timeout after the
  handshake (a boot can take minutes). `--in-process` (or `SIMEE_IN_PROCESS=1`) skips the server
  altogether. Running here costs several times more per call (screenshot 150 ms instead of 30 ms,
  tap 316 ms instead of 78 ms, measured), so a farm can see or forbid it: `--verbose` (or
  `SIMEE_VERBOSE=1`) prints where each command ran and why the server wasn't used
  (`simee: `tap` runs in this process: the server on … can't be used: the server is another build
  or environment: …`), and `SIMEE_REQUIRE_SERVER=1` makes such a command fail with that reason
  instead of running here (methods that never forward still run here).
- **Never forwarded** (`sim_api::IN_CALLER`): `boot` and `boot_many` (on Android both start `emu` as a child of the
  calling process; on a server it would be the server's child, in its process group, killed by
  Ctrl-C on `simee serve` and never reaped), `network_shape`, `network_state`, `network_flows`,
  `network_clear` (the shaping proxy is a thread of the process that starts it),
  `audio_capture_start`, `audio_capture_stop`, `audio_mute` and `audio_state` (a capture or mute is
  a helper process its caller owns). These always run in the caller, and need no handshake.

Relative `path` arguments (`screenshot`, `install`) are made absolute before they are sent.

`device` is a UDID (an Android device's UDID is its AVD name), an exact name, or `booted` (the default: the only running device). It is resolved across both platforms; pass `platform` (`ios` or `android`) to look at one. Parameters ending in `?` are optional.

**Platforms** says whose devices a method works on. Calling a method on a device of another platform fails with the platforms it does work on, e.g. `` `status_bar` isn't available on Android devices (it works on iOS) ``. Every device in a result carries `platform`, `form_factor` and `sub_platform`.

| Method | Platforms | Parameters | Description |
| --- | --- | --- | --- |
| `help` | Android, iOS | — | List methods with the platforms each supports |
| `identity` | Android, iOS | — | This process's build, executable and environment (hashed): the CLI forwards a command to a server only when they match its own |
| `form_factors` | Android, iOS | platform?, form_factor? | Every form factor on every platform: sub-platform, inputs and reference profile, or why it's unsupported |
| `devices` | Android, iOS | booted?, platform?(android\|ios), form_factor? | List devices with their platform, form factor and sub-platform |
| `runtimes` | Android, iOS | platform? | List installed runtimes (Android: system images) |
| `device_types` | iOS | — | List device types |
| `create` | Android, iOS | name, platform?(ios\|android), form_factor?; iOS: device_type, runtime; Android: system_image, device?(pixel_fold\|surface_duo) | Create a device. form_factor is checked against the device type (iOS) or the system image (Android) and shapes a new AVD |
| `delete` | Android, iOS | device | Delete a device |
| `boot` | Android, iOS | device, timeout_s?; iOS: env?, shaped?, settle?, settle_cpu_pct?, settle_window_s?, settle_foreground?(true), settle_timeout_s?, profile?(slim); Android: audio?(false) | Boot and wait until finished (iOS: env is the device launchd's environment, shaped injects the network shaper; settle also waits up to settle_timeout_s (600) until the device's processes use under settle_cpu_pct (10) % of a core for settle_window_s (10) s (settle_foreground, the default: counting only foreground-QoS CPU) and returns the timings with cpu_pct, foreground_cpu_pct, background_cpu_pct and cpu_s (CPU-seconds the tree used since the boot; for a device already booted, since the first sample), settled false if it ran out of time; profile turns off a named set of daemons (written before the boot of a shut-down device, so they never start); Android: starts emu and waits for sys.boot_completed, silent unless audio, which plays the device on the host's default output) |
| `boot_many` | Android, iOS | devices, concurrency?, timeout_s?; iOS: env?, shaped?, settle?(default true), settle_cpu_pct?, settle_window_s?, settle_foreground?(true), settle_timeout_s?, profile?; Android: first_port?(5554) | Boot several devices of one platform, at most `concurrency` at once (default: a quarter of the host's CPUs, at least 2), each as `boot` does (iOS: a slot is held until the device settles, unless settle is false); returns each device's timings or error, failed and unsettled counts, and the time until all were done. Android: each on its own console port from first_port up |
| `settle` | iOS | device, cpu_pct?, window_s?, foreground?(true), timeout_s?(600) | Wait until a booted simulator is quiet: its processes under cpu_pct (10) % of a core for window_s (10) s (foreground, the default: only foreground-QoS CPU counts), SpringBoard running; returns settle_s, cpu_pct, foreground_cpu_pct and background_cpu_pct, and cpu_s and background_cpu_s (CPU-seconds used since the first sample) |
| `profile` | iOS | device, name(slim), undo? | Turn a named set of daemons off on a booted simulator (disabled in its launchd database and stopped: stays off across reboots), or with undo back on (from the next boot); returns how many changed, those that failed and those that weren't loaded |
| `clone` | Android, iOS | device, name | Create a device as a copy of a shut-down one. iOS: a simulator (data, settings, apps; APFS clones of its files); Android: an AVD from a golden that has booted under simee (copy-on-write clones of its disks). A booted-once template's copies skip the first-boot work |
| `shutdown` | Android, iOS | device | Shut down |
| `erase` | iOS | device | Erase content and settings (shut down first) |
| `screen` | Android, iOS | device, display?(main\|outer\|inner\|carplay) | Screen size in pixels, the space tap, swipe and touch take (a folded Android foldable: the size its guest lays out for, a Pixel Fold's 1080x2092, as uiautomator's bounds). display (iOS): main is the screen in use (an unfolded iPhone Duo's inner one), outer and inner an iPhone Duo's two, carplay an iPhone's CarPlay screen |
| `screenshot` | Android, iOS | device, path?, display?(main\|outer\|inner\|carplay) | PNG to path, else base64 in `png` (a folded Android foldable: its outer screen as the scanout shows it, a Pixel Fold's 949x1840; an iPhone Duo's inner screen as its framebuffer holds it, 2007x2853) |
| `tap` | Android, iOS | device, x, y, hold_ms?, display?(main\|outer\|inner\|carplay) | Tap at pixels, held hold_ms (default: iOS 10, Android 60) (display, iOS only: an iPhone Duo's outer or inner screen, an iPhone's CarPlay screen) |
| `swipe` | Android, iOS | device, x1, y1, x2, y2, duration_ms?, settle?, display?(main\|outer\|inner\|carplay), edge?(top\|bottom\|left\|right; iOS) | Swipe between pixels over duration_ms (default 300; iOS sends its samples on schedule, so a busy host doesn't stretch it); with edge (iOS) the touch comes in from that screen edge, as a drag from Simulator.app's bezel: home and the app switcher from the bottom, Notification Center and Control Center from the top (Android's edge gestures need only the start point) |
| `touch` | Android, iOS | device, x, y, phase(down\|move\|up), display?(main\|outer\|inner\|carplay), edge?(top\|bottom\|left\|right; iOS) | One raw touch sample |
| `button` | Android, iOS | device, button(home\|lock\|side\|siri\|apple-pay\|action \| back\|volume-up\|volume-down\|recents\|menu\|up\|down\|left\|right\|select\|play-pause), hold_ms? | Press a hardware button, as the device's form factor has it: the Siri Remote on Apple TV (menu/back, home, select, play-pause, arrows) and the D-pad on Android TV; on Apple Watch home is the crown's press. Held hold_ms (default: 10 for home and lock on an iPhone or iPad, else 80) |
| `type` | Android, iOS | device, text | Type on the hardware keyboard (iOS: a key report every 3 ms) |
| `key` | Android, iOS | device, usage | Press a key by HID usage (iOS: down and up 3 ms apart) |
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
| `prefs_stores` | Android, iOS | device, app | An app's preference stores, each with the id the other prefs_* methods take: Android `shared_prefs/<name>.xml` and DataStore `datastore/<name>.preferences_pb`, iOS the app's domain (its bundle id) and its app-group suites; any other iOS domain is reachable by its name |
| `prefs_read` | Android, iOS | device, app, store? | Every key of an app's preference stores (or one) with its typed value, {type, value}: string, int, long, float, double, bool, string_set (Android), date (ISO 8601), data (base64), array and dictionary (iOS; of typed values) |
| `prefs_write` | Android, iOS | device, app, store, key, value, type(string\|int\|long\|float\|double\|bool\|string_set\|date\|data\|array\|dictionary) | Set one key to a typed value (refused when the value isn't of its type or the store can't hold the type); visible says whether the running app sees it now or after_relaunch, with a note on why |
| `prefs_delete` | Android, iOS | device, app, store, key | Remove one key; visible as prefs_write's |
| `prefs_batch` | Android, iOS | device, app, store, ops([{key, type, value} \| {key, delete: true}]) | Set and remove several keys of one store, all or nothing: every op is checked first (each key once), then the store is written once or not at all |
| `prefs_watch` | Android, iOS | device, app, store?, cursor?, wait_ms? | Changes to an app's preference stores (or one), as `logs` stream does: the first call starts a feed in this process and returns a cursor; a call with it returns the changes since, {store, key, old, new, at}, oldest first, waiting up to wait_ms (at most 60000) for one, and the next cursor (reset: the feed restarted, re-read; missed: changes dropped unread; ended: the device stopped). A feed nobody reads for 5 minutes stops |
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
| `fold` | Android, iOS | device, folded(true\|false) \| angle(0-180; 0-360 dual-screen) \| posture(closed\|half-opened\|opened\|flipped\|tent) | Set a foldable's hinge. Android: as the console's fold, unfold and posture do (the hinge sensor, the posture and, folded, the outer screen). iOS: an iPhone Duo's (closed, half-opened is the 100° book posture, opened), which SpringBoard sweeps to, moving the content to the inner screen when it opens past its swap boundary. Returns angle, posture, folded and the size the guest lays out for (iOS: of the screen in use, named in display) |
| `xr` | Android | device, action(recenter \| rotate\|move: x, y, z \| options: environment(day\|night), passthrough(0-1) \| input: mode(mouse\|hand\|eye) \| viewport: mode(pan\|zoom\|rotate) \| pointer: x, y, phase?(move\|down\|up\|click) \| hand\|eye: x, y, press?(aim\|press) \| gesture: gesture(left-pinch\|left-grab\|left-poke\|right-pinch\|right-grab\|right-poke)) | Android XR: turn (radians) or move (metres) the head, recenter, set the room and passthrough, the input mode, aim or pinch the hand ray and look or select with gaze at pixels (stock's gRPC hand and eye events), a hand gesture, or point the dual-mode mouse |
| `touchpad` | Android | device, x, y, phase?(down\|move\|up\|tap) \| x, y, to_x, to_y, duration_ms? (a swipe) | Touch or swipe the touchpad beside the screen, in touchpad units (Android XR display glasses: `hw.touchpad0`) |
| `stylus` | Android, iOS | device, x, y, phase?(tap\|hover\|down\|move\|up\|leave), pressure?(1-255), button?, eraser? \| x, y, to_x, to_y, duration_ms? (a stroke) | A stylus on the touch screen at pixels. Android: what the stock emulator's gRPC pen events send: hover over it, touch, move, lift or leave, with the barrel button or the eraser end; or draw a stroke. iPad: the Apple Pencil (UITouch type .pencil): tap (hold_ms?), down, move, up or a stroke; hover, leave, pressure, button and eraser are refused with the reason (the simulator's touch screen has none) |
| `rotary` | Android, iOS | device, ticks \| delta (Digital Crown units; iOS) | Turn a watch's rotary input: the Wear OS crown or bezel, the Apple Watch Digital Crown |
| `vehicle` | Android | device, property?(VHAL name or id: GEAR_SELECTION, gear, speed, 0x11600207, ...), area_id?, value? | Android Automotive's vehicle HAL: list every property's value, read one, or set one when `value` is given (typed by the property: drive, 12.5, true); through the emulator console's `car` command (simee) |
| `watch_gesture` | iOS | device, gesture(double-tap\|wrist-flick) | An Apple Watch hand gesture |
| `remote_swipe` | iOS | device, direction(up\|down\|left\|right), duration_ms? | Swipe on the Siri Remote's touch surface (Apple TV) |
| `pointer` | iOS | device, x, y, action?(move\|click\|down\|up), button?(primary\|secondary), hold_ms? \| x, y, to_x, to_y, button? (a drag) | The iPad's pointer (a mouse, as Simulator.app's pointer capture connects): move it to pixels and hover there, click, press or release a button there, or drag to to_x, to_y. Exact on iOS 26.5, within ~20 pt on 27.2. A button held by `down` keeps the pointer's place for the next calls on a server (`simee serve`) until a reboot. Refused: `down` without a server (the process ends with the call: use `click` or a drag), `up` with nothing held, and `move`/`up` after a failed call lost the held button (`click`, `down` or a drag lets go and starts over) |
| `carplay` | iOS | device, on | Attach (or detach) the CarPlay display to a booted iPhone (iOS 26 runtimes); then tap/swipe/screenshot with display=carplay |

## App preferences (`prefs_*`)

One typed API over Android SharedPreferences and DataStore preferences and iOS user defaults,
with no SDK in the app (`crates/sim-api/src/prefs.rs`). The iOS and Android backends are still
being written: until a platform's lands, every `prefs_*` call on its devices checks its parameters
and then fails with "app preferences (`prefs_*`) are not yet available on <platform> devices".
`defaults_read` and `defaults_write` are unchanged.

**Stores.** `prefs_stores {device, app}` lists the app's stores as
`{id, kind, name, app, platform}`; the other methods name a store by its `id`:

| kind | Platform | id | Holds |
| --- | --- | --- | --- |
| `shared_preferences` | Android | `shared_prefs/<name>.xml` | string, int, long, float, bool, string_set |
| `datastore` | Android | `datastore/<name>.preferences_pb` | string, int, long, float, double, bool, string_set |
| `app_domain` | iOS | the bundle id | string, int, long, float, double, bool, date, data, array, dictionary |
| `app_group` | iOS | the suite, `group.…` | as `app_domain` |
| `domain` | iOS | any other domain name | as `app_domain` |

**Values.** Every value travels with its type, `{"type": <type>, "value": <value>}`, so an `int`
comes back an `int` and a `long` a `long`:

```text
{"type": "string", "value": "Ada"}
{"type": "int", "value": 3}                       32-bit
{"type": "long", "value": 9007199254740993}       64-bit
{"type": "float", "value": 0.1}                   32-bit; also "NaN", "Infinity", "-Infinity"
{"type": "double", "value": 2.5}                  64-bit; the same three strings
{"type": "bool", "value": true}
{"type": "string_set", "value": ["a", "b"]}       sorted, each string once
{"type": "date", "value": "2026-11-27T10:00:00.25Z"}   ISO 8601 in (any offset), UTC out, to the nanosecond
{"type": "data", "value": "AAEC"}                 standard base64
{"type": "array", "value": [{"type": "int", "value": 1}]}
{"type": "dictionary", "value": {"k": {"type": "bool", "value": true}}}
```

An array's elements and a dictionary's values are typed values too, nested to any depth.
`prefs_read {device, app, store?}` answers `[{store, values: {<key>: <typed value>}}]`, one entry
per store (every store of the app without `store`).

**Writes.** `prefs_write {device, app, store, key, type, value}` and `prefs_delete {device, app,
store, key}` answer `{store, key, visible, note?}`. A write is refused before anything is written
when the value isn't exactly of its type (a fraction for an `int`, a number past its range, a
string for a `bool`, an untyped array element) or the store can't hold the type (a `double` in
SharedPreferences, a `string_set` on iOS); errors name the value (`value[2].value`, `ops[1].value`).
`visible` is `now` when the running app sees the write at once, or `after_relaunch` when it sees it
only once relaunched, with `note` saying why: a platform that can't reach a running app's copy
says so instead of failing silently.

**Batches.** `prefs_batch {device, app, store, ops}` takes `{key, type, value}` sets and
`{key, delete: true}` deletes and is all or nothing: every op is checked first (each key at most
once), and the backend then writes the store once with all of them, or fails with none applied.
It answers `{store, keys, visible, note?}`.

**Watching.** `prefs_watch {device, app, store?, cursor?, wait_ms?}` works like `logs`' `stream`:
the first call (no `cursor`) starts a feed of the app's stores (or one) in the process that answers
(a `simee serve` keeps it between commands) and returns `{cursor, events: []}`. A call with that
cursor returns the changes since, oldest first, and the next cursor:

```text
→ {"method": "prefs_watch", "params": {"device": "pixel", "app": "com.example", "cursor": "3:0", "wait_ms": 30000}}
← {"id":null,"ok":true,"result":{"cursor":"3:2","events":[
    {"store":"shared_prefs/com.example_preferences.xml","key":"launches","old":{"type":"int","value":3},"new":{"type":"int","value":4},"at":"2026-11-27T10:00:01Z"},
    {"store":"shared_prefs/com.example_preferences.xml","key":"token","old":{"type":"string","value":"x"},"new":null,"at":"2026-11-27T10:00:02Z"}]}}
```

- `old` is `null` for a key that was added, `new` for one that was removed; a write that changes
  nothing reports nothing. Changes come from whoever made them: the app, these methods, another
  tool.
- With nothing new, the call waits up to `wait_ms` (at most 60000) for a change.
- `missed: <n>`: the feed keeps the latest 4096 changes, and `n` older ones went unread.
- `reset: true`: the cursor is from a feed that has stopped; the new cursor starts now, and changes
  in between may be lost, so read the store again.
- `ended: true`: the device stopped; the next call starts a new feed.
- A feed nobody reads for 5 minutes stops.
- To follow a store from a known state: `prefs_watch` first, then `prefs_read`, then poll with the
  cursor; a change made during the read is in the feed.
