---
paths:
  - "intercom/hub/src/block_clock.rs"
  - "intercom/hub/src/mix_thread.rs"
  - "intercom/hub/src/asrc.rs"
  - "intercom/hub/src/main.rs"
  - "intercom/hub/src/vban_io.rs"
  - "intercom/hub/src/inputs.rs"
  - "intercom/hub/src/state.rs"
  - "intercom/hub/src/vban_jitter.rs"
  - "intercom/hub/src/pipe_fill.rs"
  - "intercom/hub/tests/vban_jitter_state_1401.rs"
  - "intercom/hub/tests/hub_catchup_1401.rs"
  - "intercom/hub/tests/mix_thread_1401.rs"
  - "intercom/hub/tests/asrc_1461.rs"
  - "intercom/hub/tests/memory_lock_1401.rs"
  - "tests/python/test_strih_hub_memlock_1401.py"
  - "systemd/intercom-hub.service"
---

# The intercom hub's real-time block loop and its legs' fixed targets (issues 1401 + 1461)

Design 5980775411 (findings 5980766825): after the egress fixes the hub still lost blocks on every
output when its block loop woke more than a period late, and the FOH sender's gaps grew past the
fixed program-feed target. Step 4 (design 5981457044, ROZHODNUTÉ 5981448853) closed the three
residuals step 3 measured (comment 5981169929): the steep setpoint walk, the pipe refill on a
momentary low, and the cambox cap. Read this before touching the block loop in `main.rs`,
`block_clock`, `mix_thread`, the fixed targets or the caps in `vban_jitter`, the
refill rule in `pipe_fill`, or the program legs in `inputs` / `vban_io`. The VBAN-leg buffer itself
(prefill, the servo, `skip_missed`) is `.claude/rules/strih-intercom.md`, "VBAN legs".

## The hub-mix thread: catch-up instead of skip

Live: 18 missed ticks in one hour, in clusters of 1-5, several on the strih-snv dantesync NTP
bursts. The loop was a tokio task on SCHED_OTHER workers on the busy E-cores, and
`MissedTickBehavior::Skip` gave every missed block up on every output.

- **ONE OS thread, `hub-mix`** (`mix_thread::spawn_mix_thread`), runs the block loop. Tokio keeps
  HTTP, the Janus session and the VBAN receive; the picture thread is unchanged; the snapshot goes
  out through the same `watch::Sender`.
- **SCHED_FIFO 10 on that thread only** (`set_realtime_fifo` = `sched_setscheduler(0, ..)`, the
  calling thread). Above every SCHED_OTHER task, below dantesync (50) and PipeWire's own real-time
  threads.
  - The unit grants it: `LimitRTPRIO=10` in `systemd/intercom-hub.service` `[Service]`; the
    setup-strih step-12 drop-in leaves it alone. Pinned by `tests/intercom_hub_provisioning.rs` and
    by `intercom/hub/tests/mix_thread_1401.rs` against `MIX_RT_PRIORITY`.
  - Refused: ONE warning (`SCHED_FIFO 10 refused ... the unit needs LimitRTPRIO=10`) and the thread
    runs SCHED_OTHER; the hub never fails to start over it.
  - The thread reads its own class at start and publishes it as `mix_thread_sched` on
    `/api/state` (`SCHED_FIFO 10` / `SCHED_OTHER`), so a missing grant is visible without ssh.
- **Why the OBS boxes' "rtprio stays OFF" (issue 1357) does not apply here.** That rule is about
  OBS's genlock render tick: its SCHED_FIFO leaked to every NDI receiver thread it created
  (comment 5793075833). The hub is not OBS. This one thread creates no threads and sleeps to its
  next deadline after every block, so it never holds a core. The OBS-box grader's `rtprio` row
  reads OBS's own limit and the `95-*-genlock-rtprio.conf` grants; the hub unit's grant touches
  neither.
