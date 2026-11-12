---
paths:
  - "hub/src/dlink_leg.rs"
  - "hub/tests/dlink_5.rs"
  - "hub/src/inputs.rs"
  - "intercom.strih-snv.toml"
  - "hub/src/dante_guard.rs"
  - "hub/src/pktinfo.rs"
  - "hub/src/http_serve.rs"
  - "hub/tests/dante_guard_25.rs"
---

# The hub's own Dante device: dstack `dlink` in process (intercom#5)

Owner ruling (dstack#20, 1.11.2026; dstack#68, 2.11.2026): every app carries its own Dante device;
no relaying between apps. The embeddable part is dstack's `dlink` crate: dlink's default budget, 15 ms since v0.6.28
(the whole path each way; owner 4.11.2026: robustness over low latency), normal priority, no pinning, Dante PTP followed in process (UDP 319, the unit's one
capability) with the dsync service as fallback (a loopback UDP poll, works under the sandbox). strih-snv only; strih-pp has no Dante network (owner 2.11.2026). Since the cut-over
(3.11.2026, FOH turned its VBAN `fohabl-strih` off) the Dante device is the FOH program feed.

## The pieces

- **Config:** the optional `[dlink]` table (`dlink_leg::DlinkConfig`, `deny_unknown_fields`):
  `name`, `iface`, `rx` (channel names), `latency_ms` (default `dlink::BUDGET_NS`, 15 ms; since dlink v0.6.22 the BUDGET, the
  whole path each way, fed to `Config::budget_ns`; the Dante latency is dlink's `split` of it, dlink's crate docs have
  the table of splits; `/api/state` `latency_ms` is that budget), `state_file`
  (`/var/lib/intercom-hub/dlink.toml`), and `arc_port` / `cmc_port` / `flows_port` (all three or
  none), `process_id`, `rx_port_base` (32 ports from it; keep them below Linux's ephemeral 32768-60999, a socket the OS numbers can take one there and the device never starts: strih-snv 30100-30131, OBS 30000-30031). `Matrix::from_toml` runs dstack's own
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
- **No CPU pinning (owner, dlink v0.6.28):** dstack removed `Config::pump_cpu` / `io_cpu` /
  `rx_cpu` ("no CPU pinning for dlink apps"); hub 1.9.14's `[dlink] pump_cpu` / `rx_cpu` keys and
  hub-mix's reserved-CPU exclusion went with them (1.9.15), and camera-box dropped its pinner. A
  config still naming them fails the load (`deny_unknown_fields`). Do not re-add pinning: a pump
  losing blocks under load is a dstack matter.
- **W of an RX-only device = the whole budget (dlink v0.6.32):** the hub is RX-only, so its pump
  margin is `dlink::rx_work_margin` (1440 samples at 96 kHz, 15 ms), not 2/5 of the budget (576);
  P (13041666 ns) and the RX path are unchanged. A late pump catches up instead of re-bootstrapping.
  The pump WARN's `max_process_us` is that second's max.
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

**Never pin dstack v0.6.98 (12.11.2026).** Its `Link::start` waits with no limit for a missing
iface (dstack#225) and ignores the hub's stop flag, so `leg.stop()`'s join hangs a hub shutdown
until systemd's SIGKILL. dstack#230 restores the fail-at-once start (the `supervise` loop's
stop-aware backoff stays the only wait) and adds `Link::start_until(config, &AtomicBool)`; take
the tag after v0.6.98, which dstack announces over peer. On any dstack move, check that a missing
iface still lets the hub stop within `dlink::STOP_WAIT`.

## The other sockets stay off the Dante leg (intercom#25, dstack two-networks.md §4)

dstack owns the one standard for a PC with a Dante leg (`docs/two-networks.md`). For an app that is a
Dante device and a LAN service, its ruling on intercom#25 is **option B**: listen on all addresses and
drop at once whatever arrives on the Dante leg. That way a LAN address that changes needs no rebind,
and the box's nginx keeps `127.0.0.1:8790`.

- **`dante_guard::DanteGuard` is the one decision point.** The Dante leg is the union of two sets:
  - the dlink `iface`: its ifindex, every IPv4 on it and their subnets, re-resolved every 2 s;
  - `DANTE_SUBNET`, §4's per-app setting (env, CIDR, comma list). The site's subnet is never in
    this public repo.

  Neither set means no filter (strih-pp). A malformed `DANTE_SUBNET` stops the hub at start. The
  iface part counts only once the interface exists. strih-snv's NM VLAN is up before the hub
  starts; anywhere else, set `DANTE_SUBNET` for cover from the first second.
- **TCP:** `http_serve::serve` is the hub's own accept loop. axum 0.7's `serve` has no accept hook:
  its `make_service` future is `Infallible`. The loop drops a stream whose local **or peer**
  address is Dante before reading a byte. Linux accepts a packet for any local address on any
  interface, so a Dante host that dials the LAN address over the Dante leg is caught by its own
  address. The rest is served through hyper-util `auto` with upgrades and graceful shutdown, and
  the WebSocket, h2c and shutdown tests pin that. Never go back to `axum::serve` (also pinned).
- **UDP:** VBAN and Janus RTP enable `IP_PKTINFO` with `pktinfo::enable_guarded`, which is fatal
  while the guard is active. They read only through `pktinfo::recv_admitted`, a `recvmsg` loop on
  tokio's `async_io` that drops a refused datagram before the caller sees it. A datagram is refused
  when its ingress ifindex is the dlink iface or its destination or source is Dante. An active
  guard refuses a datagram without packet info (a truncated control message, warned once).
- **IPv4 binds only while the guard is active** (`check_bind`). Linux attaches `IP_PKTINFO` to
  IPv4-mapped datagrams on a dual-stack socket, but native IPv6 on the Dante iface could not be
  told apart.
- **Refusals** are counted per local port in lock-free slots, so the SCHED_FIFO hub-mix thread
  reads them once a second without waiting. The first refusal on each port is a warning, and the
  counts appear on `/api/state` as `dante_refusals` (omitted while empty). On a box whose nft
  already blocks the leg they stay empty; a count means that line was missing.
- **Announcements:** a `bind` / `vban_bind` / `rtp_bind` inside the Dante leg fails at start
  (`check_bind`; `rtp_bind`'s IP is what Janus is told). NDI `connect` skips a source whose
  `p_url_address` is Dante and keeps looking. Janus's own ICE candidates come from its core config,
  which belongs to camera-box: camera-box#1499.
- Tests: `hub/tests/dante_guard_25.rs`. It drives the real `IP_PKTINFO` on loopback (`lo` as the
  dlink iface) and the real accept loop (`DANTE_SUBNET` 127.0.0.0/8: closed with no byte; another
  subnet: 200).
