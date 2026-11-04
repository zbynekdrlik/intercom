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
fn the_hub_takes_dlink_s_15_ms_default_budget_at_96_to_48() {
    // Owner 4.11.2026 (dstack#1): robustness over low latency, no pinning. W must absorb a loaded
    // box's preemption (strih-snv at load ~11: 2.7-3.6 ms), so the hub takes dlink's default.
    let d = load("").unwrap().dlink.unwrap();
    assert_eq!(
        u64::from(d.latency_ms) * 1_000_000,
        dlink::BUDGET_NS,
        "the hub's default budget is dlink's"
    );
    assert_eq!(dlink::BUDGET_NS, 15_000_000);
    let c = d.link_config(48_000);
    assert_eq!((c.device_rate(), c.converts()), (96_000, true));
    assert_eq!(c.directions(), dlink::Directions::Rx);
    let split = c.split().unwrap();
    // RX-only: the RX path is the budget; the TX path is never run, so it may pass it (dlink docs).
    assert!(split.rx_path_ns <= dlink::BUDGET_NS, "{split:?}");
    assert_eq!(
        split.latency_ns, 13_041_666,
        "P, as dstack computed it: {split:?}"
    );
    assert_eq!(
        split.margin, 1440,
        "W: an RX-only device's is the whole 15 ms at 96 kHz (dlink v0.6.32), {split:?}"
    );
    assert_eq!(
        split.margin,
        dlink::rx_work_margin(dlink::BUDGET_NS, 96_000, dlink::BLOCK),
        "W is dlink's RX-only margin, {split:?}"
    );
    assert!(
        split.margin * 1_000_000_000 / 96_000 > 3_600_000,
        "W above the measured preemption"
    );
}
