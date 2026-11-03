---
paths:
  - "hub/src/dlink_leg.rs"
  - "hub/tests/dlink_5.rs"
  - "hub/src/inputs.rs"
  - "intercom.strih-snv.toml"
---

# The hub's own Dante device: dstack `dlink` in process (intercom#5)

Owner ruling (dstack#20, 1.11.2026; dstack#68, 2.11.2026): every app carries its own Dante device;
no relaying between apps. The embeddable part is dstack's `dlink` crate: 10 ms latency, normal
priority, no pinning, dantesync as the media clock (a loopback UDP poll, works under the unit's
sandbox). strih-snv only; strih-pp has no Dante network (owner 2.11.2026). Since the cut-over
(3.11.2026, FOH turned its VBAN `fohabl-strih` off) the Dante device is the FOH program feed.

## The pieces

- **Config:** the optional `[dlink]` table (`dlink_leg::DlinkConfig`, `deny_unknown_fields`):
  `name`, `iface`, `rx` (channel names), `latency_ms` (10), `state_file`
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
- **strih-snv's mapping** (owner + dasio 2.11.2026): `foh-abl-yamaha-accelerator` TX
  `REPRO 0dB L 07`, `REPRO 0dB R 08`, `09` (CLICK 3doba, routed by dasio in FOH VB-Matrix), `GUIDE
  track 32`. A rename of card ch 9 must follow in the converter's `DLINK`.
- **The network rate (dlink v0.5.20, `network_rate`):** a Dante flow needs both ends at ONE rate.
  The FOH Yamaha card runs 96 kHz, so a device at the hub's 48 kHz showed every subscription as
  status 21 (TX_FAIL; v0.5.20 reads 16, format mismatch, with a warning naming both rates) and no
  RX flow: `-120 dBFS` on `foh_dante` with a healthy pump and 0 drops. Silence plus a running
  device is NOT a quiet FOH: ask dstack to read the RX status from the Dante side (dev1 is not on
  VLAN 70). `network_rate = 96000` runs the device at 96 kHz; dlink's half-band 2:1 step hands
  `RxProcessor` 48 kHz blocks of half the frames (0.16 ms each way). Only 1x or 2x the hub's rate.
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
- **The crossing:** dlink's pump calls `RxProcessor::process` every 32 frames (dstack rule I7: no
  alloc, lock, syscall or log). It only copies RX as i16 into the lock-free `SampleRing`. The
  `dlink-rx` thread drains whole hub blocks into the participant's `JitterBuffer::card_capture`
  leg with `push_at` (the card leg's path), so the issue-1461 resampler crosses the clock. Never
  push into the JitterBuffer from `process` (it locks and allocates).
- **Supervision:** the `dlink` thread starts the `Link`, polls `ended()` every 200 ms, restarts
  with `restart_backoff`; a start failure logs once per distinct reason. `main` stops the leg after
  the HTTP server's graceful shutdown so the device sends its goodbyes.
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
