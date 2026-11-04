//! intercom#5: the hub's own Dante device (dstack's `dlink` in the hub process). The ring between
//! dlink's pump and the hub, the processor, the drain into the participant's resampler leg, the
//! `[dlink]` table and `adapter = "dlink"` in the routing matrix, the `/api/state` facet, and one
//! real dlink device in this process carrying samples into the hub's buffer.

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dlink::Process;
use intercom_hub::dlink_leg::{
    drain_blocks, rx_facets, spawn_dlink, to_i16, DlinkFacet, DlinkLegConfig, DlinkRxFacet,
    DlinkStats, RxEvent, RxProcessor, RxWatch, SampleRing, DEFAULT_LATENCY_MS, DEFAULT_STATE_FILE,
};
use intercom_hub::inputs::input_buffers;
use intercom_hub::matrix::{Matrix, ADAPTER_DLINK};
use intercom_hub::state::{HubState, RuntimeStats};
use intercom_hub::vban_io::{BufferKind, JitterBuffer};

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

const DLINK: &str = r#"
[dlink]
name = "strih-snv-intercom"
iface = "enp7s0.70"
rx = ["FOH L", "FOH R", "Click"]
"#;

fn participant(in_channels: usize, out_channels: usize) -> String {
    format!(
        r#"
[[participant]]
name = "foh_dante"
role = "program_ref"
adapter = "dlink"
in_channels = {in_channels}
out_channels = {out_channels}
"#
    )
}

fn load(s: &str) -> anyhow::Result<Matrix> {
    Matrix::from_toml(s)
}

fn refusal(s: &str) -> String {
    format!("{:#}", load(s).expect_err("the matrix must be refused"))
}

// --- the ring ----------------------------------------------------------------------------------

#[test]
fn the_ring_hands_out_exactly_what_was_written_in_order() {
    let ring = SampleRing::new(8, 2);
    ring.push_frames(3, |f, ch| (f * 10 + ch) as i16);
    assert_eq!(ring.frames_queued(), 3);
    assert_eq!(
        ring.pop_planar(4),
        None,
        "fewer queued than asked: nothing taken"
    );
    assert_eq!(ring.frames_queued(), 3);
    let got = ring.pop_planar(2).unwrap();
    assert_eq!(got, vec![vec![0, 10], vec![1, 11]]);
    assert_eq!(ring.frames_queued(), 1);
    assert_eq!(ring.pop_planar(1).unwrap(), vec![vec![20], vec![21]]);
    assert_eq!(ring.dropped_frames(), 0);
}

#[test]
fn the_ring_wraps_around_many_times_without_losing_a_sample() {
    let ring = SampleRing::new(5, 3);
    let mut next = 0i32;
    let mut expect = 0i32;
    for round in 0..200 {
        let n = 1 + round % 4;
        let base = next;
        ring.push_frames(n, |f, ch| ((base + f as i32) * 3 + ch as i32) as i16);
        next += n as i32;
        while let Some(planar) = ring.pop_planar(1) {
            for (ch, c) in planar.iter().enumerate() {
                assert_eq!(c[0], (expect * 3 + ch as i32) as i16, "round {round}");
            }
            expect += 1;
        }
    }
    assert_eq!(expect, next);
    assert_eq!(ring.dropped_frames(), 0);
}

#[test]
fn a_full_ring_drops_the_newest_frames_and_counts_them() {
    let ring = SampleRing::new(4, 1);
    ring.push_frames(3, |f, _| f as i16);
    ring.push_frames(3, |f, _| 100 + f as i16);
    assert_eq!(ring.frames_queued(), 4);
    assert_eq!(ring.dropped_frames(), 2);
    assert_eq!(ring.pop_planar(4).unwrap(), vec![vec![0, 1, 2, 100]]);
}

#[test]
fn samples_become_i16_rounded_and_saturating() {
    assert_eq!(to_i16(0.0), 0);
    assert_eq!(to_i16(0.5), 16384);
    assert_eq!(to_i16(-0.5), -16384);
    assert_eq!(to_i16(1.0), 32767);
    assert_eq!(to_i16(-1.0), -32768);
    assert_eq!(to_i16(7.0), 32767);
    assert_eq!(to_i16(-7.0), -32768);
    assert_eq!(to_i16(f64::NAN), 0);
    assert_eq!(to_i16(1.0 / 32768.0), 1);
}

#[test]
fn the_processor_copies_every_rx_channel_and_writes_no_tx() {
    let ring = Arc::new(SampleRing::new(64, 2));
    let mut proc = RxProcessor::new(ring.clone());
    let frames = 32;
    let mut input = vec![0.0f64; frames * 2];
    for f in 0..frames {
        input[f] = 0.25;
        input[frames + f] = -0.5;
    }
    let mut output = vec![9.0f64; frames];
    let mut block = dlink::Block::new(frames, &input, &mut output);
    proc.process(&mut block);
    assert!(output.iter().all(|&x| x == 0.0), "no TX: outputs zeroed");
    let got = ring.pop_planar(frames).unwrap();
    assert_eq!(got[0], vec![8192; frames]);
    assert_eq!(got[1], vec![-16384; frames]);
}

