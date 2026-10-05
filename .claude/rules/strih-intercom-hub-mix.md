---
paths:
  - "intercom/hub/src/block_clock.rs"
  - "intercom/hub/src/mix_thread.rs"
  - "intercom/hub/src/adaptive_target.rs"
  - "intercom/hub/src/main.rs"
  - "intercom/hub/src/vban_io.rs"
  - "intercom/hub/src/inputs.rs"
  - "intercom/hub/src/state.rs"
  - "intercom/hub/src/vban_jitter.rs"
  - "intercom/hub/src/pipe_fill.rs"
  - "intercom/hub/tests/vban_jitter_state_1401.rs"
  - "intercom/hub/tests/hub_catchup_1401.rs"
  - "intercom/hub/tests/mix_thread_1401.rs"
  - "intercom/hub/tests/adaptive_target_1401.rs"
  - "intercom/hub/tests/setpoint_walk_1401.rs"
  - "systemd/intercom-hub.service"
---

# The intercom hub's real-time block loop and the adaptive program target (issue 1401)

Design 5980775411 (findings 5980766825): after the egress fixes the hub still lost blocks on every
output when its block loop woke more than a period late, and the FOH sender's gaps grew past the
fixed program-feed target. Step 4 (design 5981457044, ROZHODNUTÉ 5981448853) closed the three
residuals step 3 measured (comment 5981169929): the steep setpoint walk, the pipe refill on a
momentary low, and the cambox cap. Read this before touching the block loop in `main.rs`,
`block_clock`, `mix_thread`, `adaptive_target`, the setpoint walk or the caps in `vban_jitter`, the
refill rule in `pipe_fill`, or the program legs in `inputs` / `vban_io`. The VBAN-leg buffer itself
(prefill, the servo, `skip_missed`) is `.claude/rules/strih-intercom.md`, "VBAN legs".

## The hub-mix thread: catch-up instead of skip

Live: 18 missed ticks in one hour, in clusters of 1-5, several on the strih-lx dantesync NTP
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
  - The starved reads are a residual on the drifting cans only, and their counts are exactly the
    step-3 code's (step 4 did not add or remove one). They happen inside the write gap, before
    the hub wakes, so no refill rule can prevent them:
    - 24 ms: the FIRST stall of each seed, 60 s after the spawn, while the cans' depth is still
      being walked in (pw-cat's second read in the gap found 1005-1010 of its 1024 frames);
    - 26 ms: the stalls at 60 / 240 / 900 / 1080 s of every seed (the same read phases each
      time), the second read finding 928-1010.

    A real pw-cat then blocks in its read until the catch-up burst arrives (an xrun), it does not
    lose the audio. Why the depth sits that low at those phases was not traced; it is a candidate
    for the supervisor.
- **Residual: locks shared with SCHED_OTHER threads.** Std mutexes have no priority inheritance.
  The mix thread takes the jitter buffers' mutex (the VBAN receive task, the local capture threads
  and the Janus adapter push into it), the Janus ring's and the `out_addrs` mutex, and the watch
  channel's write lock on every publish (its readers, the status task and the HTTP handlers, only
  clone the snapshot's Arc under the read lock). A SCHED_OTHER
  holder preempted mid-section (each holds it for a few µs) holds the real-time thread until it
  runs again; a late wake from that shows as `caught_up_ticks`. The cycle's own cost was not
  measured on the box.

## The adaptive program target

The FOH sender's stalls grew in one day: the largest gap was 19.4 ms in the morning, 27.6 ms with
31 gaps over 20 ms in 30 s at 13:56. The fixed 32 ms target underran again about 1.7 times a
minute.

- `adaptive_target::AdaptiveTarget` (pure, std-only) tracks the largest inter-arrival gap of the
  last 10 min, kept as 60 buckets of 10 s (the window is 590-600 s). Only gaps inside a running
  stream count, never the silence before a stalled (> 500 ms) stream came back (pinned: a mutant
  without that guard passed everything else).
