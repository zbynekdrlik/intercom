//! The declarative routing matrix (issue 1345 M1).
//!
//! A [`Matrix`] is loaded from `intercom.toml` (generated from the VB-Matrix XML by the checked-in
//! converter). It holds the hub config, the ordered list of [`Participant`]s, and the list of
//! [`Point`]s — one point routes a single input CHANNEL of a source participant into a single
//! output CHANNEL of a destination participant at a gain, exactly like a VB-Matrix grid cell.
//!
//! The mix-minus (N-1) invariant is enforced STRUCTURALLY here, not left to the config: a point
//! whose `src == dst` participant is REFUSED at load. So a participant never hears its own source,
//! no matter what the TOML says.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;

use crate::alsa_card::AlsaCardSpec;
use crate::alsa_spec::check_alsa;
use crate::janus_rtp::JanusCodec;

/// Adapters a participant is reached through. `Vban` is live since M1; `Janus` (M3a) carries the
/// `phones` participant over the Janus audiobridge plain-RTP leg; `None` marks a participant
/// declared for routing fidelity whose real I/O (PipeWire) arrives in M2.
pub const ADAPTER_VBAN: &str = "vban";
pub const ADAPTER_NONE: &str = "none";
pub const ADAPTER_JANUS: &str = "janus";
/// The local PipeWire adapter (issue 1344): the `program_out` sink (egress) + the talkback capture
/// (ingress) that replace the last VB-Matrix function on the strih-snv notebook.
pub const ADAPTER_PIPEWIRE: &str = "pipewire";
/// The hub's own HDMI audio leg (issue 1424 part 3): the participant's N-1 bus played straight into
/// the ALSA hw PCM of a NAMED connector (`hdmi_connector`) by a supervised `aplay`, no PipeWire node
/// (the strih-pp vestibule TV behind the HDMI-to-SDI converter on HDMI-1). An output only.
pub const ADAPTER_ALSA_HDMI: &str = "alsa_hdmi";
/// A USB sound card opened directly through ALSA (issue 1438): the participant's N-1 bus played by a
/// supervised `aplay` and its input recorded by a supervised `arecord`, both on
/// `hw:CARD=<alsa_card>,DEV=<alsa_device>`, the card named by its ALSA card ID, never an index. No
/// PipeWire node, and a missing card is the leg down, never a fallback (the cutters' MiniFuse 4 /
/// MG-XU).
pub const ADAPTER_ALSA: &str = "alsa";

/// The largest `hdmi_delay_ms` (issue 1424): a second of program audio is the most a picture path
/// could ever need; a bigger value is a typo.
pub const HDMI_DELAY_MAX_MS: u32 = 1000;

/// The one role the `janus` adapter is permitted on (the phone/VDO.Ninja replacement leg).
pub const JANUS_ONLY_ROLE: &str = "phones";

/// The role of the local PipeWire program sink OBS captures (`ASIO zvuk`) — issue 1344.
pub const PROGRAM_OUT_ROLE: &str = "program_out";

/// The role of a camera box (cam1..7): its VBAN talkback stream stops whenever it is muted.
pub const CAMBOX_ROLE: &str = "cambox";

/// Roles a participant plays (informational + validated at load; the engine never branches on it).
const KNOWN_ROLES: &[&str] = &[
    CAMBOX_ROLE,
    "program_ref",
    "cutters",
    JANUS_ONLY_ROLE,
    "speakers",
    "line34",
    "program_monitor",
    PROGRAM_OUT_ROLE,
];

/// The Janus audiobridge edge config (the optional `[janus]` table, M3a). Absent → the hub runs the
/// VBAN legs only; present → the single `phones` participant is carried over the audiobridge room.
/// The room secret is NEVER inlined here — it is read from `room_secret_file` (0600) at start.
#[derive(Debug, Clone, Deserialize)]
pub struct JanusConfig {
    /// The Janus HTTP API base URL (loopback: TLS terminates on the dev1 front, not here).
    #[serde(default = "default_janus_api_url")]
    pub api_url: String,
    /// The audiobridge room id the hub joins as a plain-RTP participant.
    #[serde(default = "default_janus_room")]
    pub room: u64,
    /// Path to the 0600 file holding the room secret (value NEVER logged / never in the TOML).
    #[serde(default)]
    pub room_secret_file: Option<String>,
    /// The local UDP `host:port` the plain-RTP leg binds (our send + the room-mix receive).
    #[serde(default = "default_janus_rtp_bind")]
    pub rtp_bind: String,
    /// The phones leg's codec: `"opus"` (default, 48 kHz mono with in-band FEC) or `"pcmu"`
    /// (G.711 µ-law). An unknown value fails the load (issue 1345, 25.9.2026).
    #[serde(default)]
    pub codec: JanusCodec,
    /// Janus's WebSocket API, which the hub's `/janus` relay connects to (issue 1453). Janus binds it
    /// to loopback (setup-strih step 14), so phones reach it only through the relay's frame
    /// allowlist. Must be a `ws://` URL.
    #[serde(default = "default_janus_ws_url")]
    pub ws_url: String,
}

