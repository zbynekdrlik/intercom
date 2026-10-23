//! The observable hub state served at `/api/state` + pushed on `/ws` (issue 1345 M1).
//!
//! Per participant: name/role/adapter/host plus the live counters a dev1 watchdog reads — rx/tx
//! packets, jitter underruns/overruns, the age of the last received packet, and the input level in
//! dBFS. A VBAN leg also carries its jitter-buffer facet (target, depth, servo corrections, issue
//! 1401). The whole snapshot is rebuilt each block-status tick from the jitter buffers.

use serde::Serialize;

use crate::alsa_card::{AlsaDirFacet, AlsaLegFacet, ALSA_CARD_PERIODS};
use crate::hdmi_audio::{HdmiLegFacet, ALSA_HDMI_BUFFER_FRAMES, ALSA_HDMI_PERIOD_FRAMES};
use crate::janus_rtp::JanusStats;
use crate::local_audio::LocalAudioFacet;
use crate::matrix::Matrix;
use crate::ndi_video::VideoStats;
use crate::vban_jitter::NetworkFillStats;

/// A VBAN leg's jitter buffer as `/api/state` shows it (issue 1401): the target and the measured
/// depth in frames (the hub `sample_rate` turns them into ms), and how often the drift servo
/// dropped or repeated a single frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct JitterFacet {
    /// The target pre-pop fill: live, a program feed's follows its sender's gaps.
    pub target_frames: usize,
    /// The mean pre-pop fill over the last 1 s window (0 before the first window).
    pub depth_frames: usize,
    /// The lowest pre-pop fill in that window (the underrun margin is this minus one block).
    pub depth_min_frames: usize,
    /// Single frames dropped because the fill sat high (the sender runs fast).
    pub servo_drops: u64,
    /// Single frames repeated because the fill sat low (the sender runs slow).
    pub servo_repeats: u64,
    /// The fill the servo holds now: `target_frames`, or while a target change is walked in (about
    /// 37 s a block), the point the walk has reached. `depth_frames` follows this, not the target.
    pub setpoint_frames: usize,
    /// How many of `servo_drops` + `servo_repeats` walked a target change in (at most 7 a second);
    /// the rest corrected the sender's drift.
    pub servo_walk_steps: u64,
    /// Times the stream stopped for more than 500 ms and came back: a sender outage on a program
    /// feed, simply a mute on a cambox (it sends only while unmuted).
    pub stalls: u64,
    /// Audio is flowing (false while priming: before the first packet or after an underrun).
    pub primed: bool,
    /// A program feed's largest gap between two packets in the last 10 min, in ms (0.1 ms
    /// resolution): what its adaptive target follows. Omitted for a leg with a fixed target (the
    /// camboxes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_gap_ms_10min: Option<f64>,
}

/// The hub's HDMI audio leg as `/api/state` shows it (issue 1424 part 3): the connector the matrix
/// names, the hw PCM it resolved to (or why it is silent), whether aplay runs, and its xruns. The
/// pipe's fill and depth are in the participant's `local_audio` facet.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HdmiLegState {
    pub connector: String,
    /// The connector matched exactly one HDMI PCM.
    pub resolved: bool,
    /// `hw:<card>,<device>` once resolved, else null.
    pub hw: Option<String>,
    /// The named reason of the last resolve (`matched`, or why the leg is silent).
    pub reason: &'static str,
    /// aplay is playing on `hw` (it has read from the pipe since its spawn).
    pub running: bool,
    /// Resolves since the hub started (one before every aplay spawn).
    pub resolves: u64,
    /// aplay's xruns (`underrun!!!`) since the hub started.
    pub xruns: u64,
    /// The leg's fixed delay after the pipe: aplay's ALSA buffer (kept full) and the period it holds.
    /// The leg's total added delay = `local_audio.pipe_depth_frames` + these two (frames at the hub
    /// rate), on top of the program feed's jitter target.
    pub alsa_buffer_frames: usize,
    pub alsa_period_frames: usize,
}

