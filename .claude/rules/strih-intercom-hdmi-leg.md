---
paths:
  - "intercom/hub/src/hdmi_audio.rs"
  - "intercom/hub/src/hdmi_leg.rs"
  - "intercom/hub/src/pipe_fill.rs"
  - "intercom/hub/tests/hdmi_*_1424.rs"
  - "intercom/hub/tests/fixtures/alsa_hdmi_1424/**"
  - "intercom/intercom.strih-pp.toml"
  - "scripts/lib/strih-intercom-hub-unit.sh"
  - "src/drm_output_audio_match.rs"
---

# The intercom hub's own HDMI audio leg: `alsa_hdmi` (issue 1424 part 3)

Owner 13.10.2026: the PP vestibule TV (strih-pp HDMI-1 -> Blackmagic HDMI-to-SDI) hears the lv1
program with zero added latency, never behind the picture. The hub receives lv1's VBAN `strih`
earliest of any process on the box, so it plays the vestibule participant's N-1 bus straight into
the connector's ALSA hw PCM. The picture stays the in-OBS fixed output on HDMI-1 (part 2); the OBS
module must leave HDMI-1's PCM alone (per-output audio off, part 2's lane), or aplay finds it busy.
Design: issue 1424 comment 6151522810 (Approach 1).

## The pieces

- **Matrix:** adapter `alsa_hdmi` + `hdmi_connector` = the X RandR output name (`HDMI-1`), never a
  card/device number. Output only (`in_channels = 0`), `out_channels` 2..=8 (an HDA HDMI PCM opens
  with 2..8), one participant per connector, `hdmi_connector` refused on every other adapter.
  `Matrix::alsa_hdmi_outputs()`. strih-pp: `vestibule` (program_monitor) + lv1 ch1/ch2 -> ch1/ch2 at
  unity.
- **Resolve (`hdmi_audio.rs`):** the connector EDID from `xrandr --current --props` (NVIDIA puts no
  EDID in `/sys/class/drm`; `--current` never makes X re-probe every output), every HDMI PCM's ELD
  from `amixer -c N controls` + `cget iface=PCM,name=ELD,device=D`, each call under `timeout 5`. The
  match is the part-1 matcher `src/drm_output_audio_match.rs`, mounted into the hub crate by
  `#[path = "../../../src/drm_output_audio_match.rs"]` in `intercom/hub/src/lib.rs` (one rule for the
  OBS module and the hub; the file must stay std-only with no `crate::`). No match / ambiguous / no
  EDID = silent with one named log line per change (an X failure names the tool + its first stderr
  line). The resolved PCM's `IEC958 Playback Switch` is turned on by its rank (part-1 rule; an
  ambiguous switch count is left untouched).
