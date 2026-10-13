//! issue 1424 part 3: the hub's own HDMI audio leg, a supervised `aplay` on a named connector's hw
//! PCM (the strih-pp vestibule: lv1's program on HDMI-1, straight from the hub).
//!
//! The same egress shape as the pw-cat playback legs ([`crate::local_audio`]): an OS thread owns the
//! child, the block loop `try_send`s one interleaved PCM16 block per tick, and every write goes
//! through the pipe fill guard ([`PipeFillWriter`]: FIONREAD before every block and about every
//! millisecond between them, the starvation top-up, the start hold and the drift servo, since the
//! HDMI PCM runs on the GPU's audio clock, not the hub's). aplay reads one 256-frame ALSA period at
//! a time, so the guard is built for that read size: 640 frames held in the pipe (13.3 ms) plus the
//! 1024-frame ALSA buffer, where a pw-cat pipe holds 1792.
//!
//! Before every spawn the leg resolves the connector again ([`resolve_connector`]): no match, an
//! ambiguous one or no EDID keeps the leg silent and logs why ONCE per change, and a busy or vanished
//! PCM ends aplay; both retry with [`restart_backoff`] (1 s doubling to 30 s), so a converter plugged
//! in later, or an ELD that NVIDIA publishes only once HDMI-1 is driven, is picked up by itself. The
//! audio queued while the leg was silent is dropped before a spawn, so it never plays late.

use std::io::{self, BufRead, BufReader};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::drm_output_audio_match::reason;
use crate::hdmi_audio::{
    aplay_argv, iec958_on, is_xrun_line, resolve_connector, HdmiLegStats, HdmiResolution,
    ALSA_HDMI_BUFFER_FRAMES, ALSA_HDMI_PERIOD_FRAMES,
};
use crate::local_audio::{
    log_pipe_write, restart_backoff, LocalAudioSink, LocalAudioStats, PipeFillWriter,
    EGRESS_QUEUE_BLOCKS, PIPE_SAMPLE_INTERVAL,
};
use crate::pipe_fill::{PipeFillControl, PipeServoDepth, PipeWriteReport};

/// An aplay that ran this long before it exited is not counted toward the backoff (an unplugged
/// converter after an hour of playing retries at once, a PCM that is busy at every open backs off).
const APLAY_STABLE_RUN: Duration = Duration::from_secs(10);

/// The pipe fill controller of an aplay leg: aplay reads one ALSA period at a time, and at its start
/// it pulls its whole ALSA buffer plus the period it then holds while blocked on the full PCM.
pub fn aplay_pipe_control(rate: u32) -> PipeFillControl {
    PipeFillControl::with_quantum(rate, ALSA_HDMI_PERIOD_FRAMES)
        .with_reader_buffer(ALSA_HDMI_BUFFER_FRAMES + ALSA_HDMI_PERIOD_FRAMES)
}

/// A supervised `aplay` on one hw PCM, fed interleaved PCM16 on stdin through the pipe fill guard.
pub struct AplaySink {
    child: Child,
    stdin: PipeFillWriter<ChildStdin>,
    /// The stderr reader: counts the xruns, and returns aplay's last other line at its exit (the
    /// open error when the PCM is busy).
    stderr_reader: Option<thread::JoinHandle<String>>,
}

impl AplaySink {
    /// Spawn aplay on `hw:<card>,<device>`; its xruns are counted into `hdmi`.
    pub fn spawn(
        card: i32,
        device: i32,
        rate: u32,
        channels: u8,
        hdmi: Arc<HdmiLegStats>,
    ) -> io::Result<Self> {
        let argv = aplay_argv(card, device, rate, channels);
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("aplay stdin not piped"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("aplay stderr not piped"))?;
        // Ends at aplay's exit (EOF on its stderr).
        let stderr_reader = thread::spawn(move || {
            let mut last = String::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if is_xrun_line(&line) {
                    hdmi.record_xrun();
                } else if !line.trim().is_empty() {
                    last = line;
                }
            }
            last
        });
        Ok(AplaySink {
            child,
            stdin: PipeFillWriter::with_control(stdin, channels, aplay_pipe_control(rate)),
            stderr_reader: Some(stderr_reader),
        })
    }

    /// End aplay (if it still runs) and return its last stderr line that was not an xrun.
    fn finish(&mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.stderr_reader
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
    }
}

