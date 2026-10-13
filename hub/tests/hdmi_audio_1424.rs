//! issue 1424 part 3: the hub's own HDMI audio leg (the strih-pp vestibule TV behind the HDMI-to-SDI
//! converter on HDMI-1). The hub resolves a NAMED connector to its ALSA hw PCM with the part-1 matcher
//! (`src/drm_output_audio_match.rs`, shared into the hub crate) and plays the participant's N-1 bus
//! straight into it through `aplay`, fed by the same pipe fill controller the pw-cat legs use.
//!
//! The fixtures under `fixtures/alsa_hdmi_1424/` are read verbatim from strih-pp (10.76.8.202) on
//! 13.10.2026, read-only: `DISPLAY=:0 xrandr --props`, `/proc/asound/cards`, `amixer -c N controls`
//! and `amixer -c 0 cget iface=PCM,name=ELD,device=D`. Std-only (no serde, no tokio), so a plain
//! rustc replica runs it under Tier-0.

use intercom_hub::drm_output_audio_match::{
    eld_monitor_id, AudioPcm, AMBIGUOUS, MATCH, NO_EDID, NO_MATCH, NO_MONITOR,
};
use intercom_hub::hdmi_audio::{
    amixer_bytes, amixer_eld_devices, amixer_iec958_switch_count, aplay_argv, asound_card_numbers,
    iec958_on_argv, is_xrun_line, resolve_from, xrandr_connector_edid, HdmiLegStats,
    HdmiResolution, ALSA_HDMI_BUFFER_FRAMES, ALSA_HDMI_PERIOD_FRAMES,
};
use intercom_hub::pipe_fill::{
    pipe_servo_setpoint, pipe_servo_setpoint_for, PipeFillControl, PipeFillPlan, PIPE_HIGH_FRAMES,
    PIPE_TARGET_FRAMES, PW_GRAPH_BURST_FRAMES,
};

const XRANDR: &str = include_str!("fixtures/alsa_hdmi_1424/xrandr-props.txt");
const CARDS: &str = include_str!("fixtures/alsa_hdmi_1424/asound-cards.txt");
const CONTROLS0: &str = include_str!("fixtures/alsa_hdmi_1424/amixer-controls-card0.txt");
const CONTROLS1: &str = include_str!("fixtures/alsa_hdmi_1424/amixer-controls-card1.txt");
const ELD3: &str = include_str!("fixtures/alsa_hdmi_1424/amixer-eld-card0-dev3.txt");
const ELD7: &str = include_str!("fixtures/alsa_hdmi_1424/amixer-eld-card0-dev7.txt");
const ELD8: &str = include_str!("fixtures/alsa_hdmi_1424/amixer-eld-card0-dev8.txt");

/// The four HDMI PCMs of strih-pp's NVIDIA HDA card as the hub reads them (devices 8 and 9 carry an
/// empty ELD: nothing plugged in).
fn strih_pp_pcms() -> Vec<AudioPcm> {
    let eld = |text: &str| eld_monitor_id(&amixer_bytes(text).expect("a values line"));
    vec![
        AudioPcm {
            card: 0,
            device: 3,
            id: eld(ELD3),
        },
        AudioPcm {
            card: 0,
            device: 7,
            id: eld(ELD7),
        },
        AudioPcm {
            card: 0,
            device: 8,
            id: eld(ELD8),
        },
        AudioPcm {
            card: 0,
            device: 9,
            id: eld(ELD8),
        },
    ]
}

#[test]
fn xrandr_props_give_each_connectors_own_edid() {
    let hdmi1 = xrandr_connector_edid(XRANDR, "HDMI-1").expect("HDMI-1 carries an EDID");
    assert_eq!(hdmi1.len(), 256, "base block + one CEA extension");
    assert_eq!(
        &hdmi1[..8],
        &[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]
    );
    // EDID bytes 8..11 = the Blackmagic converter's vendor bytes (the ELD's 16..19)
    assert_eq!(&hdmi1[8..12], &[0x09, 0xa4, 0x0c, 0xbe]);
    let hdmi0 = xrandr_connector_edid(XRANDR, "HDMI-0").expect("HDMI-0 carries an EDID");
    assert_eq!(
        &hdmi0[8..12],
        &[0x09, 0xd1, 0x24, 0x80],
        "the BenQ, a different monitor"
    );
    // the panel on DP-2 (an NEC): its own block, never a neighbour's
    let dp2 = xrandr_connector_edid(XRANDR, "DP-2").expect("DP-2 carries an EDID");
    assert_eq!(&dp2[8..12], &[0x38, 0xa3, 0x4f, 0x68]);
    // a disconnected output (no EDID property), an unknown name
    assert_eq!(xrandr_connector_edid(XRANDR, "DP-0"), None);
    assert_eq!(xrandr_connector_edid(XRANDR, "HDMI-9"), None);
    // a prefix of a real name never matches it
    assert_eq!(xrandr_connector_edid(XRANDR, "HDMI"), None);
}

