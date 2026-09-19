//! The observable hub state served at `/api/state` + pushed on `/ws` (issue 1345 M1).
//!
//! Per participant: name/role/adapter/host plus the live counters a dev1 watchdog reads — rx/tx
//! packets, jitter underruns/overruns, the age of the last received packet, and the input level in
//! dBFS. The whole snapshot is rebuilt each block-status tick from the jitter buffers.

use serde::Serialize;

use crate::janus_rtp::JanusStats;
use crate::matrix::Matrix;
use crate::ndi_video::VideoStats;

/// Per-participant runtime counters, gathered from its jitter buffer + tx side each tick.
#[derive(Debug, Clone, Copy, Default)]
pub struct RuntimeStats {
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub underruns: u64,
    pub overruns: u64,
    pub last_rx_age_ms: Option<u64>,
    pub level_dbfs: f32,
    /// The Janus audiobridge facet, present only for the `janus`-adapter participant (M3a).
    pub janus: Option<JanusStats>,
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
    pub underruns: u64,
    pub overruns: u64,
    pub last_rx_age_ms: Option<u64>,
    pub level_dbfs: f32,
    /// The Janus audiobridge facet (joined / session age / rejoins / rtp packet counts), present
    /// only for the janus participant — omitted from the JSON for every other participant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub janus: Option<JanusStats>,
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
                ParticipantState {
                    name: p.name.clone(),
                    role: p.role.clone(),
                    adapter: p.adapter.clone(),
                    host: p.host.clone(),
                    rx_packets: s.rx_packets,
                    tx_packets: s.tx_packets,
                    underruns: s.underruns,
                    overruns: s.overruns,
                    last_rx_age_ms: s.last_rx_age_ms,
                    level_dbfs: s.level_dbfs,
                    janus: s.janus,
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
        }
    }

    /// A one-line status summary for the periodic log (a dev1 watchdog greps it): worst underruns +
    /// the participant levels, so a dead/underrunning leg is visible between E2E runs.
    pub fn status_line(&self) -> String {
        let total_underruns: u64 = self.participants.iter().map(|p| p.underruns).sum();
        let levels: Vec<String> = self
            .participants
            .iter()
            .filter(|p| p.adapter == crate::matrix::ADAPTER_VBAN)
            .map(|p| format!("{}={:.0}dBFS", p.name, p.level_dbfs))
            .collect();
        format!(
            "intercom-hub: status participants={} underruns={} {}",
            self.participants.len(),
            total_underruns,
            levels.join(" ")
        )
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
                underruns: 1,
                overruns: 0,
                last_rx_age_ms: Some(5),
                level_dbfs: -12.0,
                janus: None,
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
