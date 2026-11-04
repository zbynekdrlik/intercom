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
    let d = load("").unwrap().dlink.unwrap();
    let split = d.link_config(48_000).split().unwrap();
    eprintln!("{split:?}");
    assert!(split.rx_path_ns <= 10_000_000, "{split:?}");
}