impl HdmiLegState {
    fn new(connector: String, f: HdmiLegFacet) -> Self {
        let hw = f.hw();
        HdmiLegState {
            connector,
            resolved: hw.is_some(),
            hw,
            reason: f.reason(),
            running: f.running,
            resolves: f.resolves,
            xruns: f.xruns,
            alsa_buffer_frames: ALSA_HDMI_BUFFER_FRAMES,
            alsa_period_frames: ALSA_HDMI_PERIOD_FRAMES,
        }
    }
}

/// One side of a card leg as `/api/state` shows it (issue 1438).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AlsaSideState {
    /// `ready`, `card not present`, the named reason the card cannot be opened, or `not started
    /// yet`.
    pub state: &'static str,
    /// The hw sample format the child speaks with the card (`S16_LE` / `S32_LE` / `S24_3LE`) once
    /// ready, else null.
    pub format: Option<&'static str>,
    /// The card's channel count on this side (the hw PCM opens with exactly this many).
    pub hw_channels: usize,
    /// The child is moving audio (aplay has read from its pipe / arecord delivered a block).
    pub running: bool,
    /// The child's xruns since the hub started (aplay `underrun!!!` / arecord `overrun!!!`).
    pub xruns: u64,
    /// Child spawns since the hub started.
    pub spawns: u64,
}

impl AlsaSideState {
    fn new(f: AlsaDirFacet, hw_channels: usize) -> Self {
        AlsaSideState {
            state: f.state.name(),
            format: f.state.format().map(|f| f.alsa_name()),
            hw_channels,
            running: f.running,
            xruns: f.xruns,
            spawns: f.spawns,
        }
    }
}

/// The card leg as `/api/state` shows it (issue 1438): the card the matrix names, whether it was on
/// the box at the last look, and each side the participant uses (`null` for an unused side). The
/// playback pipe's fill and depth are in the participant's `local_audio` facet.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AlsaLegState {
    /// The ALSA card ID (`M4`, `MGXU`).
    pub card: String,
    /// `hw:CARD=<id>,DEV=<n>`.
    pub pcm: String,
    /// The card was on the box at the last look of either side.
    pub present: bool,
    pub playback: Option<AlsaSideState>,
    pub capture: Option<AlsaSideState>,
    /// The ALSA period and buffer both sides open with (frames at the hub rate).
    pub alsa_period_frames: usize,
    pub alsa_buffer_frames: usize,
}

impl AlsaLegState {
    fn new(spec: &crate::alsa_card::AlsaCardSpec, f: AlsaLegFacet) -> Self {
        AlsaLegState {
            card: spec.card.clone(),
            pcm: crate::alsa_card::pcm_name(&spec.card, spec.device),
            present: f.card_present(),
            playback: (spec.out_channels > 0)
                .then(|| AlsaSideState::new(f.playback, spec.hw_out_channels)),
            capture: (spec.in_channels > 0)
                .then(|| AlsaSideState::new(f.capture, spec.hw_in_channels)),
            alsa_period_frames: spec.period_frames,
            alsa_buffer_frames: spec.period_frames * ALSA_CARD_PERIODS,
        }
    }

    /// Every side the participant uses is moving audio.
    pub fn up(&self) -> bool {
        self.playback.iter().chain(&self.capture).all(|s| s.running)
    }
}

impl From<NetworkFillStats> for JitterFacet {
    fn from(s: NetworkFillStats) -> Self {
        JitterFacet {
            target_frames: s.target_frames,
            depth_frames: s.depth_frames,
            depth_min_frames: s.depth_min_frames,
            servo_drops: s.servo_drops,
            servo_repeats: s.servo_repeats,
            setpoint_frames: s.setpoint_frames,
            servo_walk_steps: s.servo_walk_steps,
            stalls: s.stalls,
            primed: s.primed,
            max_gap_ms_10min: s
                .max_gap_us_10min
                .map(|us| (us as f64 / 100.0).round() / 10.0),
        }
    }
}

