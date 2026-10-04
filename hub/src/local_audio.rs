//! The local PipeWire audio bridge (issue 1344): the last VB-Matrix function the strih-lx notebook
//! lacked.
//!
//! Two directions, one shape (`pw-cat` supervised children — the SAME shell-out-over-FFI rationale
//! the bkshading relay uses for gphoto2: no libpipewire build dependency, so the whole hub stays
//! Tier-0 / CI-buildable and cross-compilable). The [`LocalAudioSink`] / [`LocalAudioSource`] traits
//! are the abstraction boundary the supervised loops write/read a block through; the only impls today
//! are the `pw-cat`-child [`PwCatSink`] / [`PwCatSource`]. A future native `pipewire-rs` backend
//! would add a second impl of the same trait and generalise the two loops over it — the trait exists
//! to make that swap local, not because a second impl ships yet:
//!
//! EGRESS ([`PwCatSink`]): the block loop's `program_out` mix (the decoded `fohabl-strih` +
//! `lv1-strih` VBAN blocks the engine already sums via the matrix points) is written to a PipeWire
//! playback stream targeted at the operator's `strih-program` null sink. OBS captures
//! `strih-program.monitor` as its `ASIO zvuk` program input. The same sink serves the cutters'
//! MiniFuse cans. Every write first measures the stdin pipe's fill ([`pipe_fill_bytes`]) and keeps
//! it at [`PIPE_TARGET_FRAMES`] ([`pipe_fill_plan`], issue 1401): pw-cat reads a whole graph quantum
//! per cycle, so a pipe the hub let drain would otherwise xrun on every cycle until a restart.
//!
//! INGRESS ([`PwCatSource`]): the MiniFuse 4 capture (the strih operator's talkback mic) is read as
//! PCM and pushed into the `cutters` participant's [`JitterBuffer`] the engine already pops into the
//! N-1 mix, so the operator's talkback reaches the camboxes.
//!
//! The children are SUPERVISED: a spawn/exit is logged, and a died child is respawned with an
//! exponential [`restart_backoff`]. The audio never blocks the tokio runtime — each direction runs on
//! its OWN OS thread (the `pw-cat` stdin write / stdout read are blocking I/O), fed/drained through a
//! channel + the shared jitter Arc exactly like [`crate::janus_rtp`] and [`crate::ndi_video`].

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::Serialize;

use crate::vban_io::{DecodedAudio, JitterBuffer};

/// The PCM sample format `pw-cat` speaks with the hub (signed 16-bit LE, matching the VBAN PCM16 the
/// engine mixes). Kept one place so the argv builders + the framing helpers never drift apart.
pub const PW_CAT_FORMAT: &str = "s16";

/// The queue depth for the egress channel: ~64 blocks (~0.3 s at 256-frame blocks). When it is full
/// the feeder's best-effort `try_send` DROPS THE NEW block (it never evicts a queued one and never
/// blocks the mix loop) — a dropped ~5 ms block is tolerable program jitter; a stalled mix loop is not.
const EGRESS_QUEUE_BLOCKS: usize = 64;

/// The PipeWire graph quantum the MiniFuse drives on strih-lx (`clock.quantum 1024`, ALSA
/// `period-size 1024`, live-read 24.9.2026): a `pw-cat --record` child hands the hub its capture in
/// bursts of this many frames, whatever block size the hub pops.
pub const PW_GRAPH_BURST_FRAMES: usize = 1024;

/// The node latency (in frames at `--rate`) the capture child asks PipeWire for: the MiniFuse graph
/// quantum itself ([`PW_GRAPH_BURST_FRAMES`], 1024). A smaller request pulls the WHOLE graph down to
/// it: at `--latency 256` the graph ran quantum 256 while the MiniFuse playback ran period 1024, and
/// the cameraman sounded robotic in the operator headphones (issue 1345, 24.9.2026; owner accepted
/// the 1024 state 25.9). The capture ring is already sized for bursts of this size (target
/// [`LOCAL_CAPTURE_TARGET_FRAMES`] = two bursts), so asking for them costs nothing.
/// pw-cat takes it as direct SAMPLES (`--latency 1024` + `--rate 48000` = `node.latency 1024/48000`);
/// the literal `1024/48000` is rejected by pw-cat 1.6.2 as a "bad unit" (live-verified).
pub const PW_CAT_RECORD_LATENCY_FRAMES: usize = PW_GRAPH_BURST_FRAMES;