- **The leg (`hdmi_leg.rs`):** `aplay -N -D hw:C,D -t raw -f S16_LE -r 48000 -c 2
  --period-size=256 --buffer-size=1024 -`. `-N` = a busy PCM fails the open at once (without it aplay
  hangs in the open and reads as playing). Re-resolve before every spawn, `restart_backoff` 1..30 s,
  stale queued blocks dropped right before a spawn, xruns counted from aplay's `underrun!!!` stderr.
  An **ELD watcher** re-reads the playing PCM's ELD every 2 s; 2 readings in a row without the matched
  monitor end aplay (NVIDIA's PCM map follows the ELD, so an OBS output start / re-mode can move it);
  an unreadable ELD never counts. The feed loop is a copy of `local_sink_loop`'s on purpose: anchor
  tests pin that function's body text.
- **The pipe (`pipe_fill.rs`):** `PipeFillControl::with_quantum(rate, 256)
  .with_reader_buffer(1024 + 256)` (`aplay_pipe_control`). aplay reads one 256-frame period, so the
  target is 512, the setpoint 640 (13.3 ms). The reader buffer is added to the PRIME (2048: aplay
  pulls its whole ALSA buffer + the period it holds at once) and to the TRIM mark (2304: a catch-up
  burst lands before aplay moves it on). After the prime a reader-buffered pipe is **never topped
  up with silence**: after a block-loop stall aplay takes each block the moment it lands (the pipe
  under one block on every reading while ALSA is full), and the old refill wrote a gap there; the
  servo walks the level back, a really dry reserve is aplay's own xrun. A counted aplay xrun re-arms
  ONE prime (`rearm_prime`): aplay refills its whole buffer from the pipe, and one top-up inside the
  gap that already happened beats a ~13 s steep servo walk. A block before the reader's first read
  is the start hold, never a trim. pw-cat (reader buffer 0) is byte-for-byte unchanged.
- **Lip-sync delay `hdmi_delay_ms` (14.10.2026):** an optional fixed delay (0..=1000 ms, only on
  `alsa_hdmi`) applied by `hdmi_leg::SampleDelay` BEFORE the aplay pipe, so the pipe servo's depth is
  untouched; the line is refilled with silence at every aplay spawn. strih-pp = 180 ms: the vestibule
  picture runs ~263 ms behind the stage (camera -> strih p50 117 ms + the HDMI-1 output 171 ms
  measured with a QR burn captured on cam1, minus ~25 ms grabber), this leg ~80 ms. Re-measure after
  any change to the output path (the 29.97 Hz FIFO queue is most of the 171 ms) and refine with a
  stage clap test recorded on cam1. Tests: `intercom/hub/tests/hdmi_delay_1424.rs` (std-only) + the
  matrix case in `hdmi_matrix_1424.rs`.
- **Delay budget:** program-feed jitter target (32-64 ms) + pipe ~13 ms + ALSA buffer 21 ms + the
  held period 5 ms. `/api/state` carries `alsa_buffer_frames` / `alsa_period_frames` next to
  `local_audio.pipe_depth_frames`, so the sum is readable.
- **Observability:** participant facet `hdmi` = connector, resolved, hw, reason, running (PLAYING:
  aplay has read since its spawn), resolves, xruns; status line ` hdmi=<name>:hw:C,D:depth=N:xruns=N`
  or ` hdmi=<name>:silent`.
- **Provisioning + verify:** `alsa-utils` rides setup-strih step 4b's apt line (avahi-utils'). verify
  item 9 calls `strih_intercom_hub_hdmi_leg_grade` (hub-unit lib; it replaced one comment line, verify
  stays at 1000): PASS = resolved + playing + blocks written, FAIL names the reason, NOTE when the hub
  state is unreadable, FAIL when aplay/amixer are missing. Seam: `STRIH_HUB_HDMI_STATE_FILE`.

## Box facts read live (strih-pp, 13.10.2026, read-only)

- card 0 `HDA NVidia`: HDMI PCMs 3/7/8/9; device 7 = `Blackmagic` (ELD vendor bytes `09 a4 0c be`),
  device 3 = the BenQ on HDMI-0; hw:0,7 takes S16_LE/S32_LE, 2..8 ch, 32..48 kHz.
- Xorg listens on the abstract `@/tmp/.X11-unix/X0`, so xrandr works under the hub unit's
  `PrivateTmp=true`; `XAUTHORITY` falls back to `~/.Xauthority` (HOME set for `User=newlevel`,
  `ProtectHome=read-only`).
- The fixtures under `intercom/hub/tests/fixtures/alsa_hdmi_1424/` are those captures; committing the
  EDID hex trips the secret-staging hook (32-hex lines): `# airuleset:secret-ok <reason>` on the
  `git commit`.

## Tests + Tier-0

- `hdmi_audio_1424.rs` (std-only): parsers on the live captures, the resolve, the IEC958 argv, the
  aplay/xrandr argv, the ELD watch decision, the leg stats, the start hold.
- `hdmi_leg_bench_1424.rs` (std-only): the pipe against a modelled aplay (period reads into a
  1024-frame ALSA buffer on its own clock, 0.1 ms steps, the sink's 1 ms readings continue through a
  block-loop stall): no refill/trim/xrun at 0/±50/±200 ppm, connect delays 0..200 ms, 15..26 ms stalls,
  the pipe back at 640.
- `hdmi_matrix_1424.rs` + `deployed_matrix_pp_1412.rs`: CI only (serde/toml).
- Local: a plain-rustc replica (`#[path]` mounts of hdmi_audio, hdmi_leg, pipe_fill, local_audio with
  its serde derive stripped, vban_io + deps; rlibs anyhow/tracing/libc/intercom-vban per
  `strih-intercom.md` "VBAN rate") + `clippy-driver --test -D warnings`; state.rs/matrix.rs type-check
  with serde stripped and a `toml` stub module.

## Live acceptance (supervisor, after the hub deploy + part 2)

`curl -s http://strih-pp:8790/api/state` -> `vestibule.hdmi`: `resolved: true`, `hw: "hw:0,7"`,
`running: true`, xruns flat; the journal names the resolve once; listen on the vestibule TV; restart
the strih OBS once and confirm the leg is still (or again) on the Blackmagic PCM.