fn default_janus_api_url() -> String {
    "http://127.0.0.1:8088/janus".to_string()
}
fn default_janus_room() -> u64 {
    1000
}
fn default_janus_rtp_bind() -> String {
    "0.0.0.0:6990".to_string()
}
fn default_janus_ws_url() -> String {
    "ws://127.0.0.1:8188/janus".to_string()
}

/// The systemd credential name the production `intercom-hub.service` exposes the room secret under
/// (`LoadCredential=janus-room.secret:<file>`). When systemd runs the hub it sets
/// `$CREDENTIALS_DIRECTORY` and drops each loaded credential there, readable by the (dynamic) user
/// even under `DynamicUser=yes`.
pub const JANUS_ROOM_SECRET_CRED: &str = "janus-room.secret";

/// Resolve WHERE to read the Janus room secret from, preferring systemd's credential store over the
/// configured file path. `credentials_dir` is `$CREDENTIALS_DIRECTORY` (`Some` when the unit sets
/// `LoadCredential=`); `configured` is the `[janus].room_secret_file`. Returns the path to READ, or
/// `None` when neither is available (the hub then joins without a secret). This NEVER returns or
/// logs the secret VALUE — only the path. An empty/whitespace credentials dir is treated as unset so
/// a stray env var can never resolve to a bare `/janus-room.secret`.
pub fn resolve_secret_path(
    credentials_dir: Option<&str>,
    configured: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dir) = credentials_dir {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Some(Path::new(dir).join(JANUS_ROOM_SECRET_CRED));
        }
    }
    configured.map(Path::to_path_buf)
}

/// The Interkom picture (MJPEG) config (the optional `[video]` table, M3c). Absent → the hub serves no
/// picture; present + `enabled` → the hub runs the NDI low-bandwidth receiver → decimate → JPEG →
/// `/interkom.mjpeg` pipe. Until issue 1347 builds the `STRIH-SNV (interkom)` NDI output, a dev override
/// points `ndi_source_name` at `CAM1 (usb)`.
#[derive(Debug, Clone, Deserialize)]
pub struct VideoConfig {
    /// The NDI source name the receiver looks for (default `STRIH-SNV (interkom)`).
    #[serde(default = "default_video_source")]
    pub ndi_source_name: String,
    /// Target picture rate, frames/sec (validated 1..=30 at load).
    #[serde(default = "default_video_fps")]
    pub fps: u32,
    /// JPEG encode quality (validated 30..=95 at load — a monitor picture, not an archival master).
    #[serde(default = "default_video_jpeg_quality")]
    pub jpeg_quality: u8,
    /// Whether the picture leg runs at all (default true).
    #[serde(default = "default_video_enabled")]
    pub enabled: bool,
}

fn default_video_source() -> String {
    "STRIH-SNV (interkom)".to_string()
}
fn default_video_fps() -> u32 {
    10
}
fn default_video_jpeg_quality() -> u8 {
    70
}
fn default_video_enabled() -> bool {
    true
}

/// Hub-wide config (the `[hub]` table).
#[derive(Debug, Clone, Deserialize)]
pub struct HubConfig {
    /// `host:port` the HTTP `/api/state` + `/ws` panel binds on.
    pub bind: String,
    /// `host:port` the single VBAN receive socket binds on (`0.0.0.0:6980`).
    pub vban_bind: String,
    /// The audio sample rate for every leg (48000).
    pub sample_rate: u32,
    /// The fixed mix block size in frames (e.g. 256 = ~5.3 ms at 48 kHz).
    pub block_frames: usize,
}

