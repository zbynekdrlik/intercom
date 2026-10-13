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
//! in later, or an ELD that NVIDIA publishes only once HDMI-1 is driven, is picked up by itself.
//!
//! While aplay plays, a watcher re-reads the PCM's ELD every [`ELD_WATCH_INTERVAL`]: NVIDIA's HDMI
//! PCM map follows the ELD, so a re-mode or an output re-acquire (the OBS output starting on the
//! same GPU) can move the connector to another PCM. Two readings in a row that no longer show the
//! matched monitor end aplay and the leg re-resolves; it never keeps writing to a PCM that now
//! carries another screen. The audio queued while the leg was silent is dropped right before a
//! spawn, so it never plays late.

use std::io::{self, BufRead, BufReader};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::drm_output_audio_match::{reason, MonitorId};
use crate::hdmi_audio::{
    aplay_argv, eld_still_matches, iec958_on, is_banner_line, is_xrun_line, read_pcm_eld,
    resolve_connector, HdmiLegStats, HdmiResolution, ALSA_HDMI_BUFFER_FRAMES,
    ALSA_HDMI_PERIOD_FRAMES,
};
use crate::local_audio::{
    log_pipe_write, restart_backoff, LocalAudioSink, LocalAudioStats, PipeFillWriter,
    EGRESS_QUEUE_BLOCKS, PIPE_SAMPLE_INTERVAL,
};
use crate::pipe_fill::{PipeFillControl, PipeServoDepth, PipeWriteReport};

/// An aplay that ran this long before it exited is not counted toward the backoff (an unplugged
/// converter after an hour of playing retries at once, a PCM that is busy at every open backs off).
const APLAY_STABLE_RUN: Duration = Duration::from_secs(10);

/// How often the watcher re-reads the playing PCM's ELD (one bounded amixer call, no X call).
pub const ELD_WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// Consecutive ELD readings that no longer show the matched monitor before aplay is ended.
pub const ELD_WATCH_MISSES: u32 = 2;

/// The pipe fill controller of an aplay leg: aplay reads one ALSA period at a time, and at its start
/// it pulls its whole ALSA buffer plus the period it then holds while blocked on the full PCM.
pub fn aplay_pipe_control(rate: u32) -> PipeFillControl {
    PipeFillControl::with_quantum(rate, ALSA_HDMI_PERIOD_FRAMES)
        .with_reader_buffer(ALSA_HDMI_BUFFER_FRAMES + ALSA_HDMI_PERIOD_FRAMES)
}

/// The watcher's decision after one ELD reading: `Some(true)` the PCM still carries the monitor,
/// `Some(false)` it does not, `None` the ELD could not be read (never counted either way).
/// Returns the new miss count and whether aplay must end.
pub fn eld_watch_step(misses: u32, reading: Option<bool>) -> (u32, bool) {
    let misses = match reading {
        Some(true) => 0,
        Some(false) => misses.saturating_add(1),
        None => misses,
    };
    (misses, misses >= ELD_WATCH_MISSES)
}

/// A supervised `aplay` on one hw PCM, fed interleaved PCM16 on stdin through the pipe fill guard.
pub struct AplaySink {
    child: Child,
    stdin: PipeFillWriter<ChildStdin>,
    /// The stderr reader: counts the xruns, and returns aplay's last other line at its exit (the
    /// open error when the PCM is busy).
    stderr_reader: Option<thread::JoinHandle<String>>,
    /// Set by the ELD watcher once the PCM no longer carries the matched monitor.
    pcm_moved: Arc<AtomicBool>,
    /// Ends the ELD watcher.
    stop: Arc<AtomicBool>,
}

impl AplaySink {
    /// Spawn aplay on `hw:<card>,<device>`; its xruns are counted into `hdmi`. `watch` is the
    /// monitor the PCM was matched to: the watcher ends the sink once the PCM stops carrying it.
    pub fn spawn(
        card: i32,
        device: i32,
        rate: u32,
        channels: u8,
        hdmi: Arc<HdmiLegStats>,
        watch: Option<MonitorId>,
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
        let stderr_reader = thread::Builder::new()
            .name("hdmi-leg-stderr".into())
            .spawn(move || {
                let mut last = String::new();
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if is_xrun_line(&line) {
                        hdmi.record_xrun();
                    } else if !line.trim().is_empty() && !is_banner_line(&line) {
                        last = line;
                    }
                }
                last
            })?;
        let pcm_moved = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        if let Some(want) = watch {
            let (moved, stop) = (pcm_moved.clone(), stop.clone());
            thread::Builder::new()
                .name("hdmi-leg-eld".into())
                .spawn(move || eld_watch(card, device, &want, &moved, &stop))?;
        }
        Ok(AplaySink {
            child,
            stdin: PipeFillWriter::with_control(stdin, channels, aplay_pipe_control(rate)),
            stderr_reader: Some(stderr_reader),
            pcm_moved,
            stop,
        })
    }

    /// aplay has read from the pipe since its spawn: it is playing.
    pub fn playing(&self) -> bool {
        self.stdin.started()
    }

    /// The ELD watcher saw the PCM stop carrying the matched monitor.
    pub fn pcm_moved(&self) -> bool {
        self.pcm_moved.load(Ordering::Relaxed)
    }

    /// End aplay (if it still runs) and the watcher; return aplay's last stderr line that was not an
    /// xrun or its start banner.
    fn finish(&mut self) -> String {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.stderr_reader
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
    }
}

