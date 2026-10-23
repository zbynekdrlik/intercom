---
paths:
  - "intercom/hub/src/alsa_card.rs"
  - "intercom/hub/src/alsa_leg.rs"
  - "intercom/hub/src/alsa_spec.rs"
  - "intercom/hub/tests/alsa_*_1438.rs"
  - "intercom/hub/tests/check_config_1438.rs"
  - "intercom/hub/tests/fixtures/alsa_card_1438/**"
  - "intercom/intercom.strih-pp.toml"
  - "scripts/lib/strih-intercom-hub-unit.sh"
---

# The hub's card leg: the cutters' USB card through ALSA, no PipeWire (issue 1438)

Phase 3 step 1 of the issue-1437 plan (PipeWire out of every production path). The cutters' USB card
(strih-snv MiniFuse 4, strih-pp Yamaha MG-XU) is opened by the hub itself. Before, two `pw-cat`
children carried no `node.dont-fallback`: when the MG-XU left USB on 22.10.2026, WirePlumber re-routed
the cutters' playback INTO the `strih-program` sink and their capture onto its monitor. Design: issue
1438 comment 6374826031 (Approach 1).

## The pieces

- **Matrix** (`alsa_spec.rs`, checked at load): adapter `alsa` + `alsa_card` = the ALSA card ID
  (`/proc/asound/cards`: `M4`, `MGXU`; `[A-Za-z0-9_]{1,15}`, never all digits, because a bare number is a
  card INDEX to ALSA and moves on USB re-enumeration), `alsa_device` (0), `alsa_period_frames` (256 = one
  hub block, 64..=512), `alsa_out_channels` / `alsa_in_channels` = the CARD's channel counts (the hw PCM
  opens with exactly that many), `alsa_channel_map` / `alsa_in_channel_map` = 1-based hw channel per
  participant channel (outputs unique; unmapped hw outputs play silence). One participant per card +
  device; every `alsa_*` key is refused on another adapter. The specs are resolved once at load
  (`Matrix::alsa_cards` / `alsa_spec`): nothing is parsed on the hub-mix thread.
- **The pipe must fit a Linux pipe** (`playback_pipe_trim_bytes`): the aplay guard trims at 9 periods
  (target 2 + two reads + the 4-period ALSA buffer + the held period) at up to 4 bytes a sample; over
  65536 bytes a write would block before the trim acts, so the load refuses it. The MiniFuse's 6
  channels fit up to period 303.
- **The format comes from the card** (`alsa_card.rs`, pure): `/proc/asound/<id>/stream0`'s altsets per
  side, first of S16_LE, S32_LE, S24_3LE at the hub rate and the declared channel count. The hub converts
  (`PcmFormat::encode` / `decode`, the top 16 bits; `PipeFillWriter::with_format` widens the pipe,
  an S16_LE pipe is byte-identical). A card with no `stream0` gets S16_LE and the tool's open error.
- **The leg** (`alsa_leg.rs`): playback = the HDMI leg's `AplaySink::spawn_argv` (any argv, any
  `XrunCounter`) with `aplay_pipe_control_for(rate, period, 4 x period)`: the start hold, the prime and
  the pipe-fill drift servo carry the clock crossing (the card's crystal vs the hub; a resampler for
  every crossing is issue 1461). Capture = `ArecordSource`, blocks framed, the mapped hw channels picked
  and pushed into the participant's local-capture ring (the pw-cat capture path's ring, still 2048
  frames target and no drift servo). `aplay -N` / `arecord -N` on `hw:CARD=<id>,DEV=<n>`: nothing
  converts or mixes, a busy PCM fails at once.
- **Down, never a fallback:** before every spawn each side looks at `/proc/asound/<id>`. Card missing /
  no format = the side DOWN, one log line per change of state, `restart_backoff` 1..30 s, the playback
  feed drained meanwhile. A child exit logs once per distinct reason; a busy PCM retried every 30 s logs
  once. The prime of each spawn is a debug line (an info line per retry was the review-round-1 finding).
