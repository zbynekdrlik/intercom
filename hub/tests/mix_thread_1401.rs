//! Issue 1401, design 5980775411: the block loop runs on ONE real-time OS thread, "hub-mix".
//!
//! Live on strih-lx (4.10.2026) the loop, a tokio task woken on SCHED_OTHER workers that the kernel
//! put on the busy E-cores, missed ticks in clusters, several right on the dantesync NTP bursts.
//! The mix thread asks for SCHED_FIFO 10 on itself only (above every SCHED_OTHER task, below
//! dantesync's 50), the unit grants it with `LimitRTPRIO=10`, and a refused request falls back to
//! SCHED_OTHER with one loud warning, never a failed start. It sleeps to absolute deadlines on
//! `CLOCK_MONOTONIC`. Tokio keeps HTTP, the Janus session and the VBAN receive.

use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use intercom_hub::mix_thread::{
    apply_realtime, current_sched_class, set_realtime_fifo, spawn_mix_thread, MonoClock,
    SchedClass, MIX_RT_PRIORITY, MIX_THREAD_NAME,
};

#[test]
fn the_mix_thread_asks_for_fifo_10_above_sched_other_and_below_dantesync() {
    assert_eq!(MIX_THREAD_NAME, "hub-mix");
    assert_eq!(MIX_RT_PRIORITY, 10);
    // dantesync's main thread runs SCHED_FIFO 50; the mix thread must never preempt it.
    const { assert!(MIX_RT_PRIORITY >= 1 && MIX_RT_PRIORITY < 50) };
    // A thread name over 15 bytes would be cut by the kernel (ps shows the cut name).
    const { assert!(MIX_THREAD_NAME.len() <= 15) };
}

#[test]
fn the_class_reads_the_way_ps_and_chrt_name_it() {
    assert_eq!(SchedClass::Fifo(10).label(), "SCHED_FIFO 10");
    assert_eq!(SchedClass::Other.label(), "SCHED_OTHER");
}

#[test]
fn a_refused_priority_falls_back_to_sched_other_with_one_loud_warning() {
    let mut asked = None;
    let (class, warning) = apply_realtime(|p| {
        asked = Some(p);
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    });
    assert_eq!(
        asked,
        Some(MIX_RT_PRIORITY),
        "it asks for exactly priority 10"
    );
    assert_eq!(class, SchedClass::Other, "the hub still runs");
    let warning = warning.expect("a refusal is never silent");
    assert!(warning.contains("SCHED_OTHER"), "{warning}");
    assert!(warning.contains("hub-mix"), "{warning}");
    assert!(
        warning.contains("LimitRTPRIO=10"),
        "the warning names the missing grant: {warning}"
    );
}

#[test]
fn a_granted_priority_runs_sched_fifo_10_without_a_warning() {
    let (class, warning) = apply_realtime(|_| Ok(()));
    assert_eq!(class, SchedClass::Fifo(MIX_RT_PRIORITY));
    assert_eq!(warning, None);
}

/// Whether this box lets a thread take SCHED_FIFO 10 (root, or an RLIMIT_RTPRIO grant): asked on a
/// throwaway thread, so the test threads never change class.
fn box_grants_fifo() -> bool {
    thread::spawn(|| set_realtime_fifo(MIX_RT_PRIORITY).is_ok())
        .join()
        .unwrap()
}

#[test]
fn the_real_request_changes_only_the_calling_thread() {
    let before = current_sched_class().unwrap();
    let (granted, after) = thread::spawn(|| {
        let granted = set_realtime_fifo(MIX_RT_PRIORITY).is_ok();
        (granted, current_sched_class().unwrap())
    })
    .join()
    .unwrap();
    // Whatever this box allows, the class read back matches the answer...
    let want = if granted {
        SchedClass::Fifo(MIX_RT_PRIORITY)
    } else {
        SchedClass::Other
    };
    assert_eq!(after, want);
    // ...and no other thread changed: never the whole process.
    assert_eq!(current_sched_class().unwrap(), before);
}

