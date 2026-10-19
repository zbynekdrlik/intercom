//! Issue 1401, design 6265012279: the hub keeps its real-time path in RAM and is never the OOM victim.
//!
//! On 14.10.2026 stream-pp ran out of memory with no swap. The hub held no memory lock (`VmLck 0`),
//! its code pages had been evicted, and the SCHED_FIFO hub-mix thread stalled in page faults: 711
//! lost ticks in 6 s, and a page fault in `intercom-hub` itself invoked the second OOM kill
//! (finding 6264952452). Real-time priority cannot help a thread blocked on a page fault.
//!
//! So the hub locks its memory (`mlockall(MCL_CURRENT | MCL_FUTURE)`) before it spawns the hub-mix
//! thread, logs one line (`memory locked`, or a WARNING naming the errno; never fatal), pre-faults
//! a fixed stack region at hub-mix thread start, and publishes `memory_locked` on `/api/state`. The
//! unit grants the lock (`LimitMEMLOCK=infinity`) and puts the hub last in the OOM killer's order
//! (`OOMScoreAdjust=-900`).

use std::io;
use std::path::PathBuf;
use std::thread;

use intercom_hub::mix_thread::{
    apply_memory_lock, prefault_stack, MIX_STACK_PREFAULT_BYTES, MIX_THREAD_NAME,
};

fn read(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn service_section(unit: &str) -> &str {
    unit.split("[Service]")
        .nth(1)
        .and_then(|s| s.split("[Install]").next())
        .expect("the unit has a [Service] section")
}

#[test]
fn a_granted_lock_is_logged_once_and_published_as_locked() {
    let mut asked = 0;
    let lock = apply_memory_lock(|| {
        asked += 1;
        Ok(())
    });
    assert_eq!(asked, 1, "the lock is asked for exactly once");
    assert!(lock.locked);
    assert!(
        lock.message.contains("memory locked"),
        "the one line names it: {}",
        lock.message
    );
    assert!(lock.message.contains("MCL_FUTURE"), "{}", lock.message);
    assert!(
        !lock.message.contains("WARNING"),
        "a granted lock is no warning: {}",
        lock.message
    );
}

#[test]
fn a_refused_lock_is_a_warning_naming_the_errno_and_the_hub_still_runs() {
    // ENOMEM = RLIMIT_MEMLOCK below the hub's size (a unit without LimitMEMLOCK=infinity).
    let lock = apply_memory_lock(|| Err(io::Error::from_raw_os_error(12)));
    assert!(!lock.locked, "a refused lock is published as false");
    let m = &lock.message;
    assert!(m.contains("WARNING"), "{m}");
    assert!(m.contains("ENOMEM"), "the errno by name: {m}");
    assert!(m.contains("os error 12"), "and by number: {m}");
    assert!(
        m.contains("LimitMEMLOCK=infinity"),
        "it names the missing unit grant: {m}"
    );
    assert!(m.contains(MIX_THREAD_NAME), "and what it costs: {m}");

    // EPERM = a kernel that forbids it outright: named the same way.
    let lock = apply_memory_lock(|| Err(io::Error::from_raw_os_error(1)));
    assert!(!lock.locked);
    assert!(lock.message.contains("EPERM"), "{}", lock.message);
    assert!(lock.message.contains("os error 1)"), "{}", lock.message);
}

#[test]
fn an_error_without_a_known_errno_still_reads_and_never_claims_a_lock() {
    let lock = apply_memory_lock(|| Err(io::Error::other("no such call here")));
    assert!(!lock.locked);
    assert!(lock.message.contains("WARNING"), "{}", lock.message);
    assert!(
        lock.message.contains("no such call here"),
        "the error text is kept: {}",
        lock.message
    );
    let lock = apply_memory_lock(|| Err(io::Error::from_raw_os_error(99_999)));
    assert!(!lock.locked);
    assert!(lock.message.contains("99999"), "{}", lock.message);
}

#[test]
fn the_stack_prefault_is_a_fixed_whole_page_region_well_inside_a_thread_stack() {
    const PAGE: usize = 4096;
    const { assert!(MIX_STACK_PREFAULT_BYTES > 0) };
    const { assert!(MIX_STACK_PREFAULT_BYTES % PAGE == 0) };
    // std's default thread stack is 2 MiB; the pre-fault stays at half of it or less, so it can
    // never overflow the hub-mix stack.
    const { assert!(MIX_STACK_PREFAULT_BYTES <= 1024 * 1024) };
    let touched = thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(prefault_stack)
        .expect("spawn")
        .join()
        .expect("the pre-fault never overflows a 2 MiB stack");
    assert_eq!(
        touched,
        MIX_STACK_PREFAULT_BYTES / PAGE,
        "every page touched"
    );
}

#[test]
fn the_mix_thread_prefaults_its_stack_before_the_block_loop_runs() {
    let src = read("src/mix_thread.rs");
    let spawn = src
        .find("pub fn spawn_mix_thread")
        .expect("spawn_mix_thread");
    let body = &src[spawn..];
    let prefault = body
        .find("prefault_stack();")
        .expect("the mix thread pre-faults its stack");
    let run = body.find("body();").expect("then runs the block loop");
    assert!(prefault < run, "the pre-fault comes before the block loop");
}

#[test]
fn main_locks_the_memory_before_it_spawns_the_hub_mix_thread() {
    let src = read("src/main.rs");
    let lock = src
        .find("apply_memory_lock(lock_all_memory)")
        .expect("main asks for the memory lock");
    let spawn = src
        .find("spawn_mix_thread(move || run_block_loop(block_loop))")
        .expect("the hub-mix spawn");
    assert!(lock < spawn, "the memory is locked before hub-mix starts");
    assert!(
        src.contains("snapshot.memory_locked = Some("),
        "the outcome is published on /api/state"
    );
}

#[test]
fn the_unit_grants_the_lock_and_keeps_the_hub_out_of_the_oom_killers_way() {
    let unit = read("../../systemd/intercom-hub.service");
    let service = service_section(&unit);
    assert!(
        service.lines().any(|l| l.trim() == "LimitMEMLOCK=infinity"),
        "the unit lets the hub lock all its memory"
    );
    assert!(
        service.lines().any(|l| l.trim() == "OOMScoreAdjust=-900"),
        "the kernel picks a browser before the hub"
    );
}