/// The local-capture ring's cap, in hub blocks (issue 1345: at least 32 blocks).
pub const LOCAL_CAPTURE_CAP_BLOCKS: usize = 32;

/// The local-capture ring's TARGET fill: about 2x the pw-cat burst, so a whole burst can arrive late
/// without the ring running dry (issue 1345).
pub const LOCAL_CAPTURE_TARGET_FRAMES: usize = 2 * PW_GRAPH_BURST_FRAMES;

/// The fill a `pw-cat --playback` stdin pipe is kept at (issue 1401, 4.10.2026): two graph quanta,
/// 2048 frames (42.7 ms at 48 kHz). pw-cat `fread`s a whole quantum ([`PW_GRAPH_BURST_FRAMES`])
/// from the pipe inside each process callback. A pipe holding less blocks that callback until the
/// hub has written the rest, the graph cycle overruns and the stream xruns. The hub writes at
/// exactly the consumer's rate, so nothing refills a pipe a lost block drained (live: ERR ~50/s on
/// the program sink and the cutters' cans until a hub restart). A pipe below one hub block is
/// topped up to this with silence ([`pipe_fill_plan`]).
pub const PIPE_TARGET_FRAMES: usize = 2 * PW_GRAPH_BURST_FRAMES;

/// Above this fill (the target + two more quanta, 4096 frames, 85 ms) a block is dropped instead of
/// written, so a pipe that grew never holds more delay than this.
pub const PIPE_HIGH_FRAMES: usize = PIPE_TARGET_FRAMES + 2 * PW_GRAPH_BURST_FRAMES;

/// What one block write does to a `pw-cat` playback pipe ([`pipe_fill_plan`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeFillPlan {
    /// The pipe holds less than one hub block: write `silence_frames` of silence (up to
    /// [`PIPE_TARGET_FRAMES`]), then the block.
    TopUp { silence_frames: usize },
    /// The pipe holds more than [`PIPE_HIGH_FRAMES`]: drop the block.
    Drop,
    /// Healthy: write the block as it is.
    Write,
}

/// Decide one block write from the pipe's measured fill (issue 1401). `fill_frames` is the fill
/// before the write, `block_frames` the block being written (one hub block):
///
/// - below one block: top up with silence to [`PIPE_TARGET_FRAMES`], then write the block. A
///   fresh pipe (the first write after a spawn) reads 0 and gets the same top-up;
/// - above [`PIPE_HIGH_FRAMES`]: drop the block;
/// - otherwise: write it. The healthy steady state (1024..2048 frames) never triggers either guard.
pub fn pipe_fill_plan(fill_frames: usize, block_frames: usize) -> PipeFillPlan {
    if fill_frames < block_frames {
        PipeFillPlan::TopUp {
            silence_frames: PIPE_TARGET_FRAMES.saturating_sub(fill_frames),
        }
    } else if fill_frames > PIPE_HIGH_FRAMES {
        PipeFillPlan::Drop
    } else {
        PipeFillPlan::Write
    }
}

/// The bytes queued in a pipe right now: the `FIONREAD` ioctl, which a Linux pipe answers on either
/// end. The sink asks it on its WRITE end before every block.
pub fn pipe_fill_bytes<F: AsFd>(pipe: F) -> io::Result<usize> {
    let mut queued: libc::c_int = 0;
    // SAFETY: FIONREAD writes one c_int through the pointer, which is valid for the whole call; the
    // fd is borrowed from its live owner for that call.
    let rc = unsafe { libc::ioctl(pipe.as_fd().as_raw_fd(), libc::FIONREAD, &mut queued) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    usize::try_from(queued).map_err(|_| io::Error::other("FIONREAD reported a negative pipe fill"))
}

/// What one [`PipeFillWriter::write_block`] did, for the `local_audio` facet and the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipeWriteReport {
    /// The pipe fill measured before the write, in frames.
    pub fill_frames: usize,
    /// What the write did.
    pub plan: PipeFillPlan,
    /// The first write into this pipe (just after the spawn). Its top-up is the prime, not a
    /// refill: `pipe_refills` counts only a pipe that drained under a running pw-cat.
    pub first: bool,
}

