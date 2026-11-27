# Streaming

Device video over WebRTC: simee publishes each device's screen as H.264 over WHIP to a media
server (MediaMTX), and browsers play it over WHEP. The plan is epic #262; this page describes what
exists and what comes next.

## `sim-rtc`: the WHIP publisher

`crates/sim-rtc` (published as `simee-rtc`) takes an H.264 elementary stream (Annex B access
units with presentation times) and publishes it to a WHIP endpoint. It has no async runtime: the
WHIP requests block the caller, and the media path is one thread per publisher that sleeps on its
UDP socket.

```rust
let config = PublisherConfig::new("http://127.0.0.1:8889/device-1/whip");
let mut publisher = Publisher::connect(config, |event| match event {
    PublisherEvent::KeyframeRequested(_) => { /* encode an IDR next */ }
    PublisherEvent::BitrateEstimate { bps, .. } => { /* retarget the encoder */ }
    _ => {}
})?;
publisher.send(&access_unit, pts)?; // Annex B, one picture
// drop(publisher) or publisher.close(): DELETE the WHIP resource
```

| Module | What it does |
| --- | --- |
| `whip` | RFC 9725: POST the SDP offer (`application/sdp`), `201 Created` with `Location` (resolved against the endpoint) and `ETag`, up to 3 redirects, PATCH trickle ICE (`application/trickle-ice-sdpfrag`, `If-Match`; `405`/`501` means the server doesn't trickle), DELETE to end (`404` counts as done), `Authorization: Bearer` |
| `http`, `url` | A blocking HTTP/1.1 client on `std::net`: one deadline for the whole exchange, `Content-Length`, chunked and read-to-close bodies, size limits, no header injection |
| `h264` | Annex B splitting, access units, the SPS `profile-level-id`, the check that a stream may go out on the negotiated one, and the cached SPS/PPS put in front of every IDR |
| `rtp` | RFC 6184 packetization mode 1: single NAL unit packets and FU-A fragments at the MTU (1200 by default), the marker on each access unit's last packet, 90 kHz timestamps from the PTS; a depacketizer (also STAP-A) for tests and receivers |
| `sdp` | The `profile-level-id` the answer settled on; trickle ICE fragments |
| `publisher` | `Publisher`: str0m in RTP mode with one send-only H.264 format, one host candidate, the media thread, the keyframe-request throttle and the encoder callback |
| `datachannel` | The `simee-input` data channel protocol: viewer input decoded and validated, rate limited, moves coalesced, handed to an `InputSink`; acks that echo the viewer's timestamp (see "Input over a data channel") |

Rules it enforces:

- **Plaintext WHIP only to loopback.** `http://` to any other host is refused unless
  `allow_insecure_http` is set (a trusted LAN): the bearer token and the DTLS fingerprints that
  authenticate the media would cross the network in the clear. The `Location` and redirects are
  held to the same rule. `https://` is refused for now (see below).
- **The SPS must match what was negotiated.** The offer carries the configured
  `profile-level-id` (Constrained Baseline `42e01f` by default; Main `4d001f` and any other value
  work); the answer's is the session's. Every SPS sent must have the same `profile_idc` and a level
  no higher, or `send` refuses the access unit (`Error::ProfileMismatch`). A Baseline session
  takes Baseline with or without `constraint_set1`, as AutoMobile learned from Android encoders
  that emit `42 80 xx`.
- **Every IDR goes with its SPS and PPS**, from the access unit or the last ones seen, so a viewer
  that joins late or recovers from loss can decode it. Access units before the first IDR are
  dropped (`Sent::WaitingForKeyframe`); an IDR before any SPS/PPS is refused.
- **Keyframe requests reach the encoder, throttled.** PLI and FIR become
  `PublisherEvent::KeyframeRequested`, at most one per `keyframe_interval` (500 ms); requests
  inside it are coalesced into one at its end, never dropped.
- **Bandwidth estimates reach the encoder.** TWCC estimation (str0m's, on by default from 1 Mbit/s,
  probing up to `max_bitrate_bps`) and REMB from the server become
  `PublisherEvent::BitrateEstimate`. NACKs are answered from str0m's RTX cache.
- **The resource is deleted** when the publisher is dropped or closed, and when `connect` fails
  after the POST.

### Why str0m

[str0m](https://github.com/algesten/str0m) is sans-IO WebRTC: ICE, DTLS-SRTP, RTP/RTCP, NACK/RTX,
TWCC and the pacer as a state machine that the caller feeds with datagrams and time. That fits
simee, which has no async runtime (the MCP server avoided tokio for the same reason,
[benchmarks.md](benchmarks.md#mcp-server)). webrtc-rs needs tokio. Dependency trees, from a
scratch package each (`cargo tree -e normal --prefix none | sort -u | wc -l`, 2026-11-27):

| Stack | Crates in the normal tree |
| --- | --- |
| `simee-rtc` on str0m 0.26, macOS (CryptoKit backend) | 70 (80 with build dependencies) |
| `simee-rtc` on str0m 0.26, Linux, default features | 52 |
| `simee-rtc` on str0m 0.26, Linux, `--features rust-crypto` | 109 |
| `webrtc` 0.21 (webrtc-rs), default features | 219, tokio among them |

The publisher uses str0m's RTP mode and packetizes itself, so the MTU, the marker bit and the
SPS/PPS-before-IDR rule are ours and tested here; str0m does SRTP, the RTX cache and the pacer.
The counts include the root package.

### DTLS backends

str0m needs a crypto backend for DTLS. sim-rtc uses CryptoKit (`str0m-apple-crypto`) on Apple
targets, which builds no C. On Linux the pure-Rust backend (`--features rust-crypto`) still
generates its certificate with `rcgen`, which builds aws-lc's C; that would break
`cargo clippy --target x86_64-unknown-linux-gnu` from macOS (no C cross toolchain), so it's
opt-in. Without it, `Publisher::connect` is `Error::Unsupported` and the publisher's end-to-end
tests print SKIP; everything else (WHIP, HTTP, H.264, RTP) builds and tests everywhere.

### Tests

`cargo test -p simee-rtc`. The end-to-end tests (`tests/publish.rs`) run a fake WHIP server on
`std::net` whose answer comes from a str0m receiver on loopback UDP: offer and answer, ICE and
DTLS, every access unit back intact (FU-A included) with SPS/PPS before each IDR, PLI to the
callback, a bandwidth estimate, DELETE on drop, and refusals (profile mismatch, IDR without
parameter sets, a rejected answer). The fixtures are ten 96x64 frames of ffmpeg's `testsrc`
encoded by libx264 (Constrained Baseline `42c01e` and Main `4d401f`, two GOPs, about 3 KB each):

```bash
ffmpeg -f lavfi -i testsrc=size=96x64:rate=10 -frames:v 10 -c:v libx264 -profile:v baseline \
  -level 3.0 -g 5 -keyint_min 5 -sc_threshold 0 -bf 0 -pix_fmt yuv420p -x264-params slices=1 \
  -f h264 testsrc-baseline.h264   # -profile:v main -level 3.1 for testsrc-main.h264
```

`publishes_to_a_local_mediamtx` (ignored) publishes the fixture to a running MediaMTX
(`SIMEE_WHIP_URL`, default `http://127.0.0.1:8889/simee-test/whip`).

### The MediaMTX harness (#267)

`tests/mediamtx/mediamtx.yml` is the test config: WHIP and WHEP on loopback, the control API on,
`webrtcTrackGatherTimeout: 30s` (AutoMobile measured first frames up to 15 s on a device; the 2 s
default closes the publisher first), host candidates only (no STUN), RTSP, RTMP, HLS, SRT, MoQ,
metrics and pprof off, and anonymous publish, read and API from loopback only. It is valid on its
own (ports 8889, 9997 and 8189); the harness and the script start it on free ports through
MediaMTX's `MTX_*` environment overrides instead.

- **Finding MediaMTX.** `$MEDIAMTX_BIN`, else `mediamtx` on PATH. Nothing is downloaded by the
  tests or the script; with no binary the MediaMTX tests print `SKIP: no MediaMTX ...` and pass.
- **Rust.** `crates/sim-rtc/tests/common/mod.rs` has `MediaMtx`: it writes the config to a temp dir,
  starts the server on free ports, waits for `GET /v3/config/global/get` (failing with the log when
  the process dies or stalls), exposes `api_get` and `ready_path`, and on drop kills the child by
  the PID it spawned, removes the temp dir and, if the test is panicking, prints the server log.
  `tests/mediamtx.rs` publishes both fixtures (Constrained Baseline and Main) with the publisher
  and checks through the API that the path is `ready` with an `H264` track.
- **By hand.** `scripts/mediamtx.sh start` prints `WHIP_URL`, `WHEP_URL`, `API_URL`, the PID and
  the log path; `stop` kills the recorded PID; `status` reports it. `MEDIAMTX_STATE` moves its
  state dir. Like the Rust tests, the script exits 3 with a message when there is no binary.

Run it: `MEDIAMTX_BIN=/path/to/mediamtx cargo test -p simee-rtc --test mediamtx` (on Linux add
`--features rust-crypto`, or the publisher tests skip for lack of DTLS; the harness test still runs).

#### CI step to add

Not wired yet (this change touches no workflow). The step for the macOS and Linux jobs, with the
binary verified by hash and cached; fill in the SHA-256 of each archive from the release's
`checksums.sha256` before merging:

```yaml
env:
  MEDIAMTX_VERSION: v1.19.2   # pinned; bump with the hashes below
- name: MediaMTX
  run: |
    case "$RUNNER_OS-$RUNNER_ARCH" in
      Linux-X64)    asset=linux_amd64;  sha=<SHA256_LINUX_AMD64> ;;
      macOS-ARM64)  asset=darwin_arm64; sha=<SHA256_DARWIN_ARM64> ;;
      *) echo "no MediaMTX pin for $RUNNER_OS-$RUNNER_ARCH"; exit 1 ;;
    esac
    f="mediamtx_${MEDIAMTX_VERSION}_${asset}.tar.gz"
    dir="$RUNNER_TEMP/mediamtx"
    mkdir -p "$dir"
    curl -fsSL -o "$dir/$f" "https://github.com/bluenviron/mediamtx/releases/download/${MEDIAMTX_VERSION}/$f"
    echo "$sha  $dir/$f" | shasum -a 256 -c -
    tar -xzf "$dir/$f" -C "$dir" mediamtx
    echo "MEDIAMTX_BIN=$dir/mediamtx" >> "$GITHUB_ENV"
```

Cache `$RUNNER_TEMP/mediamtx` with `actions/cache`, keyed on the version and the asset, restoring
before the step and re-checking the hash after. The `linux` job needs `--features rust-crypto` for
the publisher tests to run rather than skip (see "Linux in CI" below).

### Input over a data channel (#271)

`Publisher::connect_with_input(config, InputSession::new(sink, InputConfig::default()), on_event)`
adds a reliable, ordered data channel labelled `simee-input` to the offer (an `m=application`
section beside the video); a peer that opens a channel with that label itself is accepted too.
Plain `connect` offers no channel. The channel's messages go to an `InputSession` on the
publisher's thread, which hands validated events to an `InputSink` (`fn input(&mut self, &InputEvent)
-> Result<(), String>`, also implemented for closures) and writes its replies back. Keep the sink
short: it runs where the media is sent. Wiring a sink to a simee device (Android display-socket
`touch`/`key`/`wheel`, iOS HID) is #266's.

The channel only reaches a viewer whose peer connection ends at the publisher. MediaMTX relays
tracks between WHIP and WHEP and, as far as its documentation says, not data channels; whether it
accepts an offer with an `m=application` section at all hasn't been run against it yet (the
MediaMTX harness publishes with plain `connect`). Input for a MediaMTX viewer likely needs a
direct peer connection or a relay of its own: #266 decides.

**Messages** are one compact JSON object per text frame. Every message has `"v":1` (the version)
and a `kind`; `seq` (an integer) and `t` (the viewer's time in milliseconds, any clock, e.g.
`performance.now()`) are optional and echoed back. Unknown fields are ignored, so a minor
addition stays readable by this version.

| `kind` | Fields | Notes |
| --- | --- | --- |
| `touch` | `phase` (`down`, `move`, `up`, `cancel`), `id` (pointer id), `x`, `y`, `w`, `h` | Multi-touch by `id` (up to 10 down). `x`/`y` in `0..=1` of the frame, `w`/`h` the frame size the viewer saw; `up`/`cancel` may omit the point and lift where the pointer last was |
| `key` | `phase` (`down`, `up`), `usage` or `key` | `usage`: a USB HID keyboard-page usage (`0x04..=0xe7`). `key`: a browser name, a `KeyboardEvent.code` (`KeyA`, `Digit1`, `ArrowLeft`, `ShiftLeft`, `F5`) or a layout-free `KeyboardEvent.key` (`Enter`, `Escape`, `Backspace`, `Tab`, `" "`, `Shift`); turned into the usage |
| `text` | `text` | Typed as it is (an IME commit, a paste) |
| `button` | `button` (`home`, `power`, `volume_up`, `volume_down`, `back`, `app_switch`, `menu`, `assistant`), `phase` (`down`, `up`, `press`; default `press`) | The sink refuses buttons its device lacks |
| `rotate` | `orientation` (`portrait`, `landscape_left`, `landscape_right`, `portrait_upside_down`) | |
| `fold` | `posture` (`closed`, `half_open`, `open`, `flipped`, `tent`) | The Android emulator's postures |
| `wheel` | `x`, `y`, `w`, `h`, `dx`, `dy` | Deltas in pixels of the frame the viewer saw (a `WheelEvent` in pixel mode), positive right and down |
| `ping` | | Answered with `pong` at once |

```json
{"v":1,"seq":7,"t":5123.4,"kind":"touch","phase":"down","id":0,"x":0.5,"y":0.25,"w":590,"h":1280}
```

**Replies**, the same shape:

| `kind` | Fields | When |
| --- | --- | --- |
| `hello` | `protocol` (`simee-input`), `versions` (`[1]`) | Once, when the channel opens |
| `ack` | `seq`, `t`, `rx`, `done` | An input message that carried `t`, once the sink took it: `rx` is its arrival and `done` the sink's return, Unix milliseconds on the server's clock |
| `pong` | `seq`, `t`, `rx` | Every `ping` |
| `error` | `seq` (when readable), `code`, `message` | A refused message; the session carries on |

Error codes: `encoding` (a binary frame or not UTF-8), `json`, `size` (over 4096 bytes), `version`
(no `v`, or not 1), `kind`, `field` (missing, wrong type or out of range: a coordinate outside
`0..=1`, a frame side of 0 or over 16384, a usage outside the keyboard page), `pointer` (a move or
lift for a pointer that isn't down, a second down, an eleventh pointer), `rate`, `sink` (the sink's
own refusal: a button the device lacks, a stale frame). Nothing a viewer sends panics the session:
the JSON reader is the crate's own (sim-rtc has no serde), bounded in depth and size.

**Rate limits and coalescing.** Messages pass a token bucket (1000 per second, bursts of 200);
beyond it they are dropped with one `rate` error per second (carrying the dropped message's `seq`).
An `up`/`cancel` for a pointer that is down, and a key or button `up` for one that is held, are
never dropped, whatever the budget. At most one move per pointer per
`min_move_interval` (8 ms) reaches the sink; moves inside it are replaced by the latest, which is
delivered when the interval ends or before any other event, so order is kept: a lift always
follows the last position. A move replaced before delivery is not acked; the one that replaced it
is. The pointer is freed on `up`/`cancel` even when the sink refuses it, so a finger never sticks. When the channel closes, is replaced by a new one, or the publisher
stops (or the session is dropped), `InputSession::release_all` sends `cancel` for every pointer
still down and `up` for every held key and button, so a viewer that vanishes mid-gesture leaves
nothing stuck. `text` reaches the sink raw: a sink must never pass it to a shell.

**Coordinates.** A point is normalised to the stream frame and carries the size the viewer saw, so
the encoder's downscale (the longest side capped, e.g. at 1280: an iPhone's 1179x2556 is streamed
as 590x1280) and the size the page shows the video at don't matter. `DisplayMap::new(w, h)` (the
device's framebuffer size) maps it: `to_device(&point)` is the device pixel, clamped inside the
display, and refuses a point from a frame of another shape (`StaleFrame`: the device rotated while
the input was in flight) rather than stretching it, allowing the encoder's rounding (even sides,
16-pixel minimum: 4000x20 streams as 100x16). A square display looks the same rotated, so its
rotation can't be detected from the frame's shape; `delta_to_device` scales wheel deltas;
`stream_size(Some(1280))` predicts the encoder's frame (the rule of `simee-encode`'s
`output_size`).

**Latency.** `t` echoed with `rx` and `done` gives the viewer the channel's round trip (`pong`) and
when its input reached the device's input path (`ack`), on the server's clock. Input-to-photon is
that plus the time to the first frame showing the change, which needs the video side: #268 and
#270.

**Tests.** `src/datachannel.rs` unit tests: every kind round-trips through JSON, browser-written
JSON decodes (named keys, surrogate pairs, exponents), every refusal has its code and `seq`, the
mapping through the downscale and the stale-frame refusal, coalescing per pointer with order kept,
acks and pongs, the rate limit, and a refused lift still freeing its pointer. `tests/datachannel.rs`
runs the publisher (the offerer) against a str0m answerer behind the fake WHIP server over loopback
UDP: the answerer sees the channel open and the `hello`, sends a script of touches, keys, text,
buttons, rotation, fold and wheel plus refused messages and a `ping`, and the fake sink gets the
events in order (moves coalesced) while each `t` comes back in an `ack` or the `pong` (a few
milliseconds round trip on loopback), with video flowing beside the channel. A plain `connect`
offers no channel. The Chromium test (a real browser tapping a button through the channel and
seeing the result in the video, with input-to-photon measured) belongs with #268.

## `sim-encode`: the encoder

`crates/sim-encode` (published as `simee-encode`) turns a device's frames into what the publisher
sends: Annex B access units with presentation times, an IDR with its SPS and PPS in front
whenever one is due or asked for. It is safe Rust; its FFI is `sim-coresim`'s `videotoolbox.rs`
(behind that crate's `videotoolbox` feature, so the CLI doesn't load VideoToolbox).

```rust
let context = Context::new()?;
let mut source = IosSource::new(context.device(udid)?.display()?)?; // or AndroidSource::connect(avd_dir, 0)?
let mut encoder = VtEncoder::new(EncoderConfig { fps: 30, max_size: Some(1280), ..Default::default() })?;
let controls = Arc::new(Controls::default()); // request_keyframe(), set_bitrate(), stop() from any thread
stream::run(&mut source, &mut encoder, Pacing::new(30, Some(Duration::from_millis(500))), &controls, |unit| {
    publisher.send(&unit.data, unit.pts)?; // simee-rtc
    Ok(Flow::Continue)
})?;
```

`examples/publish_device.rs` does exactly that against a WHIP URL, wiring the publisher's
`KeyframeRequested` to `Controls::request_keyframe` and `BitrateEstimate` to
`Controls::set_bitrate`, and offering the first IDR's own `profile-level-id` (VideoToolbox picks
the level from the frame size: a full-size iPhone frame is level 5.x, which the default `42e01f`
offer would refuse).

**The encoder.** A `VTCompressionSession` with the hardware encoder required, preferred (the
default; `VtEncoder::hardware()` says whether it fell back to software) or avoided; Constrained
Baseline (`kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel`, looked up at run time: macOS 12)
or Main; `RealTime`, no frame reordering, `MaxKeyFrameInterval` and `MaxKeyFrameIntervalDuration`
from `keyframe_interval` (2 s), `AverageBitRate` (4 Mbit/s) and a `DataRateLimits` ceiling over
one second (8 Mbit/s), `ExpectedFrameRate`. An IDR on demand is
`kVTEncodeFrameOptionKey_ForceKeyFrame`. Each picture is submitted and then completed
(`VTCompressionSessionCompleteFrames` up to its timestamp), so one picture is in flight at a time
and the latency below is the whole of it. VideoToolbox keeps the SPS and PPS in the format
description and stores NAL units behind 4-byte lengths; each keyframe's parameter sets are cached
and every IDR goes out as `SPS PPS [SEI] IDR` with start codes. A new input size (a rotation)
makes a new session, which starts with an IDR.

**Inputs, with no colour conversion on the CPU.** iOS: the framebuffer `IOSurface` itself,
wrapped as a `CVPixelBuffer` (`CVPixelBufferCreateWithIOSurface`, rewrapped only when the surface
changes); VideoToolbox converts BGRA to YUV on the GPU/media engine. Android: emu's frames come
through the shared ring (`shm`), a reader thread keeps only the newest, and the encoder takes it
with one copy into an IOSurface-backed `CVPixelBufferPool`. CoreVideo allocates no IOSurface
`RGBA` (or `ARGB`) buffers on this host (`kCVReturnInvalidPixelFormat`), so that copy swaps red
and blue as it goes into a BGRA buffer (`sim_core::pixels::swap_red_blue_into`, 0.26 ms for a
1080x2400 frame). Frames are only copied when one is encoded.

**Pacing.** The source says when the device presented a frame (iOS: the framebuffer's damage
callback, `Display::watch_frames`; Android: emu's ring announcements) and the capture instant is
stamped then, before any copy. The pacer encodes the newest frame no sooner than one interval
after the previous encode (the fps cap); a frame held back is encoded when its slot comes, so the
last picture of an animation is never lost. On a still screen the last frame is re-encoded every
heartbeat (`Pacing::new(fps, Some(500 ms))`), and a keyframe request on a still screen re-encodes
it at the next slot. Timestamps count from the start of the run and are kept strictly increasing
by at least one 90 kHz tick. `max_size` caps the longest edge (aspect kept, even sides);
VideoToolbox scales on the way in. On Android, `AndroidSource::connect(dir, max_width)` can also
have emu shrink frames by a whole factor before they're sent (`hello <max_width>`).

**Linux.** No encoder yet (`simee_encode::available()` is false; the pure parts build and test
everywhere). The `openh264` crate isn't in the offline registry here, and its build compiles
OpenH264's C++ (or downloads Cisco's binary), which would also break `scripts/check.sh`'s Linux
cross-clippy from macOS without a C++ cross toolchain; it's a follow-up behind a feature.

### Encode cost

`scripts/bench/encode.sh` (the `encode_bench` example): 10 s per run with a swipe every 0.7 s
(home screen pages on iOS, the app drawer on Android), medians of 3 rounds, the encoder's
defaults (Constrained Baseline, 4 Mbit/s average, 8 Mbit/s ceiling, IDR every 2 s), 2026-11-27,
Apple M3 Max (Mac15,9), macOS 26.6.2, hardware encoder in every run. Encode latency is from
handing the picture to VideoToolbox until its access unit is out; the picture step is wrapping
the surface (iOS) or the one copy into the pool (Android); CPU is the encoding process (the
capture, the copy, the frame reader) and, apart, the host's `VTEncoderXPCService`. The encoded
rate is bounded by the frames the device presented while swiping, about 48 a second on both, so
the 60 fps cap encodes about 46.

| iPhone 17 Pro, iOS 27.2 | 1206x2622, 30 fps | 1206x2622, 60 fps | 588x1280, 30 fps | 588x1280, 60 fps |
| --- | --- | --- | --- | --- |
| Encoded, fps | 26.8 | 46.4 | 26.9 | 46.8 |
| Encode latency p50 / p95, ms | 17.28 / 17.45 | 17.20 / 17.35 | 6.07 / 6.28 | 6.06 / 6.19 |
| Surface wrap p50, ms | 0.44 | 0.40 | 0.45 | 0.40 |
| Encoding process CPU, % of a core | 2.7 | 4.0 | 2.8 | 4.2 |
| `VTEncoderXPCService` CPU, % of a core | 0.9 | 1.5 | 0.9 | 1.5 |
| Output, kbit/s | 4153 | 4178 | 4142 | 4184 |
| First IDR (start, still screen) / median IDR (moving), KB | 34.2 / 64.6 | 26.2 / 58.1 | 22.2 / 46.8 | 17.3 / 38.3 |

| Android phone, API 36 google_apis, 1080x2400@420, emu | 1080x2400, 30 fps | 1080x2400, 60 fps | 576x1280, 30 fps | 576x1280, 60 fps |
| --- | --- | --- | --- | --- |
| Encoded, fps | 26.9 | 45.7 | 22.1 | 37.3 |
| Encode latency p50 / p95, ms | 14.51 / 14.62 | 14.45 / 14.58 | 5.80 / 5.91 | 5.77 / 5.89 |
| Copy into the pool p50, ms | 0.26 | 0.26 | 0.26 | 0.26 |
| Encoding process CPU, % of a core | 8.5 | 9.8 | 6.9 | 8.1 |
| `VTEncoderXPCService` CPU, % of a core | 0.9 | 1.5 | 0.7 | 1.2 |
| Output, kbit/s | 2508 | 2650 | 3461 | 4100 |
| First IDR / median IDR, KB | 23.3 / 54.9 | 19.1 / 43.4 | 21.5 / 54.7 | 23.8 / 54.7 |

```bash
scripts/bench/encode.sh --udid <booted iPhone>            # or no --udid: it makes its own
scripts/bench/encode.sh --udid <booted iPhone> --max-size 1280
scripts/bench/encode.sh --avd <running AVD>               # or --platform android: its own AVD
```

What the numbers say: at full size a moving iPhone frame takes the hardware encoder about 17 ms
(9.6 ms on a still screen, one 5 s `--still` run), which, with one picture in flight, caps a full-size stream just under
60 fps; capped at 1280 it's 6 ms. The CPU cost is small because nothing is converted on the CPU:
2.7-4.2% of a core on iOS. Android's is higher because the encoding process also reads every
frame emu presents from the ring (about 10 MB each at full size), not because of the encoder.
The tablet and iPad rows are not measured yet.

### Tests

`cargo test -p simee-encode`: Annex B and length-prefixed conversion, the SPS/PPS cache and its
repeat before every IDR, the pacer (fps cap, newest frame kept, heartbeat, keyframe refresh) and
the loop on a fake source and encoder with a fake clock, timestamp monotonicity; on macOS
(`tests/videotoolbox.rs`, synthetic frames, SKIP on a host with no H.264 encoder) Constrained
Baseline and Main, forced and periodic IDRs with their parameter sets, scaling, RGBA input, a
size change, and every access unit decoded with `VTDecompressionSession`. `tests/live.rs` does the
same on a real device's frames (`SIMEE_E2E_UDID=<udid>` or `SIMEE_E2E_AVD=<avd>`), with the SPS
read by `simee-rtc`'s parser.

## Not yet

- **TLS.** `https://` WHIP endpoints are refused: a TLS client (rustls) needs a crypto provider
  of its own, which is the same C-or-pure-Rust question as DTLS. Until then, publish to a loopback
  MediaMTX (or a LAN one with `allow_insecure_http`).
- **STUN and TURN.** The offer has one host candidate (the address the host routes to the
  endpoint from, or `local_ip`); no server-reflexive or relay candidates, and the `Link` headers
  of RFC 9725 §4.6 (ICE servers) are not read. Trickle ICE is in the client but the publisher
  doesn't need it with host candidates gathered up front.
- **Linux in CI.** The `linux` job tests without `rust-crypto`, so the publisher's end-to-end
  tests skip there (macOS runs them).
- **Reconnects.** A publisher reports `Disconnected`; publishing again is a new publisher with a
  new WHIP resource. Doing that automatically is #266's.
- **A Linux encoder** (openh264 or x264 behind a feature) and VA-API; on Linux
  `simee-encode` has no encoder.
- **Pipelined encoding.** One picture is in flight at a time, so a full-size iPhone stream can't
  quite reach 60 fps (17 ms per moving frame); submitting the next frame before the previous one
  is out would lift the rate, not the latency.
- **iOS runtimes without frame callbacks.** `IosSource` follows `Display::watch_frames`; a runtime
  whose callback never fires gets only heartbeats (the desktop's capture falls back to polling the
  surface seed; the encoder doesn't yet).

## The plan (#262)

| Issue | What |
| --- | --- |
| #263 | `sim-encode`: VideoToolbox H.264 straight from the IOSurface on macOS (a Linux encoder still to come); it answers `KeyframeRequested` and `BitrateEstimate` |
| #264 | `sim-rtc` |
| #265 | An Opus audio track |
| #266 | Start, watch and stop streams from the API, MCP, CLI and desktop |
| #267 | A MediaMTX harness for tests and CI |
| #268 | Chromium WHEP playback tests: first frame, pixel change, late join, stage timings |
| #269 | WHEP tests on every form factor |
| #270 | Latency, encode cost and bitrate per form factor, against AutoMobile |
| #271 | Input from the browser over a data channel: the `simee-input` protocol and `InputSink` in `sim-rtc` (wiring to devices is #266, the browser test #268) |