/// One participant (a VB-Matrix slot mapped to a hub participant).
#[derive(Debug, Clone, Deserialize)]
pub struct Participant {
    /// Stable name, used as the routing key and in `/api/state`.
    pub name: String,
    /// Informational role (see [`KNOWN_ROLES`]).
    pub role: String,
    /// How this participant's audio is carried — `vban` (live in M1) or `none` (M2/M3).
    pub adapter: String,
    /// The host to send this participant's OUTPUT to (VBAN adapter only), e.g. `cam1.lan`.
    #[serde(default)]
    pub host: Option<String>,
    /// The VBAN stream name this participant SENDS us as its input (e.g. `cam1`, `fohabl-strih`).
    #[serde(default)]
    pub in_stream: Option<String>,
    /// The VBAN stream name we SEND this participant's output as (e.g. `cam1`).
    #[serde(default)]
    pub out_stream: Option<String>,
    /// The PipeWire SINK node this participant's mixed output is written to (pipewire adapter only):
    /// `strih-program` for the `program_out` (issue 1344), the MiniFuse playback node for the
    /// `cutters` so the operator hears the N-1 mix in the headphones (issue 1345).
    #[serde(default)]
    pub pipewire_target: Option<String>,
    /// The `pw-cat --channel-map` for this participant's playback sink (pipewire adapter only), e.g.
    /// `AUX0,AUX1,AUX2,AUX3` for the MiniFuse pro-audio playback node. Must name exactly
    /// `out_channels` positions. Absent = pw-cat's default map (the `strih-program` null sink).
    #[serde(default)]
    pub pipewire_channel_map: Option<String>,
    /// The PipeWire CAPTURE node this participant's input is read from (pipewire adapter only), e.g.
    /// the MiniFuse 4 pro-input node feeding the operator's talkback into the N-1 mix — issue 1344.
    #[serde(default)]
    pub pipewire_source: Option<String>,
    /// For a `program_out`: the VBAN stream names the program mix is fed by (informational + verified:
    /// each must be a received VBAN `in_stream`, and none may collide with a cambox stream). The
    /// engine sums them via the matrix `[[point]]`s; this list is what `verify-strih.sh` asserts rx>0
    /// on and what `/api/state` reports — issue 1344.
    #[serde(default)]
    pub source_streams: Vec<String>,
    /// The connector an `alsa_hdmi` participant plays on, as X RandR names it (`HDMI-1`); resolved
    /// to its ALSA hw PCM by the connector's EDID against the PCMs' ELDs, never a card/device number
    /// (issue 1424 part 3). Only on the `alsa_hdmi` adapter.
    #[serde(default)]
    pub hdmi_connector: Option<String>,
    /// A fixed lip-sync delay of an `alsa_hdmi` leg in ms, 0..=1000 (issue 1424, 14.10.2026: the
    /// vestibule heard the program ~170-210 ms before the picture). Absent = 0. Only on `alsa_hdmi`.
    #[serde(default)]
    pub hdmi_delay_ms: Option<u32>,
    /// The ALSA card ID an `alsa` participant opens (`/proc/asound/cards`: `M4` = the MiniFuse 4,
    /// `MGXU` = the Yamaha MG-XU), never a card index (issue 1438). Only on `alsa`.
    #[serde(default)]
    pub alsa_card: Option<String>,
    /// The PCM device on that card (default 0).
    #[serde(default)]
    pub alsa_device: Option<u32>,
    /// The ALSA period of both sides in frames (default 256 = one hub block; 64..=512). The buffer is
    /// four periods.
    #[serde(default)]
    pub alsa_period_frames: Option<usize>,
    /// The card's playback channel count (the hw PCM opens with exactly this many; the MiniFuse
    /// takes 6). Default = `out_channels`.
    #[serde(default)]
    pub alsa_out_channels: Option<usize>,
    /// The card's capture channel count (MiniFuse: 6). Default = `in_channels`.
    #[serde(default)]
    pub alsa_in_channels: Option<usize>,
    /// Which hw playback channel carries each output channel: comma-separated 1-based hw channel
    /// numbers, one per `out_channels` (MiniFuse: `1,2,3,4`). Default = the first `out_channels`.
    #[serde(default)]
    pub alsa_channel_map: Option<String>,
    /// Which hw capture channel feeds each input channel, 1-based, one per `in_channels`
    /// (MiniFuse: `1,2`). Default = the first `in_channels`.
    #[serde(default)]
    pub alsa_in_channel_map: Option<String>,
    /// Number of input channels the matrix routes FROM this participant.
    pub in_channels: usize,
    /// Number of output channels the matrix routes TO this participant.
    pub out_channels: usize,
}