#[test]
fn xrandr_edid_parse_refuses_a_broken_hex_block() {
    let odd = "HDMI-1 connected 1920x1080+0+0\n\tEDID: \n\t\t00ffffffffffff0\n\tCTM: 1\n";
    assert_eq!(
        xrandr_connector_edid(odd, "HDMI-1"),
        None,
        "an odd hex length"
    );
    let empty = "HDMI-1 connected 1920x1080+0+0\n\tEDID: \n\tCTM: 1\n";
    assert_eq!(
        xrandr_connector_edid(empty, "HDMI-1"),
        None,
        "an EDID line with no bytes"
    );
}

#[test]
fn asound_cards_and_amixer_controls_parse_the_live_text() {
    assert_eq!(asound_card_numbers(CARDS), [0, 1, 2]);
    assert_eq!(amixer_eld_devices(CONTROLS0), [3, 7, 8, 9]);
    assert_eq!(amixer_iec958_switch_count(CONTROLS0), 4);
    assert!(
        amixer_eld_devices(CONTROLS1).is_empty(),
        "the analog card has no HDMI PCM"
    );
    let eld7 = amixer_bytes(ELD7).expect("device 7 values");
    assert_eq!(eld7.len(), 95);
    assert_eq!(&eld7[16..20], &[0x09, 0xa4, 0x0c, 0xbe]);
    assert_eq!(
        amixer_bytes(ELD8),
        Some(Vec::new()),
        "an empty ELD is no bytes, not no line"
    );
    assert_eq!(
        amixer_bytes("numid=12,iface=PCM,name='ELD',device=7\n"),
        None
    );
    assert_eq!(amixer_bytes("  : values=0x10,zz\n"), None, "a garbled byte");
}

#[test]
fn the_vestibule_connector_resolves_to_its_own_hdmi_pcm() {
    let edid = xrandr_connector_edid(XRANDR, "HDMI-1").unwrap();
    let res = resolve_from(Some(&edid), &strih_pp_pcms());
    assert_eq!(
        res,
        HdmiResolution {
            result: MATCH,
            pcm: Some((0, 7))
        }
    );
    // HDMI-0's own monitor never lands on the vestibule PCM
    let benq = xrandr_connector_edid(XRANDR, "HDMI-0").unwrap();
    assert_eq!(
        resolve_from(Some(&benq), &strih_pp_pcms()).pcm,
        Some((0, 3))
    );
}

#[test]
fn no_match_is_a_named_silent_leg_never_another_device() {
    let edid = xrandr_connector_edid(XRANDR, "HDMI-1").unwrap();
    // the converter unplugged: its ELD is empty, only the BenQ is left
    let mut pcms = strih_pp_pcms();
    pcms[1].id = eld_monitor_id(&[]);
    assert_eq!(
        resolve_from(Some(&edid), &pcms),
        HdmiResolution {
            result: NO_MATCH,
            pcm: None
        }
    );
    // NVIDIA publishes an ELD only once a mode is driven: no ELD anywhere yet
    let none: Vec<AudioPcm> = strih_pp_pcms()
        .into_iter()
        .map(|mut p| {
            p.id = eld_monitor_id(&[]);
            p
        })
        .collect();
    assert_eq!(resolve_from(Some(&edid), &none).result, NO_MONITOR);
    // two identical converters: ambiguous, never a guess
    let mut twins = strih_pp_pcms();
    twins[2].id = twins[1].id.clone();
    assert_eq!(resolve_from(Some(&edid), &twins).result, AMBIGUOUS);
    // xrandr unreadable / the connector without an EDID
    assert_eq!(resolve_from(None, &strih_pp_pcms()).result, NO_EDID);
}

#[test]
fn the_iec958_switch_of_the_resolved_pcm_is_its_rank_on_the_card() {
    // device 7 is the second HDMI PCM of card 0: switch index 1
    assert_eq!(
        iec958_on_argv(0, &[3, 7, 8, 9], 7, amixer_iec958_switch_count(CONTROLS0)),
        Some(vec![
            "amixer".to_string(),
            "-c".into(),
            "0".into(),
            "cset".into(),
            "iface=MIXER,name=IEC958 Playback Switch,index=1".into(),
            "on".into(),
        ])
    );
    // an extra switch on the card (an analog S/PDIF): ambiguous, left untouched
    assert_eq!(iec958_on_argv(0, &[3, 7, 8, 9], 7, 5), None);
}