#[test]
fn a_device_channel_the_engine_did_not_hand_reads_silent() {
    // The ring has 2 channels; the block carries only one.
    let ring = Arc::new(SampleRing::new(64, 2));
    let mut proc = RxProcessor::new(ring.clone());
    let input = vec![0.5f64; 8];
    let mut output: Vec<f64> = Vec::new();
    proc.process(&mut dlink::Block::new(8, &input, &mut output));
    let got = ring.pop_planar(8).unwrap();
    assert_eq!(got[0], vec![16384; 8]);
    assert_eq!(got[1], vec![0; 8]);
}

#[test]
fn the_drain_pushes_whole_hub_blocks_into_the_participant_s_leg() {
    let ring = SampleRing::new(2048, 2);
    ring.push_frames(256 * 3 + 100, |_, ch| if ch == 0 { 1000 } else { -1000 });
    let jitter = Mutex::new(vec![
        JitterBuffer::new(4096),
        JitterBuffer::card_capture(8192, 512, 48_000),
    ]);
    let pushed = drain_blocks(&ring, 256, "foh_dante", 1, &jitter);
    assert_eq!(pushed, 3);
    assert_eq!(
        ring.frames_queued(),
        100,
        "the partial block stays for the next drain"
    );
    let jb = jitter.lock().unwrap();
    assert!(
        jb[1].last_rx_age_ms().is_some(),
        "the dlink participant's leg received"
    );
    assert!(
        jb[0].last_rx_age_ms().is_none(),
        "no other participant touched"
    );
}

// --- the routing matrix -------------------------------------------------------------------------

#[test]
fn a_dlink_participant_loads_with_its_table_and_defaults() {
    let m = load(&format!("{HEAD}{DLINK}{}", participant(3, 0))).unwrap();
    let (d, id) = m.dlink_participant().expect("the device");
    assert_eq!(id, 1);
    assert_eq!(m.participants[id].adapter, ADAPTER_DLINK);
    assert_eq!(d.name, "strih-snv-intercom");
    assert_eq!(d.iface, "enp7s0.70");
    assert_eq!(d.rx, vec!["FOH L", "FOH R", "Click"]);
    assert_eq!(d.latency_ms, DEFAULT_LATENCY_MS);
    assert_eq!(d.state_file.to_str(), Some(DEFAULT_STATE_FILE));
    // dlink's own device config builds from it: a 10 ms budget, 48 kHz, the three RX names. Since
    // dlink v0.6.22 the 10 ms is the whole path; the Dante latency is the split's share of it.
    let c = d.link_config(48_000);
    let split = c.split().unwrap();
    let dev = c.device().unwrap();
    assert_eq!(dev.latency_ns, split.latency_ns);
    assert!(dev.latency_ns < 10_000_000 && dev.latency_ns >= dlink::MIN_LATENCY_NS);
    assert_eq!(dev.rx.len(), 3);
    assert!(dev.tx.is_empty());
}

#[test]
fn a_box_without_dlink_has_no_device() {
    let m = load(HEAD).unwrap();
    assert!(m.dlink.is_none());
    assert!(m.dlink_participant().is_none());
}

#[test]
fn the_dlink_participant_and_its_table_come_together() {
    assert!(refusal(&format!("{HEAD}{}", participant(2, 0))).contains("needs a [dlink] table"));
    assert!(refusal(&format!("{HEAD}{DLINK}")).contains("no participant has adapter 'dlink'"));
}

#[test]
fn the_dlink_participant_is_an_input_of_the_device_s_channels() {
    assert!(refusal(&format!("{HEAD}{DLINK}{}", participant(2, 2))).contains("input only"));
    assert!(refusal(&format!("{HEAD}{DLINK}{}", participant(0, 0))).contains("1..=3 in_channels"));
    assert!(refusal(&format!("{HEAD}{DLINK}{}", participant(4, 0))).contains("1..=3 in_channels"));
    // Fewer inputs than device channels is fine (an unrouted spare channel).
    load(&format!("{HEAD}{DLINK}{}", participant(2, 0))).unwrap();
}

#[test]
fn only_one_dlink_participant() {
    let two = format!(
        "{HEAD}{DLINK}{}{}",
        participant(1, 0),
        participant(1, 0).replace("foh_dante", "foh_dante2")
    );
    assert!(refusal(&two).contains("only one participant may have adapter 'dlink'"));
}