/// The wire shape of a routing point in the TOML (`[[point]]`).
#[derive(Debug, Clone, Deserialize)]
struct PointToml {
    src: String,
    in_ch: usize,
    dst: String,
    out_ch: usize,
    #[serde(default)]
    gain_db: f32,
    #[serde(default)]
    mute: bool,
}

/// A resolved routing point (participant names resolved to ids, gain precomputed to linear).
#[derive(Debug, Clone)]
pub struct Point {
    /// Source participant id (index into [`Matrix::participants`]).
    pub src: usize,
    /// 1-based source input channel.
    pub in_ch: usize,
    /// Destination participant id.
    pub dst: usize,
    /// 1-based destination output channel.
    pub out_ch: usize,
    /// Gain in dB (as authored).
    pub gain_db: f32,
    /// Precomputed linear gain (`10^(gain_db/20)`).
    pub gain_linear: f32,
    /// Whether this point is muted (excluded from the mix).
    pub mute: bool,
}

#[derive(Debug, Deserialize)]
struct MatrixToml {
    hub: HubConfig,
    #[serde(default)]
    janus: Option<JanusConfig>,
    #[serde(default)]
    video: Option<VideoConfig>,
    #[serde(default, rename = "participant")]
    participants: Vec<Participant>,
    #[serde(default, rename = "point")]
    points: Vec<PointToml>,
}

/// The loaded routing matrix: hub config + the optional Janus edge config + participants + resolved
/// points.
#[derive(Debug, Clone)]
pub struct Matrix {
    pub hub: HubConfig,
    pub janus: Option<JanusConfig>,
    pub video: Option<VideoConfig>,
    pub participants: Vec<Participant>,
    index: HashMap<String, usize>,
    pub points: Vec<Point>,
    /// The card legs' resolved specs (issue 1438), resolved once at load.
    alsa_specs: Vec<AlsaCardSpec>,
}

/// Convert a dB gain to a linear amplitude factor (`10^(dB/20)`). `0 dB` → `1.0`, `-6 dB` ≈ `0.501`,
/// `-8 dB` ≈ `0.398`.
pub fn gain_linear(gain_db: f32) -> f32 {
    10f32.powf(gain_db / 20.0)
}

