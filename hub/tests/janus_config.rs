//! Matrix `janus` adapter validation + the `[janus]` config table + the `/api/state` janus facet
//! (issue 1345 M3a). The Janus audiobridge leg is only valid on the single `phones` participant.

use intercom_hub::janus_rtp::{JanusCodec, JanusStats};
use intercom_hub::matrix::{Matrix, ADAPTER_JANUS};
use intercom_hub::state::{HubState, RuntimeStats};

fn toml_with_phones(phones_adapter: &str, extra: &str) -> String {
    format!(
        r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 256
{extra}

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
name = "phones"
role = "phones"
adapter = "{phones_adapter}"
in_channels = 2
out_channels = 2

[[point]]
src = "cam1"
in_ch = 1
dst = "phones"
out_ch = 1

[[point]]
src = "phones"
in_ch = 1
dst = "cam1"
out_ch = 1
"#
    )
}

#[test]
fn janus_adapter_is_accepted_on_the_phones_role() {
    let m = Matrix::from_toml(&toml_with_phones("janus", "")).expect("janus on phones must load");
    let pid = m
        .janus_participant()
        .expect("a janus participant is present");
    assert_eq!(m.participants[pid].name, "phones");
    assert_eq!(m.participants[pid].adapter, ADAPTER_JANUS);
    // The phones leg keeps its 2-in / 2-out shape (the adapter up/down-mixes mono<->stereo).
    assert_eq!(m.participants[pid].in_channels, 2);
    assert_eq!(m.participants[pid].out_channels, 2);
}

#[test]
fn janus_adapter_is_refused_on_a_non_phones_role() {
    // Put the janus adapter on cam1 (role cambox) — must be refused.
    let bad = r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 256

[[participant]]
name = "cam1"
role = "cambox"
adapter = "janus"
in_channels = 1
out_channels = 1

[[participant]]
name = "cam2"
role = "cambox"
adapter = "vban"
host = "cam2.lan"
in_stream = "cam2"
out_stream = "cam2"
in_channels = 1
out_channels = 1

[[point]]
src = "cam2"
in_ch = 1
dst = "cam1"
out_ch = 1
"#;
    let err = Matrix::from_toml(bad).unwrap_err().to_string();
    assert!(err.contains("only allowed on role 'phones'"), "got: {err}");
}

#[test]
fn at_most_one_janus_participant() {
    // Two janus participants (both role phones) must be refused.
    let bad = r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 256

[[participant]]
name = "phones"
role = "phones"
adapter = "janus"
in_channels = 2
out_channels = 2

[[participant]]
name = "phones2"
role = "phones"
adapter = "janus"
in_channels = 2
out_channels = 2

[[participant]]
name = "cam1"
role = "cambox"
adapter = "vban"
host = "cam1.lan"
in_stream = "cam1"
out_stream = "cam1"
in_channels = 2
out_channels = 2

[[point]]
src = "cam1"
in_ch = 1
dst = "phones"
out_ch = 1
"#;
    let err = Matrix::from_toml(bad).unwrap_err().to_string();
    assert!(err.contains("at most one 'janus'"), "got: {err}");
}

#[test]
fn janus_adapter_requires_two_in_two_out_channels() {
    // The mono<->stereo up/down-mix assumes 2/2; a 1-channel janus participant must be refused
    // rather than silently mangle the mix (F4).
    let bad = r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 256

[[participant]]
name = "phones"
role = "phones"
adapter = "janus"
in_channels = 1
out_channels = 1

[[participant]]
name = "cam1"
role = "cambox"
adapter = "vban"
host = "cam1.lan"
in_stream = "cam1"
out_stream = "cam1"
in_channels = 2
out_channels = 2

[[point]]
src = "cam1"
in_ch = 1
dst = "phones"
out_ch = 1
"#;
    let err = Matrix::from_toml(bad).unwrap_err().to_string();
    assert!(err.contains("requires 2 in / 2 out"), "got: {err}");
}

