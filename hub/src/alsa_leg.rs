//! issue 1438: the supervised card leg. A participant on the `alsa` adapter (the cutters) plays its
//! N-1 bus through `aplay` and feeds its talkback from `arecord`, both straight on the card's hw PCM
//! (`hw:CARD=<id>,DEV=<n>`), no PipeWire node and no sound server in between.
//!
//! - **Playback** reuses the HDMI leg's [`AplaySink`]: the pipe fill guard built for aplay's period
//!   read ([`aplay_pipe_control_for`]), the start hold and the resampler that carries the clock
//!   crossing (the card's crystal is not the hub's clock). The participant's channels are spread
//!   onto the card's hw channels first ([`spread_to_hw`]); unmapped hw outputs play silence.
//! - **Capture** is its twin: `arecord` on the same card, its stdout framed into hub blocks, the
//!   mapped hw channels picked ([`pick_from_hw`]) and pushed into the participant's existing
//!   local-capture [`JitterBuffer`] (the `PwCatSource` path of [`crate::local_audio`]).
//!
//! Before every spawn each side looks at `/proc/asound/<id>` ([`resolve_direction`]). A missing card
//! (unplugged), or one that offers no usable format, is the side DOWN: one log line per change of
//! state, a retry with [`restart_backoff`], and NEVER a fallback to another device. A child that
//! exits (a PCM another process holds, the card pulled mid-run) is respawned the same way; its exit
//! is logged once per distinct reason.

use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::alsa_card::{
    card_argv, is_capture_banner_line, is_capture_xrun_line, pick_from_hw, resolve_direction,
    spread_to_hw, AlsaCardSpec, AlsaDirState, AlsaLegStats, Direction, PcmFormat,
    ALSA_CARD_PERIODS, PROC_ASOUND,
};
use crate::hdmi_leg::{aplay_pipe_control_for, drop_stale, AplaySink, XrunCounter};
use crate::local_audio::{
    log_pipe_write, restart_backoff, LocalAudioSink, LocalAudioStats, EGRESS_QUEUE_BLOCKS,
    PIPE_SAMPLE_INTERVAL,
};
use crate::vban_io::{DecodedAudio, JitterBuffer};

/// A child that ran this long before it exited is not counted toward the backoff (a card pulled
/// after an hour of playing retries at once; a PCM that is busy at every open backs off).
const STABLE_RUN: Duration = Duration::from_secs(10);

/// The xrun counter of one side of a card leg (an [`XrunCounter`] for [`AplaySink`]).
struct SideXruns {
    leg: Arc<AlsaLegStats>,
    dir: Direction,
}

impl XrunCounter for SideXruns {
    fn count_xrun(&self) {
        self.leg.side(self.dir).record_xrun();
    }
}

/// What a side logs at most once per change: the state the last look at `/proc` decided, and the
/// last child exit's reason.
#[derive(Default)]
struct SideLog {
    state: Option<Result<PcmFormat, crate::alsa_card::AlsaDown>>,
    exit: Option<String>,
}

impl SideLog {
    /// Log the state the side just decided, once per change. Returns the format when ready.
    fn decided(
        &mut self,
        name: &str,
        spec: &AlsaCardSpec,
        dir: Direction,
        decided: Result<PcmFormat, crate::alsa_card::AlsaDown>,
    ) -> Option<PcmFormat> {
        if self.state != Some(decided) {
            let card = &spec.card;
            match decided {
                Ok(f) => {
                    tracing::info!(participant = %name, %card, format = f.alsa_name(), "local-audio: card leg {name} {} opens {} on card {card} ({})", dir.tool(), crate::alsa_card::pcm_name(card, spec.device), f.alsa_name())
                }
                Err(d) => {
                    tracing::warn!(participant = %name, %card, reason = d.reason(), "local-audio: card leg {name} {} is DOWN: {} -- no fallback, retrying with a backoff", dir.tool(), d.reason())
                }
            }
            self.state = Some(decided);
            self.exit = None;
        }
        decided.ok()
    }

    /// Log a child exit, once per distinct reason (a busy PCM retried every 30 s logs once).
    fn exited(&mut self, name: &str, dir: Direction, why: String) {
        if self.exit.as_deref() != Some(why.as_str()) {
            tracing::warn!(participant = %name, reason = %why, "local-audio: card leg {name} {} exited -- respawning with a backoff", dir.tool());
            self.exit = Some(why);
        } else {
            tracing::debug!(participant = %name, reason = %why, "local-audio: card leg {name} {} exited again", dir.tool());
        }
    }
}

/// Look at the card for one side and publish the decision.
fn look(
    spec: &AlsaCardSpec,
    dir: Direction,
    channels: usize,
    rate: u32,
    leg: &AlsaLegStats,
) -> Result<PcmFormat, crate::alsa_card::AlsaDown> {
    let decided = resolve_direction(Path::new(PROC_ASOUND), &spec.card, dir, channels, rate);
    leg.side(dir).set_state(match decided {
        Ok(f) => AlsaDirState::Ready(f),
        Err(d) => AlsaDirState::Down(d),
    });
    decided
}