- Target = `program_target_blocks(gap)` = (gap + one block + `PROGRAM_HALF_BURST_FRAMES` 144)
  rounded UP to whole blocks, clamped to 6..12 blocks (32-64 ms). Half a burst is half the FOH
  sender's ~6 ms burst period: just before a burst the fill sits about that far under its mean.
  19.4 ms -> 6 blocks, 27.6 ms -> 7 (37.3 ms), 35 ms -> 9 (48 ms).
- A larger gap raises it at once, in the push that measured the gap: when that gap ran the leg dry,
  the re-prime already goes to the new target. After 10 min without a gap that needs the current
  target it comes down ONE block, then one more every 10 min: back at the floor 30 min after a
  35 ms gap.
- A change goes through `NetworkFill::set_target`. The cap moves with the target (it stays
  `vban_cap_blocks(target)`, 6 blocks of headroom), nothing is dropped or padded, and the servo
  walks its setpoint to the new target GENTLY (step 4, below). The servo's current window and its
  drift budget stay: they were measured against the setpoint, which did not move.
- **Walk speed (step 4).** At most `SERVO_WALK_MAX_PER_WINDOW` = 7 corrections a second, the
  gentle zone's own ceiling: a one-block change takes ~37 s. Step 3 handed the servo the whole
  256-frame error, its STEEP zone: up to 48 a second for ~5 s per block (47 measured in the
  replay), the flutter review 1 of the VBAN-leg servo rejected. How it works: see "Step 4" below.
- One info line per change (`program feed target raised` / `lowered`, the leg, from and to frames,
  the 10 min max gap), written by the receive task after it released the jitter lock.
- `/api/state` jitter: the live `target_frames`, and for a program feed `max_gap_ms_10min` (0.1 ms
  resolution); a cambox keeps its fixed 768 and omits the gap.
- The replay (`tests/adaptive_target_1401.rs`) plays the measured fohabl pattern (6 ms bursts, a
  long gap every 402 ms) through the real buffer:
  - the long gap growing 19 -> 35 ms over 16 min: 0 underruns, raises at 25 / 31 / 35 ms (7 / 8 / 9
    blocks), back at 6 blocks 30 min after the last 35 ms gap, corrections never closer than 1000
    frames, at most 8 in one second (pinned `<= SERVO_WALK_MAX_PER_WINDOW + 1`): 188 walk seconds
    carry the walk's 7, 16 carry one more. That one is the drift servo's own: a grown gap lowers
    the 1 s mean fill a little, and the gap growth is what raised the target, so the two always
    meet. On the same pattern at a FIXED gap a walk never exceeds 7 (`setpoint_walk_1401.rs`);
  - a sudden 19 -> 35 ms jump: exactly one underrun, at the first 35 ms gap (pinned `== 1`), and the
    re-prime goes straight to 9 blocks.

## Reading it on strih-lx after a hub deploy (the supervisor)

- Step 3 changed the base unit, so on a box still on a pre-step-3 unit reinstall it before the
  restart: `install -Dm644 systemd/intercom-hub.service /etc/systemd/system/intercom-hub.service`
  (or setup-strih step 13) + `systemctl daemon-reload`. Then `systemctl show intercom-hub -p
  LimitRTPRIO` must print `LimitRTPRIO=10`. Step 4 changes the binary only (no unit, no config).
- `curl -s http://strih-lx:8790/api/state | jq -r .mix_thread_sched` -> `SCHED_FIFO 10`.
- On the box: `ps -L -o tid,cls,rtprio,comm -p "$(pidof intercom-hub)"` -> the `hub-mix` row
  `FF 10`, every other row `TS -` (the tokio workers, `janus-paced-tx`, the local-audio sink
  threads, `interkom-video` / `ndir:*`). Or `chrt -p <hub-mix tid>` -> `SCHED_FIFO`, priority 10.
  The journal at start: `hub-mix: the block loop runs SCHED_FIFO 10`, never the `refused` warning.