#[test]
fn dstack_s_own_device_checks_refuse_a_bad_table_at_load() {
    let bad_name = DLINK.replace("strih-snv-intercom", "strih snv");
    let e = refusal(&format!("{HEAD}{bad_name}{}", participant(1, 0)));
    assert!(e.starts_with("dlink: "), "{e}");
    assert!(e.contains("name"), "{e}");
    let low = format!("{DLINK}latency_ms = 2\n");
    let e = refusal(&format!("{HEAD}{low}{}", participant(1, 0)));
    assert!(e.contains("latency"), "{e}");
    let rel = format!("{DLINK}state_file = \"dlink.toml\"\n");
    assert!(refusal(&format!("{HEAD}{rel}{}", participant(1, 0)))
        .contains("state_file: an absolute path"));
    let no_iface = DLINK.replace("enp7s0.70", "");
    assert!(refusal(&format!("{HEAD}{no_iface}{}", participant(1, 0))).contains("iface"));
    let no_rx = DLINK.replace(r#"["FOH L", "FOH R", "Click"]"#, "[]");
    assert!(
        refusal(&format!("{HEAD}{no_rx}{}", participant(1, 0))).contains("at least one RX channel")
    );
    let typo = format!("{DLINK}latncy_ms = 10\n");
    assert!(refusal(&format!("{HEAD}{typo}{}", participant(1, 0))).contains("latncy_ms"));
}

#[test]
fn the_dlink_participant_gets_the_card_capture_resampler_leg_and_may_set_its_target() {
    let p = participant(2, 0).replace("in_channels", "target_ms = 21.33\nin_channels");
    let m = load(&format!("{HEAD}{DLINK}{p}")).unwrap();
    let bufs = input_buffers(&m);
    assert_eq!(bufs[1].kind(), BufferKind::CardCapture);
    assert_eq!(bufs[1].network_stats().unwrap().target_frames, 1024);
}

// --- /api/state ---------------------------------------------------------------------------------

#[test]
fn the_dlink_facet_is_on_the_dlink_participant_only() {
    let m = load(&format!("{HEAD}{DLINK}{}", participant(3, 0))).unwrap();
    let mut stats = vec![RuntimeStats::default(); 2];
    stats[1].dlink = Some(DlinkFacet {
        running: true,
        starts: 2,
        ends: 1,
        blocks: 77,
        ring_frames: 12,
        ring_dropped_frames: 3,
        rx: vec![DlinkRxFacet {
            rx: 1,
            name: "FOH L".into(),
            tx_channel: Some("Repro 0db L".into()),
            tx_device: Some("master-asio-dante".into()),
            status: Some("Receiving"),
            state: Some("connected"),
            flow: Some(1),
            packets: 900,
            written: 14_400,
        }],
    });
    // Even a stray facet on another participant is not rendered there.
    stats[0].dlink = stats[1].dlink.clone();
    let v = serde_json::to_value(HubState::snapshot(&m, "test", &stats)).unwrap();
    let p = &v["participants"];
    assert!(p[0].get("dlink").is_none());
    let d = &p[1]["dlink"];
    assert_eq!(d["device"], "strih-snv-intercom");
    assert_eq!(d["iface"], "enp7s0.70");
    assert_eq!(d["latency_ms"], 10);
    assert_eq!(d["rx_channels"], 3);
    assert_eq!(d["running"], true);
    assert_eq!(d["starts"], 2);
    assert_eq!(d["ends"], 1);
    assert_eq!(d["blocks"], 77);
    assert_eq!(d["ring_frames"], 12);
    assert_eq!(d["ring_dropped_frames"], 3);
    let rx = &d["rx"][0];
    assert_eq!(rx["rx"], 1);
    assert_eq!(rx["name"], "FOH L");
    assert_eq!(rx["tx_channel"], "Repro 0db L");
    assert_eq!(rx["tx_device"], "master-asio-dante");
    assert_eq!(rx["status"], "Receiving");
    assert_eq!(rx["state"], "connected");
    assert_eq!(rx["flow"], 1);
    assert_eq!(rx["packets"], 900);
    assert_eq!(rx["written"], 14_400);
    assert_eq!(p[1]["adapter"], "dlink");
}

fn channel(
    rx: usize,
    sub: Option<(&str, &str)>,
    status: Option<dlink::SubStatus>,
) -> dlink::RxChannel {
    dlink::RxChannel {
        rx,
        subscription: sub.map(|(c, d)| dstack::device::Subscription {
            tx_channel: c.into(),
            tx_device: d.into(),
        }),
        status,
        flow: status.map(|_| 1),
        packets: 10,
        written: 160,
    }
}

#[test]
fn rx_facets_name_each_channel_and_its_dante_state() {
    let names = vec![
        "REPRO L".to_string(),
        "REPRO R".to_string(),
        "GUIDE".to_string(),
    ];
    let rows = rx_facets(
        &names,
        &[
            channel(
                1,
                Some(("Repro 0db L", "master-asio-dante")),
                Some(dlink::SubStatus::Receiving),
            ),
            channel(
                2,
                Some(("Repro 0db R", "master-asio-dante")),
                Some(dlink::SubStatus::TxFail),
            ),
            channel(3, None, None),
        ],
    );
    assert_eq!(rows[0].name, "REPRO L");
    assert_eq!(rows[0].state, Some("connected"));
    assert_eq!(rows[0].status, Some(dlink::SubStatus::Receiving.label()));
    assert_eq!(rows[0].tx_device.as_deref(), Some("master-asio-dante"));
    assert_eq!(rows[1].state, Some("error"));
    assert_eq!(rows[1].status, Some(dlink::SubStatus::TxFail.label()));
    assert_eq!(
        (
            rows[2].name.as_str(),
            rows[2].state,
            rows[2].tx_channel.as_deref()
        ),
        ("GUIDE", None, None)
    );
}

fn row(rx: usize, state: Option<&'static str>, written: u64) -> DlinkRxFacet {
    DlinkRxFacet {
        rx,
        state,
        status: state.map(|_| "s"),
        written,
        ..DlinkRxFacet::default()
    }
}

#[test]
fn the_rx_watch_tells_a_lost_flow_from_a_state_change() {
    let t0 = Instant::now();
    let mut w = RxWatch::default();
    // The first read names every subscribed channel's state; an unsubscribed one stays quiet.
    assert_eq!(
        w.observe(t0, &[row(1, Some("connected"), 0), row(2, None, 0)]),
        vec![RxEvent::State {
            rx: 1,
            from: None,
            to: Some("connected"),
            status: Some("s")
        }]
    );
    // Samples flowing: nothing to say.
    assert!(w
        .observe(
            t0 + Duration::from_millis(200),
            &[row(1, Some("connected"), 100), row(2, None, 0)]
        )
        .is_empty());
    // Connected but `written` stands still: nothing before RX_STALL, one Stalled at it, then quiet.
    assert!(w
        .observe(
            t0 + Duration::from_millis(1100),
            &[row(1, Some("connected"), 100), row(2, None, 0)]
        )
        .is_empty());
    assert_eq!(
        w.observe(
            t0 + Duration::from_millis(1200),
            &[row(1, Some("connected"), 100), row(2, None, 0)]
        ),
        vec![RxEvent::Stalled { rx: 1 }]
    );
    assert!(w
        .observe(
            t0 + Duration::from_millis(3000),
            &[row(1, Some("connected"), 100), row(2, None, 0)]
        )
        .is_empty());
    // Samples again: Resumed; then the subscription fails: one State to error, and no stall
    // warning for a channel that is not connected.
    assert_eq!(
        w.observe(
            t0 + Duration::from_millis(3200),
            &[row(1, Some("connected"), 200), row(2, None, 0)]
        ),
        vec![RxEvent::Resumed { rx: 1 }]
    );
    assert_eq!(
        w.observe(
            t0 + Duration::from_millis(3400),
            &[row(1, Some("error"), 200), row(2, None, 0)]
        ),
        vec![RxEvent::State {
            rx: 1,
            from: Some("connected"),
            to: Some("error"),
            status: Some("s")
        }]
    );
    assert!(w
        .observe(
            t0 + Duration::from_millis(9000),
            &[row(1, Some("error"), 200), row(2, None, 0)]
        )
        .is_empty());
    // Back to connected after a long error with `written` still frozen: a state change restarts
    // the stall timer, so no "flow lost" fires before RX_STALL has passed in the new state.
    assert_eq!(
        w.observe(
            t0 + Duration::from_millis(9200),
            &[row(1, Some("connected"), 200), row(2, None, 0)]
        ),
        vec![RxEvent::State {
            rx: 1,
            from: Some("error"),
            to: Some("connected"),
            status: Some("s")
        }]
    );
    assert!(w
        .observe(
            t0 + Duration::from_millis(10_100),
            &[row(1, Some("connected"), 200), row(2, None, 0)]
        )
        .is_empty());
    assert_eq!(
        w.observe(
            t0 + Duration::from_millis(10_200),
            &[row(1, Some("connected"), 200), row(2, None, 0)]
        ),
        vec![RxEvent::Stalled { rx: 1 }]
    );
}

#[test]
fn the_unit_keeps_a_writable_state_directory_for_the_device() {
    let unit = include_str!("../../systemd/intercom-hub.service");
    assert!(
        unit.lines()
            .any(|l| l.trim() == "StateDirectory=intercom-hub"),
        "the device state file lives under /var/lib/intercom-hub"
    );
    assert!(DEFAULT_STATE_FILE.starts_with("/var/lib/intercom-hub/"));
}

/// The hub's Dante device follows Dante PTP in process (dlink's dsync-ptp follower), like DVS; the
/// dsync service is only its fallback (owner, 4.11.2026, dstack#1). The follower binds UDP 319, so
/// the unit grants CAP_NET_BIND_SERVICE: ambient, so it holds for setup-strih's non-root User=
/// drop-in, and the only capability the hub keeps. Without it dlink logs "dsync's in-process
/// follower did not start ... port 319 needs CAP_NET_BIND_SERVICE" and runs on the service clock.
#[test]
fn the_unit_lets_the_ptp_follower_bind_port_319_and_grants_no_other_capability() {
    let unit = include_str!("../../systemd/intercom-hub.service");
    let directives: Vec<&str> = unit
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .collect();
    assert!(
        directives.contains(&"AmbientCapabilities=CAP_NET_BIND_SERVICE"),
        "the follower binds port 319 as the unit's (non-root) user"
    );
    assert!(
        directives.contains(&"CapabilityBoundingSet=CAP_NET_BIND_SERVICE"),
        "the hub keeps no capability but the one the follower needs"
    );
    let grants = directives
        .iter()
        .filter(|d| {
            d.starts_with("AmbientCapabilities=") || d.starts_with("CapabilityBoundingSet=")
        })
        .count();
    assert_eq!(
        grants, 2,
        "one grant and one bounding set, nothing added elsewhere"
    );
}

// --- one real dlink device in this process -------------------------------------------------------

/// The box's IPv4 address on its default route (connecting a UDP socket sends nothing).
fn routed_ipv4() -> std::net::Ipv4Addr {
    let probe = UdpSocket::bind("0.0.0.0:0").unwrap();
    probe.connect("192.0.2.1:9").unwrap();
    match probe.local_addr().expect("the routed address") {
        SocketAddr::V4(a) => *a.ip(),
        other => panic!("no routed IPv4 address on this box: {other}"),
    }
}

/// The hub's processor, plus a constant 0.5 on one TX channel this test device carries so its own
/// RX 1 can subscribe to it (the hub's device has no TX; this is the only way one device in one
/// process hears real Dante audio, dstack's own dlink test does the same).
struct HubPlusTone(RxProcessor);

impl Process for HubPlusTone {
    fn process(&mut self, block: &mut dlink::Block<'_>) {
        self.0.process(block);
        block.output(0).fill(0.5);
    }
}

#[test]
fn a_real_dlink_device_hears_its_declared_subscription_in_the_hub_s_leg() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("dlink.toml");
    let table = format!(
        "{HEAD}[dlink]\nname = \"intercom-test\"\niface = \"lo\"\nrx = [\"In 1\", \"In 2\"]\nstate_file = \"{}\"\n\
         [[dlink.subscribe]]\nrx = \"In 1\"\ntx_channel = \"Tone\"\ntx_device = \"intercom-test\"\n{}",
        state.display(),
        participant(2, 0)
    );
    let m = load(&table).unwrap();
    let (d, pid) = m.dlink_participant().unwrap();
    // The hub's config, plus one TX channel for the tone.
    let config = d.link_config(48_000).tx(["Tone"]);
    // RX 1's subscription comes only from the TOML's `[[dlink.subscribe]]`: no state file yet.
    assert!(!state.exists());

    let ring = Arc::new(SampleRing::new(4096, 2));
    let link = dlink::Link::start_on(
        &config,
        &dlink::Place::Ip(routed_ipv4()),
        dlink::Map {
            rx: vec![0, 1],
            tx: vec![0],
        },
        HubPlusTone(RxProcessor::new(ring.clone())),
        dlink::Clock::System,
    )
    .expect("the dlink device started");

    let jitter = Mutex::new(input_buffers(&m));
    let end = Instant::now() + Duration::from_secs(10);
    let mut heard = false;
    while !heard && Instant::now() < end {
        // Pop through the ring directly to see the values, then hand a block to the leg.
        if let Some(block) = ring.pop_planar(256) {
            heard = block[0].contains(&16384);
            assert!(
                block[1].iter().all(|&s| s == 0),
                "RX 2 is not subscribed: silent"
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // Empty the ring, then let the pump deliver more: only frames it wrote after this point can
    // reach the drain, so `pushed` proves the pump is still running.
    while ring.pop_planar(1).is_some() {}
    std::thread::sleep(Duration::from_millis(50));
    let pushed = drain_blocks(&ring, 256, "foh_dante", pid, &jitter);
    assert_eq!(link.ended(), None);
    let stopped = link.stop();
    assert!(
        heard,
        "our TX tone came back on RX 1 through the hub's processor and ring"
    );
    assert!(pushed >= 1);
    assert!(jitter.lock().unwrap()[pid].last_rx_age_ms().is_some());
    assert!(matches!(stopped, dlink::Stopped::Done), "{stopped:?}");
}

// --- the network rate (dstack #20: the site runs 96 kHz, the hub 48 kHz) -------------------------

#[test]
fn network_rate_runs_the_device_at_96k_and_the_hub_at_48k() {
    let m = load(&format!(
        "{HEAD}{DLINK}network_rate = 96000\n{}",
        participant(3, 0)
    ))
    .unwrap();
    let (d, _) = m.dlink_participant().unwrap();
    let c = d.link_config(48_000);
    // The device joins a 96 kHz transmitter's flow; the hub's processor gets 48 kHz blocks.
    assert_eq!(c.device().unwrap().sample_rate, 96_000);
    assert_eq!(c.device_rate(), 96_000);
    assert!(c.converts());
}

#[test]
fn no_network_rate_runs_the_device_at_the_hub_s_rate() {
    let m = load(&format!("{HEAD}{DLINK}{}", participant(3, 0))).unwrap();
    let (d, _) = m.dlink_participant().unwrap();
    let c = d.link_config(48_000);
    assert_eq!(c.device().unwrap().sample_rate, 48_000);
    assert!(!c.converts());
}

#[test]
fn a_network_rate_dante_does_not_run_fails_the_load() {
    let err = load(&format!(
        "{HEAD}{DLINK}network_rate = 12345\n{}",
        participant(3, 0)
    ))
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("12345") && err.contains("is not one of"),
        "{err}"
    );
}

/// A native-rate transmitter: a constant 0.5 on its one TX channel, nothing else.
struct Tone;

impl Process for Tone {
    fn process(&mut self, block: &mut dlink::Block<'_>) {
        block.output(0).fill(0.5);
    }
}

#[test]
fn a_96k_transmitter_reaches_the_hub_at_48k_through_dlink_s_rate_step() {
    // strih-snv's shape: a native 96 kHz Dante transmitter (FOH's Ableton devices) and the hub's
    // device with network_rate 96000 handing the hub 48 kHz. Two devices in one process, so the
    // transmitter takes its own ports and process id (as two devices on one address must).
    let dir = tempfile::tempdir().unwrap();
    let tx_cfg = dlink::Config::new("intercom-tx-96", dir.path().join("tx.toml"))
        .tx(["Tone"])
        .sample_rate(96_000)
        .product("newlevel", "intercom-test")
        .ports(dlink::Ports {
            arc: 14441,
            cmc: 18801,
            flows: 14456,
        })
        .process_id(7);
    assert!(!tx_cfg.converts());
    let state = dir.path().join("dlink.toml");
    let table = format!(
        "{HEAD}[dlink]\nname = \"intercom-rx-96\"\niface = \"lo\"\nrx = [\"In 1\", \"In 2\"]\n\
         network_rate = 96000\nstate_file = \"{}\"\n\
         arc_port = 14442\ncmc_port = 18802\nflows_port = 14457\nprocess_id = 8\n\
         [[dlink.subscribe]]\nrx = \"In 1\"\ntx_channel = \"Tone\"\ntx_device = \"intercom-tx-96\"\n{}",
        state.display(),
        participant(2, 0)
    );
    let m = load(&table).unwrap();
    let (d, pid) = m.dlink_participant().unwrap();
    let rx_cfg = d.link_config(48_000);
    assert_eq!(rx_cfg.device().unwrap().sample_rate, 96_000);
    assert!(rx_cfg.converts());

    let place = dlink::Place::Ip(routed_ipv4());
    let tx = dlink::Link::start_on(
        &tx_cfg,
        &place,
        dlink::Map {
            rx: vec![],
            tx: vec![0],
        },
        Tone,
        dlink::Clock::System,
    )
    .expect("the 96 kHz transmitter started");
    let ring = Arc::new(SampleRing::new(4096, 2));
    let rx = dlink::Link::start_on(
        &rx_cfg,
        &place,
        dlink::Map {
            rx: vec![0, 1],
            tx: vec![],
        },
        RxProcessor::new(ring.clone()),
        dlink::Clock::System,
    )
    .expect("the hub's 96 kHz device started");

    // The claim: a flow forms at 96 kHz and the hub reads the transmitter's constant at its scale,
    // 16384, at 48 kHz. Count samples at 16384 +-8 until a whole second of them (48000) came
    // back. Not gapless: under a slow build (CI's llvm-cov) the two devices' pumps run late, the
    // missing blocks ring through the rate step, and that timing is dstack's to test, not ours.
    // Without network_rate no flow forms and not one sample arrives (range 0..0).
    let jitter = Mutex::new(input_buffers(&m));
    let end = Instant::now() + Duration::from_secs(20);
    let mut at_tone = 0usize;
    let (mut lo, mut hi) = (i16::MAX, i16::MIN);
    while at_tone < 48_000 && Instant::now() < end {
        // Drain all that is there, so the ring never overflows on a slow runner.
        while let Some(block) = ring.pop_planar(256) {
            lo = lo.min(*block[0].iter().min().unwrap());
            hi = hi.max(*block[0].iter().max().unwrap());
            at_tone += block[0].iter().filter(|&&s| s.abs_diff(16384) <= 8).count();
            assert!(
                block[1].iter().all(|&s| s == 0),
                "RX 2 is not subscribed: silent"
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    while ring.pop_planar(1).is_some() {}
    std::thread::sleep(Duration::from_millis(50));
    let pushed = drain_blocks(&ring, 256, "foh_dante", pid, &jitter);
    // dlink's own view of the same flow, as the hub's /api/state shows it.
    let rows = rx_facets(&d.rx, &rx.rx_channels());
    assert_eq!(rows[0].name, "In 1");
    assert_eq!(rows[0].state, Some("connected"), "{rows:?}");
    assert_eq!(rows[0].tx_device.as_deref(), Some("intercom-tx-96"));
    assert!(
        rows[0].flow.is_some() && rows[0].packets > 0 && rows[0].written > 0,
        "{rows:?}"
    );
    assert_eq!((rows[1].state, rows[1].written), (None, 0), "{rows:?}");
    assert_eq!(rx.ended(), None);
    assert_eq!(tx.ended(), None);
    let rx_stopped = rx.stop();
    let tx_stopped = tx.stop();
    assert!(
        at_tone >= 48_000,
        "a second of the 96 kHz tone at 16384 in the hub's 48 kHz (got {at_tone}, range {lo}..{hi})"
    );
    assert!(pushed >= 1);
    assert!(matches!(rx_stopped, dlink::Stopped::Done), "{rx_stopped:?}");
    assert!(matches!(tx_stopped, dlink::Stopped::Done), "{tx_stopped:?}");
}

// --- the ports (a second device on strih-snv: OBS's strih-obs) -----------------------------------

#[test]
fn the_ports_and_process_id_reach_dstack_s_device_config() {
    let table = format!(
        "{DLINK}arc_port = 4441\ncmc_port = 8801\nflows_port = 4456\nprocess_id = 1\nrx_port_base = 47100\n"
    );
    let m = load(&format!("{HEAD}{table}{}", participant(3, 0))).unwrap();
    let (d, _) = m.dlink_participant().unwrap();
    let dev = d.link_config(48_000).device().unwrap();
    assert_eq!(
        (
            dev.arc_port,
            dev.cmc_port,
            dev.flows_port,
            dev.process_id,
            dev.rx_port_base
        ),
        (4441, 8801, 4456, 1, Some(47_100))
    );
}

#[test]
fn absent_ports_keep_dante_s_defaults() {
    let m = load(&format!("{HEAD}{DLINK}{}", participant(3, 0))).unwrap();
    let dev = m.dlink.unwrap().link_config(48_000).device().unwrap();
    assert_eq!(
        (
            dev.arc_port,
            dev.cmc_port,
            dev.flows_port,
            dev.process_id,
            dev.rx_port_base
        ),
        (4440, 8800, 4455, 0, None)
    );
}

#[test]
fn the_three_control_ports_come_together_and_the_rx_range_clears_them() {
    let partial = format!("{DLINK}arc_port = 4441\n");
    assert!(refusal(&format!("{HEAD}{partial}{}", participant(1, 0))).contains("all three or none"));
    // base 4430 covers 4430..=4461, which holds the ARC port 4440: dstack refuses it.
    let overlap = format!("{DLINK}rx_port_base = 4430\n");
    assert!(refusal(&format!("{HEAD}{overlap}{}", participant(1, 0))).contains("rx_port_base"));
}

// --- declared subscriptions (dlink v0.5.12) -----------------------------------------------------

#[test]
fn declared_subscriptions_reach_dstack_by_rx_number() {
    let table = format!(
        "{DLINK}[[dlink.subscribe]]\nrx = \"Click\"\ntx_channel = \"02\"\ntx_device = \"dasio\"\n\
         [[dlink.subscribe]]\nrx = \"FOH L\"\ntx_channel = \"01\"\ntx_device = \"dasio\"\n"
    );
    let m = load(&format!("{HEAD}{table}{}", participant(3, 0))).unwrap();
    let dev = m.dlink.unwrap().link_config(48_000).device().unwrap();
    let got: Vec<_> = dev
        .subscribe
        .iter()
        .map(|s| (s.rx, s.tx_channel.as_str(), s.tx_device.as_str()))
        .collect();
    // rx is the 1-based position of the name in `rx = ["FOH L", "FOH R", "Click"]`.
    assert_eq!(got, vec![(3, "02", "dasio"), (1, "01", "dasio")]);
}

#[test]
fn no_declared_subscription_leaves_dstack_s_list_empty() {
    let m = load(&format!("{HEAD}{DLINK}{}", participant(3, 0))).unwrap();
    assert!(m
        .dlink
        .unwrap()
        .link_config(48_000)
        .device()
        .unwrap()
        .subscribe
        .is_empty());
}

#[test]
fn a_subscription_names_a_known_rx_channel_once() {
    let unknown = format!(
        "{DLINK}[[dlink.subscribe]]\nrx = \"Talkback\"\ntx_channel = \"01\"\ntx_device = \"dasio\"\n"
    );
    assert!(refusal(&format!("{HEAD}{unknown}{}", participant(3, 0)))
        .contains("rx 'Talkback' is not one of the RX channels"));
    let twice = format!(
        "{DLINK}[[dlink.subscribe]]\nrx = \"Click\"\ntx_channel = \"01\"\ntx_device = \"dasio\"\n\
         [[dlink.subscribe]]\nrx = \"Click\"\ntx_channel = \"02\"\ntx_device = \"dasio\"\n"
    );
    assert!(refusal(&format!("{HEAD}{twice}{}", participant(3, 0)))
        .contains("rx 'Click' is declared twice"));
    let typo = format!(
        "{DLINK}[[dlink.subscribe]]\nrx = \"Click\"\nchannel = \"01\"\ntx_channel = \"01\"\ntx_device = \"dasio\"\n"
    );
    assert!(refusal(&format!("{HEAD}{typo}{}", participant(3, 0))).contains("unknown field"));
    let empty = format!(
        "{DLINK}[[dlink.subscribe]]\nrx = \"Click\"\ntx_channel = \"\"\ntx_device = \"dasio\"\n"
    );
    assert!(refusal(&format!("{HEAD}{empty}{}", participant(3, 0)))
        .contains("subscribe: rx 3: a valid Dante channel and device name"));
}

// --- the ring across threads (review round 1) ---------------------------------------------------

#[test]
fn a_third_thread_never_reads_more_frames_queued_than_the_ring_holds() {
    // The pump pushes, dlink-rx pops, and the block loop's snapshot reads `frames_queued` from a
    // third thread. A snapshot that loaded `written` before `read` could see the consumer pass its
    // stale `written` and wrap to ~usize::MAX.
    use std::sync::atomic::{AtomicBool, Ordering};
    let ring = Arc::new(SampleRing::new(64, 2));
    let stop = Arc::new(AtomicBool::new(false));
    let producer = {
        let (ring, stop) = (ring.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                ring.push_frames(1, |_, _| 1);
            }
        })
    };
    let consumer = {
        let (ring, stop) = (ring.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = ring.pop_planar(1);
            }
        })
    };
    let end = Instant::now() + Duration::from_secs(3);
    let (mut worst, mut worst_raw) = (0usize, 0usize);
    while Instant::now() < end {
        worst = worst.max(ring.frames_queued());
        worst_raw = worst_raw.max(ring.queued_samples_unclamped());
    }
    stop.store(true, Ordering::Relaxed);
    producer.join().unwrap();
    consumer.join().unwrap();
    assert!(
        worst <= 64,
        "frames_queued read {worst} from a 64-frame ring"
    );
    // The load order itself (the clamp alone would hide a wrap as a full ring): both counters
    // move between the loads, so the raw reading may overshoot a little, but never wraps.
    assert!(
        worst_raw < 1 << 32,
        "the raw reading wrapped: {worst_raw} samples (written loaded before read?)"
    );
}

#[test]
fn two_threads_move_every_sample_in_order_and_untorn() {
    let ring = Arc::new(SampleRing::new(37, 3));
    const FRAMES: usize = 300_000;
    let producer = {
        let ring = ring.clone();
        std::thread::spawn(move || {
            let mut sent = 0usize;
            while sent < FRAMES {
                let before = ring.dropped_frames();
                let n = 7.min(FRAMES - sent);
                let base = sent;
                // Push only what fits, so nothing is dropped and the sequence stays whole.
                if ring.frames_queued() + n <= 37 {
                    ring.push_frames(n, |f, ch| (((base + f) * 3 + ch) % 30_000) as i16);
                    assert_eq!(ring.dropped_frames(), before);
                    sent += n;
                }
            }
        })
    };
    let mut got = 0usize;
    while got < FRAMES {
        if let Some(planar) = ring.pop_planar(1) {
            for (ch, c) in planar.iter().enumerate() {
                assert_eq!(
                    c[0],
                    ((got * 3 + ch) % 30_000) as i16,
                    "frame {got} ch {ch}"
                );
            }
            got += 1;
        }
    }
    producer.join().unwrap();
}

// --- the supervisor (review round 1) -------------------------------------------------------------

#[test]
fn a_device_that_cannot_start_is_counted_down_and_the_leg_stops_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let table = format!(
        "{HEAD}[dlink]\nname = \"intercom-test2\"\niface = \"no-such-nic0\"\nrx = [\"In 1\"]\nstate_file = \"{}\"\n{}",
        dir.path().join("dlink.toml").display(),
        participant(1, 0)
    );
    let m = load(&table).unwrap();
    let (d, pid) = m.dlink_participant().unwrap();
    let stats = Arc::new(DlinkStats::default());
    let leg = spawn_dlink(
        DlinkLegConfig {
            config: d.clone(),
            participant: pid,
            name: "foh_dante".into(),
            sample_rate: 48_000,
            block_frames: 256,
        },
        Arc::new(Mutex::new(input_buffers(&m))),
        stats.clone(),
    );
    // The first attempt runs at once (backoff 0), the second after 1 s.
    let end = Instant::now() + Duration::from_secs(5);
    while stats.snapshot().ends < 2 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    let f = stats.snapshot();
    assert!(f.ends >= 2, "each failed start is counted: {f:?}");
    assert_eq!((f.starts, f.running, f.blocks), (0, false, 0), "{f:?}");
    // The stop lands inside a backoff sleep and returns within a watch step or two.
    let t = Instant::now();
    leg.stop();
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "stop took {:?}",
        t.elapsed()
    );
}