/// The ELD watcher of one playing PCM (see the module doc). Sleeps in short slices so the sink's
/// end stops it promptly.
fn eld_watch(card: i32, device: i32, want: &MonitorId, moved: &AtomicBool, stop: &AtomicBool) {
    let mut misses = 0;
    loop {
        let mut slept = Duration::ZERO;
        while slept < ELD_WATCH_INTERVAL {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
            slept += Duration::from_millis(100);
        }
        let reading = read_pcm_eld(card, device).map(|eld| eld_still_matches(want, &eld));
        let (m, end) = eld_watch_step(misses, reading);
        misses = m;
        if end {
            moved.store(true, Ordering::Relaxed);
            return;
        }
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
        self.stop.store(true, Ordering::Relaxed);
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

/// Spawn the supervised HDMI leg thread (`hdmi-leg`) and return the feed the block loop `try_send`s
/// the participant's N-1 bus into (a full queue drops the block, never blocking the mix loop). The
/// thread runs until the sender is dropped (hub shutdown).
pub fn spawn_alsa_hdmi_sink(
    cfg: AlsaHdmiSinkConfig,
    stats: Arc<LocalAudioStats>,
    hdmi: Arc<HdmiLegStats>,
) -> SyncSender<Vec<i16>> {
    let (tx, rx) = sync_channel::<Vec<i16>>(EGRESS_QUEUE_BLOCKS);
    thread::Builder::new()
        .name("hdmi-leg".into())
        .spawn(move || alsa_hdmi_sink_loop(cfg, rx, stats, hdmi))
        .expect("spawn the hdmi-leg thread");
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
    let mut logged: Option<(HdmiResolution, Option<String>)> = None;
    loop {
        let backoff = restart_backoff(failures);
        if !backoff.is_zero() {
            thread::sleep(backoff);
        }
        let resolved = resolve_connector(&connector);
        let res = resolved.res;
        hdmi.record_resolution(&res);
        let state = (res, resolved.x_error.clone());
        let Some((card, device)) = res.pcm else {
            if logged.as_ref() != Some(&state) {
                let x = resolved.x_error.as_deref().unwrap_or("-");
                tracing::warn!(%connector, reason = reason(res.result), x_error = %x, "local-audio: HDMI leg {connector} has no audio device -- the leg stays silent and re-resolves with a backoff");
                logged = Some(state);
            }
            failures = failures.saturating_add(1);
            // A silent leg still drains its feed, so a hub shutdown ends the thread.
            if !drop_stale(&rx) {
                return;
            }
            continue;
        };
        let hw = format!("hw:{card},{device}");
        let iec958 = iec958_on(card, device, &resolved.pcms);
        if logged.as_ref() != Some(&state) {
            tracing::info!(%connector, %hw, %iec958, "local-audio: HDMI leg {connector} resolved to {hw}");
            logged = Some(state);
        }
        // The audio queued while the leg was silent or resolving is stale: never play it late.
        if !drop_stale(&rx) {
            return;
        }
        let mut sink = match AplaySink::spawn(
            card,
            device,
            rate,
            channels,
            hdmi.clone(),
            resolved.matched,
        ) {
            Ok(sink) => sink,
            Err(e) => {
                failures = failures.saturating_add(1);
                tracing::warn!(%connector, %hw, error = %e, "local-audio: HDMI leg aplay spawn failed -- backing off");
                continue;
            }
        };
        stats.record_spawn();
        let started = Instant::now();
        let mut playing = false;
        let label = "HDMI leg";
        // The feed loop of `local_audio::local_sink_loop`, kept as a copy: anchor tests pin that
        // function's body text, so it is not factored out. Plus the playing flag and the ELD watch.
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
                    if sink.pcm_moved() {
                        break io::Error::other(
                            "the PCM no longer carries the connector's monitor (its ELD changed)",
                        );
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if !playing && sink.playing() {
                playing = true;
                hdmi.set_running(true);
            }
        };
        hdmi.set_running(false);
        stats.record_exit();
        failures = if started.elapsed() >= APLAY_STABLE_RUN {
            1
        } else {
            failures.saturating_add(1)
        };
        let aplay = sink.finish();
        tracing::warn!(%connector, %hw, error = %err, %aplay, "local-audio: HDMI leg aplay exited -- re-resolving the connector");
    }
}