#[test]
fn aplay_plays_s16_stereo_48k_on_the_resolved_hw_pcm_with_a_short_buffer() {
    let a = aplay_argv(0, 7, 48000, 2);
    assert_eq!(a[0], "aplay");
    let pos = |k: &str| a.iter().position(|x| x == k).unwrap();
    assert_eq!(
        a[pos("-D") + 1],
        "hw:0,7",
        "the hw PCM itself, never a plug/dmix/default device"
    );
    assert_eq!(a[pos("-t") + 1], "raw");
    assert_eq!(
        a[pos("-f") + 1],
        "S16_LE",
        "the hub's own sample format, accepted by hw:0,7"
    );
    assert_eq!(a[pos("-r") + 1], "48000");
    assert_eq!(a[pos("-c") + 1], "2");
    assert!(a.contains(&format!("--period-size={ALSA_HDMI_PERIOD_FRAMES}")));
    assert!(a.contains(&format!("--buffer-size={ALSA_HDMI_BUFFER_FRAMES}")));
    assert_eq!(a.last().unwrap(), "-", "PCM on stdin");
    // zero latency by default: the ALSA buffer is 4 periods of one hub block, 21.3 ms
    assert_eq!(ALSA_HDMI_PERIOD_FRAMES, 256);
    assert_eq!(ALSA_HDMI_BUFFER_FRAMES, 1024);
}

#[test]
fn an_aplay_underrun_line_counts_as_an_xrun() {
    assert!(is_xrun_line("underrun!!! (at least 3.211 ms long)"));
    assert!(!is_xrun_line(
        "Playing raw data 'stdin' : Signed 16 bit Little Endian, Rate 48000 Hz, Stereo"
    ));
    assert!(!is_xrun_line(
        "aplay: main:850: audio open error: Device or resource busy"
    ));
}

#[test]
fn the_leg_stats_report_the_resolved_hw_and_the_named_reason() {
    let s = HdmiLegStats::default();
    let f = s.snapshot();
    assert_eq!(f.hw(), None);
    assert_eq!(f.reason(), "not resolved yet");
    s.record_resolution(&HdmiResolution {
        result: MATCH,
        pcm: Some((0, 7)),
    });
    s.record_xrun();
    s.set_running(true);
    let f = s.snapshot();
    assert_eq!(f.hw().as_deref(), Some("hw:0,7"));
    assert_eq!((f.resolves, f.xruns, f.running), (1, 1, true));
    s.record_resolution(&HdmiResolution {
        result: NO_MONITOR,
        pcm: None,
    });
    let f = s.snapshot();
    assert_eq!(f.hw(), None, "a lost match never keeps the old device");
    assert!(f.reason().contains("no ELD"), "{}", f.reason());
    assert_eq!(f.resolves, 2);
}

#[test]
fn the_aplay_leg_holds_its_pipe_at_its_own_read_size_and_pw_cat_keeps_1024() {
    // pw-cat reads a 1024-frame graph quantum: unchanged
    assert_eq!(PIPE_TARGET_FRAMES, 2 * PW_GRAPH_BURST_FRAMES);
    assert_eq!(PIPE_HIGH_FRAMES, 4 * PW_GRAPH_BURST_FRAMES);
    assert_eq!(pipe_servo_setpoint(256), 1792);
    assert_eq!(pipe_servo_setpoint_for(256, PW_GRAPH_BURST_FRAMES), 1792);
    // aplay reads one period (256 frames): two periods primed, 640 frames held (13.3 ms)
    assert_eq!(pipe_servo_setpoint_for(256, ALSA_HDMI_PERIOD_FRAMES), 640);
    let mut c = PipeFillControl::with_quantum(48000, ALSA_HDMI_PERIOD_FRAMES);
    assert_eq!(
        c.plan_block(0, 0, 256).plan,
        PipeFillPlan::TopUp {
            silence_frames: 512
        },
        "the prime is two aplay periods, not two pw-cat quanta"
    );
    assert_eq!(
        c.plan_block(5_333_333, 768, 256).plan,
        PipeFillPlan::StartHold,
        "aplay has not read yet: the block would take the fill above the target"
    );
    c.sample(6_000_000, 512);
    assert!(c.started(), "aplay's first read ends the start hold");
    let r = c.plan_block(10_666_666, 512, 256);
    assert_eq!(r.plan, PipeFillPlan::Write);
    assert_eq!(c.servo_depth().map(|d| d.setpoint_frames), Some(640));
    assert_eq!(
        c.plan_block(16_000_000, 1100, 256).plan,
        PipeFillPlan::Drop,
        "above target + two periods (1024) the block is trimmed"
    );
    // the pw-cat controller is untouched
    let mut pw = PipeFillControl::new(48000);
    assert_eq!(
        pw.plan_block(0, 0, 256).plan,
        PipeFillPlan::TopUp {
            silence_frames: 2048
        }
    );
}

