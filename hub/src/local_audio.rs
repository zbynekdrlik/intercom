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
//! `strih-program.monitor` as its `ASIO zvuk` program input.
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

/// Build the `pw-cat` PLAYBACK argv for the program sink (egress). Raw interleaved s16 PCM is fed on
/// stdin (`-`); `--target` names the operator's `strih-program` sink node.
pub fn pw_cat_playback_argv(target: &str, rate: u32, channels: u8) -> Vec<String> {
    vec![
        "pw-cat".into(),
        "--playback".into(),
        "--raw".into(),
        "--rate".into(),
        rate.to_string(),
        "--channels".into(),
        channels.to_string(),
        "--format".into(),
        PW_CAT_FORMAT.into(),
        "--target".into(),
        target.to_string(),
        "-".into(),
    ]
}

/// Build the `pw-cat` RECORD argv for a capture source (ingress). Raw interleaved s16 PCM comes out
/// on stdout (`-`); `--target` names the MiniFuse 4 capture node.
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
}

impl LocalAudioStats {
    /// A point-in-time snapshot for `/api/state`.
    pub fn snapshot(&self) -> LocalAudioFacet {
        LocalAudioFacet {
            tx_blocks: self.tx_blocks.load(Ordering::Relaxed),
            rx_blocks: self.rx_blocks.load(Ordering::Relaxed),
            spawns: self.spawns.load(Ordering::Relaxed),
            exits: self.exits.load(Ordering::Relaxed),
        }
    }
}

/// A local audio EGRESS sink: write interleaved PCM16 blocks to a PipeWire node.
pub trait LocalAudioSink: Send {
    /// Write one interleaved PCM16 block; an `Err` means the underlying child died (the supervisor
    /// respawns).
    fn write_block(&mut self, interleaved: &[i16]) -> io::Result<()>;
}

/// A local audio INGRESS source: read one fixed-size raw block from a PipeWire node.
pub trait LocalAudioSource: Send {
    /// Fill `buf` with exactly one block of raw little-endian s16 bytes; an `Err` (incl. EOF) means
    /// the child died (the supervisor respawns).
    fn read_block(&mut self, buf: &mut [u8]) -> io::Result<()>;
}

/// A supervised `pw-cat --playback` child fed interleaved PCM16 on stdin.
pub struct PwCatSink {
    child: Child,
    stdin: ChildStdin,
}

impl PwCatSink {
    /// Spawn `pw-cat --playback` targeting `target` at `rate`/`channels`.
    pub fn spawn(target: &str, rate: u32, channels: u8) -> io::Result<Self> {
        let argv = pw_cat_playback_argv(target, rate, channels);
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
        Ok(PwCatSink { child, stdin })
    }
}

impl LocalAudioSink for PwCatSink {
    fn write_block(&mut self, interleaved: &[i16]) -> io::Result<()> {
        self.stdin.write_all(&interleaved_to_le_bytes(interleaved))
    }
}

impl Drop for PwCatSink {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A supervised `pw-cat --record` child producing interleaved PCM16 on stdout.
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

/// Spawn the supervised EGRESS thread that owns the [`PwCatSink`] and returns the [`SyncSender`] the
/// block loop feeds one interleaved PCM16 block per tick. A full queue drops the block (best-effort
/// `try_send`, like the Janus feed). The thread respawns a died `pw-cat` with [`restart_backoff`],
/// forever, until the sender is dropped (hub shutdown).
pub fn spawn_program_sink(
    target: String,
    rate: u32,
    channels: u8,
    stats: Arc<LocalAudioStats>,
) -> SyncSender<Vec<i16>> {
    let (tx, rx) = sync_channel::<Vec<i16>>(EGRESS_QUEUE_BLOCKS);
    thread::spawn(move || program_sink_loop(target, rate, channels, rx, stats));
    tx
}

fn program_sink_loop(
    target: String,
    rate: u32,
    channels: u8,
    rx: Receiver<Vec<i16>>,
    stats: Arc<LocalAudioStats>,
) {
    let mut failures: u32 = 0;
    loop {
        let backoff = restart_backoff(failures);
        if !backoff.is_zero() {
            thread::sleep(backoff);
        }
        match PwCatSink::spawn(&target, rate, channels) {
            Ok(mut sink) => {
                stats.spawns.fetch_add(1, Ordering::Relaxed);
                failures = 0;
                tracing::info!(%target, rate, channels, "local-audio: program sink pw-cat spawned");
                loop {
                    match rx.recv() {
                        Ok(block) => {
                            if sink.write_block(&block).is_err() {
                                // The child died mid-write — break to respawn.
                                break;
                            }
                            stats.tx_blocks.fetch_add(1, Ordering::Relaxed);
                        }
                        // The sender was dropped: the hub is shutting down.
                        Err(_) => return,
                    }
                }
                stats.exits.fetch_add(1, Ordering::Relaxed);
                failures = failures.saturating_add(1);
                tracing::warn!(%target, "local-audio: program sink pw-cat exited — respawning");
            }
            Err(e) => {
                failures = failures.saturating_add(1);
                tracing::warn!(%target, error=%e, "local-audio: program sink pw-cat spawn failed — backing off");
            }
        }
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
