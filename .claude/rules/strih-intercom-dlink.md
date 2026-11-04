---
paths:
  - "hub/src/dlink_leg.rs"
  - "hub/tests/dlink_5.rs"
  - "hub/tests/dlink_cpus_5.rs"
  - "hub/src/mix_thread.rs"
  - "hub/src/inputs.rs"
  - "intercom.strih-snv.toml"
---

# The hub's own Dante device: dstack `dlink` in process (intercom#5)

Owner ruling (dstack#20, 1.11.2026; dstack#68, 2.11.2026): every app carries its own Dante device;
no relaying between apps. The embeddable part is dstack's `dlink` crate: a 10 ms budget (the whole
path each way), normal priority, no pinning, Dante PTP followed in process (UDP 319, the unit's one
capability) with the dsync service as fallback (a loopback UDP poll, works under the sandbox). strih-snv only; strih-pp has no Dante network (owner 2.11.2026). Since the cut-over
(3.11.2026, FOH turned its VBAN `fohabl-strih` off) the Dante device is the FOH program feed.

## The pieces

- **Config:** the optional `[dlink]` table (`dlink_leg::DlinkConfig`, `deny_unknown_fields`):
  `name`, `iface`, `rx` (channel names), `latency_ms` (10: since dlink v0.6.22 the BUDGET, the
  whole path each way, fed to `Config::budget_ns`; the Dante latency is dlink's `split` of it, dlink's crate docs have
  the table of splits; `/api/state` `latency_ms` is that budget), `state_file`
  (`/var/lib/intercom-hub/dlink.toml`), and `arc_port` / `cmc_port` / `flows_port` (all three or
  none), `process_id`, `rx_port_base` (32 ports from it). `Matrix::from_toml` runs dstack's own
  `Config::device()` checks, so a bad table fails the load. Exactly one `adapter = "dlink"`
  participant iff the table; input only, `in_channels` 1..=rx.len().
