//! The publisher against a real MediaMTX (tests/mediamtx/mediamtx.yml) on free loopback ports:
//! publish each H.264 fixture over WHIP and read the server's control API until the path is ready
//! with an H264 track whose profile (from the SPS MediaMTX parsed) is the fixture's. Also pins
//! what MediaMTX does with the `simee-input` data channel offer: it refuses it.
//! Skipped, with the reason printed, when no MediaMTX is on PATH or in `MEDIAMTX_BIN`, and when
//! this build has no DTLS backend (Linux needs `--features rust-crypto`); with
//! `SIMEE_REQUIRE_MEDIAMTX` set (CI) either is a failure instead.

mod common;

use std::time::Duration;

use common::{BASELINE, MAIN, MediaMtx};
use simee_rtc::h264;
use simee_rtc::{Error, InputConfig, InputSession, ProfileLevelId, Publisher, PublisherConfig};

/// Publish `fixture` in a loop (an IDR every five frames, so MediaMTX gathers its tracks whenever
/// it looks) until the path is ready, then check the API's view of it: a WebRTC session as the
/// source, one H264 track whose SPS profile is `mediamtx_profile`, and bytes received.
fn publishes_and_is_ready(fixture: &[u8], profile: ProfileLevelId, name: &str, mediamtx_profile: &str) {
    if !simee_rtc::crypto_available() {
        common::skip("no DTLS backend in this build (Linux needs --features rust-crypto)");
        return;
    }
    let Some(bin) = MediaMtx::find_or_skip() else { return };
    let mtx = MediaMtx::start(&bin).unwrap_or_else(|e| panic!("{e}"));
    eprintln!("mediamtx pid {} webrtc :{} api :{}", mtx.pid(), mtx.webrtc_port, mtx.api_port);

    let config = PublisherConfig { profile, keyframe_interval: Duration::ZERO, ..PublisherConfig::new(mtx.whip_url(name)) };
    let mut publisher = Publisher::connect(config, |_| {}).expect("connect to mediamtx");

    let units = h264::access_units(fixture);
    let end = std::time::Instant::now() + Duration::from_secs(20);
    let mut frame = 0u64;
    let body = loop {
        let au: Vec<u8> = units[(frame % units.len() as u64) as usize].iter().flat_map(|n| [&[0, 0, 0, 1][..], n].concat()).collect();
        publisher.send(&au, Duration::from_millis(100 * frame)).expect("send");
        frame += 1;
        std::thread::sleep(Duration::from_millis(100));
        if let Some(body) = mtx.ready_path(name) {
            break body;
        }
        assert!(
            std::time::Instant::now() < end,
            "path {name} never became ready; API said {:?}",
            mtx.api_get(&format!("/v3/paths/get/{name}"))
        );
    };
    assert!(body.contains("\"tracks\":[\"H264\"]"), "no single H264 track in {body}");
    assert!(body.contains(&format!("\"profile\":\"{mediamtx_profile}\"")), "profile is not {mediamtx_profile} in {body}");
    assert!(body.contains("\"source\":{\"type\":\"webRTCSession\""), "source is not a WebRTC session in {body}");
    assert!(!body.contains("\"inboundBytes\":0,"), "no bytes received in {body}");
    publisher.close().expect("close");
}

#[test]
fn mediamtx_takes_constrained_baseline_h264() {
    publishes_and_is_ready(BASELINE, ProfileLevelId::CONSTRAINED_BASELINE_3_1, "simee-baseline", "Baseline");
}

#[test]
fn mediamtx_takes_main_h264() {
    publishes_and_is_ready(MAIN, ProfileLevelId::MAIN_3_1, "simee-main", "Main");
}

/// The harness alone: starts, answers its API, and the process is gone after drop.
#[test]
fn harness_starts_and_stops_mediamtx() {
    let Some(bin) = MediaMtx::find_or_skip() else { return };
    let mtx = MediaMtx::start(&bin).unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(mtx.api_get("/v3/config/global/get"), Some((200, _))));
    assert!(mtx.ready_path("nobody-publishes-here").is_none());
    let port = mtx.api_port;
    drop(mtx);
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err(), "api still listening after drop");
}

/// MediaMTX v1.19.2 doesn't carry the `simee-input` channel (#271): it refuses an offer with an
/// `m=application` section outright (406, `unsupported media 'application'`), so a stream whose
/// endpoint is MediaMTX must be started without input. When a MediaMTX release accepts the offer
/// this fails: update docs/streaming.md ("Input over a data channel") with what it relays.
#[test]
fn mediamtx_refuses_the_input_channel_offer() {
    if !simee_rtc::crypto_available() {
        common::skip("no DTLS backend in this build (Linux needs --features rust-crypto)");
        return;
    }
    let Some(bin) = MediaMtx::find_or_skip() else { return };
    let mtx = MediaMtx::start(&bin).unwrap_or_else(|e| panic!("{e}"));
    let config = PublisherConfig::new(mtx.whip_url("simee-input"));
    let sink = |_: &simee_rtc::InputEvent| -> Result<(), String> { Ok(()) };
    let input = InputSession::new(sink, InputConfig::default());
    match Publisher::connect_with_input(config, input, |_| {}) {
        Err(Error::Status { status: 406, body, .. }) => assert!(body.contains("application"), "406 for another reason: {body}"),
        Err(e) => panic!("expected a 406 for the data channel, got {e}"),
        Ok(_) => panic!("MediaMTX accepted the input channel offer: update docs/streaming.md"),
    }
    assert!(mtx.ready_path("simee-input").is_none());
}