#[test]
fn more_rx_channels_than_the_hub_reads_are_refused() {
    let names: Vec<String> = (1..=17).map(|n| format!("\"In {n}\"")).collect();
    let table = DLINK.replace(
        r#"["FOH L", "FOH R", "Click"]"#,
        &format!("[{}]", names.join(", ")),
    );
    let e = refusal(&format!("{HEAD}{table}{}", participant(1, 0)));
    assert!(e.contains("the hub reads at most 16"), "{e}");
}

// --- the deployed device over a phase-1 state file (review of the subscriptions) -----------------

#[test]
fn strih_snv_s_device_loads_over_the_phase_1_state_file() {
    // 1.9.3 ran with rx = ["Master", "Click"]. dstack keeps a saved file's channel names and adds the
    // config's for the new channels, compared case-insensitively: a new name equal to an old one
    // up to case would refuse the state and the device would never start.
    let m = Matrix::from_toml(include_str!("../../intercom.strih-snv.toml")).unwrap();
    let mut d = m.dlink.unwrap();
    let dir = tempfile::tempdir().unwrap();
    d.state_file = dir.path().join("dlink.toml");
    let mut phase1 = d.clone();
    phase1.rx = vec!["Master".into(), "Click".into()];
    phase1.subscribe.clear();
    let old = phase1.link_config(48_000).device().unwrap();
    dstack::device::DeviceState::from_config(&old)
        .save(&d.state_file)
        .unwrap();

    let dev = d.link_config(48_000).device().unwrap();
    let state = dstack::device::DeviceState::load_or_default(&dev)
        .expect("the 1.9.4 device state loads over 1.9.3's file");
    // The saved names hold for the old channels (dstack keeps a Controller rename), the config's
    // for the new: Dante Controller lists "Master"/"Click" on RX 1/2 until someone renames them.
    // Routing is by number, so REPRO L/R still arrive there.
    assert_eq!(state.rx, ["Master", "Click", "CLICK 3doba", "GUIDE"]);
    // The declared ones hold on the channels nobody subscribed in Dante Controller.
    assert!(state.subscriptions[0].is_some() && state.subscriptions[1].is_some());
}
