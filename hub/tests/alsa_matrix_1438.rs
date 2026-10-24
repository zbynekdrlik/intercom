//! issue 1438: the `alsa` adapter (the cutters' USB card, opened directly through ALSA) in the
//! routing matrix, its input buffer, its leg on `/api/state` and the status line, and the S32_LE
//! pipe of its aplay. CI-only (serde + toml); the pure half is `alsa_card_1438.rs`.

use std::io::Read;

use intercom_hub::alsa_card::{AlsaDirState, AlsaDown, AlsaLegStats, PcmFormat};
use intercom_hub::inputs::input_buffers;
use intercom_hub::local_audio::{pipe_fill_bytes, LocalAudioFacet, PipeFillWriter};
use intercom_hub::matrix::{Matrix, ADAPTER_ALSA};
use intercom_hub::state::{HubState, RuntimeStats};
use intercom_hub::vban_io::BufferKind;

const HEAD: &str = r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 256

[[participant]]
name = "cam1"
role = "cambox"
adapter = "vban"
host = "cam1.lan"
in_stream = "cam1"
out_stream = "cam1"
in_channels = 1
out_channels = 2
"#;

/// The strih-snv cutters on the MiniFuse 4: 2 mics in, 4 outputs, the card's 6 channels each way.
const CUTTERS: &str = r#"
[[participant]]
name = "cutters"
role = "cutters"
adapter = "alsa"
alsa_card = "M4"
alsa_device = 0
alsa_period_frames = 256
alsa_out_channels = 6
alsa_in_channels = 6
alsa_channel_map = "1,2,3,4"
alsa_in_channel_map = "1,2"
in_channels = 2
out_channels = 4

[[point]]
src = "cam1"
in_ch = 1
dst = "cutters"
out_ch = 1

[[point]]
src = "cutters"
in_ch = 1
dst = "cam1"
out_ch = 1
"#;

fn matrix(cutters: &str) -> Matrix {
    Matrix::from_toml(&format!("{HEAD}{cutters}")).expect("the matrix loads")
}

fn refused(cutters: &str) -> String {
    Matrix::from_toml(&format!("{HEAD}{cutters}"))
        .expect_err("the matrix must be refused")
        .to_string()
}

#[test]
fn an_alsa_participant_resolves_to_its_card_spec() {
    let m = matrix(CUTTERS);
    let cutters = m.id_of("cutters").unwrap();
    assert_eq!(m.participants[cutters].adapter, ADAPTER_ALSA);
    let cards = m.alsa_cards();
    assert_eq!(cards.len(), 1);
    let c = &cards[0];
    assert_eq!(c.participant, cutters);
    assert_eq!((c.card.as_str(), c.device, c.period_frames), ("M4", 0, 256));
    assert_eq!((c.out_channels, c.hw_out_channels), (4, 6));
    assert_eq!((c.in_channels, c.hw_in_channels), (2, 6));
    assert_eq!(c.out_map, [0, 1, 2, 3]);
    assert_eq!(c.in_map, [0, 1]);
    // No PipeWire node is left on it.
    assert!(m.local_inputs().is_empty());
    assert!(m.local_outputs().is_empty());
}

#[test]
fn the_defaults_are_device_0_one_hub_block_and_the_participants_own_channels() {
    let m = matrix(
        &CUTTERS
            .replace("alsa_device = 0\n", "")
            .replace("alsa_period_frames = 256\n", "")
            .replace("alsa_out_channels = 6\n", "")
            .replace("alsa_in_channels = 6\n", "")
            .replace("alsa_channel_map = \"1,2,3,4\"\n", "")
            .replace("alsa_in_channel_map = \"1,2\"\n", ""),
    );
    let c = &m.alsa_cards()[0];
    assert_eq!((c.device, c.period_frames), (0, 256));
    assert_eq!((c.hw_out_channels, c.hw_in_channels), (4, 2));
    assert_eq!(c.out_map, [0, 1, 2, 3]);
    assert_eq!(c.in_map, [0, 1]);
}

#[test]
fn a_bad_card_leg_is_refused_at_load() {
    let cases: [(&str, &str, &str); 9] = [
        ("alsa_card = \"M4\"\n", "", "alsa_card"),
        (
            "alsa_card = \"M4\"",
            "alsa_card = \"3\"",
            "never a bare index",
        ),
        ("alsa_card = \"M4\"", "alsa_card = \"hw:M4\"", "alsa_card"),
        (
            "alsa_out_channels = 6",
            "alsa_out_channels = 2",
            "alsa_out_channels",
        ),
        (
            "alsa_channel_map = \"1,2,3,4\"",
            "alsa_channel_map = \"1,2\"",
            "alsa_channel_map",
        ),
        (
            "alsa_channel_map = \"1,2,3,4\"",
            "alsa_channel_map = \"1,2,3,7\"",
            "alsa_channel_map",
        ),
        (
            "alsa_channel_map = \"1,2,3,4\"",
            "alsa_channel_map = \"1,1,2,3\"",
            "alsa_channel_map",
        ),
        (
            "alsa_in_channel_map = \"1,2\"",
            "alsa_in_channel_map = \"9,1\"",
            "alsa_in_channel_map",
        ),
        (
            "alsa_period_frames = 256",
            "alsa_period_frames = 4096",
            "alsa_period_frames",
        ),
    ];
    for (from, to, want) in cases {
        let err = refused(&CUTTERS.replace(from, to));
        assert!(err.contains(want), "{from} -> {to}: {err}");
    }
}

