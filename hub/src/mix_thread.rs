//! The hub's real-time mix thread (issue 1401, design 5980775411).
//!
//! The block loop (pop every input, mix N-1, write every output, once per 5.33 ms) runs on ONE
//! dedicated OS thread, [`MIX_THREAD_NAME`], instead of a tokio task:
//!
//! - **SCHED_FIFO [`MIX_RT_PRIORITY`] on this thread only** ([`set_realtime_fifo`]). It is above
//!   every SCHED_OTHER task, so a few ms of contention on the box no longer makes the loop more
//!   than a period late (live 4.10.2026: the tokio workers sat on the busy E-cores and the loop
//!   missed ticks in clusters, several right on the strih-snv dantesync NTP bursts). It stays below
//!   dantesync (50) and PipeWire's own real-time threads. The tokio runtime (HTTP, the Janus
//!   session, the VBAN receive task) and the picture thread stay SCHED_OTHER.
//! - The unit grants it with `LimitRTPRIO=10` (`systemd/intercom-hub.service`). Refused (no grant,
//!   a container), the thread logs ONE loud warning and runs SCHED_OTHER ([`apply_realtime`]): the
//!   hub never fails to start over it.
//! - **Absolute deadlines** ([`MonoClock::sleep_until`]: `clock_nanosleep(CLOCK_MONOTONIC,
//!   TIMER_ABSTIME)`) on the exact block grid ([`crate::block_clock::BlockGrid`]): a late wake
//!   never shifts the next deadline, and the wake-up is not rounded to whole ms as tokio's timer
//!   wheel does.
//! - **Its memory stays in RAM** (issue 1401, design 6265012279). On 14.10.2026 stream-pp ran out of
//!   memory: the unlocked hub's code pages were evicted and this thread stalled in page faults, 711
//!   lost ticks in 6 s (no real-time priority helps a thread blocked on a page fault). `main` calls
//!   [`apply_memory_lock`] ([`lock_all_memory`] = `mlockall(MCL_CURRENT | MCL_FUTURE)`) before it
//!   spawns this thread, so this thread's stack and every later allocation are locked too, and the
//!   thread touches [`MIX_STACK_PREFAULT_BYTES`] of its stack before its first block. The unit grants
//!   it with `LimitMEMLOCK=infinity` (and `OOMScoreAdjust=-900` keeps the hub last in the OOM
//!   killer's order). Refused: ONE warning naming the errno, the hub runs unlocked as before, and
//!   `/api/state` reads `memory_locked: false`.
//!
//! The rtprio-off rule of the OBS boxes (issue 1357) is about OBS's render tick and the NDI threads
//! that inherited its SCHED_FIFO. It does not apply here: the hub is not OBS, this one thread
//! creates no threads, and it sleeps between blocks, so it can never hold a core.

use std::io;
use std::thread;
use std::time::Duration;

/// The block loop thread's name (`ps -L -o tid,cls,rtprio,comm -p <hub pid>` shows it).
pub const MIX_THREAD_NAME: &str = "hub-mix";

/// The block loop thread's SCHED_FIFO priority: above every SCHED_OTHER task, below dantesync's 50.
/// The unit's `LimitRTPRIO` must allow it.
pub const MIX_RT_PRIORITY: i32 = 10;

/// A thread's scheduling class as the mix thread reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedClass {
    /// SCHED_FIFO at this priority.
    Fifo(i32),
    /// Anything else (SCHED_OTHER in practice).
    Other,
}

impl SchedClass {
    /// The class as `chrt` names it: `SCHED_FIFO 10` or `SCHED_OTHER` (the `/api/state`
    /// `mix_thread_sched` value).
    pub fn label(&self) -> String {
        match self {
            SchedClass::Fifo(priority) => format!("SCHED_FIFO {priority}"),
            SchedClass::Other => "SCHED_OTHER".to_string(),
        }
    }
}

