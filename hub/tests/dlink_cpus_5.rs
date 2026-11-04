//! intercom#5 / camera-box#1485 (dlink v0.6.26): the Dante device's pump and RX threads on CPUs
//! the box reserves (`[dlink] pump_cpu` / `rx_cpu`), and the SCHED_FIFO `hub-mix` thread kept off
//! them. On strih-snv at load ~11 the unpinned pump was preempted ~3.5 ms, past its 3.16 ms
//! margin, and lost blocks; alone on a cleared CPU it lost none (camera-box, 4.11.2026).

use std::io;
use std::path::PathBuf;
use std::sync::mpsc;

use intercom_hub::matrix::Matrix;
use intercom_hub::mix_thread::{
    current_affinity, keep_off_cpus, plan_affinity, spawn_mix_thread, AffinityPlan,
};

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
"#;

fn load(extra: &str) -> anyhow::Result<Matrix> {
    Matrix::from_toml(&format!("{HEAD}{extra}"))
}

// --- the config keys -----------------------------------------------------------------------------

#[test]
fn pump_cpu_and_rx_cpu_reach_dstack_s_device_config() {
    let m = load("pump_cpu = 14\nrx_cpu = 15\n").unwrap();
    let d = m.dlink.unwrap();
    let dev = d.link_config(48_000).device().unwrap();
    assert_eq!(
        (dev.pump_cpu, dev.rx_cpu, dev.io_cpu),
        (Some(14), Some(15), None)
    );
    assert_eq!(d.reserved_cpus(), vec![14, 15]);
}

#[test]
fn one_cpu_for_both_is_reserved_once() {
    let d = load("pump_cpu = 14\nrx_cpu = 14\n").unwrap().dlink.unwrap();
    assert_eq!(d.reserved_cpus(), vec![14]);
    let only_rx = load("rx_cpu = 3\n").unwrap().dlink.unwrap();
    assert_eq!(only_rx.reserved_cpus(), vec![3]);
    let dev = only_rx.link_config(48_000).device().unwrap();
    assert_eq!((dev.pump_cpu, dev.rx_cpu), (None, Some(3)));
}

#[test]
fn unset_keys_leave_the_threads_anywhere() {
    let d = load("").unwrap().dlink.unwrap();
    let dev = d.link_config(48_000).device().unwrap();
    assert_eq!((dev.pump_cpu, dev.rx_cpu, dev.io_cpu), (None, None, None));
    assert!(d.reserved_cpus().is_empty());
}

#[test]
fn a_cpu_past_dstack_s_limit_fails_the_load() {
    let err = format!("{:#}", load("pump_cpu = 100000\n").unwrap_err());
    assert!(err.contains("pump_cpu"), "{err}");
    let err = format!("{:#}", load("rx_cpu = 100000\n").unwrap_err());
    assert!(err.contains("rx_cpu"), "{err}");
}

#[test]
fn io_cpu_is_not_a_hub_key() {
    // The hub's device is RX-only; only the two threads camera-box measured are configurable.
    let err = format!("{:#}", load("io_cpu = 14\n").unwrap_err());
    assert!(err.contains("io_cpu"), "{err}");
}

// --- hub-mix keeps off them ------------------------------------------------------------------------

#[test]
fn the_plan_takes_the_reserved_cpus_out_of_the_mask() {
    let all: Vec<usize> = (0..20).collect();
    let rest: Vec<usize> = (0..20).filter(|&c| c != 14).collect();
    assert_eq!(plan_affinity(&all, &[14]), AffinityPlan::Narrow(rest));
    assert_eq!(
        plan_affinity(&[2, 3, 4], &[3, 4]),
        AffinityPlan::Narrow(vec![2])
    );
    assert_eq!(plan_affinity(&[0, 1], &[14]), AffinityPlan::Unchanged);
    assert_eq!(plan_affinity(&[0, 1], &[]), AffinityPlan::Unchanged);
    assert_eq!(plan_affinity(&[14], &[14]), AffinityPlan::AllReserved);
}

#[test]
fn keep_off_sets_the_narrowed_mask_and_says_so() {
    let mut set_to = None;
    let (warn, line) = keep_off_cpus(
        &[1],
        || Ok(vec![0, 1, 2]),
        |c| {
            set_to = Some(c.to_vec());
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(set_to, Some(vec![0, 2]));
    assert!(!warn);
    assert!(line.contains("[0, 2]") && line.contains("[1]"), "{line}");
}

#[test]
fn nothing_reserved_touches_nothing() {
    let r = keep_off_cpus(
        &[],
        || panic!("no affinity read without a reserved CPU"),
        |_| panic!("no affinity write"),
    );
    assert!(r.is_none());
}

#[test]
fn every_cpu_reserved_keeps_the_mask_and_warns() {
    let (warn, line) =
        keep_off_cpus(&[0], || Ok(vec![0]), |_| panic!("never an empty mask")).unwrap();
    assert!(warn, "{line}");
    assert!(line.contains("preempt"), "{line}");
}

#[test]
fn a_refused_mask_warns_and_never_stops_the_hub() {
    let (warn, line) = keep_off_cpus(
        &[1],
        || Ok(vec![0, 1]),
        |_| Err(io::Error::from_raw_os_error(libc_einval())),
    )
    .unwrap();
    assert!(warn && line.contains("refused"), "{line}");
    let (warn, line) = keep_off_cpus(
        &[1],
        || Err(io::Error::from_raw_os_error(libc_einval())),
        |_| Ok(()),
    )
    .unwrap();
    assert!(warn && line.contains("cannot read"), "{line}");
}

fn libc_einval() -> i32 {
    22
}

#[test]
fn the_real_hub_mix_thread_runs_off_the_reserved_cpu() {
    let allowed = current_affinity().unwrap();
    assert!(
        allowed.len() >= 2,
        "this test needs two CPUs to reserve one (has {allowed:?})"
    );
    let reserved = allowed[0];
    let (tx, rx) = mpsc::channel();
    spawn_mix_thread(&[reserved], move || {
        tx.send(current_affinity().unwrap()).unwrap();
    })
    .unwrap()
    .join()
    .unwrap();
    let mix = rx.recv().unwrap();
    assert!(
        !mix.contains(&reserved),
        "hub-mix on {mix:?}, reserved {reserved}"
    );
    assert_eq!(mix, allowed[1..].to_vec(), "every other CPU stays");
    // Only hub-mix's own mask changed: this thread (and so every thread spawned later) keeps all.
    assert_eq!(current_affinity().unwrap(), allowed);
}

#[test]
fn main_hands_the_device_s_cpus_to_hub_mix() {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    let src = std::fs::read_to_string(p).unwrap();
    let reserved = src
        .find(".map(|d| d.reserved_cpus())")
        .expect("main reads the device's reserved CPUs");
    let spawn = src
        .find("spawn_mix_thread(&reserved_cpus, move || run_block_loop(block_loop))")
        .expect("and hands them to hub-mix");
    assert!(reserved < spawn);
}