impl Matrix {
    /// Parse + validate a matrix from a TOML string. Fails loud on: a malformed TOML, an unknown
    /// role/adapter, a duplicate participant name, a point naming an unknown participant, a point
    /// channel outside a participant's declared channel count, or a `src == dst` self-route (the
    /// mix-minus invariant).
    pub fn from_toml(s: &str) -> Result<Self> {
        let raw: MatrixToml = toml::from_str(s).map_err(|e| anyhow!("parse intercom.toml: {e}"))?;

        if raw.hub.sample_rate == 0 {
            bail!("hub.sample_rate must be > 0");
        }
        if raw.hub.block_frames == 0 {
            bail!("hub.block_frames must be > 0");
        }

        // The Interkom picture (M3c): validate the `[video]` bounds at load — an out-of-range fps or
        // JPEG quality is a config error, caught here, never at rig startup.
        if let Some(v) = &raw.video {
            if !(1..=30).contains(&v.fps) {
                bail!("video.fps {} out of range 1..=30", v.fps);
            }
            if !(30..=95).contains(&v.jpeg_quality) {
                bail!("video.jpeg_quality {} out of range 30..=95", v.jpeg_quality);
            }
        }
        // The phone relay's Janus WebSocket (issue 1453): plain ws:// to Janus's loopback bind.
        if let Some(j) = &raw.janus {
            if !j.ws_url.starts_with("ws://") {
                bail!("janus.ws_url '{}' must be a ws:// URL", j.ws_url);
            }
        }

        let mut index: HashMap<String, usize> = HashMap::new();
        let mut janus_count = 0usize;
        let mut hdmi_connectors: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let (mut alsa_pcms, mut alsa_specs) = (std::collections::HashSet::new(), Vec::new());
        for (id, p) in raw.participants.iter().enumerate() {
            if !KNOWN_ROLES.contains(&p.role.as_str()) {
                bail!("participant '{}': unknown role '{}'", p.name, p.role);
            }
            if p.adapter != ADAPTER_VBAN
                && p.adapter != ADAPTER_NONE
                && p.adapter != ADAPTER_JANUS
                && p.adapter != ADAPTER_PIPEWIRE
                && p.adapter != ADAPTER_ALSA_HDMI
                && p.adapter != ADAPTER_ALSA
            {
                bail!("participant '{}': unknown adapter '{}'", p.name, p.adapter);
            }
            // The card leg (issue 1438): the alsa_* keys resolve and are checked in `alsa_spec`.
            alsa_specs.extend(check_alsa(id, p, &mut alsa_pcms)?);
            // The HDMI leg (issue 1424 part 3): a named connector, an output only, one participant
            // per connector (one aplay per hw PCM).
            if p.adapter == ADAPTER_ALSA_HDMI {
                let connector = p.hdmi_connector.as_deref().unwrap_or("");
                if connector.is_empty() || connector.chars().any(char::is_whitespace) {
                    bail!(
                        "participant '{}': adapter 'alsa_hdmi' needs an hdmi_connector (the X RandR output name, e.g. HDMI-1), got '{}'",
                        p.name,
                        connector
                    );
                }
                if p.in_channels != 0 {
                    bail!(
                        "participant '{}': adapter 'alsa_hdmi' is an output only (in_channels must be 0, got {})",
                        p.name,
                        p.in_channels
                    );
                }
                // An HDA HDMI PCM opens with 2..=8 channels only (hw:0,7 on strih-pp: CHANNELS [2 8]).
                if !(2..=8).contains(&p.out_channels) {
                    bail!(
                        "participant '{}': adapter 'alsa_hdmi' needs 2..=8 out_channels, got {}",
                        p.name,
                        p.out_channels
                    );
                }
                if !hdmi_connectors.insert(connector) {
                    bail!(
                        "participant '{}': hdmi_connector '{}' is already played by another participant",
                        p.name,
                        connector
                    );
                }
                if p.hdmi_delay_ms.is_some_and(|ms| ms > HDMI_DELAY_MAX_MS) {
                    bail!(
                        "participant '{}': hdmi_delay_ms must be 0..={}, got {}",
                        p.name,
                        HDMI_DELAY_MAX_MS,
                        p.hdmi_delay_ms.unwrap_or(0)
                    );
                }
            } else if p.hdmi_delay_ms.is_some() {
                bail!(
                    "participant '{}': hdmi_delay_ms is only for adapter 'alsa_hdmi' (got '{}')",
                    p.name,
                    p.adapter
                );
            } else if p.hdmi_connector.is_some() {
                bail!(
                    "participant '{}': hdmi_connector is only for adapter 'alsa_hdmi' (got '{}')",
                    p.name,
                    p.adapter
                );
            }
            if p.adapter == ADAPTER_VBAN && p.out_stream.is_some() && p.host.is_none() {
                bail!(
                    "participant '{}': vban out_stream needs a host to send to",
                    p.name
                );
            }
            // The Janus audiobridge leg is ONLY for the phones participant, and there is at most one
            // (a single audiobridge room / plain-RTP participant). Fail loud otherwise (M3a).
            if p.adapter == ADAPTER_JANUS {
                if p.role != JANUS_ONLY_ROLE {
                    bail!(
                        "participant '{}': adapter 'janus' is only allowed on role '{}' (got '{}')",
                        p.name,
                        JANUS_ONLY_ROLE,
                        p.role
                    );
                }
                // The janus adapter's plain-RTP leg carries MONO PCMU, up/down-mixed to/from the
                // engine's STEREO block — that mono<->stereo conversion assumes exactly 2 channels
                // each way, so refuse any other channel count rather than silently mangle the mix.
                if p.in_channels != 2 || p.out_channels != 2 {
                    bail!(
                        "participant '{}': adapter 'janus' requires 2 in / 2 out channels (mono<->stereo), got {} in / {} out",
                        p.name,
                        p.in_channels,
                        p.out_channels
                    );
                }
                janus_count += 1;
                if janus_count > 1 {
                    bail!("at most one 'janus' participant is allowed (one audiobridge room)");
                }
            }
            if index.insert(p.name.clone(), id).is_some() {
                bail!("duplicate participant name '{}'", p.name);
            }
        }

        // The local PipeWire adapter (issue 1344): validate the `program_out` sink + the talkback
        // capture inputs. AT MOST ONE program_out (one OBS `ASIO zvuk` sink); a program_out needs a
        // `pipewire_target` + a non-empty `source_streams`, each of which must be a received VBAN
        // `in_stream` and must NOT collide with a cambox stream (a program feed is never a cambox);
        // any OTHER pipewire participant is a capture input and needs a `pipewire_source`.
        let cambox_streams: std::collections::HashSet<&str> = raw
            .participants
            .iter()
            .filter(|p| p.role == CAMBOX_ROLE)
            .filter_map(|p| p.in_stream.as_deref())
            .collect();
        let vban_in_streams: std::collections::HashSet<&str> = raw
            .participants
            .iter()
            .filter(|p| p.adapter == ADAPTER_VBAN)
            .filter_map(|p| p.in_stream.as_deref())
            .collect();
        let mut program_out_count = 0usize;
        for p in raw.participants.iter() {
            if p.role == PROGRAM_OUT_ROLE {
                program_out_count += 1;
                if program_out_count > 1 {
                    bail!(
                        "at most one 'program_out' participant is allowed (one OBS program sink)"
                    );
                }
                if p.adapter != ADAPTER_PIPEWIRE {
                    bail!(
                        "participant '{}': role 'program_out' requires the 'pipewire' adapter (got '{}')",
                        p.name,
                        p.adapter
                    );
                }
            }
            if p.adapter != ADAPTER_PIPEWIRE {
                continue;
            }
            if p.role == PROGRAM_OUT_ROLE {
                if p.pipewire_target.as_deref().unwrap_or("").is_empty() {
                    bail!(
                        "participant '{}': program_out needs a non-empty pipewire_target (the sink node)",
                        p.name
                    );
                }
                // A program_out is a pure SINK — it must NOT also carry a capture node, or the
                // block loop would spawn a capture into its (in_channels=0) buffer.
                if p.pipewire_source.is_some() {
                    bail!(
                        "participant '{}': program_out is a sink and must not carry a pipewire_source",
                        p.name
                    );
                }
                if p.source_streams.is_empty() {
                    bail!(
                        "participant '{}': program_out needs at least one source_stream",
                        p.name
                    );
                }
                for s in &p.source_streams {
                    if cambox_streams.contains(s.as_str()) {
                        bail!(
                            "participant '{}': program_out source_stream '{}' collides with a cambox stream",
                            p.name,
                            s
                        );
                    }
                    if !vban_in_streams.contains(s.as_str()) {
                        bail!(
                            "participant '{}': program_out source_stream '{}' is not a received VBAN in_stream",
                            p.name,
                            s
                        );
                    }
                }
            } else if p.pipewire_source.as_deref().unwrap_or("").is_empty() {
                // A non-program_out pipewire participant is a capture input (the talkback mic).
                bail!(
                    "participant '{}': pipewire adapter needs a pipewire_source (capture node) unless it is the program_out",
                    p.name
                );
            } else if let Some(target) = p.pipewire_target.as_deref() {
                // A capture participant MAY also carry a playback sink (issue 1345: the cutters play
                // their own N-1 output bus to the MiniFuse headphones). It needs a real node name and
                // output channels to play.
                if target.trim().is_empty() {
                    bail!(
                        "participant '{}': pipewire_target must name a sink node (got an empty string)",
                        p.name
                    );
                }
                if p.out_channels == 0 {
                    bail!(
                        "participant '{}': pipewire_target needs out_channels > 0 (nothing to play)",
                        p.name
                    );
                }
            }
            if let Some(map) = p.pipewire_channel_map.as_deref() {
                let positions: Vec<&str> = map.split(',').map(str::trim).collect();
                if positions.iter().any(|x| x.is_empty()) || positions.len() != p.out_channels {
                    bail!(
                        "participant '{}': pipewire_channel_map '{}' must name exactly {} comma-separated positions (out_channels)",
                        p.name,
                        map,
                        p.out_channels
                    );
                }
            }
        }

        let mut points = Vec::with_capacity(raw.points.len());
        for pt in &raw.points {
            let src = *index
                .get(&pt.src)
                .ok_or_else(|| anyhow!("point references unknown src participant '{}'", pt.src))?;
            let dst = *index
                .get(&pt.dst)
                .ok_or_else(|| anyhow!("point references unknown dst participant '{}'", pt.dst))?;
            if src == dst {
                bail!(
                    "self-route point src == dst == '{}' violates the mix-minus (N-1) invariant",
                    pt.src
                );
            }
            if pt.in_ch < 1 || pt.in_ch > raw.participants[src].in_channels {
                bail!(
                    "point {} in_ch {} out of range 1..={}",
                    pt.src,
                    pt.in_ch,
                    raw.participants[src].in_channels
                );
            }
            if pt.out_ch < 1 || pt.out_ch > raw.participants[dst].out_channels {
                bail!(
                    "point {} out_ch {} out of range 1..={}",
                    pt.dst,
                    pt.out_ch,
                    raw.participants[dst].out_channels
                );
            }
            points.push(Point {
                src,
                in_ch: pt.in_ch,
                dst,
                out_ch: pt.out_ch,
                gain_db: pt.gain_db,
                gain_linear: gain_linear(pt.gain_db),
                mute: pt.mute,
            });
        }

        Ok(Matrix {
            hub: raw.hub,
            janus: raw.janus,
            video: raw.video,
            participants: raw.participants,
            index,
            points,
            alsa_specs,
        })
    }