/// Per-participant runtime counters, gathered from its jitter buffer + tx side each tick.
#[derive(Debug, Clone, Copy, Default)]
pub struct RuntimeStats {
    pub rx_packets: u64,
    pub tx_packets: u64,
    /// VBAN packets to this participant dropped because its own send socket was backed up (issue
    /// 1401: a NIC or queue stall), instead of blocking the block loop.
    pub tx_dropped: u64,
    pub underruns: u64,
    pub overruns: u64,
    pub last_rx_age_ms: Option<u64>,
    pub level_dbfs: f32,
    /// The sample rate of the participant's last VBAN packet (issue 1345), `None` for a non-VBAN
    /// participant or before its first packet.
    pub sample_rate: Option<u32>,
    /// VBAN packets dropped because their rate is not 1x / 2x / 4x the hub rate (issue 1345).
    pub rate_rejects: u64,
    /// VBAN packets of this participant's stream that could not be decoded (issue 1467: an
    /// unsupported codec, a non-audio sub-protocol, a payload without one whole frame).
    pub decode_rejects: u64,
    /// The Janus audiobridge facet, present only for the `janus`-adapter participant (M3a).
    pub janus: Option<JanusStats>,
    /// The local PipeWire facet, present only for a `pipewire`-adapter participant — the
    /// `program_out` sink + the talkback capture (issue 1344).
    pub local_audio: Option<LocalAudioFacet>,
    /// The VBAN leg's jitter-buffer facet (issue 1401), `None` for the other participants.
    pub jitter: Option<JitterFacet>,
    /// The HDMI audio leg facet (issue 1424 part 3), `None` for every non-`alsa_hdmi` participant.
    pub hdmi: Option<HdmiLegFacet>,
    /// The card leg facet (issue 1438), `None` for every non-`alsa` participant.
    pub alsa: Option<AlsaLegFacet>,
}

/// One participant's serialized state.
#[derive(Debug, Clone, Serialize)]
pub struct ParticipantState {
    pub name: String,
    pub role: String,
    pub adapter: String,
    pub host: Option<String>,
    pub rx_packets: u64,
    pub tx_packets: u64,
    /// VBAN packets to this participant dropped on a backed-up send socket (issue 1401). 0 in a
    /// healthy run; a cambox that is off stays 0 too (the kernel discards its packets, and its
    /// `tx_packets` keeps counting).
    pub tx_dropped: u64,
    pub underruns: u64,
    pub overruns: u64,
    pub last_rx_age_ms: Option<u64>,
    pub level_dbfs: f32,
    /// The input stream's VBAN sample rate in Hz (issue 1345: a 96 kHz source is decimated to the hub
    /// rate) — omitted for a non-VBAN participant or before its first packet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    /// VBAN packets dropped because their rate is not 1x / 2x / 4x the hub rate — present for
    /// every VBAN participant, omitted for the others.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_rejects: Option<u64>,
    /// VBAN packets of this participant's stream that could not be decoded (issue 1467): the name
    /// is known but the codec / sub-protocol / payload is not, so the leg reads silent. Present for
    /// every VBAN participant, omitted for the others.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode_rejects: Option<u64>,
    /// The Janus audiobridge facet (joined / session age / rejoins / rtp packet counts), present
    /// only for the janus participant — omitted from the JSON for every other participant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub janus: Option<JanusStats>,
    /// The local PipeWire facet (tx/rx blocks, pw-cat spawns/exits), present only for a
    /// `pipewire`-adapter participant — omitted from the JSON for every other participant (issue 1344).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_audio: Option<LocalAudioFacet>,
    /// The VBAN leg's jitter buffer (target / depth / servo drops + repeats, issue 1401), present
    /// only for a VBAN participant — omitted from the JSON for every other participant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jitter: Option<JitterFacet>,
    /// The HDMI audio leg (issue 1424 part 3), present only for an `alsa_hdmi` participant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hdmi: Option<HdmiLegState>,
    /// The card leg (issue 1438), present only for an `alsa` participant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alsa: Option<AlsaLegState>,
}

