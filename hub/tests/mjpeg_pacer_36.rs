//! issue 36 (19.11.2026): the phone page's `/interkom.mjpeg` ran at 2-7 Mbit/s through the
//! Cloudflare tunnel, alongside the live stream on strih-snv's uplink, and the tunnel kept timing
//! out ("no recent network activity") — taking the intercom audio signalling with it. A client
//! that comes through Cloudflare (`cf-connecting-ip`) gets the picture paced by a byte budget; a
//! LAN client keeps the full rate. These tests pin the pure pacer and the client classification.

use axum::http::{HeaderMap, HeaderValue};
use intercom_hub::http::{
    is_tunnel_client, MjpegPacer, TUNNEL_MJPEG_BURST_BYTES, TUNNEL_MJPEG_BYTES_PER_S,
};

/// Feed `secs` seconds of frames of `size` bytes at `fps`; return (frames sent, bytes sent).
fn run(p: &mut MjpegPacer, size: usize, fps: u64, secs: u64) -> (u64, u64) {
    let (mut frames, mut bytes) = (0, 0);
    for i in 0..fps * secs {
        if p.allow(i * 1000 / fps, size) {
            frames += 1;
            bytes += size as u64;
        }
    }
    (frames, bytes)
}

#[test]
fn a_tunnel_client_never_gets_more_than_the_budget() {
    // The frames measured on 19.11.: ~25 KB at 10 fps (~2 Mbit/s unpaced).
    let mut p = MjpegPacer::tunnel();
    let (frames, bytes) = run(&mut p, 25_000, 10, 60);
    let cap = TUNNEL_MJPEG_BYTES_PER_S * 60 + TUNNEL_MJPEG_BURST_BYTES;
    assert!(bytes <= cap, "{bytes} B in 60 s exceeds the budget {cap} B");
    // ... and still a moving picture, never starved: at least ~1 fps.
    assert!(frames >= 60, "only {frames} frames in 60 s");
}

#[test]
fn the_budget_is_well_under_the_rate_that_broke_the_tunnel() {
    // 2 Mbit/s = 250 KB/s was the gentlest rate measured on 19.11.; the budget is a fraction of it.
    assert!(TUNNEL_MJPEG_BYTES_PER_S * 4 <= 250_000);
    assert!(TUNNEL_MJPEG_BYTES_PER_S >= 25_000, "at least one measured frame per second");
}

#[test]
fn a_lan_client_gets_every_frame() {
    let mut p = MjpegPacer::unlimited();
    let (frames, _) = run(&mut p, 25_000, 10, 10);
    assert_eq!(frames, 100);
}

#[test]
fn a_frame_bigger_than_the_burst_still_gets_through() {
    // A busy picture can make one JPEG larger than the burst: it must pass once the bucket is full,
    // never be skipped forever (a frozen picture).
    let mut p = MjpegPacer::tunnel();
    let big = (TUNNEL_MJPEG_BURST_BYTES * 2) as usize;
    let (frames, _) = run(&mut p, big, 10, 30);
    assert!(frames >= 5, "only {frames} oversized frames in 30 s");
}

#[test]
fn a_skipped_frame_is_dropped_not_queued() {
    let mut p = MjpegPacer::tunnel();
    let size = TUNNEL_MJPEG_BURST_BYTES as usize;
    assert!(p.allow(0, size), "the first frame goes out at once");
    assert!(!p.allow(10, size), "the bucket is empty right after");
    // After a long pause only ONE burst worth is available, not the backlog.
    assert!(p.allow(60_000, size));
    assert!(!p.allow(60_001, size));
}

#[test]
fn only_a_request_through_cloudflare_is_a_tunnel_client() {
    let mut h = HeaderMap::new();
    assert!(!is_tunnel_client(&h), "the LAN (no Cloudflare header) keeps the full rate");
    h.insert("x-forwarded-for", HeaderValue::from_static("192.168.1.20"));
    assert!(!is_tunnel_client(&h));
    h.insert("cf-connecting-ip", HeaderValue::from_static("85.248.11.235"));
    assert!(is_tunnel_client(&h));
}