/// Spawn the supervised playback thread (`alsa-play`) and return the feed the block loop
/// `try_send`s the participant's N-1 bus into (a full queue drops the block, never blocking the
/// mix loop). The thread runs until the sender is dropped (hub shutdown).
pub fn spawn_alsa_playback(
    spec: AlsaCardSpec,
    name: String,
    rate: u32,
    stats: Arc<LocalAudioStats>,
    leg: Arc<AlsaLegStats>,
) -> SyncSender<Vec<i16>> {
    let (tx, rx) = sync_channel::<Vec<i16>>(EGRESS_QUEUE_BLOCKS);
    thread::Builder::new()
        .name("alsa-play".into())
        .spawn(move || alsa_playback_loop(spec, name, rate, rx, stats, leg))
        .expect("spawn the alsa-play thread");
    tx
}

fn alsa_playback_loop(
    spec: AlsaCardSpec,
    name: String,
    rate: u32,
    rx: Receiver<Vec<i16>>,
    stats: Arc<LocalAudioStats>,
    leg: Arc<AlsaLegStats>,
) {
    let dir = Direction::Playback;
    let hw = spec.hw_out_channels;
    let period = spec.period_frames;
    let pcm = crate::alsa_card::pcm_name(&spec.card, spec.device);
    let mut log = SideLog::default();
    let mut failures: u32 = 0;
    loop {
        let backoff = restart_backoff(failures);
        if !backoff.is_zero() {
            thread::sleep(backoff);
        }
        let Some(format) = log.decided(&name, &spec, dir, look(&spec, dir, hw, rate, &leg)) else {
            failures = failures.saturating_add(1);
            // A down side still drains its feed, so a hub shutdown ends the thread.
            if !drop_stale(&rx) {
                return;
            }
            continue;
        };
        // The audio queued while the side was down is stale: never play it late.
        if !drop_stale(&rx) {
            return;
        }
        let argv = card_argv(dir, &spec.card, spec.device, format, rate, hw, period);
        let xruns: Arc<dyn XrunCounter> = Arc::new(SideXruns {
            leg: leg.clone(),
            dir,
        });
        let mut sink = match AplaySink::spawn_argv(
            &argv,
            hw.clamp(1, u8::MAX as usize) as u8,
            aplay_pipe_control_for(rate, period, period * ALSA_CARD_PERIODS),
            format,
            xruns,
        ) {
            Ok(sink) => sink,
            Err(e) => {
                failures = failures.saturating_add(1);
                log.exited(&name, dir, format!("aplay spawn failed: {e}"));
                continue;
            }
        };
        stats.record_spawn();
        leg.playback.record_spawn();
        let started = Instant::now();
        let mut playing = false;
        let mut xruns_seen = leg.playback.snapshot().xruns;
        let label = "card leg";
        // The feed loop of `local_audio::local_sink_loop` (kept as a copy, like the HDMI leg's:
        // anchor tests pin that function's body text), plus the channel spread and the playing flag.
        let err = loop {
            match rx.recv_timeout(PIPE_SAMPLE_INTERVAL) {
                Ok(block) => {
                    match sink.write_block(&spread_to_hw(
                        &block,
                        spec.out_channels,
                        &spec.out_map,
                        hw,
                    )) {
                        Ok(report) => {
                            stats.record_write(&report);
                            stats.record_asrc_stats(sink.asrc_stats());
                            if report.first {
                                // The prime of every spawn: a busy PCM retried each backoff cycle
                                // must not repeat an info line (the side logs once per change).
                                tracing::debug!(%pcm, "local-audio: {label} pipe primed");
                            } else {
                                log_pipe_write(label, &pcm, &report);
                            }
                        }
                        Err(e) => break e,
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if let Err(e) = sink.sample_fill() {
                        break e;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if !playing && sink.playing() {
                playing = true;
                leg.playback.set_running(true);
            }
            // aplay xrun'd and refills its whole buffer from the pipe: one prime, not a phase walk.
            let xruns = leg.playback.snapshot().xruns;
            if xruns != xruns_seen {
                xruns_seen = xruns;
                sink.rearm_prime();
            }
        };
        leg.playback.set_running(false);
        stats.record_exit();
        failures = if started.elapsed() >= STABLE_RUN {
            1
        } else {
            failures.saturating_add(1)
        };
        let aplay = sink.finish();
        log.exited(&name, dir, format!("{err} (aplay: {aplay})"));
    }
}

/// A supervised `arecord` on one side of the card, its raw PCM read straight from stdout (no
/// `BufReader`, so a block reaches the ring the moment arecord writes it).
pub struct ArecordSource {
    child: Child,
    stdout: ChildStdout,
    stderr_reader: Option<thread::JoinHandle<String>>,
}

impl ArecordSource {
    /// Spawn arecord with `argv`; its overruns are counted into `xruns`.
    pub fn spawn(argv: &[String], xruns: Arc<dyn XrunCounter>) -> io::Result<Self> {
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("arecord stdout not piped"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("arecord stderr not piped"))?;
        let spawned = thread::Builder::new()
            .name("arecord-stderr".into())
            .spawn(move || {
                let mut last = String::new();
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if is_capture_xrun_line(&line) {
                        xruns.count_xrun();
                    } else if !line.trim().is_empty() && !is_capture_banner_line(&line) {
                        last = line;
                    }
                }
                last
            });
        let stderr_reader = match spawned {
            Ok(h) => h,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };
        Ok(ArecordSource {
            child,
            stdout,
            stderr_reader: Some(stderr_reader),
        })
    }

    /// Fill `buf` with exactly one block of raw bytes; an `Err` (incl. EOF) means arecord ended.
    pub fn read_block(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.stdout.read_exact(buf)
    }

    /// End arecord (if it still runs); return its last stderr line that was not an xrun or its
    /// start banner.
    pub fn finish(&mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.stderr_reader
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for ArecordSource {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The parameters of one card leg's capture side ([`spawn_alsa_capture`]).
pub struct AlsaCaptureConfig {
    pub spec: AlsaCardSpec,
    /// The participant's name: the stream name on the pushed [`DecodedAudio`] and in the log.
    pub name: String,
    pub rate: u32,
    pub block_frames: usize,
}

/// Spawn the supervised capture thread (`alsa-rec`): arecord's PCM framed into hub blocks and
/// pushed into the participant's [`JitterBuffer`] the engine pops (exactly like the pw-cat capture).
pub fn spawn_alsa_capture(
    cfg: AlsaCaptureConfig,
    jitter: Arc<Mutex<Vec<JitterBuffer>>>,
    stats: Arc<LocalAudioStats>,
    leg: Arc<AlsaLegStats>,
) {
    thread::Builder::new()
        .name("alsa-rec".into())
        .spawn(move || alsa_capture_loop(cfg, jitter, stats, leg))
        .expect("spawn the alsa-rec thread");
}

fn alsa_capture_loop(
    cfg: AlsaCaptureConfig,
    jitter: Arc<Mutex<Vec<JitterBuffer>>>,
    stats: Arc<LocalAudioStats>,
    leg: Arc<AlsaLegStats>,
) {
    let AlsaCaptureConfig {
        spec,
        name,
        rate,
        block_frames,
    } = cfg;
    let dir = Direction::Capture;
    let hw = spec.hw_in_channels;
    let mut log = SideLog::default();
    let mut failures: u32 = 0;
    loop {
        let backoff = restart_backoff(failures);
        if !backoff.is_zero() {
            thread::sleep(backoff);
        }
        let Some(format) = log.decided(&name, &spec, dir, look(&spec, dir, hw, rate, &leg)) else {
            failures = failures.saturating_add(1);
            continue;
        };
        let argv = card_argv(
            dir,
            &spec.card,
            spec.device,
            format,
            rate,
            hw,
            spec.period_frames,
        );
        let xruns: Arc<dyn XrunCounter> = Arc::new(SideXruns {
            leg: leg.clone(),
            dir,
        });
        let mut source = match ArecordSource::spawn(&argv, xruns) {
            Ok(s) => s,
            Err(e) => {
                failures = failures.saturating_add(1);
                log.exited(&name, dir, format!("arecord spawn failed: {e}"));
                continue;
            }
        };
        stats.record_spawn();
        leg.capture.record_spawn();
        let started = Instant::now();
        let mut buf = vec![0u8; block_frames * hw * format.bytes_per_sample()];
        let err = loop {
            if let Err(e) = source.read_block(&mut buf) {
                break e;
            }
            let planar = pick_from_hw(&format.decode(&buf), hw, &spec.in_map);
            let frames = planar.first().map_or(0, Vec::len);
            let audio = DecodedAudio {
                stream_name: name.clone(),
                channels: planar,
                frames,
            };
            // The arrival time, before the lock: the card leg's rate fit times every block by it
            // (issue 1461).
            let arrived = Instant::now();
            if let Ok(mut jb) = jitter.lock() {
                if let Some(b) = jb.get_mut(spec.participant) {
                    b.push_at(&audio, arrived);
                }
            }
            stats.record_rx();
            leg.capture.set_running(true);
        };
        leg.capture.set_running(false);
        stats.record_exit();
        failures = if started.elapsed() >= STABLE_RUN {
            1
        } else {
            failures.saturating_add(1)
        };
        let arecord = source.finish();
        log.exited(&name, dir, format!("{err} (arecord: {arecord})"));
    }
}