/// The whole hub state (the `/api/state` body + each `/ws` push).
#[derive(Debug, Clone, Serialize)]
pub struct HubState {
    pub version: String,
    pub sample_rate: u32,
    pub block_frames: usize,
    pub participants: Vec<ParticipantState>,
    /// The Interkom picture (MJPEG) facet (source/connected/fps_actual/last_frame_age_ms/frames/
    /// last_error), present only when the hub has a `[video]` config (M3c) — omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoStats>,
    /// Hub block-loop ticks run late since the daemon started (issue 1401, design 5980775411): the
    /// loop woke more than a period late and ran the missed cycles at once, so no output lost a
    /// block. Set by the daemon after the snapshot; the pure snapshot has no tick history.
    pub caught_up_ticks: u64,
    /// Hub block-loop ticks given up since the daemon started: the part of a late wake beyond the
    /// four it runs late. Each one is a block lost on EVERY output, the program sink included (the
    /// VBAN legs give the same block up). Set by the daemon after the snapshot.
    pub lost_ticks: u64,
    /// The hub-mix block loop thread's scheduling class as it read it at start (`SCHED_FIFO 10`,
    /// or `SCHED_OTHER` when the unit's realtime grant is missing): a refused grant is visible
    /// here without ssh. Set by the daemon; omitted from the pure snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mix_thread_sched: Option<String>,
    /// Whether the hub locked its memory in RAM before it started the hub-mix thread (issue 1401,
    /// design 6265012279: `mlockall`, granted by the unit's `LimitMEMLOCK=infinity`). `false` = the
    /// hub runs unlocked and its real-time thread can stall in a page fault under memory pressure
    /// (verify-strih item 16 FAILs it). Set by the daemon; omitted from the pure snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_locked: Option<bool>,
}

impl HubState {
    /// Build a snapshot from the matrix + one [`RuntimeStats`] per participant (id-indexed). A
    /// missing stats entry renders as zeros (a participant that has produced no traffic yet).
    pub fn snapshot(matrix: &Matrix, version: &str, stats: &[RuntimeStats]) -> Self {
        let participants = matrix
            .participants
            .iter()
            .enumerate()
            .map(|(id, p)| {
                let s = stats.get(id).copied().unwrap_or_default();
                let vban = p.adapter == crate::matrix::ADAPTER_VBAN;
                ParticipantState {
                    name: p.name.clone(),
                    role: p.role.clone(),
                    adapter: p.adapter.clone(),
                    host: p.host.clone(),
                    rx_packets: s.rx_packets,
                    tx_packets: s.tx_packets,
                    tx_dropped: s.tx_dropped,
                    underruns: s.underruns,
                    overruns: s.overruns,
                    last_rx_age_ms: s.last_rx_age_ms,
                    level_dbfs: s.level_dbfs,
                    sample_rate: s.sample_rate.filter(|_| vban),
                    rate_rejects: vban.then_some(s.rate_rejects),
                    decode_rejects: vban.then_some(s.decode_rejects),
                    janus: s.janus,
                    local_audio: s.local_audio,
                    jitter: s.jitter.filter(|_| vban),
                    hdmi: s
                        .hdmi
                        .filter(|_| p.adapter == crate::matrix::ADAPTER_ALSA_HDMI)
                        .map(|f| {
                            HdmiLegState::new(p.hdmi_connector.clone().unwrap_or_default(), f)
                        }),
                    // the spec was resolved once at load: no parsing on the hub-mix thread
                    alsa: matrix
                        .alsa_spec(id)
                        .map(|spec| AlsaLegState::new(spec, s.alsa.unwrap_or_default())),
                }
            })
            .collect();
        HubState {
            version: version.to_string(),
            sample_rate: matrix.hub.sample_rate,
            block_frames: matrix.hub.block_frames,
            participants,
            // The video facet is attached by the daemon (main.rs) after the snapshot when a `[video]`
            // config is present; the pure snapshot has no picture state of its own.
            video: None,
            caught_up_ticks: 0,
            lost_ticks: 0,
            mix_thread_sched: None,
            memory_locked: None,
        }
    }

