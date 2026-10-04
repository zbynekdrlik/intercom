//! The hub's real-time mix thread (issue 1401, design 5980775411).
//!
//! The block loop (pop every input, mix N-1, write every output, once per 5.33 ms) runs on ONE
//! dedicated OS thread, [`MIX_THREAD_NAME`], instead of a tokio task:
//!
//! - **SCHED_FIFO [`MIX_RT_PRIORITY`] on this thread only** ([`set_realtime_fifo`]). It is above
//!   every SCHED_OTHER task, so a few ms of contention on the box no longer makes the loop more
//!   than a period late (live 4.10.2026: the tokio workers sat on the busy E-cores and the loop
//!   missed ticks in clusters, several right on the strih-lx dantesync NTP bursts). It stays below
//!   dantesync (50) and PipeWire's own real-time threads. The tokio runtime (HTTP, the Janus
//!   session, the VBAN receive task) and the picture thread stay SCHED_OTHER.
//! - The unit grants it with `LimitRTPRIO=10` (`systemd/intercom-hub.service`). Refused (no grant,
//!   a container), the thread logs ONE loud warning and runs SCHED_OTHER ([`apply_realtime`]): the
//!   hub never fails to start over it.
//! - **Absolute deadlines** ([`MonoClock::sleep_until`]: `clock_nanosleep(CLOCK_MONOTONIC,
//!   TIMER_ABSTIME)`) on the exact block grid ([`crate::block_clock::BlockGrid`]): a late wake
//!   never shifts the next deadline, and the wake-up is not rounded to whole ms as tokio's timer
//!   wheel does.
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

/// Start the mix thread: name it [`MIX_THREAD_NAME`], ask for SCHED_FIFO [`MIX_RT_PRIORITY`] on it
/// (one warning and SCHED_OTHER when refused), then run `body` on it.
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