- **Observability:** participant facet `alsa` = card, pcm, present, per used side {state, format,
  hw_channels, running, xruns (aplay `underrun!!!` / arecord `overrun!!!`), spawns}, period + buffer
  frames; the playback pipe's depth is in `local_audio`. Status line ` alsa=<name>:<card>:depth=N:xruns=P/C`
  or ` alsa=<name>:<card>:down(<reason>)`.
- **verify-strih item 9** (`strih_intercom_hub_alsa_leg_grade`, hub-unit lib, seam
  `STRIH_HUB_ALSA_STATE_FILE`): PASS = card present + every used side moving audio (format + xruns named);
  FAIL = card not on the box / a side down or not running / no facet / aplay or arecord missing; a
  `pipewire` cutters participant (the rollback) is a NOTE. It replaced the old `talkback: MiniFuse` note.
- **setup-strih step 13 installs a routing file only when the installed hub loads it**
  (`strih_intercom_hub_config_install`, `intercom-hub --check-config --config <file>`): step 13 keeps an
  installed hub binary, and a hub built before 1438 refuses `adapter = "alsa"` -- a deploy would leave a
  hub that crash-loops at its next restart. A refusing (or pre-flag) hub keeps the installed file with
  one WARNING naming the reinstall; a fresh box (no hub yet) gets the file.

## Box facts (read live 23.10.2026, read-only)

- strih-snv card 3 `M4` (ARTURIA MiniFuse 4): playback 6 ch S32_LE (altset 1) / S16_LE (altset 2),
  capture 6 ch S32_LE only, 44.1-192 kHz. PipeWire held both PCMs (S32_LE, period 1024 x 3), and the
  MiniFuse was PipeWire's DEFAULT sink and source. TOML: hw 6/6, maps `1,2,3,4` / `1,2` (the old pw-cat
  map AUX0-3 / capture_AUX0-1).
- strih-pp: the MG-XU was NOT on USB (disconnect 23.10. 16:55:58). Card ID `MGXU` from
  `/var/lib/alsa/asound.state` (2 ch each way); its sample formats are unknown, which is why the format
  is read from the card at spawn.
- No WirePlumber deny rule on purpose (design: phase 4 removes WirePlumber). PipeWire still manages the
  card: while anything plays to it (it is the default sink on strih-snv) aplay/arecord get EBUSY on every
  retry, and verify FAILs "not moving audio". PipeWire releases an idle node (suspend timeout).

## Rollout + rollback (supervisor)

Deploy the hub binary FIRST, then the TOML (or re-run setup-strih: the guard keeps the old TOML until
the hub loads the new one), then restart the hub. strih-pp first. Live check: `curl -s
http://<box>:8790/api/state | jq '.participants[] | select(.name=="cutters") | .alsa'` -> present, both
sides `running: true`, the format; the journal shows `card leg cutters aplay opens hw:CARD=...` once per
side; xruns flat; talkback + headphones checked by ear. Rollback: the TOML adapter back to `pipewire`
with the old node keys (listed in the converter's docstring and the strih-pp TOML header).

## Tests + Tier-0

- `alsa_card_1438.rs` (std-only: the live `stream0` fixture, format choice, card ID, argv, maps,
  conversions, stats); `alsa_matrix_1438.rs` + `check_config_1438.rs` + the deployed-matrix tests (CI:
  serde / toml / the binary); `tests/python/test_strih_hub_alsa_leg_1438.py`,
  `test_strih_hub_config_install_1438.py`, the converter + strih-pp TOML pytests.
- Local: a plain-rustc replica of alsa_card + its test (13 tests + clippy-driver); a second root
  (`#[path]` mounts of alsa_leg, hdmi_leg, hdmi_audio, local_audio with serde stripped, pipe_fill,
  janus_pacing, vban_io + deps over the "VBAN rate" rlibs) runs the HDMI bench, an S32 pipe test and an
  `AplaySink::spawn_argv` run against a fake tool. A fake tool for that must `exec` the reader: a `sh -c
  'echo ...; cat'` keeps a grandchild holding stderr after the kill, and `finish()` then waits forever.
  matrix / state / inputs / alsa_spec + main.rs's `wire_local_audio` type-check and clippy with serde
  stripped and stub `toml` / `janus_rtp` / `ndi_video` modules.