/// The fill guard on a `pw-cat --playback` stdin pipe (issue 1401): measures the pipe before every
/// block and applies [`pipe_fill_plan`]. Generic over the pipe's write end so a test can drive it
/// over a real `std::io::pipe`.
pub struct PipeFillWriter<W> {
    pipe: W,
    channels: usize,
    first: bool,
}

impl<W: Write + AsFd> PipeFillWriter<W> {
    /// Guard `pipe`, which carries interleaved s16 frames of `channels` channels (2 bytes each).
    pub fn new(pipe: W, channels: u8) -> Self {
        PipeFillWriter {
            pipe,
            channels: usize::from(channels.max(1)),
            first: true,
        }
    }

    /// Write one interleaved PCM16 block through the fill guard. An `Err` means the pipe is broken
    /// (the child died).
    pub fn write_block(&mut self, interleaved: &[i16]) -> io::Result<PipeWriteReport> {
        let bytes_per_frame = self.channels * 2;
        let fill_frames = pipe_fill_bytes(&self.pipe)? / bytes_per_frame;
        let plan = pipe_fill_plan(fill_frames, interleaved.len() / self.channels);
        let first = std::mem::replace(&mut self.first, false);
        match plan {
            PipeFillPlan::Drop => {}
            PipeFillPlan::Write => self.pipe.write_all(&interleaved_to_le_bytes(interleaved))?,
            PipeFillPlan::TopUp { silence_frames } => {
                // One write: the silence, then the block.
                let mut bytes = vec![0u8; silence_frames * bytes_per_frame];
                bytes.extend_from_slice(&interleaved_to_le_bytes(interleaved));
                self.pipe.write_all(&bytes)?;
            }
        }
        Ok(PipeWriteReport {
            fill_frames,
            plan,
            first,
        })
    }
}

/// Build the `pw-cat` PLAYBACK argv for the program sink (egress), with no channel map. See
/// [`pw_cat_playback_argv_with_map`].
pub fn pw_cat_playback_argv(target: &str, rate: u32, channels: u8) -> Vec<String> {
    pw_cat_playback_argv_with_map(target, rate, channels, None)
}

/// Build the `pw-cat` PLAYBACK argv for a local sink (egress). Raw interleaved s16 PCM is fed on
/// stdin (`-`); `--target` names the sink node. `channel_map` (e.g. `AUX0,AUX1,AUX2,AUX3`) is passed
/// as `--channel-map` when given: a pro-audio node (the MiniFuse playback, ports `AUX0..AUX5`) needs
/// it, because a 4-channel stream otherwise defaults to `FL,FR,RL,RR` and never lands on the AUX
/// ports. The `strih-program` null sink takes the default stereo map (`None`), so its argv is
/// unchanged.
pub fn pw_cat_playback_argv_with_map(
    target: &str,
    rate: u32,
    channels: u8,
    channel_map: Option<&str>,
) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "pw-cat".into(),
        "--playback".into(),
        "--raw".into(),
        "--rate".into(),
        rate.to_string(),
        "--channels".into(),
        channels.to_string(),
    ];
    if let Some(map) = channel_map {
        argv.push("--channel-map".into());
        argv.push(map.to_string());
    }
    argv.extend([
        "--format".into(),
        PW_CAT_FORMAT.into(),
        "--target".into(),
        target.to_string(),
        "-".into(),
    ]);
    argv
}

