//! issue 1467 (23.10.2026): a KNOWN VBAN stream the hub cannot decode is counted and named, never
//! dropped silently.
//!
//! From ~10:52 on 23.10.2026 the FOH console sent `fohabl-strih` as 24-bit PCM to a hub binary that
//! decoded only PCM16 / Float32. `route_packet` turned the decode error into `None` BEFORE it looked
//! the stream name up, so the configured FOH stream read exactly like a sender that had stopped:
//! no counter, no log, the cutters lost FOH for ~90 min. Pinned here:
//!
//! * the header's stream name is read first; a name the matrix knows whose payload does not decode
//!   (an unsupported codec, a non-audio sub-protocol, a payload without one whole frame) is
//!   `Undecodable` with the participant id and the reason;
//! * an unknown name stays `Foreign` whatever its payload (foreign traffic is ignored silently);
//! * the per-participant count grows on every such packet, the log transition fires once per change
//!   (the first reject, a changed reason, the stream decoding again), and a good packet after the
//!   rejects still flows while the count stays;
//! * a reserved sample rate is still a RATE reject (counted by the converter), never a decode one.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use intercom_hub::vban_io::{
    classify_packet, encode_packet, note_decoded, note_undecodable, route_packet, to_hub_rate,
    DecodeRejects, DecodeTransition, OutBlock, RoutedPacket, DECODE_LOG_FLOOR,
};
use intercom_hub::vban_rate::{VbanRateConverter, VbanRateStats};
use intercom_vban::{VbanCodec, VbanHeader};

fn known() -> HashMap<String, usize> {
    let mut m = HashMap::new();
    m.insert("fohabl-strih".to_string(), 0);
    m.insert("cam1".to_string(), 1);
    m
}

/// A VBAN audio packet: our header for `name` with `codec`, `channels`, `frames`, then `payload`.
fn raw(name: &str, codec: VbanCodec, channels: u8, frames: usize, payload: &[u8]) -> Vec<u8> {
    let header = VbanHeader::new(name, 48_000, channels, codec).unwrap();
    let mut out = header.encode(frames).to_vec();
    out.extend_from_slice(payload);
    out
}

/// A good stereo PCM16 packet, as the camboxes and the hub put it on the wire.
fn good(name: &str, frames: usize) -> Vec<u8> {
    let interleaved: Vec<i16> = (0..frames * 2).map(|i| i as i16).collect();
    encode_packet(&OutBlock {
        stream_name: name,
        sample_rate: 48_000,
        channels: 2,
        frame_counter: 0,
        interleaved: &interleaved,
        frames,
    })
    .unwrap()
}

fn pcm32(name: &str) -> Vec<u8> {
    raw(name, VbanCodec::Pcm32, 2, 4, &[0u8; 4 * 2 * 4])
}

#[test]
fn a_known_stream_with_an_unsupported_codec_is_undecodable_with_its_reason() {
    match classify_packet(&known(), &pcm32("fohabl-strih")) {
        RoutedPacket::Undecodable { id, stream, reason } => {
            assert_eq!(id, 0);
            assert_eq!(stream, "fohabl-strih");
            assert!(
                reason.contains("unsupported VBAN codec 0x03"),
                "the reason names the codec: {reason}"
            );
        }
        other => panic!("a known stream that does not decode must be Undecodable, got {other:?}"),
    }
    // The old entry point still drops it (it routes audio only).
    assert!(route_packet(&known(), &pcm32("fohabl-strih")).is_none());
}

#[test]
fn an_unknown_name_with_an_unsupported_codec_is_foreign_and_silent() {
    assert!(matches!(
        classify_packet(&known(), &pcm32("lv1-strih")),
        RoutedPacket::Foreign
    ));
    assert!(matches!(
        classify_packet(&known(), &good("cam9", 8)),
        RoutedPacket::Foreign
    ));
}

#[test]
fn garbage_and_short_packets_are_foreign() {
    assert!(matches!(
        classify_packet(&known(), b""),
        RoutedPacket::Foreign
    ));
    assert!(matches!(
        classify_packet(&known(), b"VBAN"),
        RoutedPacket::Foreign
    ));
    let mut not_vban = good("fohabl-strih", 8);
    not_vban[0..4].copy_from_slice(b"XBAN");
    assert!(matches!(
        classify_packet(&known(), &not_vban),
        RoutedPacket::Foreign
    ));
}