#[test]
fn one_participant_per_card_device_and_the_keys_only_on_alsa() {
    let second = CUTTERS
        .replace("name = \"cutters\"", "name = \"speakers\"")
        .replace("role = \"cutters\"", "role = \"speakers\"")
        .replace("src = \"cutters\"", "src = \"speakers\"")
        .replace("dst = \"cutters\"", "dst = \"speakers\"");
    let err = refused(&format!("{CUTTERS}{second}"));
    assert!(err.contains("already opened"), "{err}");

    let vban_with_card = HEAD.replace(
        "in_channels = 1\nout_channels = 2",
        "alsa_card = \"M4\"\nin_channels = 1\nout_channels = 2",
    );
    let err = Matrix::from_toml(&vban_with_card).unwrap_err().to_string();
    assert!(err.contains("only for adapter 'alsa'"), "{err}");
}

#[test]
fn target_ms_is_refused_on_an_output_only_participant() {
    // Issue 1461 review round 1: a target on a participant with no ingress would be silently
    // ignored; the loader refuses it instead.
    let err = refused(
        r#"
[[participant]]
name = "speakers"
role = "speakers"
adapter = "alsa"
alsa_card = "SPK"
target_ms = 16.0
in_channels = 0
out_channels = 2
"#,
    );
    assert!(err.contains("target_ms is only for"), "{err}");
}

#[test]
fn the_card_legs_capture_gets_the_card_resampler_leg() {
    // Issue 1461: the card's crystal is a foreign clock, so its capture is crossed by the same
    // resampler leg as a VBAN leg (it had the issue-1345 local-capture ring with no rate handling).
    let m = matrix(CUTTERS);
    let buffers = input_buffers(&m);
    let cutters = m.id_of("cutters").unwrap();
    assert_eq!(buffers[cutters].kind(), BufferKind::CardCapture);
    assert_eq!(
        buffers[cutters].network_stats().unwrap().target_frames,
        2048,
        "the card's default target: two 1024-frame bursts"
    );
    let cam1 = m.id_of("cam1").unwrap();
    assert_eq!(buffers[cam1].kind(), BufferKind::VbanLeg);
}

#[test]
fn the_leg_reaches_api_state_and_the_status_line() {
    let m = matrix(CUTTERS);
    let cutters = m.id_of("cutters").unwrap();
    let leg = AlsaLegStats::default();
    leg.playback
        .set_state(AlsaDirState::Ready(PcmFormat::S16Le));
    leg.playback.set_running(true);
    leg.capture.set_state(AlsaDirState::Ready(PcmFormat::S32Le));
    leg.capture.set_running(true);
    leg.capture.record_xrun();
    let mut stats = vec![RuntimeStats::default(); m.participants.len()];
    stats[cutters].alsa = Some(leg.snapshot());
    stats[cutters].local_audio = Some(LocalAudioFacet {
        pipe_depth_frames: 640,
        ..Default::default()
    });
    let hs = HubState::snapshot(&m, "v", &stats);
    let v: serde_json::Value = serde_json::to_value(&hs).unwrap();
    let a = &v["participants"][cutters]["alsa"];
    assert_eq!(a["card"], "M4");
    assert_eq!(a["pcm"], "hw:CARD=M4,DEV=0");
    assert_eq!(a["present"], true);
    assert_eq!(a["playback"]["state"], "ready");
    assert_eq!(a["playback"]["format"], "S16_LE");
    assert_eq!(a["playback"]["hw_channels"], 6);
    assert_eq!(a["playback"]["running"], true);
    assert_eq!(a["capture"]["format"], "S32_LE");
    assert_eq!(a["capture"]["xruns"], 1);
    assert_eq!(a["alsa_period_frames"], 256);
    assert_eq!(a["alsa_buffer_frames"], 1024);
    // the other participants carry no alsa facet
    assert!(v["participants"][m.id_of("cam1").unwrap()]["alsa"].is_null());
    let line = hs.status_line();
    assert!(
        line.contains(" alsa=cutters:M4:depth=640:xruns=0/1"),
        "{line}"
    );

    // the card unplugged: present false, the side's reason on the line
    let down = AlsaLegStats::default();
    down.playback
        .set_state(AlsaDirState::Down(AlsaDown::CardMissing));
    down.capture
        .set_state(AlsaDirState::Down(AlsaDown::CardMissing));
    stats[cutters].alsa = Some(down.snapshot());
    let hs = HubState::snapshot(&m, "v", &stats);
    let v: serde_json::Value = serde_json::to_value(&hs).unwrap();
    assert_eq!(v["participants"][cutters]["alsa"]["present"], false);
    assert_eq!(
        v["participants"][cutters]["alsa"]["playback"]["state"],
        "card not present"
    );
    assert!(v["participants"][cutters]["alsa"]["playback"]["format"].is_null());
    let line = hs.status_line();
    assert!(
        line.contains(" alsa=cutters:M4:down(card not present)"),
        "{line}"
    );

    // found but not moving audio (a PCM another process holds)
    let busy = AlsaLegStats::default();
    busy.playback
        .set_state(AlsaDirState::Ready(PcmFormat::S16Le));
    busy.capture
        .set_state(AlsaDirState::Ready(PcmFormat::S32Le));
    busy.capture.set_running(true);
    stats[cutters].alsa = Some(busy.snapshot());
    let line = HubState::snapshot(&m, "v", &stats).status_line();
    assert!(
        line.contains(" alsa=cutters:M4:down(not running)"),
        "{line}"
    );
}