- The 13:58 stopgap (`chrt -f -p 10` on 22 hub threads, comment 5980789088) ends with that restart:
  the tokio workers are SCHED_OTHER again by design.
- Over >= 1 h: `lost_ticks` 0, `caught_up_ticks` growing only with the box's hiccups, `fohabl`
  underruns flat, its `target_frames` matching `max_gap_ms_10min` by the rule above, both pipes'
  `pipe_refills` flat.
- After a program target change (the `program feed target raised` / `lowered` journal line): that
  leg's `depth_frames` walks to the new `target_frames` over ~37 s a block, and its
  `servo_drops` / `servo_repeats` grow by about 256 a block at no more than ~7 a second (8 in an
  occasional second). A jump of tens within a second is the old steep walk: a finding.
- A `pipe ran low — topped up with silence` warn line now means the pipe read under one block for
  a whole hub period (a pw-cat starving), never a momentary low in a catch-up gap.

## Step 4: the step-3 residuals (design 5981457044, 4.10.2026)

Each fix sits at the policy point that owns the behaviour; there is still one servo, one pipe
guard and one cap rule.

- **The setpoint walk (`vban_jitter::NetworkFill`).**
  - The servo holds a `setpoint` (`NetworkFill::setpoint()`). `set_target` moves the target and
    the cap but leaves the setpoint where it is (a leg that is still priming takes the new target
    at once).
  - Every 1 s window: drift = `servo_corrections(|fill - setpoint|)` with its full budget, plus a
    walk of at most `SERVO_WALK_MAX_PER_WINDOW` (7, derived from the knee) toward the target, only
    what the 1 ms/s spacing leaves after the drift. Both always point the same way.
  - The drift's share of the second is spent first. Each walk correction done moves the setpoint
    one frame. So the walk can never take the drift's budget (spending the walk first let the
    -200 ppm drift error grow 133 -> 136), and under a heavy drift the walk slows instead: at
    -200 ppm ~17 are planned and ~16 fit on 256-frame blocks, so one block takes 45 s.
  - The fill is judged against the window's MEAN setpoint, carried to its end point: a walk moves
    both during the window, and against the end point the walk's own progress reads as a ~3.5-frame
    error.
  - A fill more than the band (16 frames) past the setpoint toward the target has already done
    that part of the walk: the setpoint moves to the band behind it, with no correction. That is a
    sender drifting that way (+200 ppm: a raise done in 11 s, the sender carries the fill) or a
    lost tick's give-up during a lowering. Without it the drift would pull the fill back against
    the walk at up to 40 a second.
  - Inside the band the setpoint stays. On a bursty sender the 1 s mean wobbles by ±10 frames (a
    window holds two or three long gaps); a setpoint that followed that wobble forward left the
    low windows behind the band and added a drift correction to every other walk second (8).
  - A prime, an underrun's re-prime and an overrun trim put the fill exactly at the target and
    end the walk there.
  - `set_target` keeps the window and its drift budget and drops only an unspent walk share.
    Restarting the window paused a slow sender's repeats for a whole second.
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
  - That gives `VBAN_CAP_BLOCKS` 9 (48 ms) and `VBAN_PROGRAM_CAP_BLOCKS` 12 at the floor, never a
    typed number. `set_target` keeps that 6-block headroom at every adaptive target.
  - The pop before a stall leaves a leg about one block under its target, and while the loop is
    up to four ticks late up to five more blocks plus the phase and the jitter arrive. Under the
    old 5-block headroom they reached the cap, the leg trimmed to its target and the caught-up pops
    then ran it dry.