- **Declared subscriptions** (dlink v0.5.12): `[[dlink.subscribe]]` with `rx` (our RX channel by
  its NAME in `rx`; the hub turns it into dstack's 1-based number), `tx_channel`, `tx_device`
  (Dante's names). An unknown or repeated `rx` fails the load. At start an RX channel the
  state_file holds no subscription for takes the declared one; one Dante Controller sets is saved
  and wins. Declared ones are never saved (v0.5.12; v0.5.11 saved them with the next save, so a
  TOML edit stopped reaching the channel); a Controller change of the channel clears the mark.
- **strih-snv's mapping** (dasio 3.11.2026, permanent; the owner: no return to VB-Matrix): the
  two Abletons' own Dante devices on the FOH PC (10.77.7.30, 96 kHz): `master-asio-dante`
  `Repro 0db L` / `Repro 0db R`, `band-asio-dante` `Click 3doba` / `Guide 2 iem`. A TX rename on
  the FOH side must follow in the converter's `DLINK`. History: 2.11. the Yamaha card
  (`foh-abl-yamaha-accelerator` 07/08/09/32) via FOH VB-Matrix, silent since the move.
- **A running flow of zeros is a SOURCE question, not ours:** 1.9.6 had status 9 and a granted
  flow from the card, yet -120 dBFS; the wire carried zero audio bytes because FOH had moved
  Ableton off the card. Ask dstack/dasio what feeds the TX channel now before touching the hub.
- **`/api/state` `dlink.rx[]` (dlink v0.6.3 `Link::rx_channels`, read every 200 ms):** per RX
  channel `rx`, `name`, `tx_channel`, `tx_device`, `status` (Dante's label), `state`
  (`connected`/`pending`/`error`/`idle`, absent = not subscribed), `flow`, `packets`, `written`.
  `written` standing still while `connected` = a lost flow (journal WARN `connected but no samples
  — the flow is lost` after `RX_STALL` 1 s, `RxWatch`; each new state restarts that timer, so a
  reconnect, or the start, gets 1 s for its first samples); growing `written` over -120 dBFS = a quiet source. Each
  state change logs once (an `error` at warn). Two transmitters on one address must show two
  `flow` numbers (v0.6.2 keys a transmitter by its flow-control address and port; v0.5.x put
  master-asio-dante and band-asio-dante into ONE flow).
- **`blocks` / `running` / `ends 0` never prove a flow.** dlink's pump runs on the media clock with
  or without an RX flow, so a lost flow reads as a running device at -120 dBFS (1.9.7, 3.11.2026:
  audio 03:16:04-03:17:51, then TX_FAIL with the pump still counting; called a PASS by mistake).
  Audio is proven only by dstack reading RX status 9 from the Dante side AND a level above -120
  held over time; a level that drops to exactly -120 mid-music is a flow question first.
- **The network rate (dlink v0.5.20, `network_rate`):** a Dante flow needs both ends at ONE rate.
  The FOH Dante sources run 96 kHz, so a device at the hub's 48 kHz showed every subscription as
  status 21 (TX_FAIL; v0.5.20 reads 16, format mismatch, with a warning naming both rates) and no
  RX flow: `-120 dBFS` on `foh_dante` with a healthy pump and 0 drops. Silence plus a running
  device is NOT a quiet FOH: ask dstack to read the RX status from the Dante side (dev1 is not on
  VLAN 70). `network_rate = 96000` runs the device at 96 kHz and dlink converts to the hub's 48 kHz
  (v0.5.20: a 2:1 half-band step, 0.16 ms each way, 1x/2x only; v0.6: `Rated`, any Dante rate,
  `Config::converts()` replaced `halved()`).
- **The cut-over (converter `DANTE_REPLACES = "fohabl"`):** every fohabl point (cutters -8/-10 dB,
  speakers) reads `foh_dante`, same channel/output/gain; `rx` order = fohabl-strih's channel
  order. The VBAN fohabl leg stays declared (4 channels) and routed nowhere. lv1/mbc stay VBAN.
- **Growing `rx` over a saved state:** dstack keeps the saved names for the old channels and adds
  the config's for the new, compared case-insensitively; a new name equal to an old one refuses the
  state and the device never starts (1.9.3's `Click` vs `CLICK`). The old channels also keep their
  saved NAMES (Controller shows them until renamed); routing is by number, so audio is unaffected. Pinned by
  `strih_snv_s_device_loads_over_the_phase_1_state_file`. Which source feeds Master/Click is the
  owner's call, and the TX names belong to that device: dstack never picks them.
- **Two devices on one address** (the hub + OBS's strih-obs) need distinct ARC/CMC/flows ports and
  process ids; the info port 8700 is shared by dstack itself. camera-box's nft names the RX range.
- **The crossing:** dlink's pump calls `RxProcessor::process` once per device block, converted to
  the hub's rate by `Rated` with `network_rate` (dstack rule I7: no alloc, lock, syscall or log). It only copies RX as i16 into the lock-free `SampleRing`. The
  `dlink-rx` thread drains whole hub blocks into the participant's `JitterBuffer::card_capture`
  leg with `push_at` (the card leg's path), so the issue-1461 resampler crosses the clock. Never
  push into the JitterBuffer from `process` (it locks and allocates).
- **Supervision:** the `dlink` thread starts the `Link`, polls `ended()` every 200 ms, restarts
  with `restart_backoff`; a start failure logs once per distinct reason. `main` stops the leg after
  the HTTP server's graceful shutdown so the device sends its goodbyes.
- **The clock: the in-process PTP follower, the dsync service only as fallback (owner, 4.11.2026
  10:26, verbatim on dstack#1).** Every dstack device follows Dante PTP in process (dsync-ptp) like
  DVS. The follower binds UDP 319, so the unit grants `AmbientCapabilities=CAP_NET_BIND_SERVICE`
  (ambient: holds for setup-strih's non-root `User=`) with `CapabilityBoundingSet` of that one cap.
  camera-box owns the box side: nft lets UDP 319/320 in, verify-strih item 16 expects the cap.
  Healthy log: the LAST dstack `media clock source` line reads `source=Internal` (dsync-ptp; camera-box
  verify item 16 grades it); `source=Service` plus "port 319
  needs CAP_NET_BIND_SERVICE" means the grant or the firewall is missing. History: a rig rule
  relayed at 10:19 the same day said the opposite; the owner's later word replaced it. A relayed
  rule that contradicts a verbatim owner decision: find the decision's ticket before changing the
  unit. Pinned by `the_unit_lets_the_ptp_follower_bind_port_319_and_grants_no_other_capability`.
- **The device's CPUs (dlink v0.6.26, camera-box#1485):** optional `[dlink] pump_cpu` / `rx_cpu`
  pin `dstack-engine` / `dstack-rx` (unset = anywhere; `io_cpu` is not a hub key, the device is
  RX-only). dstack's `Config::device()` refuses a CPU past `MAX_CPU` at load; a CPU the kernel
  refuses at run time only WARNs `not pinned` from dstack. camera-box writes the value (strih-snv
  14) from its facts and keeps nice -10 with its pinner. Live 4.11.2026: unpinned at load ~11 the
  pump was preempted ~3.5 ms past its 3.16 ms margin (WARN ~30 / 3 min, rebootstraps missed=4/5);
  alone on a cleared CPU 0. `hub-mix` (SCHED_FIFO 10) takes those CPUs out of ITS OWN mask before
  its FIFO request (`DlinkConfig::reserved_cpus` -> `spawn_mix_thread(&reserved_cpus, ..)`,
  `mix_thread::plan_affinity`): a FIFO thread on the pump's CPU preempts it. One journal line
  `hub-mix: runs on CPUs [..], off the Dante device's reserved CPUs [..]`; a mask that would be
  empty is kept with a WARN. Check on the box: `taskset -pc <tid>` of `hub-mix` / `dstack-engine`
  / `dstack-rx` (`ps -L -o tid,psr,comm`). Tests: `hub/tests/dlink_cpus_5.rs`.
- **The unit:** `StateDirectory=intercom-hub` (ProtectSystem=strict leaves nothing else writable).
- **`/api/state`:** `participants[].dlink` on the dlink participant only: device, iface,
  latency_ms, rx_channels, running, starts, ends, blocks, ring_frames, ring_dropped_frames. The
  participant's `jitter` facet is the resampler leg's as on the card capture.

## Tests

`hub/tests/dlink_5.rs`: the ring (order, wrap, drops), `to_i16`, the processor, the drain, every
matrix refusal, the ports reaching `DeviceConfig`, the facet, the unit, and one REAL dlink device
on `Clock::System` hearing its own test TX tone through `RxProcessor`, subscribed only by the
TOML's `[[dlink.subscribe]]` (no seeded state file; dstack's own dlink test
shape; one device per test file, the timing profile is process-global). The test needs `dstack`
as a dev-dependency at the SAME tag as `dlink` (one dstack in the lockfile).

## License

`intercom-hub` is GPL-3.0-or-later since intercom#5 (owner 2.11.2026): it links dstack's GPL-3.0
`dlink`. `intercom-vban` stays MIT; camera-box links only that crate, never the hub.

## Moving the dstack version

Both `dlink` and the `dstack` dev-dependency move together in `hub/Cargo.toml`; then
`cargo update -p dstack`. A Dante need or bug goes to dstack over peer; intercom only moves the tag.
Read dstack's CHANGELOG between the two tags first: a `Port:` line names an API that moved
(v0.6.22: `Config::latency_ns` gone, `budget_ns` replaces it) and a test that pinned the old value.
Then run the 96k test under CPU contention before pushing: CI's Coverage runner is slow enough to
expose a pump-margin regression a quiet dev box hides (v0.6.22/v0.6.23 shrank W and lost blocks;
fixed in v0.6.24). Recipe: pin the test and two shell busy loops (`while :; do :; done`, killed by
PID, never `pkill -f`) to one core with `taskset -c 0`, then
`taskset -c 0 cargo test -p intercom-hub --test dlink_5 a_96k_transmitter`, three times.
A failure prints both devices' `Link::status()` JSON: read `engine` (`overruns`, `rebootstraps`,
`missed`, `max_process_us`), `rx[].gaps` and `tx[].holes` to tell which side lost the tone. Since v0.6.25 the journal also carries a 1/s WARN `engine pump lost or overran blocks this second` (deltas, `max_process_us`, B/W/P/offset): a tone gap in that second is the pump, one without it is the flow.
**Keep `[profile.dev.package.rubato]` and `[profile.dev.package.dstack-resample]` at `opt-level = 3`** in the workspace `Cargo.toml` (dlink docs, v0.6.25): unoptimised,
rubato (dlink's converter, under dstack-resample) took 2.3-3.3 ms per 1.33 ms 96 kHz block, so every
debug/llvm-cov block overran and the pump kept restarting (4.11.2026; dstack and dlink at opt 3 did
not help, rubato alone did). Release builds read 0 overruns either way.
