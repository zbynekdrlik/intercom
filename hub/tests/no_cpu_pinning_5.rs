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
fn the_hub_s_10_ms_budget_still_splits_at_96_to_48() {
    // The hub sets its own budget (`latency_ms`, 10), so v0.6.28's 15 ms default does not apply.
    let d = load("").unwrap().dlink.unwrap();
    let c = d.link_config(48_000);
    assert_eq!((c.device_rate(), c.converts()), (96_000, true));
    assert_eq!(c.directions(), dlink::Directions::Rx);
    let split = c.split().unwrap();
    // RX-only: the RX path is the budget; the TX path is never run, so it may pass it (dlink docs).
    assert!(split.rx_path_ns <= 10_000_000, "{split:?}");
    assert!(split.latency_ns >= dlink::MIN_LATENCY_NS, "{split:?}");
    assert_eq!(split.margin, 384, "W: 2/5 of 10 ms at 96 kHz, {split:?}");
}
