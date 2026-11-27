//! The publisher against a real MediaMTX (tests/mediamtx/mediamtx.yml) on free loopback ports:
//! publish each H.264 fixture over WHIP and read the server's control API until the path is ready
//! with an H264 track. Skipped, with the reason printed, when no MediaMTX is on PATH or in
//! `MEDIAMTX_BIN`, and when this build has no DTLS backend (Linux needs `--features rust-crypto`).

mod common;

use std::time::Duration;

use common::{BASELINE, MAIN, MediaMtx};
use simee_rtc::h264;
use simee_rtc::{ProfileLevelId, Publisher, PublisherConfig};

/// Publish `fixture` in a loop (an IDR every five frames, so MediaMTX gathers its tracks whenever
/// it looks) until the path is ready, then check the API's view of it.
fn publishes_and_is_ready(fixture: &[u8], profile: ProfileLevelId, name: &str) {
    if !simee_rtc::crypto_available() {
        eprintln!("SKIP: no DTLS backend in this build (Linux needs --features rust-crypto)");
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
    assert!(body.contains("\"H264\""), "no H264 track in {body}");
    publisher.close().expect("close");
}

#[test]
fn mediamtx_takes_constrained_baseline_h264() {
    publishes_and_is_ready(BASELINE, ProfileLevelId::CONSTRAINED_BASELINE_3_1, "simee-baseline");
}

#[test]
fn mediamtx_takes_main_h264() {
    publishes_and_is_ready(MAIN, ProfileLevelId::MAIN_3_1, "simee-main");
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