    /// A one-line status summary for the periodic log (a dev1 watchdog greps it): the underruns and
    /// overruns summed over every participant, each naming the leg with the most (issue 1401:
    /// `underruns=220(fohabl)`), + the participant levels, so a dead/underrunning leg is visible
    /// between E2E runs. When there are any, it also shows the non-cambox VBAN legs' stalls (a
    /// program feed that stopped for more than 500 ms and came back, `stalls=1(fohabl)`; a cambox
    /// stops on every mute, so it is left out), the block loop's lost ticks as `lost=N` (a block
    /// lost on every output; a tick it caught up is no loss and stays off the line), the VBAN
    /// packets dropped on a backed-up send socket as
    /// `tx_dropped=N(<worst leg>)` (issue 1401: a NIC or queue stall), the VBAN legs' drift-servo
    /// corrections as `servo=<drops>/<repeats>`, and dropped wrong-rate VBAN packets as
    /// `rate_rejects=N` (issue 1345), so a rejected stream (which reads as silent) stays explained
    /// after its one warn scrolls away, and a known stream's undecodable packets as
    /// `decode_rejects=N(<worst leg>)` (issue 1467) for the same reason.
    pub fn status_line(&self) -> String {
        let total_rate_rejects: u64 = self
            .participants
            .iter()
            .filter_map(|p| p.rate_rejects)
            .sum();
        let levels: Vec<String> = self
            .participants
            .iter()
            .filter(|p| p.adapter == crate::matrix::ADAPTER_VBAN)
            .map(|p| format!("{}={:.0}dBFS", p.name, p.level_dbfs))
            .collect();
        let rejects = if total_rate_rejects > 0 {
            format!(" rate_rejects={total_rate_rejects}")
        } else {
            String::new()
        };
        let decode_rejects = |p: &ParticipantState| p.decode_rejects.unwrap_or(0);
        let undecodable = if self.participants.iter().map(decode_rejects).sum::<u64>() > 0 {
            format!(
                " decode_rejects={}",
                self.total_naming_worst(decode_rejects)
            )
        } else {
            String::new()
        };
        let (drops, repeats) = self
            .participants
            .iter()
            .filter_map(|p| p.jitter)
            .fold((0u64, 0u64), |(d, r), j| {
                (d + j.servo_drops, r + j.servo_repeats)
            });
        let servo = if drops + repeats > 0 {
            format!(" servo={drops}/{repeats}")
        } else {
            String::new()
        };
        let program_stalls = |p: &ParticipantState| {
            if p.role == crate::matrix::CAMBOX_ROLE {
                0
            } else {
                p.jitter.map_or(0, |j| j.stalls)
            }
        };
        let stalls = if self.participants.iter().map(program_stalls).sum::<u64>() > 0 {
            format!(" stalls={}", self.total_naming_worst(program_stalls))
        } else {
            String::new()
        };
        let lost = if self.lost_ticks > 0 {
            format!(" lost={}", self.lost_ticks)
        } else {
            String::new()
        };
        let tx_dropped = if self.participants.iter().map(|p| p.tx_dropped).sum::<u64>() > 0 {
            format!(" tx_dropped={}", self.total_naming_worst(|p| p.tx_dropped))
        } else {
            String::new()
        };
        // The HDMI audio legs (issue 1424 part 3): where each plays, its pipe depth and its xruns,
        // or that it is silent (the reason is on /api/state and in the leg's own log line).
        let hdmi: String = self
            .participants
            .iter()
            .filter_map(|p| p.hdmi.as_ref().map(|h| (p, h)))
            .map(|(p, h)| match &h.hw {
                Some(hw) => format!(
                    " hdmi={}:{hw}:depth={}:xruns={}",
                    p.name,
                    p.local_audio.map_or(0, |l| l.pipe_depth_frames),
                    h.xruns
                ),
                None => format!(" hdmi={}:silent", p.name),
            })
            .collect();
        // The card legs (issue 1438): the card, the playback pipe depth and each side's xruns
        // (playback/capture) while every side moves audio, else which side is down and why.
        let alsa: String = self
            .participants
            .iter()
            .filter_map(|p| p.alsa.as_ref().map(|a| (p, a)))
            .map(|(p, a)| {
                if a.up() {
                    format!(
                        " alsa={}:{}:depth={}:xruns={}/{}",
                        p.name,
                        a.card,
                        p.local_audio.map_or(0, |l| l.pipe_depth_frames),
                        a.playback.as_ref().map_or(0, |s| s.xruns),
                        a.capture.as_ref().map_or(0, |s| s.xruns)
                    )
                } else {
                    let why = a
                        .playback
                        .iter()
                        .chain(&a.capture)
                        .find(|s| !s.running)
                        .map_or("down", |s| {
                            // The card was found and the child spawned, but it moves no audio
                            // (a PCM another process holds, or it never read yet).
                            if s.state == "ready" {
                                "not running"
                            } else {
                                s.state
                            }
                        });
                    format!(" alsa={}:{}:down({why})", p.name, a.card)
                }
            })
            .collect();
        format!(
            "intercom-hub: status participants={} underruns={} overruns={}{}{}{}{}{}{}{}{} {}",
            self.participants.len(),
            self.total_naming_worst(|p| p.underruns),
            self.total_naming_worst(|p| p.overruns),
            stalls,
            lost,
            tx_dropped,
            servo,
            rejects,
            undecodable,
            hdmi,
            alsa,
            levels.join(" ")
        )
    }