    /// The participant id for a name, if any.
    pub fn id_of(&self, name: &str) -> Option<usize> {
        self.index.get(name).copied()
    }

    /// The id of the single `janus`-adapter participant (the phones leg), if one is declared.
    pub fn janus_participant(&self) -> Option<usize> {
        self.participants
            .iter()
            .position(|p| p.adapter == ADAPTER_JANUS)
    }

    /// Map of VBAN input stream name → participant id (for the receiver's demux). Only participants
    /// on the `vban` adapter with an `in_stream` are included.
    pub fn vban_input_streams(&self) -> HashMap<String, usize> {
        let mut m = HashMap::new();
        for (id, p) in self.participants.iter().enumerate() {
            if p.adapter == ADAPTER_VBAN {
                if let Some(name) = &p.in_stream {
                    m.insert(name.clone(), id);
                }
            }
        }
        m
    }

    /// The single local PipeWire program sink (issue 1344), as `(participant id, target sink node,
    /// source stream names)`, or `None` when the matrix declares no `program_out`.
    pub fn program_out(&self) -> Option<(usize, String, Vec<String>)> {
        self.participants.iter().enumerate().find_map(|(id, p)| {
            if p.adapter == ADAPTER_PIPEWIRE && p.role == PROGRAM_OUT_ROLE {
                let target = p.pipewire_target.clone()?;
                Some((id, target, p.source_streams.clone()))
            } else {
                None
            }
        })
    }