// --- review round 1 (issue 1424 part 3) ----------------------------------------------------------

#[test]
fn aplay_opens_non_blocking_and_xrandr_never_reprobes() {
    use intercom_hub::hdmi_audio::xrandr_argv;
    // a PCM another process holds must fail the open at once, never hang it (aplay -N)
    assert_eq!(aplay_argv(0, 7, 48000, 2)[1], "-N");
    // the EDID is read from the server's state, without a re-probe of every output
    assert_eq!(xrandr_argv(), ["xrandr", "--current", "--props"]);
}

#[test]
fn aplays_start_banner_is_never_its_exit_reason() {
    use intercom_hub::hdmi_audio::is_banner_line;
    assert!(is_banner_line(
        "Playing raw data 'stdin' : Signed 16 bit Little Endian, Rate 48000 Hz, Stereo"
    ));
    assert!(!is_banner_line(
        "aplay: main:850: audio open error: Device or resource busy"
    ));
}

#[test]
fn the_eld_watch_ends_aplay_once_the_pcm_carries_another_monitor() {
    use intercom_hub::hdmi_audio::eld_still_matches;
    use intercom_hub::hdmi_leg::{eld_watch_step, ELD_WATCH_MISSES};
    let blackmagic = eld_monitor_id(&amixer_bytes(ELD7).unwrap());
    assert!(eld_still_matches(&blackmagic, &amixer_bytes(ELD7).unwrap()));
    assert!(
        !eld_still_matches(&blackmagic, &amixer_bytes(ELD3).unwrap()),
        "the BenQ moved onto the PCM"
    );
    assert!(
        !eld_still_matches(&blackmagic, &[]),
        "the converter unplugged (empty ELD)"
    );
    // two readings in a row end it; one odd reading, or an unreadable ELD, never does
    assert_eq!(ELD_WATCH_MISSES, 2);
    assert_eq!(eld_watch_step(0, Some(false)), (1, false));
    assert_eq!(eld_watch_step(1, Some(true)), (0, false));
    assert_eq!(eld_watch_step(1, None), (1, false));
    assert_eq!(eld_watch_step(1, Some(false)), (2, true));
}

#[test]
fn before_aplays_first_read_a_block_is_held_never_trimmed() {
    use intercom_hub::hdmi_leg::aplay_pipe_control;
    let mut c = aplay_pipe_control(48000);
    // the prime: pipe target + aplay's ALSA buffer + the period it holds
    assert_eq!(
        c.plan_block(0, 0, 256).plan,
        PipeFillPlan::TopUp {
            silence_frames: 512 + 1024 + 256
        }
    );
    // aplay has not read: the prime still sits in the pipe, the block is the start hold
    assert_eq!(
        c.plan_block(5_333_333, 2048, 256).plan,
        PipeFillPlan::StartHold
    );
    // even above the trim mark (2304 = 1024 + aplay's buffer) it is the start hold, never a trim
    assert_eq!(
        c.plan_block(10_666_666, 2400, 256).plan,
        PipeFillPlan::StartHold
    );
}

#[test]
fn an_aplay_xrun_rearms_one_prime_and_otherwise_never_tops_up() {
    use intercom_hub::hdmi_leg::aplay_pipe_control;
    let mut c = aplay_pipe_control(48000);
    let _ = c.plan_block(0, 0, 256);
    c.sample(1_000_000, 0); // aplay's first read
    assert!(c.started());
    // a pipe left near empty for a long time is NOT topped up: aplay's ALSA buffer is the reserve
    for k in 1..20u64 {
        c.sample(k * 5_333_333 - 1_000_000, 10);
        let r = c.plan_block(k * 5_333_333, 10, 256);
        assert!(
            !matches!(r.plan, PipeFillPlan::TopUp { .. }),
            "{k}: {:?}",
            r.plan
        );
    }
    // after aplay's own xrun the next write is a prime again (inside the gap that already happened)
    c.rearm_prime();
    let r = c.plan_block(20 * 5_333_333, 10, 256);
    assert!(r.first);
    assert_eq!(
        r.plan,
        PipeFillPlan::TopUp {
            silence_frames: 512 - 10 + 1024 + 256
        }
    );
}