/// Ask for SCHED_FIFO at `priority` for the CALLING thread only (`sched_setscheduler(0, ..)`
/// addresses the calling thread on Linux, never the whole process).
pub fn set_realtime_fifo(priority: i32) -> io::Result<()> {
    let param = libc::sched_param {
        sched_priority: priority,
    };
    // SAFETY: `param` is a valid, initialised sched_param that outlives the call; pid 0 = the
    // calling thread.
    let rc = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The calling thread's scheduling class.
pub fn current_sched_class() -> io::Result<SchedClass> {
    // SAFETY: no pointer is passed; pid 0 = the calling thread.
    let policy = unsafe { libc::sched_getscheduler(0) };
    if policy < 0 {
        return Err(io::Error::last_os_error());
    }
    if policy != libc::SCHED_FIFO {
        return Ok(SchedClass::Other);
    }
    let mut param = libc::sched_param { sched_priority: 0 };
    // SAFETY: `param` is a valid, writable sched_param that outlives the call.
    let rc = unsafe { libc::sched_getparam(0, &mut param) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(SchedClass::Fifo(param.sched_priority))
}

/// Ask for [`MIX_RT_PRIORITY`] through `set` and decide what the thread runs as: SCHED_FIFO when
/// granted, else SCHED_OTHER plus the ONE warning to log (it names the missing unit grant).
pub fn apply_realtime(set: impl FnOnce(i32) -> io::Result<()>) -> (SchedClass, Option<String>) {
    match set(MIX_RT_PRIORITY) {
        Ok(()) => (SchedClass::Fifo(MIX_RT_PRIORITY), None),
        Err(e) => (
            SchedClass::Other,
            Some(format!(
                "intercom-hub: {MIX_THREAD_NAME}: SCHED_FIFO {MIX_RT_PRIORITY} refused ({e}) -- the \
                 block loop runs SCHED_OTHER, so contention on the box can make it miss ticks \
                 (lost blocks on every output); the unit needs LimitRTPRIO={MIX_RT_PRIORITY}"
            )),
        ),
    }
}

/// The hub-mix stack the thread touches at its start ([`prefault_stack`]): 256 KiB, an eighth of
/// std's default 2 MiB thread stack and far more than the block loop's frames use, so the loop's
/// stack pages are resident before its first block (and, once the memory is locked, stay so).
pub const MIX_STACK_PREFAULT_BYTES: usize = 256 * 1024;

/// The page size the pre-fault steps by (x86_64 / arm64 base pages).
const PREFAULT_PAGE_BYTES: usize = 4096;

/// Touch [`MIX_STACK_PREFAULT_BYTES`] of the CALLING thread's stack, one write per page, and return
/// the number of pages touched. The writes go through [`std::hint::black_box`], so the compiler
/// cannot drop the buffer.
#[inline(never)]
pub fn prefault_stack() -> usize {
    let mut region = [0u8; MIX_STACK_PREFAULT_BYTES];
    let pages = region
        .iter_mut()
        .step_by(PREFAULT_PAGE_BYTES)
        .fold(0, |pages, byte| {
            *byte = 1;
            pages + 1
        });
    std::hint::black_box(&mut region);
    pages
}

/// What the hub's memory lock came to: `locked` for `/api/state` (`memory_locked`) and the ONE line
/// to log (info when locked, warn when not).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryLock {
    pub locked: bool,
    pub message: String,
}

impl MemoryLock {
    /// Log the ONE line: info when locked, warn when the lock was refused.
    pub fn log(&self) {
        if self.locked {
            tracing::info!("{}", self.message);
        } else {
            tracing::warn!("{}", self.message);
        }
    }
}

/// Lock every current and future page of the process in RAM: `mlockall(MCL_CURRENT | MCL_FUTURE)`.
/// Non-root needs the unit's `LimitMEMLOCK=infinity` (else ENOMEM).
pub fn lock_all_memory() -> io::Result<()> {
    // SAFETY: mlockall takes only flags and touches no memory of ours.
    let rc = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// The symbolic name of the errnos `mlockall(2)` documents.
fn mlockall_errno_name(code: i32) -> Option<&'static str> {
    match code {
        libc::ENOMEM => Some("ENOMEM"),
        libc::EPERM => Some("EPERM"),
        libc::EAGAIN => Some("EAGAIN"),
        libc::EINVAL => Some("EINVAL"),
        _ => None,
    }
}

/// Ask for the memory lock through `lock` and decide what the hub publishes and logs (issue 1401,
/// design 6265012279). Granted: `locked` + "memory locked". Refused: not locked + a WARNING that
/// names the errno (by name and number) and the missing unit grant. Never fatal: an unlocked hub
/// runs as before the lock existed.
pub fn apply_memory_lock(lock: impl FnOnce() -> io::Result<()>) -> MemoryLock {
    match lock() {
        Ok(()) => MemoryLock {
            locked: true,
            message: "intercom-hub: memory locked (mlockall MCL_CURRENT|MCL_FUTURE): the \
                      hub-mix block loop never waits on a page fault or a swap-in"
                .to_string(),
        },
        Err(e) => {
            let errno = match e.raw_os_error() {
                Some(code) => format!("{} ", mlockall_errno_name(code).unwrap_or("errno")),
                None => String::new(),
            };
            MemoryLock {
                locked: false,
                message: format!(
                    "intercom-hub: WARNING: mlockall(MCL_CURRENT|MCL_FUTURE) refused: {errno}({e}) \
                     -- the hub's memory stays swappable, so under memory pressure the \
                     {MIX_THREAD_NAME} thread can stall in a page fault (lost blocks on every \
                     output); the unit needs LimitMEMLOCK=infinity. The hub runs unlocked."
                ),
            }
        }
    }
}

/// Start the mix thread: name it [`MIX_THREAD_NAME`], ask for SCHED_FIFO [`MIX_RT_PRIORITY`] on it
/// (one warning and SCHED_OTHER when refused), pre-fault its stack ([`prefault_stack`]), then run
/// `body` on it.
pub fn spawn_mix_thread<F>(body: F) -> io::Result<thread::JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    thread::Builder::new()
        .name(MIX_THREAD_NAME.into())
        .spawn(move || {
            match apply_realtime(set_realtime_fifo) {
                (_, Some(warning)) => tracing::warn!("{warning}"),
                (class, None) => tracing::info!(
                    ?class,
                    "intercom-hub: {MIX_THREAD_NAME}: the block loop runs SCHED_FIFO {MIX_RT_PRIORITY}"
                ),
            }
            prefault_stack();
            body();
        })
}

