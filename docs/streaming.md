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

## The plan (#262)

| Issue | What |
| --- | --- |
| #263 | The encoder: VideoToolbox H.264 straight from the IOSurface on macOS, a Linux encoder; it answers `KeyframeRequested` and `BitrateEstimate` |
| #264 | This crate |
| #265 | An Opus audio track |
| #266 | Start, watch and stop streams from the API, MCP, CLI and desktop |
| #267 | A MediaMTX harness for tests and CI |
| #268 | Chromium WHEP playback tests: first frame, pixel change, late join, stage timings |
| #269 | WHEP tests on every form factor |
| #270 | Latency, encode cost and bitrate per form factor, against AutoMobile |
| #271 | Input from the browser over a data channel |