    /// The local PipeWire CAPTURE inputs (the talkback mic), as `(participant id, capture node,
    /// in_channels)` — every pipewire-adapter participant that carries a `pipewire_source` (issue
    /// 1344).
    pub fn local_inputs(&self) -> Vec<(usize, String, usize)> {
        self.participants
            .iter()
            .enumerate()
            .filter_map(|(id, p)| {
                // A capture input is a pipewire participant that is NOT the program_out sink (the
                // sink can't carry a pipewire_source — validated at load — but exclude it explicitly).
                if p.adapter == ADAPTER_PIPEWIRE && p.role != PROGRAM_OUT_ROLE {
                    p.pipewire_source
                        .clone()
                        .map(|node| (id, node, p.in_channels))
                } else {
                    None
                }
            })
            .collect()
    }

    /// The local PipeWire PLAYBACK sinks other than the `program_out` (issue 1345: the cutters' N-1 mix
    /// to the MiniFuse headphones), as `(participant id, sink node, out_channels, channel map)` —
    /// every pipewire participant that is not the `program_out`, has output channels and a non-empty
    /// `pipewire_target`. Each is fed from that participant's OWN output bus.
    pub fn local_outputs(&self) -> Vec<(usize, String, usize, Option<String>)> {
        self.participants
            .iter()
            .enumerate()
            .filter_map(|(id, p)| {
                let target = p.pipewire_target.as_deref().unwrap_or("");
                if p.adapter == ADAPTER_PIPEWIRE
                    && p.role != PROGRAM_OUT_ROLE
                    && p.out_channels > 0
                    && !target.trim().is_empty()
                {
                    Some((
                        id,
                        target.to_string(),
                        p.out_channels,
                        p.pipewire_channel_map.clone(),
                    ))
                } else {
                    None
                }
            })
            .collect()
    }

    /// The hub's HDMI audio legs (issue 1424 part 3), as `(participant id, connector, out_channels)`:
    /// every `alsa_hdmi` participant, fed from its OWN N-1 output bus.
    pub fn alsa_hdmi_outputs(&self) -> Vec<(usize, String, usize)> {
        self.participants
            .iter()
            .enumerate()
            .filter(|(_, p)| p.adapter == ADAPTER_ALSA_HDMI)
            .filter_map(|(id, p)| {
                p.hdmi_connector
                    .clone()
                    .map(|connector| (id, connector, p.out_channels))
            })
            .collect()
    }