/// Build the `pw-cat` RECORD argv for a capture source (ingress). Raw interleaved s16 PCM comes out
/// on stdout (`-`); `--target` names the MiniFuse 4 capture node; `--latency` asks for the graph
/// quantum ([`PW_CAT_RECORD_LATENCY_FRAMES`]) instead of pw-cat's 100 ms default.
pub fn pw_cat_record_argv(target: &str, rate: u32, channels: u8) -> Vec<String> {
    vec![
        "pw-cat".into(),
        "--record".into(),
        "--raw".into(),
        "--rate".into(),
        rate.to_string(),
        "--channels".into(),
        channels.to_string(),
        "--format".into(),
        PW_CAT_FORMAT.into(),
        "--latency".into(),
        PW_CAT_RECORD_LATENCY_FRAMES.to_string(),
        "--target".into(),
        target.to_string(),
        "-".into(),
    ]
}

/// Interleaved PCM16 → little-endian bytes (the exact framing `pw-cat --raw --format s16` reads on
/// stdin). No allocation beyond the output vec.
pub fn interleaved_to_le_bytes(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Little-endian bytes → interleaved PCM16 (the framing `pw-cat --raw --format s16` writes on
/// stdout). A trailing partial sample (odd byte) is dropped, so a short read never panics.
pub fn le_bytes_to_interleaved(bytes: &[u8]) -> Vec<i16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c))
        .collect()
}

/// Deinterleave one interleaved PCM16 block into planar channels (`out[ch][frame]`), the shape a
/// [`DecodedAudio`] carries. `channels` must be ≥ 1; the frame count is derived from the payload, so
/// a short/long block never panics (a trailing partial frame is dropped).
pub fn deinterleave(interleaved: &[i16], channels: usize) -> Vec<Vec<i16>> {
    let n_ch = channels.max(1);
    let frames = interleaved.len() / n_ch;
    let mut planar = vec![Vec::with_capacity(frames); n_ch];
    for (i, &s) in interleaved.iter().enumerate() {
        let ch = i % n_ch;
        if planar[ch].len() < frames {
            planar[ch].push(s);
        }
    }
    planar
}

/// The exponential restart backoff for a supervised `pw-cat` child, by consecutive failure count.
/// `0` failures → no delay (the first spawn is immediate); then 1 s, 2 s, 4 s, 8 s, 16 s, capped at
/// 30 s — so a persistently-broken node (a missing PipeWire session, a mistyped target) retries
/// forever without hammering.
pub fn restart_backoff(consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return Duration::ZERO;
    }
    let exp = consecutive_failures.min(6);
    let secs = (1u64 << (exp - 1)).min(30);
    Duration::from_secs(secs)
}

/// Shared, atomic counters for one local-audio direction, read by `/api/state` without a lock.
#[derive(Debug, Default)]
pub struct LocalAudioStats {
    tx_blocks: AtomicU64,
    rx_blocks: AtomicU64,
    spawns: AtomicU64,
    exits: AtomicU64,
    pipe_refills: AtomicU64,
    pipe_refill_frames: AtomicU64,
    pipe_trims: AtomicU64,
    pipe_fill_frames: AtomicU64,
}

/// The serialized local-audio facet added to `/api/state` for a `program_out` / `cutters`
/// (pipewire-adapter) participant.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct LocalAudioFacet {
    /// Blocks written to the PipeWire sink (egress) — 0 for a capture-only participant.
    pub tx_blocks: u64,
    /// Blocks read from the PipeWire source (ingress) — 0 for a sink-only participant.
    pub rx_blocks: u64,
    /// `pw-cat` child spawns (a restart increments this — a rising value = an unstable node).
    pub spawns: u64,
    /// `pw-cat` child exits (a died child that got respawned).
    pub exits: u64,
    /// Times the playback pipe had drained below one hub block under a running pw-cat and was
    /// topped up with silence (issue 1401). 0 in a healthy run; the prime after a spawn is not one.
    pub pipe_refills: u64,
    /// The silence those refills wrote, in frames.
    pub pipe_refill_frames: u64,
    /// Blocks dropped because the playback pipe held more than the trim mark (4096 frames).
    pub pipe_trims: u64,
    /// The playback pipe's fill measured before the last write, in frames (healthy: ~1024..2048).
    pub pipe_fill_frames: u64,
}

