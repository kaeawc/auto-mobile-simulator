# Camera sources

`crates/sim-camera` (`simee-camera`) is the host side of device cameras: a catalog of the host's
cameras and the synthetic sources, one source id scheme for all of them, and a capture hub that
feeds any number of consumers from one capture per source. `emu`'s Android camera HAL service
([below](#android-the-goldfish-camera-hal)) links it; the API methods and CLI (`camera_sources`,
`camera_link`, #323) and the iOS capture shim (#322) build on it next. The plan is epic #319; the
iOS background is [research/audio-camera.md](research/audio-camera.md) §3.

## What exists

| Piece | macOS | Linux |
|---|---|---|
| Host camera listing (`catalog::host_cameras`) | AVFoundation `AVCaptureDeviceDiscoverySession`: built-in, external, Continuity Camera, Desk View, and every CoreMediaIO camera extension | V4L2: every `/dev/video*` node that can capture, with `VIDIOC_QUERYCAP` and the format, size and interval enumerations |
| Virtual camera marking | transport type `virt` (what camera extensions report), or a known name, maker or model (OBS, Camo, mmhmm, "virtual", …) | the v4l2loopback driver or bus (`platform:v4l2loopback-*`), the vivid and akvcam test drivers |
| Capture from a host camera | `AVCaptureSession` with a BGRA video data output, scaled to each request | not yet: V4L2 streaming capture returns `Unsupported` |
| Camera permission (`tcc::status`) | `authorizationStatusForMediaType:`, never prompts | none needed (`not-required`) |
| Synthetic sources | all but `video:` | all but `video:` |
| Hot-plug | `catalog::changes` diffs two listings into added/removed events; the caller polls | same |

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

Everything after the first `:` belongs to the source, so `qr:https://example.com/a:b` works.

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
- Capture from a camera on Linux: V4L2 streaming (`VIDIOC_REQBUFS` with mmap buffers, YUYV and
  MJPEG to BGRA). Listing works.
- A live capture from a macOS camera has no automated test: it needs the camera permission, which
  the tests never ask for.
- NV12 frames and IOSurface-backed output: frames are BGRA bytes.
- Hot-plug is a diff of two listings; nothing watches for changes yet.
