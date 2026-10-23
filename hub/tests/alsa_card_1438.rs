//! issue 1438: the cutters' card leg, the pure half (std-only, `intercom_hub::alsa_card`).
//!
//! The hub opens the cutters' USB card on `hw:CARD=<id>,DEV=<n>`, where nothing converts: these
//! tests pin how it matches the card itself. The card is named by its ALSA card ID (never an
//! index); a missing card is the leg down, never a fallback; the format comes from the card's own
//! USB stream description (the MiniFuse 4's, read live on strih-snv 23.10.2026, is the fixture:
//! playback S32_LE or S16_LE, capture S32_LE only, 6 channels both ways); the participant's
//! channels are mapped onto the card's hw channels.

use std::path::{Path, PathBuf};

use intercom_hub::alsa_card::{
    card_argv, card_id_ok, choose_format, decide_direction, identity_map, is_capture_banner_line,
    is_capture_xrun_line, parse_channel_map, parse_stream_formats, pcm_name, pick_from_hw,
    rates_offer, resolve_direction, spread_to_hw, AlsaDirState, AlsaDown, AlsaLegStats, Direction,
    PcmFormat, StreamAltset,
};

fn proc_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/alsa_card_1438/proc_asound")
}

fn minifuse_stream0() -> String {
    std::fs::read_to_string(proc_root().join("M4/stream0")).expect("the live MiniFuse capture")
}

#[test]
fn the_minifuse_stream_description_lists_each_altset_per_side() {
    let text = minifuse_stream0();
    let play = parse_stream_formats(&text, Direction::Playback);
    assert_eq!(play.len(), 2, "{play:?}");
    assert_eq!((play[0].format.as_str(), play[0].channels), ("S32_LE", 6));
    assert_eq!((play[1].format.as_str(), play[1].channels), ("S16_LE", 6));
    assert!(play[1].rates.contains("48000"));
    let rec = parse_stream_formats(&text, Direction::Capture);
    assert_eq!(rec.len(), 1, "{rec:?}");
    assert_eq!((rec[0].format.as_str(), rec[0].channels), ("S32_LE", 6));
}

#[test]
fn the_leg_prefers_s16_and_takes_what_the_card_offers_at_its_own_channel_count() {
    let text = minifuse_stream0();
    let play = parse_stream_formats(&text, Direction::Playback);
    let rec = parse_stream_formats(&text, Direction::Capture);
    assert_eq!(choose_format(&play, 6, 48_000), Some(PcmFormat::S16Le));
    // The MiniFuse captures S32_LE only: the hub converts.
    assert_eq!(choose_format(&rec, 6, 48_000), Some(PcmFormat::S32Le));
    // hw: opens only with the card's own channel count, never the participant's 4 or 2.
    assert_eq!(choose_format(&play, 4, 48_000), None);
    assert_eq!(choose_format(&rec, 2, 48_000), None);
    assert_eq!(
        choose_format(&play, 6, 32_000),
        None,
        "a rate the card does not list"
    );
    // A 24-bit packed card is spoken too, after the wider containers.
    let packed = [StreamAltset {
        format: "S24_3LE".into(),
        channels: 2,
        rates: "48000".into(),
    }];
    assert_eq!(
        choose_format(&packed, 2, 48_000),
        Some(PcmFormat::S24Packed)
    );
    let unknown = [StreamAltset {
        format: "FLOAT_LE".into(),
        channels: 2,
        rates: "48000".into(),
    }];
    assert_eq!(choose_format(&unknown, 2, 48_000), None);
}

#[test]
fn a_rates_line_is_a_list_or_a_continuous_range() {
    assert!(rates_offer("44100, 48000, 96000", 48_000));
    assert!(!rates_offer("44100, 96000", 48_000));
    assert!(rates_offer("8000 - 48000 (continuous)", 48_000));
    assert!(!rates_offer("8000 - 44100 (continuous)", 48_000));
    assert!(!rates_offer("", 48_000));
}

#[test]
fn a_missing_card_is_down_never_a_fallback() {
    let text = minifuse_stream0();
    assert_eq!(
        decide_direction(false, Some(&text), Direction::Playback, 6, 48_000),
        Err(AlsaDown::CardMissing)
    );
    assert_eq!(
        decide_direction(true, Some(&text), Direction::Capture, 2, 48_000),
        Err(AlsaDown::NoFormat)
    );
    // A card without a USB stream description: S16_LE, and the tool's open error speaks if not.
    assert_eq!(
        decide_direction(true, None, Direction::Playback, 2, 48_000),
        Ok(PcmFormat::S16Le)
    );
}

