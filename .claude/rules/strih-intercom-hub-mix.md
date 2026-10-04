---
paths:
  - "intercom/hub/src/block_clock.rs"
  - "intercom/hub/src/mix_thread.rs"
  - "intercom/hub/src/adaptive_target.rs"
  - "intercom/hub/src/main.rs"
  - "intercom/hub/src/vban_io.rs"
  - "intercom/hub/src/inputs.rs"
  - "intercom/hub/tests/hub_catchup_1401.rs"
  - "intercom/hub/tests/mix_thread_1401.rs"
  - "intercom/hub/tests/adaptive_target_1401.rs"
  - "systemd/intercom-hub.service"
---

# The intercom hub's real-time block loop and the adaptive program target (issue 1401)

Design 5980775411 (findings 5980766825): after the egress fixes the hub still lost blocks on every
output when its block loop woke more than a period late, and the FOH sender's gaps grew past the
fixed program-feed target. Read this before touching the block loop in `main.rs`, `block_clock`,
`mix_thread`, `adaptive_target`, or the program legs in `inputs` / `vban_io`. The VBAN-leg buffer
itself (prefill, the servo, `skip_missed`) is `.claude/rules/strih-intercom.md`, "VBAN legs".

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
- **What the real-time thread does NOT do.** It never writes the status line (a tokio task logs it
  from the watch channel) and never logs a program target change (the receive task logs it after
  releasing the jitter lock, `JitterBuffer::take_target_change` / `TargetChange::log`). It still
  logs a lost-tick warning at most once a second and an unresolved-output warning once a second.
- A mix thread that ends (a panic) stops the daemon (the `hub-mix-watch` thread exits 1,
  `Restart=on-failure`) instead of leaving every output silent behind a frozen `/api/state`.
- **The bench numbers** (`tests/hub_catchup_1401.rs`: a +20 ppm cambox leg, the FOH leg at
  -20 ppm, 1 ms jitter, the program pipe on the hub's clock, the cans pipe at +50 ppm; one stall
  every 60 s, 19 stalls, 3 seeds; issue comment 5981169929):
  - up to 18 ms: 0 lost, 0 underruns, 0 overruns, 0 pipe refills (the design's 15 ms case is
    pinned);
  - 21-24 ms (3-4 ticks run late, 0 lost): the legs stay clean, but the cans pipe refills in 3-5 of
    the 19 stalls. When two pw-cat quantum reads fall inside the write gap, the pipe drops under one
    block and the guard tops it up with silence. The program pipe never hit that phase;
  - 26 ms (4 run late, 0 lost): a cambox leg overruns its cap while the loop is late (the arrivals
    pass its 5 blocks of headroom), then underruns, in 6-9 of 19;
  - 30-40 ms (1-3 lost per stall): each stall one cambox underrun + overrun, one FOH overrun trim,
    one refill per pipe; bounded and counted (pinned for 40 ms).

  So the design's "a <= 21 ms late burst plays complete" holds for the VBAN legs to about 24 ms,
  while a pw-cat pipe can refill from about 16 ms in an unlucky read phase (a residual for the
  main, on the ticket).
- **Residual: locks shared with SCHED_OTHER threads.** Std mutexes have no priority inheritance.
  The mix thread takes the jitter buffers' mutex (the VBAN receive task, the local capture threads
  and the Janus adapter push into it), the Janus ring's and the `out_addrs` mutex. A SCHED_OTHER
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
- A change goes through `NetworkFill::set_target`. The cap moves with the target (5 blocks of
  headroom), nothing is dropped or padded, the servo's current window and pending corrections are
  forgotten (they were measured against the old target), and the servo walks the fill there.
- **Walk speed.** A one-block step is a 256-frame error, the servo's STEEP zone, so it is walked at
  the servo's full budget: up to 48 corrections a second for about 5 s per block (47/s measured in
  the replay). The design accepted "walked by the servo"; it is the rate review 1 of the VBAN-leg
  servo called a flutter when it was a steady state. A residual for the main, on the ticket.
- One info line per change (`program feed target raised` / `lowered`, the leg, from and to frames,
  the 10 min max gap), written by the receive task after it released the jitter lock.
- `/api/state` jitter: the live `target_frames`, and for a program feed `max_gap_ms_10min` (0.1 ms
  resolution); a cambox keeps its fixed 768 and omits the gap.
- The replay (`tests/adaptive_target_1401.rs`) plays the measured fohabl pattern (6 ms bursts, a
  long gap every 402 ms) through the real buffer:
  - the long gap growing 19 -> 35 ms over 16 min: 0 underruns, raises at 25 / 31 / 35 ms (7 / 8 / 9
    blocks), back at 6 blocks 30 min after the last 35 ms gap, corrections never closer than 1000
    frames;
  - a sudden 19 -> 35 ms jump: exactly one underrun, at the first 35 ms gap (pinned `== 1`), and the
    re-prime goes straight to 9 blocks.

## Reading it on strih-lx after a hub deploy (the supervisor)

- The base unit changed, so reinstall it before the restart: `install -Dm644
  systemd/intercom-hub.service /etc/systemd/system/intercom-hub.service` (or setup-strih step 13)
  + `systemctl daemon-reload`. Then `systemctl show intercom-hub -p LimitRTPRIO` must print
  `LimitRTPRIO=10`.
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

## Tier-0 verify (no cargo)

Three plain-rustc replicas over the real sources and the "VBAN rate" rlibs (anyhow, tracing,
intercom-vban, libc; `.claude/rules/strih-intercom.md`). The root starts with
`extern crate self as intercom_hub;`, so the real test files compile unchanged:

- **R1:** fir / mulaw / vban_rate / vban_jitter / adaptive_target / block_clock / mix_thread /
  pipe_fill / janus_pacing / vban_io, local_audio with its serde derive stripped, and the test
  files `hub_catchup_1401`, `mix_thread_1401`, `adaptive_target_1401` (minus its deployed-TOML
  test), `vban_jitter_1401`, `vban_jitter_bench_1401`, `egress_servo_1401` (minus its serde_json key
  check), under `rustc --test` and `clippy-driver --test -D warnings`. Set
  `CARGO_MANIFEST_DIR=<worktree>/intercom/hub` at COMPILE time for the tests that read the unit and
  main.rs.
- **R2:** state.rs (serde stripped, its in-file tests cut) + inputs.rs over stub matrix /
  local_audio / janus_rtp / ndi_video modules. It mirrors the status-line assertions of
  `vban_jitter_state_1401.rs` on the same three-participant matrix.
- **R3:** main.rs's `BlockLoop`, `run_block_loop`, `LocalAudioWiring`, the spawn + watch snippet
  and the receive task's push + target-change snippet, cut out and type-checked + clippy-linted
  against the real modules with stub `Engine`, `watch::Sender` and stats types.
- **CI only:** `vban_jitter_state_1401.rs` (serde_json + `Matrix::from_toml`) and the deployed-TOML
  tests (`adaptive_target_1401`'s and `vban_program_target_1401`'s). A first cut of the lost-ticks
  status-line test expected `participants=2` on a three-participant matrix and no replica compiled
  it (review 1): mirror such an assertion in R2 before pushing.
- **The granted path never runs here.** `ulimit -r` is 0 on dev1 and the GitHub runner is not root,
  so the mix-thread tests always take the refused (SCHED_OTHER) path. "Only the calling thread" is
  backed by the `sched_setscheduler(2)` man page and the live `ps -L` check above.
- The 51 min replay runs about 8.6 s in a debug build on a loaded dev1.