#[test]
fn a_known_name_on_a_non_audio_sub_protocol_is_undecodable() {
    let mut text = good("cam1", 8);
    text[4] = (text[4] & 0x1F) | 0x40; // VBAN-TEXT
    match classify_packet(&known(), &text) {
        RoutedPacket::Undecodable { id, reason, .. } => {
            assert_eq!(id, 1);
            assert!(reason.contains("audio"), "{reason}");
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn a_payload_holding_no_whole_frame_is_a_malformed_reject() {
    // Stereo PCM16 = 4 bytes a frame; 3 bytes hold none.
    let pkt = raw("cam1", VbanCodec::Pcm16, 2, 1, &[1, 2, 3]);
    match classify_packet(&known(), &pkt) {
        RoutedPacket::Undecodable { id, reason, .. } => {
            assert_eq!(id, 1);
            assert!(reason.contains("malformed"), "{reason}");
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn a_good_packet_routes_as_audio() {
    match classify_packet(&known(), &good("cam1", 16)) {
        RoutedPacket::Audio {
            id,
            audio,
            sample_rate,
        } => {
            assert_eq!(id, 1);
            assert_eq!(sample_rate, 48_000);
            assert_eq!(audio.stream_name, "cam1");
            assert_eq!(audio.frames, 16);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn the_count_grows_per_packet_and_the_log_fires_once_per_transition() {
    let mut t = DecodeRejects::default();
    assert_eq!(t.count(), 0);
    assert_eq!(
        t.accept(),
        None,
        "a stream that always decoded logs nothing"
    );

    let r = "unsupported VBAN codec 0x02 (only PCM16/PCM24/Float32)";
    assert_eq!(t.reject(r), Some(DecodeTransition::Rejecting));
    for _ in 0..99 {
        assert_eq!(t.reject(r), None, "the same reason logs once");
    }
    assert_eq!(t.count(), 100);

    let other = "malformed VBAN payload";
    assert_eq!(
        t.reject(other),
        Some(DecodeTransition::Rejecting),
        "a changed reason is a new transition"
    );
    assert_eq!(t.count(), 101);

    assert_eq!(t.accept(), Some(DecodeTransition::Recovered));
    assert_eq!(t.accept(), None, "recovery logs once");
    assert_eq!(t.count(), 101, "a good packet never resets the count");

    assert_eq!(
        t.reject(r),
        Some(DecodeTransition::Rejecting),
        "rejecting again after a recovery is logged again"
    );
    assert_eq!(t.count(), 102);
}

#[test]
fn a_malformed_reason_is_the_same_for_every_short_payload_of_a_stream() {
    // review: a byte count in the reason made every differently-short packet a new transition.
    let reason = |payload: &[u8]| match classify_packet(
        &known(),
        &raw("cam1", VbanCodec::Pcm16, 2, 1, payload),
    ) {
        RoutedPacket::Undecodable { reason, .. } => reason,
        other => panic!("got {other:?}"),
    };
    assert_eq!(reason(&[1]), reason(&[1, 2, 3]));
}

#[test]
fn the_log_floor_allows_one_line_per_stream_per_ten_seconds() {
    assert_eq!(DECODE_LOG_FLOOR, Duration::from_secs(10));
    let t0 = Instant::now();
    let mut t = DecodeRejects::default();
    assert_eq!(t.log_due(t0), Some(0), "the first transition logs");
    // A sender that mixes good and bad packets flips every packet: within the floor all suppressed.
    for i in 1..=50u64 {
        assert_eq!(t.log_due(t0 + Duration::from_millis(100 * i)), None);
    }
    assert_eq!(
        t.log_due(t0 + DECODE_LOG_FLOOR + Duration::from_millis(1)),
        Some(50),
        "the next line names how many transitions it suppressed"
    );
    assert_eq!(
        t.log_due(t0 + DECODE_LOG_FLOOR * 3),
        Some(0),
        "and the count restarts"
    );
}

#[test]
fn the_notes_count_every_packet_whatever_the_floor() {
    let from: SocketAddr = "10.77.7.30:6980".parse().unwrap();
    let t0 = Instant::now();
    let mut t = DecodeRejects::default();
    for i in 0..10u64 {
        note_undecodable(
            &mut t,
            "fohabl-strih",
            from,
            "bad",
            t0 + Duration::from_millis(i),
        );
        note_decoded(&mut t, "fohabl-strih", from, t0 + Duration::from_millis(i));
    }
    assert_eq!(t.count(), 10, "the floor limits the log, never the count");
}

#[test]
fn the_notes_consume_the_log_floor() {
    // review round 2: a note that logged every transition (skipping the floor) passed the counts.
    let from: SocketAddr = "10.77.7.30:6980".parse().unwrap();
    let t0 = Instant::now();
    let mut t = DecodeRejects::default();
    assert!(note_undecodable(&mut t, "fohabl-strih", from, "bad", t0));
    assert_eq!(
        t.log_due(t0 + Duration::from_millis(1)),
        None,
        "the note used the floor"
    );
    let mut u = DecodeRejects::default();
    assert!(note_undecodable(&mut u, "fohabl-strih", from, "bad", t0));
    let later = t0 + DECODE_LOG_FLOOR + Duration::from_millis(1);
    assert!(note_decoded(&mut u, "fohabl-strih", from, later));
    assert_eq!(u.log_due(later + Duration::from_millis(1)), None);
}

#[test]
fn a_stream_that_stays_rejected_after_a_suppressed_flip_is_logged_once_the_floor_passes() {
    // review round 2: reject at t0, decodes at t11, rejects again at t12 (inside the floor) and
    // never decodes again: the journal's last line must not stay "decodes again".
    let from: SocketAddr = "10.77.7.30:6980".parse().unwrap();
    let t0 = Instant::now();
    let s = |secs: u64| t0 + Duration::from_secs(secs);
    let mut t = DecodeRejects::default();
    assert!(note_undecodable(&mut t, "fohabl-strih", from, "bad", s(0)));
    assert!(note_decoded(&mut t, "fohabl-strih", from, s(11)));
    assert!(
        !note_undecodable(&mut t, "fohabl-strih", from, "bad", s(12)),
        "inside the floor"
    );
    assert!(!note_undecodable(
        &mut t,
        "fohabl-strih",
        from,
        "bad",
        s(15)
    ));
    assert!(
        note_undecodable(&mut t, "fohabl-strih", from, "bad", s(22)),
        "the pending state is logged once the floor has passed"
    );
    assert!(
        !note_undecodable(&mut t, "fohabl-strih", from, "bad", s(40)),
        "and only once"
    );
    assert_eq!(t.count(), 5);
}

#[test]
fn a_pending_line_reports_only_the_suppressed_transitions_never_the_packets() {
    // review round 3: every ordinary packet while a transition was pending counted as one more
    // "suppressed transition" (~9 000 in a 10 s window at FOH's ~934 pkt/s).
    let t0 = Instant::now();
    let mut t = DecodeRejects::default();
    assert_eq!(t.log_due(t0), Some(0));
    assert_eq!(
        t.log_due(t0 + Duration::from_secs(1)),
        None,
        "one suppressed transition"
    );
    for i in 0..100u64 {
        assert_eq!(
            t.pending_due(t0 + Duration::from_millis(1000 + 10 * i)),
            None
        );
    }
    assert_eq!(t.pending_due(t0 + DECODE_LOG_FLOOR), Some(1));
    assert_eq!(
        t.pending_due(t0 + DECODE_LOG_FLOOR * 2),
        None,
        "nothing pending any more"
    );
}

#[test]
fn a_suppressed_recovery_is_logged_once_the_floor_passes() {
    let from: SocketAddr = "10.77.7.30:6980".parse().unwrap();
    let t0 = Instant::now();
    let s = |secs: u64| t0 + Duration::from_secs(secs);
    let mut t = DecodeRejects::default();
    assert!(note_undecodable(&mut t, "fohabl-strih", from, "bad", s(0)));
    assert!(
        !note_decoded(&mut t, "fohabl-strih", from, s(1)),
        "inside the floor"
    );
    assert!(!note_decoded(&mut t, "fohabl-strih", from, s(5)));
    assert!(
        note_decoded(&mut t, "fohabl-strih", from, s(11)),
        "the recovery is logged late"
    );
    assert!(!note_decoded(&mut t, "fohabl-strih", from, s(30)));
}

#[test]
fn a_good_packet_after_the_rejects_flows_and_the_published_count_stays() {
    let streams = known();
    let mut t = DecodeRejects::default();
    let slot = VbanRateStats::default();
    assert_eq!(slot.decode_rejects(), 0);

    for _ in 0..3 {
        match classify_packet(&streams, &pcm32("fohabl-strih")) {
            RoutedPacket::Undecodable { reason, .. } => {
                t.reject(&reason);
                slot.publish_decode_rejects(t.count());
            }
            other => panic!("got {other:?}"),
        }
    }
    assert_eq!(slot.decode_rejects(), 3);

    let mut conv = VbanRateConverter::new(48_000);
    match classify_packet(&streams, &good("fohabl-strih", 32)) {
        RoutedPacket::Audio {
            id,
            audio,
            sample_rate,
        } => {
            assert_eq!(id, 0);
            assert_eq!(t.accept(), Some(DecodeTransition::Recovered));
            let out = to_hub_rate(&mut conv, audio, sample_rate).expect("48 kHz passes");
            assert_eq!(out.frames, 32, "the stream flows again");
        }
        other => panic!("got {other:?}"),
    }
    slot.publish(&conv);
    assert_eq!(
        slot.decode_rejects(),
        3,
        "the rate publish keeps the decode count"
    );
    assert_eq!(slot.rate_rejects(), 0);
}

#[test]
fn a_reserved_rate_stays_a_rate_reject_never_a_decode_reject() {
    let mut pkt = good("fohabl-strih", 8);
    pkt[4] = 20; // rate indices 20..=31 are reserved
    let mut conv = VbanRateConverter::new(48_000);
    match classify_packet(&known(), &pkt) {
        RoutedPacket::Audio {
            audio, sample_rate, ..
        } => {
            assert_eq!(sample_rate, 0, "no rate");
            assert!(to_hub_rate(&mut conv, audio, sample_rate).is_none());
        }
        other => panic!("a reserved rate decodes and is rejected by the converter, got {other:?}"),
    }
    assert_eq!(conv.rate_rejects(), 1);
}

// ---- wiring (needs the whole crate: the /api/state snapshot + its matrix) ----

mod wiring {
    use intercom_hub::matrix::Matrix;
    use intercom_hub::state::{HubState, RuntimeStats};

    fn matrix() -> Matrix {
        Matrix::from_toml(
            r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 256

[[participant]]
name = "fohabl"
role = "program_ref"
adapter = "vban"
host = "10.77.7.30"
in_stream = "fohabl-strih"
in_channels = 2
out_channels = 0

[[participant]]
name = "cam1"
role = "cambox"
adapter = "vban"
host = "cam1.lan"
in_stream = "cam1"
out_stream = "cam1"
in_channels = 2
out_channels = 2

[[participant]]
name = "cutters"
role = "cutters"
adapter = "none"
in_channels = 2
out_channels = 4
"#,
        )
        .unwrap()
    }

    #[test]
    fn api_state_carries_decode_rejects_only_for_vban_participants() {
        let stats = vec![
            RuntimeStats {
                decode_rejects: 12,
                ..Default::default()
            },
            RuntimeStats::default(),
            RuntimeStats::default(),
        ];
        let v = serde_json::to_value(HubState::snapshot(&matrix(), "v", &stats)).unwrap();
        assert_eq!(v["participants"][0]["decode_rejects"], 12);
        assert_eq!(v["participants"][1]["decode_rejects"], 0);
        assert!(v["participants"][2].get("decode_rejects").is_none());
    }

    #[test]
    fn the_status_line_names_decode_rejects_and_the_worst_leg_only_when_nonzero() {
        let m = matrix();
        let quiet = HubState::snapshot(&m, "v", &[RuntimeStats::default(); 3]);
        assert!(
            !quiet.status_line().contains("decode_rejects"),
            "{}",
            quiet.status_line()
        );
        let stats = vec![
            RuntimeStats {
                decode_rejects: 7,
                ..Default::default()
            },
            RuntimeStats {
                decode_rejects: 2,
                ..Default::default()
            },
            RuntimeStats::default(),
        ];
        let line = HubState::snapshot(&m, "v", &stats).status_line();
        assert!(line.contains(" decode_rejects=9(fohabl)"), "got: {line}");
        assert!(!line.contains("rate_rejects"), "got: {line}");
    }
}