impl LocalAudioStats {
    /// A point-in-time snapshot for `/api/state`.
    pub fn snapshot(&self) -> LocalAudioFacet {
        LocalAudioFacet {
            tx_blocks: self.tx_blocks.load(Ordering::Relaxed),
            rx_blocks: self.rx_blocks.load(Ordering::Relaxed),
            spawns: self.spawns.load(Ordering::Relaxed),
            exits: self.exits.load(Ordering::Relaxed),
            pipe_refills: self.pipe_refills.load(Ordering::Relaxed),
            pipe_refill_frames: self.pipe_refill_frames.load(Ordering::Relaxed),
            pipe_trims: self.pipe_trims.load(Ordering::Relaxed),
            pipe_fill_frames: self.pipe_fill_frames.load(Ordering::Relaxed),
        }
    }

    /// Count one playback write: a written block (topped up or not) is a `tx_block`, a dropped one
    /// a `pipe_trim`, a top-up that is not the prime a `pipe_refill`.
    pub fn record_write(&self, report: &PipeWriteReport) {
        self.pipe_fill_frames
            .store(report.fill_frames as u64, Ordering::Relaxed);
        match report.plan {
            PipeFillPlan::Drop => {
                self.pipe_trims.fetch_add(1, Ordering::Relaxed);
            }
            PipeFillPlan::Write => {
                self.tx_blocks.fetch_add(1, Ordering::Relaxed);
            }
            PipeFillPlan::TopUp { silence_frames } => {
                self.tx_blocks.fetch_add(1, Ordering::Relaxed);
                if !report.first {
                    self.pipe_refills.fetch_add(1, Ordering::Relaxed);
                    self.pipe_refill_frames
                        .fetch_add(silence_frames as u64, Ordering::Relaxed);
                }
            }
        }
    }
}

/// A local audio EGRESS sink: write interleaved PCM16 blocks to a PipeWire node.
pub trait LocalAudioSink: Send {
    /// Write one interleaved PCM16 block and report what it did to the sink's buffer; an `Err`
    /// means the underlying child died (the supervisor respawns).
    fn write_block(&mut self, interleaved: &[i16]) -> io::Result<PipeWriteReport>;
}

/// A local audio INGRESS source: read one fixed-size raw block from a PipeWire node.
pub trait LocalAudioSource: Send {
    /// Fill `buf` with exactly one block of raw little-endian s16 bytes; an `Err` (incl. EOF) means
    /// the child died (the supervisor respawns).
    fn read_block(&mut self, buf: &mut [u8]) -> io::Result<()>;
}

/// A supervised `pw-cat --playback` child fed interleaved PCM16 on stdin, through the pipe fill
/// guard ([`PipeFillWriter`]).
pub struct PwCatSink {
    child: Child,
    stdin: PipeFillWriter<ChildStdin>,
}

impl PwCatSink {
    /// Spawn `pw-cat --playback` targeting `target` at `rate`/`channels`, with an optional
    /// `--channel-map` (see [`pw_cat_playback_argv_with_map`]).
    pub fn spawn(
        target: &str,
        rate: u32,
        channels: u8,
        channel_map: Option<&str>,
    ) -> io::Result<Self> {
        let argv = pw_cat_playback_argv_with_map(target, rate, channels, channel_map);
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("pw-cat stdin not piped"))?;
        Ok(PwCatSink {
            child,
            stdin: PipeFillWriter::new(stdin, channels),
        })
    }
}

impl LocalAudioSink for PwCatSink {
    fn write_block(&mut self, interleaved: &[i16]) -> io::Result<PipeWriteReport> {
        self.stdin.write_block(interleaved)
    }
}