- **Tests** (all Tier-0 replicable):
  - `tests/setpoint_walk_1401.rs` (new). It counts every correction by its output frame and bounds
    them in EVERY 1 s span, not in a bench second that may straddle two servo windows. It covers:
    - a raise and a lowering on a clean sender (<= 7, exactly one block, 36-40 s);
    - the FOH burst pattern at a fixed 19.4 / 25 / 27.6 ms gap (<= 7);
    - the give-up carried by the setpoint (no correction against the walk);
    - +-200 ppm (depth within 147 of the setpoint, the drift error within 2 frames of its
      pre-raise equilibrium, no pause in a slow sender's repeats at the raise);
    - `set_target` / prime / trim / re-prime.
  - `hub_catchup_1401.rs`: the derived caps, the 21-24 ms and 26 ms rows.
  - `egress_servo_1401.rs`: the refill rule (one reading, a period short by 1 ns, a period, a good
    reading in between, a blocked pw-cat, the prime / trim / top-up).
  - `egress_fill_1401.rs`: the guard table with `starved`.
  - `adaptive_target_1401.rs`: the replays' walk bound.
  - A mutation pass over the walk, the refill and the caps (14 mutants) left no survivor. Three of
    the pins above exist because the first test set let a mutant through: the band, the drift-first
    order, and keeping the window at `set_target`.

## Tier-0 verify (no cargo)

Three plain-rustc replicas over the real sources and the "VBAN rate" rlibs (anyhow, tracing,
intercom-vban, libc; `.claude/rules/strih-intercom.md`). The root starts with
`extern crate self as intercom_hub;`, so the real test files compile unchanged:

- **R1:** fir / mulaw / vban_rate / vban_jitter / adaptive_target / block_clock / mix_thread /
  pipe_fill / janus_pacing / vban_io, local_audio with its serde derive stripped, and the test
  files `hub_catchup_1401`, `mix_thread_1401`, `adaptive_target_1401` (minus its deployed-TOML
  test and the matrix / inputs imports; keep its `VBAN_TARGET_BLOCKS` import, the cambox leg uses
  it), `vban_jitter_1401`, `vban_jitter_bench_1401`, `egress_servo_1401` (minus its serde_json key
  check), `egress_fill_1401` (cut to its five pipe tests: the target, the guard table, FIONREAD,
  the two real-pipe sink writers; the rest needs state / matrix) and `setpoint_walk_1401`, under
  `rustc --test` and `clippy-driver --test -D warnings`. Set
  `CARGO_MANIFEST_DIR=<worktree>/intercom/hub` at COMPILE time for the tests that read the unit and
  main.rs. 109 tests, ~4 s at opt-level 2; at opt-level 0 (CI's debug) the 21-24 ms bench row
  takes ~6 s and the growing-gap replay ~10 s.
- **R2:** state.rs (serde stripped, its in-file tests cut) + inputs.rs over stub matrix /
  local_audio / janus_rtp / ndi_video modules. Since step 4 `vban_jitter` needs `block_clock`
  (`vban_cap_blocks` reads `CATCHUP_MAX_BLOCKS`), so the root `#[path]`-includes it too. It
  mirrors the status-line assertions of `vban_jitter_state_1401.rs` on the same three-participant
  matrix.
- **R3:** main.rs's `BlockLoop`, `run_block_loop`, `LocalAudioWiring`, the spawn + watch snippet
  and the receive task's push + target-change snippet, cut out and type-checked + clippy-linted
  against the real modules with stub `Engine`, `watch::Sender` and stats types.
- **CI only:** `vban_jitter_state_1401.rs` (serde_json + `Matrix::from_toml`) and the deployed-TOML
  tests (`adaptive_target_1401`'s and `vban_program_target_1401`'s). `vban_program_target_1401`'s
  three other tests run in a one-off root the same way (minus the matrix imports). A first cut of the lost-ticks
  status-line test expected `participants=2` on a three-participant matrix and no replica compiled
  it (review 1): mirror such an assertion in R2 before pushing.
- **The granted path never runs here.** `ulimit -r` is 0 on dev1 and the GitHub runner is not root,
  so the mix-thread tests always take the refused (SCHED_OTHER) path. "Only the calling thread" is
  backed by the `sched_setscheduler(2)` man page and the live `ps -L` check above.
- The 51 min replay runs about 8.6 s in a debug build on a loaded dev1.