#[test]
fn janus_table_defaults_when_absent_or_empty() {
    // No [janus] table -> None (a VBAN-only hub).
    let m = Matrix::from_toml(&toml_with_phones("none", "")).unwrap();
    assert!(m.janus.is_none());

    // An empty [janus] table -> all defaults.
    let m2 = Matrix::from_toml(&toml_with_phones("janus", "\n[janus]")).unwrap();
    let j = m2.janus.expect("[janus] present");
    assert_eq!(j.api_url, "http://127.0.0.1:8088/janus");
    assert_eq!(j.room, 1000);
    assert_eq!(j.rtp_bind, "0.0.0.0:6990");
    assert!(j.room_secret_file.is_none());
    // Issue 1345 (25.9.2026): the phones leg defaults to Opus.
    assert_eq!(j.codec, JanusCodec::Opus);
}

#[test]
fn janus_codec_is_selectable_and_fails_loud_on_an_unknown_value() {
    let m = Matrix::from_toml(&toml_with_phones("janus", "\n[janus]\ncodec = \"pcmu\"")).unwrap();
    assert_eq!(m.janus.unwrap().codec, JanusCodec::Pcmu);
    let m = Matrix::from_toml(&toml_with_phones("janus", "\n[janus]\ncodec = \"opus\"")).unwrap();
    assert_eq!(m.janus.unwrap().codec, JanusCodec::Opus);
    // A typo must not silently fall back to a default codec.
    assert!(
        Matrix::from_toml(&toml_with_phones("janus", "\n[janus]\ncodec = \"g722\"")).is_err(),
        "an unknown codec is refused at load"
    );
}

#[test]
fn janus_table_explicit_values_parse() {
    let extra = r#"
[janus]
api_url = "http://127.0.0.1:9099/janus"
room = 2222
room_secret_file = "/etc/intercom-hub/janus-room.secret"
rtp_bind = "0.0.0.0:7001"
"#;
    let m = Matrix::from_toml(&toml_with_phones("janus", extra)).unwrap();
    let j = m.janus.expect("[janus] present");
    assert_eq!(j.api_url, "http://127.0.0.1:9099/janus");
    assert_eq!(j.room, 2222);
    assert_eq!(
        j.room_secret_file.as_deref(),
        Some("/etc/intercom-hub/janus-room.secret")
    );
    assert_eq!(j.rtp_bind, "0.0.0.0:7001");
}

#[test]
fn state_renders_the_janus_facet_only_for_the_janus_participant() {
    let m = Matrix::from_toml(&toml_with_phones("janus", "\n[janus]")).unwrap();
    let pid = m.janus_participant().unwrap();
    let mut stats = vec![RuntimeStats::default(); m.participants.len()];
    stats[pid].janus = Some(JanusStats {
        joined: true,
        session_age_s: 12,
        rejoin_count: 1,
        rx_packets: 300,
        tx_packets: 150,
        codec: "opus",
        tx_interval_ms_sd: 0.25,
        tx_interval_ms_max: 20.5,
        tx_underflows: 2,
        tx_overflow_trims: 1,
        rx_lost_frames: 3,
    });
    let hs = HubState::snapshot(&m, "1.7.0-test", &stats);
    let v: serde_json::Value = serde_json::to_value(&hs).unwrap();

    // Find the phones + cam1 participant objects by name (order-independent).
    let parts = v["participants"].as_array().unwrap();
    let phones = parts.iter().find(|p| p["name"] == "phones").unwrap();
    let cam1 = parts.iter().find(|p| p["name"] == "cam1").unwrap();

    assert_eq!(phones["janus"]["joined"], true);
    assert_eq!(phones["janus"]["session_age_s"], 12);
    assert_eq!(phones["janus"]["rejoin_count"], 1);
    assert_eq!(phones["janus"]["rx_packets"], 300);
    assert_eq!(phones["janus"]["tx_packets"], 150);
    // Issue 1345 (25.9.2026): the codec and the pacing proof (the spacing of the last sends).
    assert_eq!(phones["janus"]["codec"], "opus");
    assert_eq!(phones["janus"]["tx_interval_ms_sd"], 0.25);
    assert_eq!(phones["janus"]["tx_interval_ms_max"], 20.5);
    assert_eq!(phones["janus"]["tx_underflows"], 2);
    assert_eq!(phones["janus"]["tx_overflow_trims"], 1);
    assert_eq!(phones["janus"]["rx_lost_frames"], 3);
    // A non-janus participant omits the facet entirely (skip_serializing_if).
    assert!(cam1.get("janus").is_none(), "cam1 must have no janus facet");
}
