//! intercom#5 / dlink v0.6.28: dstack removed `Config::pump_cpu` / `io_cpu` / `rx_cpu` (owner: no
//! CPU pinning for dlink apps), so the hub's 1.9.14 `[dlink] pump_cpu` / `rx_cpu` keys are gone. A
//! config that still names one fails the load instead of being silently ignored.

use intercom_hub::matrix::Matrix;

const HEAD: &str = r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 256

[[participant]]
name = "foh_dante"
role = "program_ref"
adapter = "dlink"
in_channels = 2
out_channels = 0

[dlink]
name = "strih-snv-intercom"
iface = "enp7s0.70"
rx = ["FOH L", "FOH R"]
network_rate = 96000
"#;

fn load(extra: &str) -> anyhow::Result<Matrix> {
    Matrix::from_toml(&format!("{HEAD}{extra}"))
}

#[test]
fn the_removed_cpu_keys_fail_the_load() {
    for key in ["pump_cpu", "rx_cpu", "io_cpu"] {
        let err = format!("{:#}", load(&format!("{key} = 14\n")).unwrap_err());
        assert!(err.contains(key), "{key}: {err}");
    }
}

#[test]
fn the_hub_takes_dlink_s_default_latency_terms_at_96_to_48() {
    // Owner 4.11.2026 (dstack#1): robustness over low latency, no pinning. W must absorb a loaded
    // box's preemption (strih-snv at load ~11: 2.7-3.6 ms), so the hub takes dlink's defaults.
    // dlink v0.6.106 (dstack#244) names the terms; neither is set, so P and W stay what the
    // v0.6.78 hub's 15 ms budget gave (intercom#28: the numbers below are v0.6.78's).
    let d = load("").unwrap().dlink.unwrap();
    assert_eq!((d.network_latency_ms, d.receive_buffer_ms), (None, None));
    assert_eq!(dlink::REFERENCE_PATH_NS, 15_000_000);
    let c = d.link_config(48_000);
    assert_eq!((c.device_rate(), c.converts()), (96_000, true));
    assert_eq!(c.directions(), dlink::Directions::Rx);
    let split = c.split().unwrap();
    // RX-only: the RX path is the reference path; the TX path is never run (dlink docs).
    assert!(split.rx_path_ns <= dlink::REFERENCE_PATH_NS, "{split:?}");
    assert_eq!(
        split.latency_ns, 13_041_666,
        "P, as dstack computed it on v0.6.78: {split:?}"
    );
    assert_eq!(
        split.network_latency_ns,
        dlink::NETWORK_LATENCY_NS,
        "(a) dlink's 1 ms"
    );
    assert_eq!(
        split.network_latency_ns + split.receive_buffer_ns,
        split.latency_ns,
        "P = (a) + (b), {split:?}"
    );
    assert_eq!(
        split.margin, 1440,
        "W: an RX-only device's is the whole 15 ms at 96 kHz (dlink v0.6.32), {split:?}"
    );
    assert_eq!(split.work_margin_ns, dlink::RX_WORK_MARGIN_NS, "{split:?}");
    assert!(
        split.margin * 1_000_000_000 / 96_000 > 3_600_000,
        "W above the measured preemption"
    );
    // What `/api/state` shows is that split, computed once at load.
    assert_eq!(
        d.latency,
        intercom_hub::dlink_leg::DlinkLatency::from(&split)
    );
}
