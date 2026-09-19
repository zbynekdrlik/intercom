//! RTP PCMU packetizer/depacketizer + Janus audiobridge JSON message builders/parsers (issue 1345
//! M3a). The JSON shapes are pinned to the Janus AudioBridge docs' plain-RTP participant section
//! (https://janus.conf.meetecho.com/docs/audiobridge.html): the `join` `rtp` object
//! (ip/port/payload_type), the `joined` reply (the plugin's own rtp ip/port), the `configure`
//! `muted` field, and the transport/plugin `error` shapes.

use intercom_hub::janus_rtp::{
    build_attach, build_configure, build_create, build_join, build_keepalive, build_leave,
    parse_error, parse_joined, parse_success_id, rtp_depacketize, JoinedInfo, RtpPacketizer,
    AUDIOBRIDGE_PLUGIN, PCMU_PAYLOAD_TYPE, RTP_HEADER_LEN,
};
use serde_json::json;

// --- RTP ------------------------------------------------------------------------------------

#[test]
fn rtp_header_exact_bytes_of_a_known_packet() {
    let mut p = RtpPacketizer::new(0x1122_3344);
    let payload = [0x7Fu8; 160];
    let pkt = p.packetize(&payload);
    // V=2 (0x80); marker set on the FIRST packet + PT 0 => 0x80; seq 0; ts 0; ssrc 0x11223344.
    assert_eq!(
        &pkt[..RTP_HEADER_LEN],
        &[0x80, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x11, 0x22, 0x33, 0x44]
    );
    assert_eq!(pkt.len(), RTP_HEADER_LEN + 160);
}

#[test]
fn rtp_seq_and_timestamp_continuity_and_marker_only_first() {
    let mut p = RtpPacketizer::new(0xDEAD_BEEF);
    let payload = [0u8; 160];
    let p0 = p.packetize(&payload);
    let p1 = p.packetize(&payload);
    let p2 = p.packetize(&payload);
    // marker: only the first packet after start-up.
    assert_eq!(p0[1] & 0x80, 0x80, "first packet marks");
    assert_eq!(p1[1] & 0x80, 0x00, "later packets do not mark");
    assert_eq!(p2[1] & 0x80, 0x00);
    // PT 0 on every packet.
    assert_eq!(p0[1] & 0x7F, PCMU_PAYLOAD_TYPE);
    // seq increments by 1.
    assert_eq!(u16::from_be_bytes([p0[2], p0[3]]), 0);
    assert_eq!(u16::from_be_bytes([p1[2], p1[3]]), 1);
    assert_eq!(u16::from_be_bytes([p2[2], p2[3]]), 2);
    // timestamp increments by the 160-sample payload.
    assert_eq!(u32::from_be_bytes([p0[4], p0[5], p0[6], p0[7]]), 0);
    assert_eq!(u32::from_be_bytes([p1[4], p1[5], p1[6], p1[7]]), 160);
    assert_eq!(u32::from_be_bytes([p2[4], p2[5], p2[6], p2[7]]), 320);
}

#[test]
fn rtp_marker_rearms_after_silence() {
    let mut p = RtpPacketizer::new(1);
    let _ = p.packetize(&[0u8; 160]);
    let a = p.packetize(&[0u8; 160]);
    assert_eq!(a[1] & 0x80, 0x00);
    p.mark_silence();
    let b = p.packetize(&[0u8; 160]);
    assert_eq!(
        b[1] & 0x80,
        0x80,
        "first packet after a silence gap re-marks"
    );
}

#[test]
fn rtp_depacketize_roundtrip_and_rejects_runts() {
    let mut p = RtpPacketizer::new(0x0A0B_0C0D);
    let payload: Vec<u8> = (0..160).map(|i| i as u8).collect();
    let pkt = p.packetize(&payload);
    let parsed = rtp_depacketize(&pkt).expect("valid RTP parses");
    assert!(parsed.marker);
    assert_eq!(parsed.payload_type, PCMU_PAYLOAD_TYPE);
    assert_eq!(parsed.seq, 0);
    assert_eq!(parsed.timestamp, 0);
    assert_eq!(parsed.ssrc, 0x0A0B_0C0D);
    assert_eq!(parsed.payload, payload);
    // A runt (< 12 bytes) and a non-v2 datagram are ignored, exactly like the VBAN receiver.
    assert!(rtp_depacketize(&[0x80, 0x00, 0x00]).is_none());
    let mut bad = pkt.clone();
    bad[0] = 0x40; // version 1
    assert!(rtp_depacketize(&bad).is_none());
}

// --- Janus JSON builders --------------------------------------------------------------------

#[test]
fn build_create_shape() {
    assert_eq!(
        build_create("tx1"),
        json!({ "janus": "create", "transaction": "tx1" })
    );
}