#[test]
fn spawn_mix_thread_names_its_thread_and_runs_the_body_at_the_granted_class() {
    let (tx, rx) = mpsc::channel();
    let handle = spawn_mix_thread(move || {
        tx.send((
            thread::current().name().map(str::to_string),
            current_sched_class().unwrap(),
        ))
        .unwrap();
    })
    .unwrap();
    handle.join().unwrap();
    let (name, class) = rx.recv().unwrap();
    assert_eq!(name.as_deref(), Some(MIX_THREAD_NAME));
    let want = if box_grants_fifo() {
        SchedClass::Fifo(MIX_RT_PRIORITY)
    } else {
        SchedClass::Other
    };
    assert_eq!(
        class, want,
        "a refused request still runs the body (SCHED_OTHER)"
    );
}

#[test]
fn the_clock_sleeps_to_an_absolute_deadline() {
    let clock = MonoClock::start();
    let deadline = clock.elapsed() + Duration::from_millis(20);
    clock.sleep_until(deadline);
    let woke = clock.elapsed();
    assert!(woke >= deadline, "never early: {woke:?} vs {deadline:?}");
    assert!(woke < deadline + Duration::from_millis(500), "{woke:?}");
    // A deadline that has passed returns at once (a late wake never sleeps a whole period more).
    let t = clock.elapsed();
    clock.sleep_until(deadline);
    clock.sleep_until(Duration::ZERO);
    assert!(clock.elapsed() - t < Duration::from_millis(100));
}

#[test]
fn the_unit_grants_exactly_the_mix_threads_priority() {
    let unit_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../systemd/intercom-hub.service");
    let unit = std::fs::read_to_string(&unit_path).expect("read the intercom-hub unit");
    let service = unit
        .split("[Service]")
        .nth(1)
        .and_then(|s| s.split("[Install]").next())
        .expect("a [Service] section");
    assert!(
        service
            .lines()
            .any(|l| l.trim() == format!("LimitRTPRIO={MIX_RT_PRIORITY}")),
        "the unit grants SCHED_FIFO {MIX_RT_PRIORITY} to the hub"
    );
    assert!(
        !unit.contains("RestrictRealtime=yes"),
        "and does not forbid it"
    );
}

#[test]
fn the_block_loop_runs_on_the_mix_thread_with_absolute_deadlines() {
    // The daemon is not unit-testable; anchor that main.rs wires the pieces tested above.
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    let src = std::fs::read_to_string(&p).expect("read main.rs");
    assert!(
        src.contains("spawn_mix_thread(move || run_block_loop(block_loop))"),
        "the block loop is the mix thread's body"
    );
    assert!(
        !src.contains("async fn run_block_loop"),
        "no longer a tokio task"
    );
    assert!(
        !src.contains("MissedTickBehavior"),
        "no tokio interval skips a tick"
    );
    let sleep = src
        .find("clock.sleep_until(grid.next_deadline())")
        .expect("an absolute-deadline sleep on the grid");
    let due = src
        .find("grid.take_due(clock.elapsed())")
        .expect("the due ticks from the same clock");
    assert!(sleep < due);
    // A mix thread that ends stops the daemon (systemd restarts it) instead of freezing the audio.
    let join = src.find("mix.join()").expect("the mix thread is watched");
    let exit = src
        .find("std::process::exit(1)")
        .expect("and its end exits");
    assert!(join < exit);
    // It publishes its own class to /api/state, so a refused grant is visible without ssh.
    assert!(
        src.contains("snapshot.mix_thread_sched = "),
        "the class on /api/state"
    );
}

#[test]
fn the_real_time_thread_publishes_the_status_and_tokio_writes_it_to_the_journal() {
    // A journal write can block; the real-time thread only hands the snapshot to the watch
    // channel, and a tokio task logs the status line from there.
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    let src = std::fs::read_to_string(&p).expect("read main.rs");
    let start = src.find("fn run_block_loop(").expect("the block loop");
    let body = &src[start..start + src[start..].find("\n}\n").expect("its end")];
    assert!(
        !body.contains("status_line()"),
        "the real-time thread never writes the status line itself"
    );
    assert!(body.contains("live_tx.send("), "it publishes the snapshot");
    // The tokio task only clones the snapshot's Arc under the watch channel's read lock and
    // formats the line after it, so the real-time thread's send never waits on the formatting.
    let clone = src
        .find("let snap = status_rx.borrow_and_update().clone();")
        .expect("a tokio task takes every published snapshot");
    let line = src
        .find("let line = snap.status_line();")
        .expect("and formats its status line after the borrow");
    assert!(clone < line);
    assert!(!src.contains("borrow_and_update().status_line()"));
}