- **Its memory stays in RAM, and it is never the OOM victim** (design 6265012279, finding
  6264952452). On 14.10.2026 stream-pp ran out of memory with no swap: the unlocked hub's code pages
  were evicted and hub-mix stalled in page faults, 711 lost ticks in 6 s, and a page fault in the hub
  invoked the second OOM kill. SCHED_FIFO cannot help a thread blocked on a page fault.
  - `main` calls `mix_thread::apply_memory_lock(lock_all_memory)` before it seeds the live
    channel (so the seed snapshot already carries `memory_locked`) and so before the hub-mix spawn:
    `mlockall(MCL_CURRENT | MCL_FUTURE)`, so the hub-mix stack and every later allocation are
    locked too. `MemoryLock::log` writes ONE line: info `memory locked`, or a WARNING naming the
    errno by name and number (`ENOMEM` = no `LimitMEMLOCK=infinity`) and the missing grant. Never
    fatal: a refused lock runs the hub as before.
  - **Never `MCL_FUTURE` under a finite `RLIMIT_MEMLOCK`** (review 1): every LATER mapping then
    counts against the limit, so an allocation past it fails long after a lock that succeeded (an
    abort in Rust, a failed thread spawn). `lock_all_memory` reads the soft limit first and the pure
    `memlock_limit_check` refuses the lock unless it is infinity (or the hub runs as root), with a
    reason naming the limit in the same WARNING.
  - hub-mix touches `MIX_STACK_PREFAULT_BYTES` (256 KiB, whole pages) of its own stack right after
    its SCHED_FIFO request and before the block loop (`prefault_stack`).
  - The unit: `LimitMEMLOCK=infinity` + `OOMScoreAdjust=-900` in `[Service]`; no provisioning
    drop-in may set either (`tests/intercom_hub_provisioning.rs` scans every `scripts/lib/*.sh` for a
    line-leading directive).
  - Locked = resident whole: every thread stack (tokio workers, the blocking pool, the NDI and pw-cat
    threads) is faulted in at its full size, so RSS is tens of MB, not the ~1.5-17 MB the hub showed
    unlocked. `VmLck` counts every `VM_LOCKED` mapping, also glibc's reserved-but-untouched
    (PROT_NONE) malloc arenas, so it can read far MORE than RSS: grade `VmLck >= VmRSS` and
    `VmSwap 0 kB`, never "VmLck about equal to RSS" or a fixed size.
  - Known limit: the children (pw-cat, aplay, `timeout`) do not inherit the lock (fork + exec) but
    DO inherit `oom_score_adj` -900, so an egress player can still stall in a page fault, and a
    leaking child is shielded from the OOM killer too.
  - `/api/state` `memory_locked` (true / false; omitted from the pure snapshot). verify-strih item 16
    grades it through `strih_intercom_hub_memlock_grade` (`scripts/lib/strih-intercom-hub-unit.sh`,
    seam `STRIH_HUB_MEMLOCK_STATE_FILE`): a JSON true PASSes; false FAILs naming the grant and the
    restart; a hub without the field FAILs as a binary from before the lock; unreadable is a NOTE.
    The call shares the credential grade's line (verify-strih stays at 1000).
  - The unit change applies at the next hub restart: setup-strih step 13 reinstalls the unit but
    never restarts a running hub, so a box reads `memory_locked: false` until it is restarted.
  - Tests: `intercom/hub/tests/memory_lock_1401.rs` (std + the crate; the decision, the pre-fault,
    the main.rs order and the unit pins), the `/api/state` field in `vban_jitter_state_1401.rs` (CI
    only), `tests/python/test_strih_hub_memlock_1401.py`. Tier-0: add `memory_lock_1401` to R1.
- **Absolute deadlines.** `MonoClock::sleep_until` = `clock_nanosleep(CLOCK_MONOTONIC,
  TIMER_ABSTIME)`. EINTR resumes toward the same deadline; any other error falls back to a relative
  sleep, so the real-time thread can never spin. The grid is `block_clock::BlockGrid`: tick n is due
  at n x block / rate exactly (u128 ns), `hub_block_period` per tick without its 0.33 ns truncation
  summing up. A late wake never moves the grid.