    /// `N` summed over the participants, followed by `(name)` of the one with the most when `N` > 0
    /// (the first of a tie). The total keeps leading, so a parser of the bare number still works.
    fn total_naming_worst(&self, count: impl Fn(&ParticipantState) -> u64) -> String {
        let total: u64 = self.participants.iter().map(&count).sum();
        // The FIRST participant with the highest count (`max_by_key` would name the last of a tie).
        let mut worst: Option<&ParticipantState> = None;
        for p in &self.participants {
            if worst.is_none_or(|w| count(p) > count(w)) {
                worst = Some(p);
            }
        }
        match worst {
            Some(p) if total > 0 => format!("{total}({})", p.name),
            _ => total.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::Matrix;

    fn matrix() -> Matrix {
        let toml = r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 256

[[participant]]
name = "cam1"
role = "cambox"
adapter = "vban"
host = "cam1.lan"
in_stream = "cam1"
out_stream = "cam1"
in_channels = 2
out_channels = 2

[[participant]]
name = "cutters"
role = "cutters"
adapter = "none"
in_channels = 2
out_channels = 4
"#;
        Matrix::from_toml(toml).unwrap()
    }

    #[test]
    fn snapshot_serializes_expected_fields() {
        let m = matrix();
        let stats = vec![
            RuntimeStats {
                rx_packets: 10,
                tx_packets: 9,
                tx_dropped: 0,
                underruns: 1,
                overruns: 0,
                last_rx_age_ms: Some(5),
                level_dbfs: -12.0,
                sample_rate: Some(48000),
                rate_rejects: 0,
                decode_rejects: 0,
                janus: None,
                local_audio: None,
                jitter: None,
                hdmi: None,
                alsa: None,
            },
            RuntimeStats::default(),
        ];
        let hs = HubState::snapshot(&m, "1.7.0-test", &stats);
        let v: serde_json::Value = serde_json::to_value(&hs).unwrap();
        assert_eq!(v["version"], "1.7.0-test");
        assert_eq!(v["sample_rate"], 48000);
        assert_eq!(v["participants"][0]["name"], "cam1");
        assert_eq!(v["participants"][0]["adapter"], "vban");
        assert_eq!(v["participants"][0]["rx_packets"], 10);
        assert_eq!(v["participants"][0]["last_rx_age_ms"], 5);
        assert_eq!(v["participants"][0]["sample_rate"], 48000);
        assert_eq!(v["participants"][1]["name"], "cutters");
        assert_eq!(v["participants"][1]["adapter"], "none");
        // default stats render as zeros / null age.
        assert_eq!(v["participants"][1]["rx_packets"], 0);
        assert!(v["participants"][1]["last_rx_age_ms"].is_null());
    }

    #[test]
    fn status_line_reports_underruns_and_vban_levels() {
        let m = matrix();
        let stats = vec![
            RuntimeStats {
                underruns: 3,
                level_dbfs: -20.0,
                ..Default::default()
            },
            RuntimeStats::default(),
        ];
        let hs = HubState::snapshot(&m, "v", &stats);
        let line = hs.status_line();
        assert!(line.contains("underruns=3"), "got: {line}");
        assert!(line.contains("cam1=-20dBFS"), "got: {line}");
        // The non-vban cutters participant is not a VBAN level line.
        assert!(!line.contains("cutters="), "got: {line}");
    }
}