/// `CLOCK_MONOTONIC` from an origin, with absolute-deadline sleeps.
#[derive(Debug, Clone, Copy)]
pub struct MonoClock {
    origin_ns: u64,
}

impl MonoClock {
    /// A clock whose origin is now.
    pub fn start() -> Self {
        MonoClock {
            origin_ns: monotonic_ns(),
        }
    }

    /// The time since the origin.
    pub fn elapsed(&self) -> Duration {
        Duration::from_nanos(monotonic_ns().saturating_sub(self.origin_ns))
    }

    /// Sleep until `at` after the origin (returns at once when it has passed). An interrupted sleep
    /// resumes toward the same absolute deadline. Should `clock_nanosleep` ever fail any other way,
    /// a relative sleep for the rest stands in, so the real-time thread can never spin.
    pub fn sleep_until(&self, at: Duration) {
        let abs = u64::try_from(at.as_nanos())
            .unwrap_or(u64::MAX)
            .saturating_add(self.origin_ns);
        let ts = libc::timespec {
            tv_sec: (abs / 1_000_000_000) as libc::time_t,
            tv_nsec: (abs % 1_000_000_000) as libc::c_long,
        };
        loop {
            // SAFETY: `ts` is a valid timespec that outlives the call; the remaining-time pointer
            // may be null with TIMER_ABSTIME.
            let rc = unsafe {
                libc::clock_nanosleep(
                    libc::CLOCK_MONOTONIC,
                    libc::TIMER_ABSTIME,
                    &ts,
                    std::ptr::null_mut(),
                )
            };
            match rc {
                0 => return,
                libc::EINTR => continue,
                _ => {
                    thread::sleep(at.saturating_sub(self.elapsed()));
                    return;
                }
            }
        }
    }
}

/// `CLOCK_MONOTONIC` in nanoseconds (0 should the call ever fail, which it cannot for this clock).
fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec that outlives the call.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0;
    }
    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64)
}