#[test]
fn a_capture_only_leg_shows_no_playback_side() {
    let m = matrix(
        &CUTTERS
            .replace("alsa_out_channels = 6\n", "")
            .replace("alsa_channel_map = \"1,2,3,4\"\n", "")
            .replace("out_channels = 4", "out_channels = 0")
            .replace(
                "[[point]]\nsrc = \"cam1\"\nin_ch = 1\ndst = \"cutters\"\nout_ch = 1\n",
                "",
            ),
    );
    let cutters = m.id_of("cutters").unwrap();
    let hs = HubState::snapshot(
        &m,
        "v",
        &vec![RuntimeStats::default(); m.participants.len()],
    );
    let v: serde_json::Value = serde_json::to_value(&hs).unwrap();
    assert!(v["participants"][cutters]["alsa"]["playback"].is_null());
    assert_eq!(
        v["participants"][cutters]["alsa"]["capture"]["state"],
        "not started yet"
    );
}

#[test]
fn an_s32_pipe_carries_four_bytes_a_sample_and_reads_its_fill_in_frames() {
    // aplay on a card that takes S32_LE: the hub's s16 block is widened on the way into the pipe,
    // and the guard reads the fill in 6-channel S32 frames.
    let (mut reader, writer) = std::io::pipe().expect("pipe");
    let probe = writer.try_clone().expect("clone the write end");
    let control = intercom_hub::hdmi_leg::aplay_pipe_control_for(48_000, 256, 1024);
    let mut sink = PipeFillWriter::with_control(writer, 6, control).with_format(PcmFormat::S32Le);
    let block: Vec<i16> = (0..256 * 6).map(|i| i as i16 + 1).collect();
    let report = sink.write_block(&block).unwrap();
    assert!(report.first, "the prime");
    let queued = pipe_fill_bytes(&probe).unwrap();
    assert_eq!(queued % (6 * 4), 0, "whole S32 frames");
    let mut all = vec![0u8; queued];
    reader.read_exact(&mut all).unwrap();
    let tail = &all[queued - block.len() * 4..];
    assert_eq!(tail, PcmFormat::S32Le.encode(&block), "the block, widened");
    assert!(
        all[..queued - block.len() * 4].iter().all(|&b| b == 0),
        "the prime is silence"
    );
}

#[test]
fn the_playback_pipe_must_fit_a_linux_pipe_at_its_trim_mark() {
    use intercom_hub::alsa_spec::{playback_pipe_trim_bytes, LINUX_PIPE_BYTES};
    // 9 periods of the card's channels at up to 4 bytes a sample
    assert_eq!(playback_pipe_trim_bytes(256, 6), 9 * 256 * 6 * 4);
    assert!(playback_pipe_trim_bytes(303, 6) <= LINUX_PIPE_BYTES);
    assert!(playback_pipe_trim_bytes(304, 6) > LINUX_PIPE_BYTES);
    // the MiniFuse at one hub block fits
    matrix(CUTTERS);
    // an 8-channel card at 256 frames does not: a write would block before the trim acts
    let err = refused(&CUTTERS.replace("alsa_out_channels = 6", "alsa_out_channels = 8"));
    assert!(err.contains("does not fit the aplay pipe"), "{err}");
    // a shorter period fits again
    matrix(
        &CUTTERS
            .replace("alsa_out_channels = 6", "alsa_out_channels = 8")
            .replace("alsa_period_frames = 256", "alsa_period_frames = 128"),
    );
}