#[test]
fn the_card_is_looked_up_by_id_under_proc_asound() {
    let root = proc_root();
    assert_eq!(
        resolve_direction(&root, "M4", Direction::Playback, 6, 48_000),
        Ok(PcmFormat::S16Le)
    );
    assert_eq!(
        resolve_direction(&root, "M4", Direction::Capture, 6, 48_000),
        Ok(PcmFormat::S32Le)
    );
    // The MG-XU unplugged (strih-pp 22.10.2026): no /proc/asound/MGXU.
    assert_eq!(
        resolve_direction(&root, "MGXU", Direction::Playback, 2, 48_000),
        Err(AlsaDown::CardMissing)
    );
}

#[test]
fn a_card_id_is_a_name_never_an_index() {
    for ok in ["M4", "MGXU", "Generic_1", "NVidia"] {
        assert!(card_id_ok(ok), "{ok}");
    }
    for bad in [
        "",
        "3",
        "03",
        "M 4",
        "M4,DEV=1",
        "hw:M4",
        "ABCDEFGHIJKLMNOP",
    ] {
        assert!(!card_id_ok(bad), "{bad}");
    }
    assert_eq!(pcm_name("M4", 0), "hw:CARD=M4,DEV=0");
}

#[test]
fn the_argv_opens_the_hw_pcm_nonblocking_with_four_periods() {
    let play = card_argv(
        Direction::Playback,
        "M4",
        0,
        PcmFormat::S16Le,
        48_000,
        6,
        256,
    );
    let want: Vec<String> = [
        "aplay",
        "-N",
        "-D",
        "hw:CARD=M4,DEV=0",
        "-t",
        "raw",
        "-f",
        "S16_LE",
        "-r",
        "48000",
        "-c",
        "6",
        "--period-size=256",
        "--buffer-size=1024",
        "-",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(play, want);
    let rec = card_argv(
        Direction::Capture,
        "MGXU",
        1,
        PcmFormat::S32Le,
        48_000,
        2,
        128,
    );
    assert_eq!(rec[0], "arecord");
    assert_eq!(rec[3], "hw:CARD=MGXU,DEV=1");
    assert_eq!(rec[7], "S32_LE");
    assert_eq!(rec[11], "2");
    assert_eq!(rec[12], "--period-size=128");
    assert_eq!(rec[13], "--buffer-size=512");
    assert!(
        !rec.iter().any(|a| a.contains("plughw") || a == "default"),
        "nothing converts or mixes"
    );
}

#[test]
fn every_format_round_trips_the_hub_samples_at_the_same_level() {
    let samples: [i16; 6] = [0, 1, -1, 12_345, i16::MAX, i16::MIN];
    for f in [PcmFormat::S16Le, PcmFormat::S32Le, PcmFormat::S24Packed] {
        let bytes = f.encode(&samples);
        assert_eq!(bytes.len(), samples.len() * f.bytes_per_sample(), "{f:?}");
        assert_eq!(f.decode(&bytes), samples, "{f:?}");
        assert_eq!(PcmFormat::from_alsa_name(f.alsa_name()), Some(f));
    }
    // A 32-bit word's top half is the hub sample: 0x40000000 is half scale.
    assert_eq!(PcmFormat::S32Le.decode(&[0, 0, 0, 0x40]), [0x4000]);
    assert_eq!(
        PcmFormat::S32Le.decode(&0x1234_5678i32.to_le_bytes()),
        [0x1234]
    );
    assert_eq!(PcmFormat::S24Packed.decode(&[0x56, 0x34, 0x12]), [0x1234]);
    // A trailing partial sample is dropped, never a panic.
    assert_eq!(PcmFormat::S32Le.decode(&[0, 0, 0, 0x40, 1, 2]), [0x4000]);
    // The pipe writer's s16 bytes: untouched for S16_LE, widened otherwise.
    let s16 = PcmFormat::S16Le.encode(&[0x1234]);
    assert_eq!(PcmFormat::S16Le.from_s16le_bytes(s16.clone()), s16);
    assert_eq!(
        PcmFormat::S32Le.from_s16le_bytes(s16.clone()),
        [0, 0, 0x34, 0x12]
    );
    assert_eq!(PcmFormat::S24Packed.from_s16le_bytes(s16), [0, 0x34, 0x12]);
}

#[test]
fn a_channel_map_names_one_hw_channel_per_participant_channel() {
    assert_eq!(
        parse_channel_map("1,2,3,4", 4, 6, true),
        Ok(vec![0, 1, 2, 3])
    );
    assert_eq!(parse_channel_map(" 2 , 1 ", 2, 2, true), Ok(vec![1, 0]));
    assert!(
        parse_channel_map("1,7", 2, 6, true).is_err(),
        "past the card"
    );
    assert!(parse_channel_map("0,1", 2, 6, true).is_err(), "1-based");
    assert!(
        parse_channel_map("1,2,3", 2, 6, true).is_err(),
        "one per channel"
    );
    assert!(parse_channel_map("1,x", 2, 6, true).is_err());
    assert!(
        parse_channel_map("1,1", 2, 6, true).is_err(),
        "two outputs on one hw channel"
    );
    // A capture may read one mic into both channels.
    assert_eq!(parse_channel_map("1,1", 2, 6, false), Ok(vec![0, 0]));
    assert_eq!(identity_map(3), [0, 1, 2]);
}

#[test]
fn the_cutters_four_outputs_land_on_hw_1_to_4_and_hw_5_6_play_silence() {
    // two frames of 4 channels
    let block: [i16; 8] = [1, 2, 3, 4, 11, 12, 13, 14];
    let hw = spread_to_hw(&block, 4, &identity_map(4), 6);
    assert_eq!(hw, [1, 2, 3, 4, 0, 0, 11, 12, 13, 14, 0, 0]);
    // a swapped stereo map
    let swapped = spread_to_hw(&[1, 2, 3, 4], 2, &[1, 0], 2);
    assert_eq!(swapped, [2, 1, 4, 3]);
    // a trailing partial frame is dropped
    assert_eq!(spread_to_hw(&[1, 2, 3], 2, &[0, 1], 2), [1, 2]);
    assert!(spread_to_hw(&[1, 2], 0, &[], 2).is_empty());
}

#[test]
fn the_talkback_mics_are_picked_from_the_cards_six_capture_channels() {
    // two frames of 6 hw channels; the mics are hw 1 and 2
    let hw: [i16; 12] = [1, 2, 3, 4, 5, 6, 11, 12, 13, 14, 15, 16];
    assert_eq!(pick_from_hw(&hw, 6, &[0, 1]), [[1, 11], [2, 12]]);
    assert_eq!(pick_from_hw(&hw, 6, &[5]), [[6, 16]]);
    // the real path: S32_LE bytes off arecord, decoded, then picked
    let bytes = PcmFormat::S32Le.encode(&hw);
    let planar = pick_from_hw(&PcmFormat::S32Le.decode(&bytes), 6, &[0, 1]);
    assert_eq!(planar, [[1, 11], [2, 12]]);
}

#[test]
fn arecord_overruns_count_and_its_banner_is_never_the_exit_reason() {
    assert!(is_capture_xrun_line("overrun!!! (at least 2.304 ms long)"));
    assert!(!is_capture_xrun_line(
        "arecord: main:850: audio open error: Device or resource busy"
    ));
    assert!(is_capture_banner_line(
        "Recording raw data 'stdout' : Signed 32 bit Little Endian, Rate 48000 Hz, Channels 6"
    ));
    assert!(!is_capture_banner_line(
        "arecord: main:850: audio open error: No such device"
    ));
}

#[test]
fn the_leg_stats_name_each_sides_state_and_whether_the_card_is_there() {
    let s = AlsaLegStats::default();
    let f = s.snapshot();
    assert_eq!(f.playback.state, AlsaDirState::NotStarted);
    assert!(!f.card_present(), "nothing looked yet");
    s.playback
        .set_state(AlsaDirState::Down(AlsaDown::CardMissing));
    assert!(!s.snapshot().card_present());
    assert_eq!(s.snapshot().playback.state.name(), "card not present");
    s.capture.set_state(AlsaDirState::Ready(PcmFormat::S32Le));
    s.capture.set_running(true);
    s.capture.record_xrun();
    s.capture.record_spawn();
    let f = s.snapshot();
    assert!(f.card_present());
    assert_eq!(f.capture.state.format(), Some(PcmFormat::S32Le));
    assert!(f.capture.running);
    assert_eq!((f.capture.xruns, f.capture.spawns), (1, 1));
    assert_eq!((f.playback.xruns, f.playback.spawns), (0, 0));
    for st in [
        AlsaDirState::NotStarted,
        AlsaDirState::Ready(PcmFormat::S16Le),
        AlsaDirState::Ready(PcmFormat::S32Le),
        AlsaDirState::Ready(PcmFormat::S24Packed),
        AlsaDirState::Down(AlsaDown::CardMissing),
        AlsaDirState::Down(AlsaDown::NoFormat),
    ] {
        s.playback.set_state(st);
        assert_eq!(
            s.snapshot().playback.state,
            st,
            "the atomic code round-trips"
        );
    }
}