impl LocalAudioSink for AplaySink {
    fn write_block(&mut self, interleaved: &[i16]) -> io::Result<PipeWriteReport> {
        self.stdin.write_block(interleaved)
    }

    fn sample_fill(&mut self) -> io::Result<()> {
        self.stdin.sample_fill()
    }

    fn servo_depth(&self) -> Option<PipeServoDepth> {
        self.stdin.servo_depth()
    }
}

impl Drop for AplaySink {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The parameters of one HDMI leg ([`spawn_alsa_hdmi_sink`]).
pub struct AlsaHdmiSinkConfig {
    /// The X RandR output name (`HDMI-1`), resolved to its hw PCM before every spawn.
    pub connector: String,
    pub rate: u32,
    pub channels: u8,
}

/// Spawn the supervised HDMI leg thread and return the feed the block loop `try_send`s the
/// participant's N-1 bus into (a full queue drops the block, never blocking the mix loop). The
/// thread runs until the sender is dropped (hub shutdown).
pub fn spawn_alsa_hdmi_sink(
    cfg: AlsaHdmiSinkConfig,
    stats: Arc<LocalAudioStats>,
    hdmi: Arc<HdmiLegStats>,
) -> SyncSender<Vec<i16>> {
    let (tx, rx) = sync_channel::<Vec<i16>>(EGRESS_QUEUE_BLOCKS);
    thread::spawn(move || alsa_hdmi_sink_loop(cfg, rx, stats, hdmi));
    tx
}

/// Drop every block queued while the leg was silent. `false` once the hub is shutting down.
fn drop_stale(rx: &Receiver<Vec<i16>>) -> bool {
    loop {
        match rx.try_recv() {
            Ok(_) => {}
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => return false,
        }
    }
}

fn alsa_hdmi_sink_loop(
    cfg: AlsaHdmiSinkConfig,
    rx: Receiver<Vec<i16>>,
    stats: Arc<LocalAudioStats>,
    hdmi: Arc<HdmiLegStats>,
) {
    let AlsaHdmiSinkConfig {
        connector,
        rate,
        channels,
    } = cfg;
    let mut failures: u32 = 0;
    let mut logged: Option<HdmiResolution> = None;
    loop {
        let backoff = restart_backoff(failures);
        if !backoff.is_zero() {
            thread::sleep(backoff);
        }
        if !drop_stale(&rx) {
            return;
        }
        let (res, pcms) = resolve_connector(&connector);
        hdmi.record_resolution(&res);
        let Some((card, device)) = res.pcm else {
            if logged != Some(res) {
                tracing::warn!(%connector, reason = reason(res.result), "local-audio: HDMI leg {connector} has no audio device -- the leg stays silent and re-resolves with a backoff");
                logged = Some(res);
            }
            failures = failures.saturating_add(1);
            continue;
        };
        let hw = format!("hw:{card},{device}");
        let iec958 = iec958_on(card, device, &pcms);
        if logged != Some(res) {
            tracing::info!(%connector, %hw, %iec958, "local-audio: HDMI leg {connector} resolved to {hw}");
            logged = Some(res);
        }
        let mut sink = match AplaySink::spawn(card, device, rate, channels, hdmi.clone()) {
            Ok(sink) => sink,
            Err(e) => {
                failures = failures.saturating_add(1);
                tracing::warn!(%connector, %hw, error = %e, "local-audio: HDMI leg aplay spawn failed -- backing off");
                continue;
            }
        };
        stats.record_spawn();
        hdmi.set_running(true);
        let started = Instant::now();
        let label = "HDMI leg";
        let err = loop {
            match rx.recv_timeout(PIPE_SAMPLE_INTERVAL) {
                Ok(block) => match sink.write_block(&block) {
                    Ok(report) => {
                        stats.record_write(&report);
                        stats.record_servo_depth(sink.servo_depth());
                        log_pipe_write(label, &hw, &report);
                    }
                    Err(e) => break e,
                },
                Err(RecvTimeoutError::Timeout) => {
                    if let Err(e) = sink.sample_fill() {
                        break e;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };
        hdmi.set_running(false);
        stats.record_exit();
        failures = if started.elapsed() >= APLAY_STABLE_RUN {
            1
        } else {
            failures.saturating_add(1)
        };
        tracing::warn!(%connector, %hw, error = %err, aplay = %sink.finish(), "local-audio: HDMI leg aplay exited -- re-resolving the connector");
    }
}
