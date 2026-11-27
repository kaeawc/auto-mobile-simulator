# Camera sources

`crates/sim-camera` (`simee-camera`) is the host side of device cameras: a catalog of the host's
cameras and the synthetic sources, one source id scheme for all of them, and a capture hub that
feeds any number of consumers from one capture per source. Nothing links it yet: the API methods
and CLI (`camera_sources`, `camera_link`, #323), the Android camera HAL (#321) and the iOS capture
shim (#322) build on it. The plan is epic #319; the iOS background is
[research/audio-camera.md](research/audio-camera.md) §3.

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
- The capture runs at the largest size and the highest frame rate its subscribers asked for. Each
  subscriber gets frames at its own size and rate; a frame already the right size is shared.
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

## Not yet

- `video:<path>`: needs VideoToolbox decoding (`Unsupported` until then).
- Capture from a camera on Linux: V4L2 streaming (`VIDIOC_REQBUFS` with mmap buffers, YUYV and
  MJPEG to BGRA). Listing works.
- A live capture from a macOS camera has no automated test: it needs the camera permission, which
  the tests never ask for.
- NV12 frames and IOSurface-backed output: frames are BGRA bytes.
- Hot-plug is a diff of two listings; nothing watches for changes yet.
