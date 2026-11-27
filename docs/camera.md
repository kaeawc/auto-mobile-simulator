# Camera sources

`crates/sim-camera` (`simee-camera`) is the host side of device cameras: a catalog of the host's
cameras and the synthetic sources, one source id scheme for all of them, and a capture hub that
feeds any number of consumers from one capture per source. `emu`'s Android camera HAL service
([below](#android-the-goldfish-camera-hal)) links it, and the API, CLI and MCP tools
([below](#linking-a-device-camera-api-cli-mcp), #323) link a device camera to a source, on
Android and on iOS through the simulator capture shim (#322). The plan is epic #319; the iOS
background is [research/audio-camera.md](research/audio-camera.md) §3.

The iOS feed (`ios_feed`, #322) serves a linked source to the simulator shim over
`<data>/cam.sock`: BGRA frames with size, frame number and link generation, paced by the source;
`Feed::set_source` relinks live. See [ios.md "Camera"](ios.md#camera).

## What exists

| Piece | macOS | Linux |
|---|---|---|
| Host camera listing (`catalog::host_cameras`) | AVFoundation `AVCaptureDeviceDiscoverySession`: built-in, external, Continuity Camera, Desk View, and every CoreMediaIO camera extension | V4L2: every `/dev/video*` node that can capture, with `VIDIOC_QUERYCAP` and the format, size and interval enumerations |
| Virtual camera marking | transport type `virt` (what camera extensions report), or a known name, maker or model (OBS, Camo, mmhmm, "virtual", …) | the v4l2loopback driver or bus (`platform:v4l2loopback-*`), the vivid and akvcam test drivers |
| Capture from a host camera | `AVCaptureSession` with a BGRA video data output, in the format nearest the request, scaled to each request | V4L2 streaming (4 mmap buffers), YUYV and Motion JPEG to BGRA ([details](#capture-details)) |
| Camera permission (`tcc::status`) | `authorizationStatusForMediaType:`, never prompts | none needed (`not-required`) |
| Synthetic sources | all but `video:` | all but `video:` |
| Another device's screen (`device:`) | AVDs and simulators | AVDs |
| Hot-plug (`Hub::events`) | `AVCaptureDeviceWasConnected` / `WasDisconnected`, debounced, then a fresh listing diffed against the last | inotify on `/dev` for `video*` nodes, the same way |

Each host camera has a stable id: AVFoundation's `uniqueID` on macOS and the V4L2 bus info on
Linux (`usb-0000:00:14.0-1`, `platform:v4l2loopback-000`), with a name, kind (`built_in`,
`external`, `continuity`, `desk_view`, `virtual`, `unknown`), the virtual flag, maker, model,
transport, and its formats (size, pixel format, highest frame rate). Listing never needs the
camera permission.

The listing test prints what the host has and passes with no cameras:

```bash
cargo test -p simee-camera lists_host_cameras -- --nocapture
```

## Source ids

| Id | Source |
|---|---|
| `webcam:<query>` | a host camera: its exact id, its exact name (any case), its index in the listing (`webcam:0`, or `webcam0` as the stock emulator says), or part of its name ignoring case, spaces and punctuation (`webcam:obs` finds "OBS Virtual Camera"). More than one match is an error listing them; none lists what exists. |
| `<name>` | the same as `webcam:<name>` when the id has no `:` |
| `pattern:testcard` | colour bars, a marker that moves each frame, and text with the frame number, size, rate and wall-clock time (UTC). A strip of 32 black and white cells across the top carries the frame number for machines: `synth::testcard_seq` reads it back, also from a scaled frame, so a consumer can measure pacing and latency. |
| `qr:<text>` | a QR code of the text (error correction M), black on white with a 4-module quiet zone, as large as fits |
| `text:<text>` | the text in white on dark grey, centred, wrapped at spaces, as large as fits (`\n` breaks lines) |
| `image:<path>` | a PNG or JPEG still, filling the frame (aspect kept, overflow cropped) |
| `solid:<colour>` | one colour: a name (`red`, `grey`, `orange`, …) or hex (`#336699`, `#369`) |
| `video:<path>` | not yet: returns `Unsupported` until the VideoToolbox decoder is wired up |
| `device:<udid or name>[:x,y,w,h]` | another simee device's live screen, optionally cropped to a region in that device's pixels: an AVD name, or a simulator's UDID or name (macOS). [Below](#another-devices-screen). |

Everything after the first `:` belongs to the source, so `qr:https://example.com/a:b` works. In a
`device:` id, a last `:x,y,w,h` (four numbers) is the region; any other `:` is part of the name.

### Another device's screen

`device:<A>` makes device A's screen another device's camera feed, so B's app can scan a QR code
or a document that A shows (pairing and login flows, two-device tests). `crates/sim-camera/src/device.rs`.

- **Build:** behind the `device-source` cargo feature of `simee-camera` (off by default), which
  `simee-api` enables; `emu` links the crate without it, so it carries neither the Android nor the
  iOS stack. Without the feature a `device:` id still parses and opening it fails Unsupported
  ("device sources are served by simee, not emu").
- **Frames:** an Android device's through emu's display socket and shared frame ring at full size
  (one `pread` per device frame, as the desktop window reads them); a simulator's through its
  framebuffer `IOSurface`'s frame callback, copied out once when the surface's seed says it
  changed. Each subscriber size is then drawn in one pass: the region cropped, scaled to fill the
  size (aspect kept, overflow cropped from both sides, nearest neighbour), swizzled to BGRA.
- **Pacing follows A:** a capture tick takes the frame A presented since the last tick, waiting up
  to one tick for it, so B gets A's frames as they come up to its own rate; while A's screen is
  still, its last frame repeats at B's rate (shared, not redrawn).
- **Region:** `x,y,w,h` in A's own screen pixels (what `simee screenshot` gives); a region past
  the screen's edge is clamped to it, one outside it fails.
- **Lookup:** an AVD of that name first (the AVD home `emu` and `simee` use), else a simulator by
  UDID or name. Only the API refuses a self-link; emu's console `camera link` doesn't check.
- **Errors:** a device's camera linked to its own screen is refused (`camera_link` compares the
  source with the target's UDID or AVD name and its name, ignoring case: a feedback loop). A
  source device that isn't running (no display socket) or isn't booted fails the open with
  `Missing`; one with no AVD and no simulator of that name is `Invalid`; a simulator source on
  Linux is `Unsupported`. No first frame within 5 s is a capture error. A source device that shuts
  down mid-capture (its display socket closes; the simulator leaves the booted state) ends the
  capture with `Missing`, reported as `SourceMissing`.
- **Who captures:** on an Android target, emu opens the source in its own process (linking it, as
  any source); on an iOS target, the simee server does.

```bash
simee open-url <A> https://example.com/qr                  # iOS; on Android: adb -s <serial> shell am start -a android.intent.action.VIEW -d <url>
simee camera link <B> --back device:<A>                    # Android: live through emu's console; iOS: both cameras
simee camera link <B> --back device:<A>:0,600,1080,1080    # just the code
```

## Frames and capture

Frames are BGRA, top row first, tightly packed, behind an `Arc`: a frame fanned out to several
consumers, and a still repeated every tick, is never copied. A consumer that needs an IOSurface
copies one in with `sim_coresim::videotoolbox::PixelBufferPool::fill`; this crate doesn't link
VideoToolbox.

`Hub` runs one capture per source (keyed by the canonical id, so `webcam:obs` and the camera's own
id share one):

- The first `subscribe` opens the source, and returns its error there: a missing camera, an
  ambiguous name, or the permission.
- The capture runs at the highest frame rate its subscribers asked for. Each subscriber gets
  frames at its own size and rate. A synthetic source draws one frame per distinct subscriber size
  (a QR code keeps its quiet zone and the test card its counter strip at any aspect ratio); a
  camera's frame is fitted to each size. A frame already the right size is shared.
- Sources open outside the hub's lock: a slow `image:` path or camera start delays only subscribers
  to that source.
- `image:` files over 8192 px a side or 36 megapixels are refused before any pixels are allocated.
- A slow subscriber falls at most two frames behind; newer frames are dropped for it, and the
  capture never waits.
- Dropping the last `Subscription` stops the capture at once and drops the source, which releases
  the camera. The next subscription starts a fresh capture.
- A capture that fails (a camera unplugged, frames stopping) ends every subscription with that
  error, after the frames already queued.

### Capture details

**Capture (Linux):** V4L2 streaming. simee picks the format, size and rate nearest the request
(`ENUM_FMT`/`FRAMESIZES`/`FRAMEINTERVALS`) and sets them with `S_FMT`/`G_FMT` and `S_PARM`. It
then streams through 4 mmap buffers (`REQBUFS`, `QBUF`/`DQBUF` with `poll`, `STREAMON`/`OFF`).

- YUYV (BT.601) and MJPEG become BGRA only when a subscriber asks for a frame. MJPEG frames that
  leave out the standard Huffman tables get them added.
- Damaged frames are skipped.
- A camera already in use (EBUSY) gives `Busy`. EACCES gives a permission error naming the `video`
  group. An unplug (ENODEV) gives `Missing`.

**Webcam size and fps (macOS):** the `AVCaptureDevice` format nearest the request becomes
`activeFormat`, matched on size first, then rate.

- Both frame-duration limits are set to the requested rate, or to the nearer end of the format's
  own range.
- The device stays locked across `startRunning`, so the session preset doesn't reset it.
- A device that can't be locked keeps its current format.

**Hot-plug:** `Hub::events()` gives `Added` and `Removed` events.

- **Linux:** inotify on `/dev` for `video*` nodes.
- **macOS:** `AVCaptureDeviceWasConnected` and `AVCaptureDeviceWasDisconnected`.
- Signals are debounced (300 ms quiet, 2 s cap) before the cameras are listed again and compared
  with the last listing.
- `SourceMissing` is reported when a running capture loses its camera.
- The watch starts with the first stream and stops with the last.

## Linking a device camera (API, CLI, MCP)

Four API methods (docs/api.md), the `simee camera` commands (docs/cli.md) and the same four tools in
the MCP server's `camera` toolset (docs/mcp.md):

| Method | CLI | What it does |
|---|---|---|
| `camera_sources {kind?}` | `simee camera sources [--kind host\|virtual\|synthetic]` | The host's cameras (each with its `virtual` flag) and the synthetic kinds, each with the `source` id to link, and the camera permission's status and fix. Reads the TCC status; never prompts. |
| `camera_link {device, facing, source}` | `simee camera link <device> [--back\|--front\|--both] <source>` | Links a camera (`facing`: back, front or both) to a source. |
| `camera_unlink {device, facing?}` | `simee camera unlink <device> [--back\|--front]` | Removes the link (default both); the camera goes back to the AVD's own (`hw.camera.*` in config.ini: none, the test card, or a source id there). |
| `camera_links {device?}` | `simee camera links [<device>]` | Every link, one row per camera, with its state. |

`boot` takes `camera_back` and `camera_front` (`simee boot <device> --camera-back <source>
--camera-front <source>`): each is linked as `camera_link` does, then the device boots.

- **Sources:** any source id above, `emulated` (the test card) or `none` (the camera off), or a
  host camera's name, index or part of its name: `obs` finds "OBS Virtual Camera", and a name
  that matches several cameras fails, listing them. A host camera is saved by its stable id
  (`webcam:<id>`), so a rename or a replug keeps the link. An `image:` path must be absolute (the
  CLI makes it so); `video:` isn't supported yet.
- **Saved:** links live in the device's state (`<avd>/simee/cameras.ini`, `back=<source>` and
  `front=<source>` lines). Every boot passes them to emu as `-camera-back` / `-camera-front`, so
  they come back after a reboot or a server restart, whichever front end boots the device.
- **Live:** a running device is relinked through emu's console (`camera link`), on the guest's
  next frame. The guest's camera provider lists cameras once, when it starts: a camera that was
  off at boot appears after a reboot, and `camera_link` says so (`note`).
- **States** (`camera_links`): `linked` (running, as saved), `saved` (shut down: applies at the next
  boot), `relinked` (changed from the console since: `live` says to what; the next boot restores
  the saved link), `unsaved` (linked from the console only), `off`, `source missing` (the host no
  longer lists the camera, and emu shows the test card until it is back) and `permission missing`
  (with the fix).
- **Errors name the fix:** a camera permission that isn't granted (the table above; checked
  before a webcam is linked), an ambiguous or unknown name (with the matches, or what exists and
  the synthetic sources), a device the stock emulator runs (boot it with `simee boot`), and an emu
  built before its camera console (#321: rebuild it and reboot).
- **iOS** ([below](#ios-simulators)): the same methods and flags, served by the simee server.

### iOS simulators

A simulator has no camera of its own: simee injects its capture shim (`shims/ios-camera`, see
[ios.md "Camera"](ios.md#camera)) into the device's apps and feeds it from the linked source.

- **One feed per device.** The shim's back and front cameras read one socket, so a link sets
  both cameras (`--back` or `--front` alone links both, and `note` says so); `--camera-back` and
  `--camera-front` on `boot` must give the same source. Unlinking one camera unlinks both.
- **The server owns the feed.** A booted simulator's feed (`ios_feed::Feed` on `<data>/cam.sock`)
  runs inside the simee server (`simee serve`) for as long as it runs, one per device, and a
  relink moves every connected app to the new source without a reconnect. A command that runs in
  its own process (`simee camera link` with no server running, or an MCP server) hands the call to
  a running server of the same build, or fails naming `simee serve` and saves nothing. Linking a
  shut-down simulator only saves the link, so it needs no server.
- **Injection** is device-wide: `launchctl setenv DYLD_INSERT_LIBRARIES <shim> SIMEE_CAMERA_SOCKET
  <socket>` in the device's launchd, joined with `:` to a library already inserted (the network
  shaper's), and taken out again on unlink. Apps started afterwards get the camera; apps that were
  already running need a relaunch (`simee terminate`, then `simee launch`), and the first link's
  `note` says so. An app still needs its camera permission: `simee privacy <udid> grant camera
  <bundle id>`.
- **The shim** is found at `$SIMEE_CAMERA_SHIM`, else in `ios-camera/` beside the simee
  executable or one directory up (`target/ios-camera/`, where `shims/ios-camera/build.sh` writes
  it for `target/{debug,release}/simee`), with `libsimee-camera-hooks.dylib` beside it. A link
  fails naming the build command when it is missing, and refuses a path with a `:` or a line
  break (dyld would split it). The link copies the shim and its hooks to
  `<data>/simee/camera-<hash>/` in the device's data directory and inserts that copy, so a
  rebuild, `cargo clean` or a removed worktree can't leave the device inserting a missing
  library; an erase removes the copy with the link. Before the `setenv`, a `launchctl` runs with
  the shim inserted, and a dyld failure refuses the link with nothing injected. A `launchctl` that
  prints anything on stderr is an error (never "unset"), and an unlink succeeds only once the
  device's environment no longer holds the shim. A link and an unlink of one device run one at a
  time.
- **The injection outlives the server.** `DYLD_INSERT_LIBRARIES` lives in the device's launchd
  for the rest of that boot. When `simee serve` exits, or the device is shut down outside simee
  (`simctl`, Simulator.app, an in-process shutdown), nothing takes it out: every app launched
  afterwards gets the hooks and a simulated camera with no frames. Clear it with `simee camera
  unlink <udid>` (needs the device booted) or by rebooting the device. A device shut down
  outside simee also keeps its feed thread in the server until the device is linked again.
- **Saved** like Android's, in the simulator's data directory (`<data>/simee/cameras.ini`; an
  erase removes it). A boot through simee (`simee boot`, the `boot` method) reapplies the saved
  link: on a server it starts the feed and injects the shim; elsewhere it hands the link to a
  running server. A shutdown, erase or delete through simee stops the feed.
- **States:** `linked` (served as saved), `relinked` (the feed serves something else; `live` says
  what), `shim missing` (a feed serves it, but `network_shape` or `network_clear` replaced the
  device's `DYLD_INSERT_LIBRARIES`: link again), `saved` (shut down; or booted with no feed in this server, after a boot outside simee or a
  server restart, with a `fix`: link again), `off` (`none`), and a webcam's `source missing` (its
  apps get no frames until the camera is back) or `permission missing`. A webcam source is
  captured by the server, so it is the server's camera permission (the table below).

```bash
simee serve &                                   # owns the feeds
shims/ios-camera/build.sh                       # once: target/ios-camera/libsimee-camera.dylib
simee privacy <udid> grant camera <bundle id>
simee camera link <udid> qr:hello               # both cameras; relaunch running apps
simee camera link <udid> pattern:testcard       # live: no relaunch
simee boot <udid> --camera-back solid:orange    # saved, then applied at the boot
simee camera unlink <udid>
```

```bash
simee camera sources
simee camera link Pixel --back qr:hello      # Pixel back: qr:hello (linked)
simee camera link Pixel --front obs          # Pixel front: webcam:<id> [OBS Virtual Camera] (linked)
simee camera links
simee camera unlink Pixel --front            # Pixel front: none (unlinked)
simee boot Pixel --camera-back pattern:testcard
```

## Camera permission (TCC)

On macOS camera access is a TCC grant held by the capturing process's *responsible app*: the app
that started the simee server (Terminal, iTerm, an IDE, or a launchd job's bundle), not `simee`
itself. Without it a capture gets no frames or black ones, so a webcam source reads the status
first and fails with an error that names the fix instead:

| Status | Fix in the error |
|---|---|
| `authorized` | none: capture starts |
| `denied` | turn it on in System Settings > Privacy & Security > Camera, or `tccutil reset Camera <bundle id>` to be asked again; restart the simee server |
| `not-determined` | ask once from a logged-in session (`tcc::request_access`, which shows the prompt), allow it, restart the simee server |
| `restricted` | a configuration profile or Screen Time blocks it; lift the restriction |
| `not-required` | Linux: access is the device node's mode (usually the `video` group) |

Only `tcc::request_access` can show the prompt, and nothing calls it implicitly; the tests read
the status and never prompt. Synthetic sources need no permission. The permission errors are
tested with a fake status (`tcc::Authorizer`).

## Android: the goldfish camera HAL

`emu` serves the goldfish camera protocol on `qemud:camera` (`crates/emu-vmm/src/pipe/camera.rs`),
so an unmodified system image's camera HAL sees the cameras linked to it.

### Linking

| Flag | config.ini | Values |
|---|---|---|
| `-camera-back S` | `hw.camera.back` | `emulated` or `virtualscene` (the test card, `pattern:testcard`), `webcam0`, `webcam1`, ... (the host's cameras by index), `none`, or any source id above (`qr:hello`, `webcam:obs`, ...) |
| `-camera-front S` | `hw.camera.front` | the same |

A flag wins over config.ini; neither means no camera. A bad flag fails the launch; a bad
config.ini value is reported and that camera is off. A webcam id is resolved, and its permission
checked, when the guest opens the camera, not at launch.

A host camera is never opened implicitly from an AVD's config.ini (opening it triggers the macOS
camera permission and the camera light). From config.ini, `emulated`, `virtualscene`,
`videoplayback`, `webcam0`, `webcamN` and any other bare word mean `pattern:testcard`, `none` means
no camera, and `emu` logs one line when it substitutes the test card for a webcam or another
non-test-card value. A host camera needs an explicit choice: a `-camera-back` / `-camera-front`
flag (`-camera-back webcam0`), the console's `camera link`, or a simee source id in the file
(`hw.camera.back=webcam:obs`). Stock semantics are kept for `emulated`, `virtualscene` and `none`.

### Limits

Each camera allows 4 open sessions; opens of a name that matches no camera, or beyond the cap, are
answered `ko` on the spot without a thread. The guest may start only the frame sizes `list`
offered (up to 1280x720) and gets `ko` for others, including in `offset=` frames. One query is in
flight per session: a query sent before the guest has read the previous reply is answered
`ko:busy` without producing a frame, and a guest that keeps doing so has its pipe closed, so host
memory per session is bounded by one frame. A source that fails to open or dies mid-stream shows
the test card ("source missing" in the log) and is retried every 0.5 s, doubling to 8 s, instead
of freezing on its last frame. A frame whose size doesn't match is `ko:conversion failed`, never
zeros.

The console relinks while the guest runs, and while it has the camera open:

```bash
adb -s emulator-5554 emu camera list                 # back: pattern:testcard / front: none
adb -s emulator-5554 emu camera link back qr:hello   # shows on the guest's next frame
adb -s emulator-5554 emu camera link front none
```

A relink keeps the size the guest opened the camera at: the new source is drawn (a synthetic one)
or fitted (a camera) to it. Unlinking an open camera keeps its last frame until the guest closes
it. The guest's provider reads `list` once, when it starts, so a camera linked after boot that
wasn't linked at boot appears only after the provider restarts (a reboot); one linked at boot can
be relinked freely.

### Protocol

Queries are NUL-terminated; replies are an 8-hex-digit length and a payload: `ok\0`,
`ok:<data>`, or `ko:<reason>\0`.

| Pipe | Query | Reply |
|---|---|---|
| `qemud:camera` | `list` | one line per linked camera: `name=simee-back channel=0 pix=<NV21> dir=back framedims=1280x720,640x480,352x288,320x240,176x144` (back first); `ok\0` when none |
| `qemud:camera:name=<name>` | `connect`, `disconnect`, `stop` | `ok` |
| | `start dim=WxH pix=<fourcc>` (API 30–33) or `start` (API 34+) | `ok`; NV21, NV12, YV12, YU12 (I420) and RGB32 are accepted |
| | `frame video=<n> preview=<n> whiteb=.. expcomp=.. time=..` (API 30–33) | `ok:` then the frame in the started format, then an RGB32 preview, each when its size is non-zero (and must match the started size) |
| | `frame dim=WxH pix=<fourcc> offset=<n> ...` (both HALs; the only one API 34+ sends) | the frame is written into the guest's gralloc buffer at `offset` in the goldfish address-space area; `ok` |

Pixel formats are V4L2 fourccs as decimals. RGB32 is R, G, B, A bytes, as the HAL reads it into
RGBA_8888 buffers; YUV is BT.601 limited range with 2x2-averaged chroma, as stock's converters
make it (`sim_core::yuv`). `whiteb` and `expcomp` are ignored (sources are drawn as they are), and
no frame timestamp is returned, so the HAL stamps frames with its own clock. Each session answers
from a thread of its own, so a source's first frame (up to 500 ms) or a conversion never stalls
the pipe transport that also carries adb. `EMU_CAMERA_LOG=1` logs every query; without it emu logs
the session queries and the first three frames after each start.

The offset path needs the address-space device, which emu has with graphics; without it (`-gpu
off`, the software device path) offset frames answer `ko`, so API 34+ guests get no camera frames
there.

Orientation isn't part of `list`: the guest HAL fixes it (the API 34+ provider reports 90° for the
back camera and 270° for the front, which it also mirrors), so a portrait app shows a landscape
source turned, as stock does with `webcam0`.

### System images

From the images' vendor partitions (`strings` on `vendor.img`) and boots under emu:

| API | Camera HAL | Frame path | Properties it reads |
|---|---|---|---|
| 30 | libhardware `camera.ranchu` (EmulatedQemuCamera3) | `video=`/`preview=`, or `offset=` | `ro.kernel.qemu.camera_protocol_ver`, `ro.kernel.qemu.camera_hq_edge_processing`, `ro.kernel.qemu.legacy_fake_camera`, `qemu.sf.fake_camera`, `qemu.camera.webcam.orientation`, `qemu.camera.fake.orientation` |
| 31–33 | the same | the same | as API 30 with `ro.boot.qemu.*` for `ro.kernel.qemu.*`, and `vendor.qemu.sf.fake_camera` |
| 34–36 (and 37) | AIDL `android.hardware.camera.provider.ranchu` | `offset=` only, RGB32 buffers (it converts to YUV and JPEG itself) | `ro.boot.qemu.camera_hq_edge_processing`, `ro.boot.qemu.legacy_fake_camera`, `vendor.qemu.sf.fake_camera` |

None of the properties is needed for the qemud cameras: emu sets none, the provider waits a
moment for `vendor.qemu.sf.fake_camera` and goes on, and `qemu.sf.fake_camera` unset means none
of the HAL's own fake cameras, so every camera is one of simee's.

Verified on an API 36 google_apis AVD booted by emu (`-camera-back emulated -camera-front
solid:orange`): the stock Camera app (`am start -a android.media.action.STILL_IMAGE_CAMERA`) shows
the test card with its counter advancing at 30 fps (frame 178 to 755 in 19 s), `camera link back
qr:hello` shows the QR code within a second, and the front camera shows its own source throughout.
On an API 33 google_apis AVD the libhardware HAL starts the back camera with `start dim=1280x720
pix=<YU12>` and reads `frame video=1382400 preview=0 ... time=1`: the same app shows the test card
and the relink to `qr:hello`.

## Not yet

Tracked in #335:

- `video:<path>`: needs VideoToolbox decoding (`Unsupported` until then).
- A live capture from a macOS camera has no automated test: it needs the camera permission, which
  the tests never ask for. The live macOS check of the nearest format and of hot-plug, and
  confirming that the device notifications arrive without a main run loop, are still to do.
- NV12 frames and IOSurface-backed output: frames are BGRA bytes.
- `device:`: the optional `:warp` (a mild perspective and noise, like a camera pointed at a phone)
  isn't done. Not yet run on devices: the checks (a QR code on A, B's back camera linked to
  `device:<A>`, B's scanner fixture decodes it, Android to Android and iOS to iOS) are to do.

Camera links (#323):

- iOS: `help` and docs/api.md still list `camera_link`, `camera_unlink` and `camera_links` as
  Android methods (the method table changes after #266). A server doesn't serve the feeds of
  simulators booted before it started (or booted outside simee) until they are linked again;
  `boot_many` doesn't reapply saved links; separate back and front sources need a shim with one
  socket per camera. Not yet run on a device: the checks are in docs/ios.md "Camera".
- `fps` and `size` on `camera_link`: an Android guest picks its camera's size itself (from the
  sizes `list` offers) and its rate, so they are not taken yet.
- The source list as an MCP resource: the `camera_sources` tool only.
- `source missing` comes from the host's camera listing (a webcam link whose camera isn't listed
  now); emu's own capture failures are in its log (`<avd>/simee/emu.log`).