impl Drop for PwCatSink {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A supervised `pw-cat --record` child producing interleaved PCM16 on stdout. The stdout pipe is
/// read directly (a raw [`ChildStdout`], NO `BufReader`), so a block reaches the ring the moment
/// pw-cat writes it.
pub struct PwCatSource {
    child: Child,
    stdout: ChildStdout,
}

impl PwCatSource {
    /// Spawn `pw-cat --record` capturing `target` at `rate`/`channels`.
    pub fn spawn(target: &str, rate: u32, channels: u8) -> io::Result<Self> {
        let argv = pw_cat_record_argv(target, rate, channels);
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("pw-cat stdout not piped"))?;
        Ok(PwCatSource { child, stdout })
    }
}

impl LocalAudioSource for PwCatSource {
    fn read_block(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.stdout.read_exact(buf)
    }
}

impl Drop for PwCatSource {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The parameters for one supervised local EGRESS sink ([`spawn_local_sink`]).
pub struct LocalSinkConfig {
    /// The PipeWire sink node name (`pw-cat --target`).
    pub target: String,
    pub rate: u32,
    pub channels: u8,
    /// The `--channel-map` (e.g. `AUX0,AUX1,AUX2,AUX3`), or `None` for pw-cat's default map.
    pub channel_map: Option<String>,
    /// What the sink is, for the log lines (`program sink`, `talkback playback`).
    pub label: &'static str,
}

/// Spawn the supervised EGRESS thread for the `program_out` sink (the OBS program capture). It uses
/// the default channel map, so its argv is exactly the issue-1344 one. See [`spawn_local_sink`].
pub fn spawn_program_sink(
    target: String,
    rate: u32,
    channels: u8,
    stats: Arc<LocalAudioStats>,
) -> SyncSender<Vec<i16>> {
    spawn_local_sink(
        LocalSinkConfig {
            target,
            rate,
            channels,
            channel_map: None,
            label: "program sink",
        },
        stats,
    )
}

/// Spawn a supervised EGRESS thread that owns a [`PwCatSink`] and returns the [`SyncSender`] the
/// block loop feeds one interleaved PCM16 block per tick. A full queue drops the block (best-effort
/// `try_send`, like the Janus feed). The thread respawns a died `pw-cat` with [`restart_backoff`],
/// forever, until the sender is dropped (hub shutdown). Serves the `program_out` sink and every
/// local playback participant (the operator's MiniFuse headphones, issue 1345).
pub fn spawn_local_sink(cfg: LocalSinkConfig, stats: Arc<LocalAudioStats>) -> SyncSender<Vec<i16>> {
    let (tx, rx) = sync_channel::<Vec<i16>>(EGRESS_QUEUE_BLOCKS);
    thread::spawn(move || local_sink_loop(cfg, rx, stats));
    tx
}

fn local_sink_loop(cfg: LocalSinkConfig, rx: Receiver<Vec<i16>>, stats: Arc<LocalAudioStats>) {
    let LocalSinkConfig {
        target,
        rate,
        channels,
        channel_map,
        label,
    } = cfg;
    let mut failures: u32 = 0;
    loop {
        let backoff = restart_backoff(failures);
        if !backoff.is_zero() {
            thread::sleep(backoff);
        }
        match PwCatSink::spawn(&target, rate, channels, channel_map.as_deref()) {
            Ok(mut sink) => {
                stats.spawns.fetch_add(1, Ordering::Relaxed);
                failures = 0;
                tracing::info!(%target, rate, channels, channel_map = ?channel_map, "local-audio: {label} pw-cat spawned");
                let err = loop {
                    match rx.recv() {
                        Ok(block) => match sink.write_block(&block) {
                            Ok(report) => {
                                stats.record_write(&report);
                                log_pipe_write(label, &target, &report);
                            }
                            // The child died mid-write — break to respawn.
                            Err(e) => break e,
                        },
                        // The sender was dropped: the hub is shutting down.
                        Err(_) => return,
                    }
                };
                stats.exits.fetch_add(1, Ordering::Relaxed);
                failures = failures.saturating_add(1);
                tracing::warn!(%target, error = %err, "local-audio: {label} pw-cat exited — respawning");
            }
            Err(e) => {
                failures = failures.saturating_add(1);
                tracing::warn!(%target, error=%e, "local-audio: {label} pw-cat spawn failed — backing off");
            }
        }
    }
}

/// Log a playback write the fill guard acted on: the prime after a spawn at info, a refill (the
/// pipe drained under a running pw-cat, issue 1401) as one warn line, a trim at debug (a stalled
/// pw-cat trims every block; `pipe_trims` counts them).
fn log_pipe_write(label: &str, target: &str, report: &PipeWriteReport) {
    match report.plan {
        PipeFillPlan::TopUp { silence_frames } if report.first => {
            tracing::info!(%target, silence_frames, "local-audio: {label} pipe primed to the fill target");
        }
        PipeFillPlan::TopUp { silence_frames } => {
            tracing::warn!(%target, fill_frames = report.fill_frames, silence_frames, "local-audio: {label} pipe ran low — topped up with silence");
        }
        PipeFillPlan::Drop => {
            tracing::debug!(%target, fill_frames = report.fill_frames, "local-audio: {label} pipe above the trim mark — block dropped");
        }
        PipeFillPlan::Write => {}
    }
}

/// The parameters for the INGRESS thread ([`spawn_local_source`]) — bundled so the spawn stays a
/// two-argument call (avoids the `too_many_arguments` clippy lint).
pub struct LocalSourceConfig {
    /// The PipeWire capture node name (`pw-cat --target`), e.g. the MiniFuse 4 pro-input node.
    pub target: String,
    pub rate: u32,
    pub channels: usize,
    pub block_frames: usize,
    /// The participant id whose [`JitterBuffer`] the captured blocks are pushed into.
    pub participant_id: usize,
    /// The stream name stamped on the pushed [`DecodedAudio`] (informational; the participant id is
    /// what routes it).
    pub stream_name: String,
}

/// Spawn the supervised INGRESS thread that owns the [`PwCatSource`], frames its stdout into
/// `block_frames`-frame blocks, and pushes each into the participant's [`JitterBuffer`] the engine
/// pops (exactly like the Janus adapter pushes the room mix into the phones buffer). Respawns a died
/// `pw-cat` with [`restart_backoff`], forever.
pub fn spawn_local_source(
    cfg: LocalSourceConfig,
    jitter: Arc<Mutex<Vec<JitterBuffer>>>,
    stats: Arc<LocalAudioStats>,
) {
    thread::spawn(move || local_source_loop(cfg, jitter, stats));
}

fn local_source_loop(
    cfg: LocalSourceConfig,
    jitter: Arc<Mutex<Vec<JitterBuffer>>>,
    stats: Arc<LocalAudioStats>,
) {
    let channels = cfg.channels.max(1);
    let block_bytes = cfg.block_frames * channels * 2;
    let mut buf = vec![0u8; block_bytes];
    let mut failures: u32 = 0;
    loop {
        let backoff = restart_backoff(failures);
        if !backoff.is_zero() {
            thread::sleep(backoff);
        }
        match PwCatSource::spawn(&cfg.target, cfg.rate, channels as u8) {
            Ok(mut source) => {
                stats.spawns.fetch_add(1, Ordering::Relaxed);
                failures = 0;
                tracing::info!(target = %cfg.target, rate = cfg.rate, channels, "local-audio: capture source pw-cat spawned");
                loop {
                    if source.read_block(&mut buf).is_err() {
                        break;
                    }
                    let interleaved = le_bytes_to_interleaved(&buf);
                    let planar = deinterleave(&interleaved, channels);
                    let frames = planar.first().map(|c| c.len()).unwrap_or(0);
                    let audio = DecodedAudio {
                        stream_name: cfg.stream_name.clone(),
                        channels: planar,
                        frames,
                    };
                    if let Ok(mut jb) = jitter.lock() {
                        if let Some(b) = jb.get_mut(cfg.participant_id) {
                            b.push(&audio);
                        }
                    }
                    stats.rx_blocks.fetch_add(1, Ordering::Relaxed);
                }
                stats.exits.fetch_add(1, Ordering::Relaxed);
                failures = failures.saturating_add(1);
                tracing::warn!(target = %cfg.target, "local-audio: capture source pw-cat exited — respawning");
            }
            Err(e) => {
                failures = failures.saturating_add(1);
                tracing::warn!(target = %cfg.target, error=%e, "local-audio: capture source pw-cat spawn failed — backing off");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_argv_is_raw_s16_stereo_to_the_target_on_stdin() {
        let a = pw_cat_playback_argv("strih-program", 48000, 2);
        assert_eq!(a[0], "pw-cat");
        assert!(a.contains(&"--playback".to_string()));
        assert!(a.contains(&"--raw".to_string()));
        // rate/channels/format/target are the flag VALUES.
        let pos = |k: &str| a.iter().position(|x| x == k).unwrap();
        assert_eq!(a[pos("--rate") + 1], "48000");
        assert_eq!(a[pos("--channels") + 1], "2");
        assert_eq!(a[pos("--format") + 1], "s16");
        assert_eq!(a[pos("--target") + 1], "strih-program");
        // PCM on stdin: the trailing `-`.
        assert_eq!(a.last().unwrap(), "-");
    }

    #[test]
    fn record_argv_is_raw_s16_from_the_target_on_stdout() {
        let a = pw_cat_record_argv("alsa_input.minifuse", 48000, 2);
        assert!(a.contains(&"--record".to_string()));
        let pos = |k: &str| a.iter().position(|x| x == k).unwrap();
        assert_eq!(a[pos("--rate") + 1], "48000");
        assert_eq!(a[pos("--target") + 1], "alsa_input.minifuse");
        assert_eq!(a.last().unwrap(), "-");
    }

    #[test]
    fn framing_roundtrips_interleaved_s16() {
        let samples: Vec<i16> = vec![0, 1, -1, 32767, -32768, 100];
        let bytes = interleaved_to_le_bytes(&samples);
        assert_eq!(bytes.len(), samples.len() * 2);
        assert_eq!(le_bytes_to_interleaved(&bytes), samples);
        // An odd trailing byte is dropped, never a panic.
        let mut short = bytes.clone();
        short.push(0x7f);
        assert_eq!(le_bytes_to_interleaved(&short), samples);
    }

    #[test]
    fn deinterleave_stereo_planar() {
        // frame0 L,R ; frame1 L,R ; frame2 L,R = [1,2, 3,4, 5,6]
        let interleaved: [i16; 6] = [1, 2, 3, 4, 5, 6];
        let planar = deinterleave(&interleaved, 2);
        assert_eq!(planar.len(), 2);
        assert_eq!(planar[0], vec![1, 3, 5]); // L
        assert_eq!(planar[1], vec![2, 4, 6]); // R
                                              // A trailing partial frame (odd sample) is dropped.
        let planar2 = deinterleave(&[1, 2, 3, 4, 5], 2);
        assert_eq!(planar2[0], vec![1, 3]);
        assert_eq!(planar2[1], vec![2, 4]);
    }

    #[test]
    fn restart_backoff_is_immediate_then_exponential_capped_at_30s() {
        assert_eq!(restart_backoff(0), Duration::ZERO);
        assert_eq!(restart_backoff(1), Duration::from_secs(1));
        assert_eq!(restart_backoff(2), Duration::from_secs(2));
        assert_eq!(restart_backoff(3), Duration::from_secs(4));
        assert_eq!(restart_backoff(4), Duration::from_secs(8));
        assert_eq!(restart_backoff(5), Duration::from_secs(16));
        // Capped at 30 s no matter how high the failure count climbs.
        assert_eq!(restart_backoff(6), Duration::from_secs(30));
        assert_eq!(restart_backoff(50), Duration::from_secs(30));
    }

    #[test]
    fn stats_snapshot_reads_counters() {
        let s = LocalAudioStats::default();
        s.tx_blocks.fetch_add(3, Ordering::Relaxed);
        s.spawns.fetch_add(1, Ordering::Relaxed);
        let f = s.snapshot();
        assert_eq!(f.tx_blocks, 3);
        assert_eq!(f.spawns, 1);
        assert_eq!(f.rx_blocks, 0);
    }
}