    /// The card legs (issue 1438): every `alsa` participant's resolved card spec, fed from its OWN
    /// N-1 output bus (playback) and feeding its own input buffer (capture).
    pub fn alsa_cards(&self) -> Vec<AlsaCardSpec> {
        self.alsa_specs.clone()
    }

    /// The card leg spec of participant `id`, if it is on the `alsa` adapter.
    pub fn alsa_spec(&self, id: usize) -> Option<&AlsaCardSpec> {
        self.alsa_specs.iter().find(|c| c.participant == id)
    }

    /// The lip-sync delay of the `alsa_hdmi` participant `id` in ms (0 when absent).
    pub fn alsa_hdmi_delay_ms(&self, id: usize) -> u32 {
        self.participants
            .get(id)
            .and_then(|p| p.hdmi_delay_ms)
            .unwrap_or(0)
    }

    /// Participants that receive a VBAN output from us (adapter `vban`, an `out_stream` + `host`),
    /// as `(participant id, stream name, host)`.
    pub fn vban_outputs(&self) -> Vec<(usize, String, String)> {
        let mut out = Vec::new();
        for (id, p) in self.participants.iter().enumerate() {
            if p.adapter == ADAPTER_VBAN {
                if let (Some(stream), Some(host)) = (&p.out_stream, &p.host) {
                    out.push((id, stream.clone(), host.clone()));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINY: &str = r#"
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
in_channels = 1
out_channels = 1

[[participant]]
name = "cam2"
role = "cambox"
adapter = "vban"
host = "cam2.lan"
in_stream = "cam2"
out_stream = "cam2"
in_channels = 1
out_channels = 1

[[point]]
src = "cam1"
in_ch = 1
dst = "cam2"
out_ch = 1

[[point]]
src = "cam2"
in_ch = 1
dst = "cam1"
out_ch = 1
gain_db = -6.0
"#;

    #[test]
    fn parses_participants_and_points() {
        let m = Matrix::from_toml(TINY).unwrap();
        assert_eq!(m.participants.len(), 2);
        assert_eq!(m.points.len(), 2);
        assert_eq!(m.hub.sample_rate, 48000);
        assert_eq!(m.id_of("cam1"), Some(0));
        assert_eq!(m.id_of("cam2"), Some(1));
        assert_eq!(m.id_of("nope"), None);
    }

    #[test]
    fn gain_math_minus_8_db_is_about_0_398() {
        // -8 dB ≈ ×0.398107; -6 dB ≈ ×0.501187; 0 dB = ×1.0.
        assert!((gain_linear(-8.0) - 0.398_107).abs() < 1e-4);
        assert!((gain_linear(-6.0) - 0.501_187).abs() < 1e-4);
        assert!((gain_linear(0.0) - 1.0).abs() < 1e-9);
        // The -6 dB point in TINY carries its precomputed linear gain.
        let m = Matrix::from_toml(TINY).unwrap();
        let p = m.points.iter().find(|p| p.gain_db == -6.0).unwrap();
        assert!((p.gain_linear - 0.501_187).abs() < 1e-4);
    }

    #[test]
    fn self_route_point_is_refused_at_load() {
        let bad = r#"
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
in_channels = 1
out_channels = 1

[[point]]
src = "cam1"
in_ch = 1
dst = "cam1"
out_ch = 1
"#;
        let err = Matrix::from_toml(bad).unwrap_err().to_string();
        assert!(err.contains("mix-minus"), "got: {err}");
    }

    #[test]
    fn unknown_participant_in_point_is_refused() {
        let bad = r#"
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
in_channels = 1
out_channels = 1

[[point]]
src = "cam1"
in_ch = 1
dst = "ghost"
out_ch = 1
"#;
        let err = Matrix::from_toml(bad).unwrap_err().to_string();
        assert!(err.contains("unknown dst participant"), "got: {err}");
    }

    #[test]
    fn vban_stream_maps() {
        let m = Matrix::from_toml(TINY).unwrap();
        let ins = m.vban_input_streams();
        assert_eq!(ins.get("cam1"), Some(&0));
        assert_eq!(ins.get("cam2"), Some(&1));
        let outs = m.vban_outputs();
        assert_eq!(outs.len(), 2);
        assert!(outs
            .iter()
            .any(|(id, s, h)| *id == 0 && s == "cam1" && h == "cam1.lan"));
    }
}