- **Catch-up.** A wake that finds k missed ticks runs min(k, `CATCHUP_MAX_BLOCKS` = 4) of them at
  once, in order (pop, mix, send, write), then the current one (`run_batch`). Only the part beyond 4
  is LOST.
  - It is given up through `skip_missed` right before the CURRENT pop, never before the catch-up
    pops: the give-up floor (target minus half a block) holds for one following pop, so given up
    first, the catch-up pops would drain the leg up to 4 blocks under it.
  - The grid moves past every due tick, so a lost tick never runs later.
  - `/api/state`: `caught_up_ticks` (no loss) and `lost_ticks` (a block lost on every output).
    The status line shows `lost=N` only; a caught-up tick stays off it.
- **What the real-time thread does NOT do.** It never writes the status line (a tokio task clones
  each snapshot's Arc from the watch channel and formats + logs it after the borrow) and never logs
  a program target change (the receive task logs it after
  releasing the jitter lock, `JitterBuffer::take_target_change` / `TargetChange::log`). It still
  logs a lost-tick warning at most once a second and an unresolved-output warning once a second.
- A mix thread that ends (a panic) stops the daemon (the `hub-mix-watch` thread exits 1,
  `Restart=on-failure`) instead of leaving every output silent behind a frozen `/api/state`.
- **The bench numbers** (`tests/hub_catchup_1401.rs`: a +20 ppm cambox leg, the FOH leg at
  -20 ppm, 1 ms jitter, the program pipe on the hub's clock, the cans pipe at +50 ppm; one stall
  every 60 s, 19 stalls, 3 seeds = 57 stalls a row; step 3 in comment 5981169929, step 4 measured
  by the lane with the same bench):

  | stall | lost / stall | VBAN legs | program pipe | cans pipe (+50 ppm) |
  |---|---|---|---|---|
  | up to 23 ms | 0 | clean | clean | clean (step 3: 3-12 refills from 19 ms) |
  | 24 ms | 0 | clean (step 3: 1 cambox overrun + underrun) | clean | 0 refills (step 3: 15), 3 starved reads |
  | 25-26 ms | 0 | clean (step 3: cambox 12-24 overrun + underrun, FOH 10) | clean | 3-6 refills (step 3: 18-24), 6-12 starved reads |
  | 27-30 ms | 1 | cambox 0-4 underrun + overrun (step 3: 39-57) | 30 starved reads | 9-17 refills |
  | 40 ms | 3 | each stall one cambox underrun + overrun, one FOH overrun trim | refills in 30, starved in 57 | refills in 42 |

  - Pinned: 15 ms clean; 21-24 ms zero refills on both pipes and clean legs, at most the one
    starved read of the 24 ms row; 26 ms zero overruns and underruns on the cambox AND the FOH
    leg; 40 ms bounded and counted (at most one underrun + overrun per leg and one refill per pipe
    a stall).
  - The starved reads up to 26 ms are a residual on the drifting cans only (from 27 ms the program
    pipe starves too, 30 in 57, as in step 3), and their counts are exactly the step-3 code's
    (step 4 did not add or remove one). They happen inside the write gap, before
    the hub wakes, so no refill rule can prevent them:
    - 24 ms: the FIRST stall of each seed, 60 s after the spawn, while the cans' depth is still
      being walked in (pw-cat's second read in the gap found 1005-1010 of its 1024 frames);
    - 26 ms: the stalls at 60 / 240 / 900 / 1080 s of every seed (the same read phases each
      time), the second read finding 928-1010.

    A real pw-cat then blocks in its read until the catch-up burst arrives (an xrun), it does not
    lose the audio. Why the depth sits that low at those phases was not traced: a follow-up
    candidate for the supervisor (cans: 3 / 6 / 12 / 15 starved reads in 57 stalls at 24 / 25 /
    26 / 27 ms; from 27 ms the program pipe 30).
- **Residual: locks shared with SCHED_OTHER threads.** Std mutexes have no priority inheritance.
  The mix thread takes the jitter buffers' mutex (the VBAN receive task, the local capture threads
  and the Janus adapter push into it), the Janus ring's and the `out_addrs` mutex, and the watch
  channel's write lock on every publish (its readers, the status task and the HTTP handlers, only
  clone the snapshot's Arc under the read lock). A SCHED_OTHER
  holder preempted mid-section (each holds it for a few µs) holds the real-time thread until it
  runs again; a late wake from that shows as `caught_up_ticks`. The cycle's own cost was not
  measured on the box.

## The program target is FIXED (issue 1461, design 6376769274)

The issue-1401 adaptive program target (`adaptive_target.rs`: the largest gap of the last 10 min,
6..12 blocks, raised at once, lowered a block per 10 min) and its gentle setpoint walk are GONE. They
moved the cutters' and the vestibule's program latency while running, against the owner's
no-runtime-latency rule.

- Every ingress leg now holds a FIXED target from its TOML `target_ms`, read at start. A program
  feed's default is `VBAN_PROGRAM_TARGET_BLOCKS` = 7 (37.3 ms): the afternoon's 27.6 ms gap + one
  block + half a burst.
- An underrun re-primes to the same target. Above the cap there is one counted resync.
- The crossing itself is the resampler, `.claude/rules/strih-intercom.md` "VBAN legs". The map of
  every path is `.claude/rules/audio-clock-map.md`.
- The caps keep the step-4 derivation (`vban_cap_blocks`, below): a program feed's cap is 13 blocks.

## Reading it on strih-snv after a hub deploy (the supervisor)

- Step 3 changed the base unit, so on a box still on a pre-step-3 unit reinstall it before the
  restart: `install -Dm644 systemd/intercom-hub.service /etc/systemd/system/intercom-hub.service`
  (or setup-strih step 13) + `systemctl daemon-reload`. Then `systemctl show intercom-hub -p
  LimitRTPRIO` must print `LimitRTPRIO=10`. Step 4 changes the binary only (no unit, no config).
- `curl -s http://strih-snv:8790/api/state | jq -r .mix_thread_sched` -> `SCHED_FIFO 10`.
- On the box: `ps -L -o tid,cls,rtprio,comm -p "$(pidof intercom-hub)"` -> the `hub-mix` row
  `FF 10`, every other row `TS -` (the tokio workers, `janus-paced-tx`, the local-audio sink
  threads, `interkom-video` / `ndir:*`). Or `chrt -p <hub-mix tid>` -> `SCHED_FIFO`, priority 10.
  The journal at start: `hub-mix: the block loop runs SCHED_FIFO 10`, never the `refused` warning.
- The memory lock (design 6265012279): after reinstalling the unit + `daemon-reload` + a hub
  restart, `systemctl show intercom-hub -p LimitMEMLOCK -p OOMScoreAdjust` reads `infinity` / `-900`,
  `/api/state` `memory_locked: true`, the journal `memory locked` (never the `mlockall ... refused`
  WARNING), `grep -E 'VmLck|VmRSS|VmSwap' /proc/$(pidof intercom-hub)/status` shows VmLck >= VmRSS
  and VmSwap 0 kB (VmLck can be much larger, see above), and
  `cat /proc/$(pidof intercom-hub)/oom_score_adj` = -900.
- The 13:58 stopgap (`chrt -f -p 10` on 22 hub threads, comment 5980789088) ends with that restart:
  the tokio workers are SCHED_OTHER again by design.
- Over >= 1 h: `lost_ticks` 0, `caught_up_ticks` growing only with the box's hiccups, `fohabl`
  underruns and `resyncs` flat, its `target_frames` 1792 and never moving, its `rate_ppm` settled
  (the FOH desk measured +0.5 ppm) and `asrc_phase_frames` inside +-448 (a quarter of the target),
  both pipes' `pipe_refills` flat.
- A `pipe ran low — topped up with silence` warn line now means the pipe read under one block for
  a whole hub period (a pw-cat starving), never a momentary low in a catch-up gap.

## Step 4: the step-3 residuals (design 5981457044, 4.10.2026)

Each fix sits at the policy point that owns the behaviour; there is still one rate rule, one pipe
guard and one cap rule.

- **The setpoint walk** (`vban_jitter::NetworkFill`, `set_target`, `SERVO_WALK_*`): REMOVED by
  issue 1461 with the adaptive target it walked in. The 1 s mean wobble of a bursty sender, the
  drift-first order and the walk spacing it handled no longer exist: the target is fixed and the
  ingress has no single-frame servo.
- **The starvation-only refill (`pipe_fill`).**
  - `pipe_fill_plan(fill, block, starved)`. `PipeFillControl` decides `starved`: the first write
    (the prime), or a fill under one block on EVERY reading for at least one hub period
    (`hub_block_period(block, rate)`, 5.33 ms). The readings are the 1 ms samples and the
    pre-write readings.
  - A reading of one block or more ends the run; so does a top-up. A plain block write does not:
    a pw-cat blocked in its `fread` swallows each block the moment it lands, so a really starved
    pipe reads ~0 on every reading and is refilled at the next block.
  - The momentary low it ignores: two pw-cat quantum reads in a catch-up gap leave the pipe under
    one block from the second read to the hub's wake. Up to a 24 ms stall that is shorter than a
    hub period, and the catch-up burst refills it.
  - `PipeFillControl::new(sample_rate)` and `PipeFillWriter::new(pipe, channels, sample_rate)`:
    `PwCatSink::spawn` passes the rate it spawns pw-cat with. The trim (one reading above
    `PIPE_HIGH_FRAMES`) and the prime are unchanged.
- **The derived caps (`vban_jitter::vban_cap_blocks`).**
  - `cap = target + CATCHUP_MAX_BLOCKS (4) + 1 (the current tick) + VBAN_CAP_HEADROOM_BLOCKS (1,
    the wake's phase and the arrival jitter)`.
  - That gives `VBAN_CAP_BLOCKS` 9 (48 ms) and `VBAN_PROGRAM_CAP_BLOCKS` 13 (the fixed 7-block
    program target, issue 1461), never a typed number. A TOML `target_ms` gets the same 6 blocks
    of headroom.
  - The pop before a stall leaves a leg about one block under its target, and while the loop is
    up to four ticks late up to five more blocks plus the phase and the jitter arrive. Under the
    old 5-block headroom they reached the cap, the leg trimmed to its target and the caught-up pops
    then ran it dry.
- **Tests** (all Tier-0 replicable):
  - `tests/setpoint_walk_1401.rs` and `tests/adaptive_target_1401.rs` were removed with the walk
    and the adaptive target (issue 1461).
  - `hub_catchup_1401.rs`: the derived caps, the 21-24 ms and 26 ms rows.
  - `egress_servo_1401.rs`: the refill rule (one reading, a period short by 1 ns, a period, a good
    reading in between, a blocked pw-cat, the prime / trim / top-up); since issue 1461 lane 2 a
    refill re-primes (the start hold until the next read), which keeps the 40 ms row's pipes free of
    trims now that the resampler's phase term, not a steep servo, would take the excess out.
  - `egress_fill_1401.rs`: the guard table with `starved`.
  - `vban_jitter_state_1401.rs` (CI only): since issue 1461 `rate_ppm`, `asrc_phase_frames` and
    `resyncs` in the facet, never a servo field.
  - A mutation pass over the walk, the refill and the caps (17 mutants) leaves no survivor. Several
    pins exist because an earlier test set let a mutant through: the band, the drift-first order
    and keeping the window at `set_target` (the lane's own pass), the cancel of the unspent walk
    share (found by review round 1), and a second's corrections staying spread across it (an
    uncapped walk plan otherwise bunched them at the 1000-frame spacing).

## Tier-0 verify (no cargo)

Three plain-rustc replicas over the real sources and the "VBAN rate" rlibs (anyhow, tracing,
intercom-vban, libc; `.claude/rules/strih-intercom.md`). The root starts with
`extern crate self as intercom_hub;`, so the real test files compile unchanged:

- **R1:** fir / mulaw / vban_rate / vban_jitter / asrc / block_clock / mix_thread / pipe_fill /
  janus_pacing / vban_io, local_audio with its serde derive stripped, and the
  test files `hub_catchup_1401`, `mix_thread_1401`, `vban_jitter_1401`, `vban_jitter_bench_1401`,
  `asrc_1461`, `asrc_leg_1461`, `egress_asrc_1461`, `egress_servo_1401` (minus its serde_json key check),
  `egress_fill_1401` (cut to its five pipe tests: the target, the guard table, FIONREAD, the two
  real-pipe sink writers; the rest needs state / matrix), under `rustc --test` and
  `clippy-driver --test -D warnings`. A lighter std-only root (issue 1461 lane 1) mounts only
  fir / block_clock / vban_jitter / asrc / pipe_fill / janus_pacing and the
  JitterBuffer section of vban_io (its `tracing::` calls pointed at a stub macro module). Set
  `CARGO_MANIFEST_DIR=<worktree>/intercom/hub` at COMPILE time for the tests that read the unit and
  main.rs. 112 tests, ~5 s at opt-level 2; at opt-level 0 (CI's debug) the 21-24 ms bench row
  takes ~6 s and the growing-gap replay ~10 s.
- **R2:** state.rs (serde stripped, its in-file tests cut) + inputs.rs over stub matrix /
  local_audio / janus_rtp / ndi_video modules. Since step 4 `vban_jitter` needs `block_clock`
  (`vban_cap_blocks` reads `CATCHUP_MAX_BLOCKS`), so the root `#[path]`-includes it too. It
  mirrors the status-line assertions of `vban_jitter_state_1401.rs` on the same three-participant
  matrix.
- **R3:** main.rs's `BlockLoop`, `run_block_loop`, `LocalAudioWiring`, the spawn + watch snippet
  and the receive task's push + target-change snippet, cut out and type-checked + clippy-linted
  against the real modules with stub `Engine`, `watch::Sender` and stats types. Since issue 1467
  the receive task is `spawn_vban_receive`: cut its `Ok((len, from)) => match classify_packet ...`
  arm and SPLICE the text into a sync `match` over a list of datagrams with python (`include!` cannot
  stand in for match arms), so the real arm runs against the real vban_io / vban_classify /
  vban_rate. Every root that mounts `vban_io.rs` also mounts `vban_classify.rs`.
- **CI only:** `vban_jitter_state_1401.rs` (serde_json + `Matrix::from_toml`) and the deployed-TOML
  tests (`vban_program_target_1401`'s two). `vban_program_target_1401`'s
  three other tests run in a one-off root the same way (minus the matrix imports). A first cut of the lost-ticks
  status-line test expected `participants=2` on a three-participant matrix and no replica compiled
  it (review 1): mirror such an assertion in R2 before pushing.
- **The granted path never runs here.** `ulimit -r` is 0 on dev1 and the GitHub runner is not root,
  so the mix-thread tests always take the refused (SCHED_OTHER) path. "Only the calling thread" is
  backed by the `sched_setscheduler(2)` man page and the live `ps -L` check above.
- The resampler tests (`asrc_1461`, `asrc_leg_1461`, the re-pinned benches) run about 19 s at
  opt-level 0 on dev1; `vban_program_target_1401`'s 10 min sample-level replay alone takes ~30 s.
- **Mutation probe (no cargo, step 4).** Copy `intercom/hub/src`, `intercom/hub/tests` and
  `systemd/intercom-hub.service` into a scratch root, apply ONE exact-string replacement to a
  source file (assert it matches once), and run the R1 script with `WT=<scratch root>`,
  `CLIPPY=0` and `TESTS=` the touched test files. A mutant that passes is a missing pin. Step 4's
  first test set let 4 of 14 through (the band, drift-first, keeping the window at `set_target`,
  the cancel of the unspent walk share), each now pinned. Two survivors are equivalent on
  256-frame blocks and are not gaps: `since_walk` starting at 0, and the walk spacing rounded
  down.
- **A per-second bound needs a SLIDING 1 s window.** A "bench second" can straddle two servo
  windows, and a budget of 7 per window does not bound a sliding second once the plan changes
  between windows: 8 walk steps fitted inside one bench second until each step got its own
  spacing. Count events by output frame and bound every span of 48 000 frames (the removed
  `setpoint_walk_1401.rs` did it with `max_of_in_any_second`; no counted per-second correction
  is left since issue 1461 lane 2).