#[test]
fn build_attach_targets_the_audiobridge_plugin() {
    let v = build_attach("tx2");
    assert_eq!(v["janus"], "attach");
    assert_eq!(v["plugin"], AUDIOBRIDGE_PLUGIN);
    assert_eq!(v["plugin"], "janus.plugin.audiobridge");
    assert_eq!(v["transaction"], "tx2");
}

#[test]
fn build_join_plain_rtp_participant_shape() {
    // The plain-RTP join body: request/room/display + the rtp object (ip/port/payload_type 0).
    let v = build_join("tx3", 1000, "strih-lx-hub", "10.77.9.203", 6990, None, None);
    assert_eq!(v["janus"], "message");
    assert_eq!(v["transaction"], "tx3");
    let body = &v["body"];
    assert_eq!(body["request"], "join");
    assert_eq!(body["room"], 1000);
    assert_eq!(body["display"], "strih-lx-hub");
    assert_eq!(body["rtp"]["ip"], "10.77.9.203");
    assert_eq!(body["rtp"]["port"], 6990);
    assert_eq!(body["rtp"]["payload_type"], PCMU_PAYLOAD_TYPE);
    // Live finding (strih-lx Janus 1.1.2, 19.9.2026): the plain-RTP leg's CODEC is chosen by the
    // top-level `codec` field of the join request, NOT by `rtp.payload_type` -- without it Janus
    // defaults the participant to Opus (its `joined` reply carried payload_type 100) and silently
    // discards the PCMU the hub sends (hub rx stayed 0 while a second participant was mixing). With
    // `"codec":"pcmu"` Janus answers payload_type 0 and PCMU flows both ways.
    assert_eq!(body["codec"], "pcmu");
    // No secret/pin keys when not supplied.
    assert!(body.get("secret").is_none());
    assert!(body.get("pin").is_none());
}

#[test]
fn build_join_carries_secret_and_pin_when_present() {
    let v = build_join(
        "tx4",
        42,
        "hub",
        "127.0.0.1",
        7000,
        Some("s3cr3t"),
        Some("1234"),
    );
    assert_eq!(v["body"]["secret"], "s3cr3t");
    assert_eq!(v["body"]["pin"], "1234");
}

#[test]
fn build_configure_muted_field() {
    let v = build_configure("tx5", false);
    assert_eq!(v["janus"], "message");
    assert_eq!(v["body"]["request"], "configure");
    assert_eq!(v["body"]["muted"], false);
}

#[test]
fn build_keepalive_and_leave_shapes() {
    assert_eq!(build_keepalive("k")["janus"], "keepalive");
    let leave = build_leave("l");
    assert_eq!(leave["janus"], "message");
    assert_eq!(leave["body"]["request"], "leave");
}

// --- Janus JSON parsers ---------------------------------------------------------------------

#[test]
fn parse_success_id_reads_the_session_or_handle_id() {
    let created = json!({ "janus": "success", "transaction": "t", "data": { "id": 987654321u64 } });
    assert_eq!(parse_success_id(&created), Some(987654321));
    // A non-success reply yields no id.
    assert_eq!(parse_success_id(&json!({ "janus": "ack" })), None);
}

#[test]
fn parse_joined_extracts_the_plugins_own_rtp_endpoint() {
    // The audiobridge `joined` event carries the plugin's OWN rtp ip/port (where we send + receive).
    let ev = json!({
        "janus": "event",
        "plugindata": {
            "plugin": "janus.plugin.audiobridge",
            "data": {
                "audiobridge": "joined",
                "room": 1000,
                "id": 42,
                "rtp": { "ip": "127.0.0.1", "port": 5005, "payload_type": 0 },
                "participants": []
            }
        }
    });
    assert_eq!(
        parse_joined(&ev),
        Some(JoinedInfo {
            room: 1000,
            id: 42,
            rtp_ip: "127.0.0.1".to_string(),
            rtp_port: 5005,
        })
    );
    // A non-joined event yields None.
    let other = json!({ "plugindata": { "data": { "audiobridge": "event" } } });
    assert_eq!(parse_joined(&other), None);
}

#[test]
fn parse_error_reads_both_transport_and_plugin_shapes() {
    // Transport-level error.
    let transport = json!({ "janus": "error", "transaction": "t", "error": { "code": 458, "reason": "no such session" } });
    let e = parse_error(&transport).expect("transport error parses");
    assert_eq!(e.code, 458);
    assert_eq!(e.reason, "no such session");
    // Plugin-level error inside a plugindata event.
    let plugin = json!({
        "janus": "event",
        "plugindata": { "data": { "audiobridge": "event", "error_code": 486, "error": "room already exists" } }
    });
    let pe = parse_error(&plugin).expect("plugin error parses");
    assert_eq!(pe.code, 486);
    assert_eq!(pe.reason, "room already exists");
    // A success reply is not an error.
    assert!(parse_error(&json!({ "janus": "success", "data": { "id": 1 } })).is_none());
}
