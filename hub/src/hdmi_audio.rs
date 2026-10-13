//! issue 1424 part 3: which ALSA hw PCM carries a NAMED HDMI connector's audio, for the hub's own
//! HDMI leg (the strih-pp vestibule TV behind the Blackmagic HDMI-to-SDI converter on HDMI-1).
//!
//! The owner wants the vestibule to hear the lv1 program with no added delay (13.10.2026): the hub
//! already holds that feed earliest of any process on the box, so it plays the participant's N-1 bus
//! straight into the connector's hw PCM through `aplay` ([`crate::hdmi_leg`]), with no PipeWire node
//! and no OBS in between.
//!
//! The matrix names the connector (`hdmi_connector = "HDMI-1"`, the X RandR output name), never a
//! card/device number: HDA numbers its PCMs by codec pin, and they move with the GPU/driver. The
//! match is the part-1 matcher ([`crate::drm_output_audio_match`], `src/drm_output_audio_match.rs`
//! shared by path): the connector's EDID vendor bytes against every HDMI/DP PCM's ELD. The inputs
//! come from the tools on the box, read bounded (`timeout`), so a hung X server never hangs the leg:
//!
//! - the EDID from `xrandr --props` (NVIDIA publishes no connector EDID in `/sys/class/drm`);
//! - the cards from `/proc/asound/cards`, each card's ELD PCMs from `amixer -c N controls`, each ELD
//!   from `amixer -c N cget iface=PCM,name=ELD,device=D`.
//!
//! No match, an ambiguous match or no EDID = the leg stays silent and says why; it never falls back
//! to another device. NVIDIA publishes a connector's ELD only once a mode is driven on it, so the
//! leg re-resolves with a backoff ([`crate::hdmi_leg`]).
//!
//! Everything but the `read_*` / `resolve_connector` glue is pure and std-only (Tier-0 replica).

use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

use crate::drm_output_audio_match::{
    edid_monitor_id, eld_monitor_id, iec958_index, pick_pcm, reason, AudioPcm, MATCH, NO_EDID,
};

/// The ALSA period `aplay` asks the HDMI PCM for: one hub block (5.33 ms). aplay reads its stdin one
/// period at a time, so this is also the pipe's read size ([`crate::pipe_fill`]).
pub const ALSA_HDMI_PERIOD_FRAMES: usize = 256;

/// The ALSA buffer: four periods, 21.3 ms. aplay keeps it full, so it is most of the leg's delay
/// after the pipe (13.3 ms); hw:0,7 on strih-pp accepts 2..32 periods.
pub const ALSA_HDMI_BUFFER_FRAMES: usize = 4 * ALSA_HDMI_PERIOD_FRAMES;

/// The sample format aplay is told: the hub's own PCM16 (hw:0,7 accepts S16_LE and S32_LE).
pub const APLAY_FORMAT: &str = "S16_LE";

/// The control that mutes an HDA HDMI PCM (one per PCM, in PCM order). No sound server unmutes it
/// on a box whose WirePlumber disables the HDMI nodes, so the leg turns it on itself.
pub const IEC958_SWITCH: &str = "IEC958 Playback Switch";

/// The result before the leg's first resolve.
pub const NOT_RESOLVED_YET: i32 = -1;

/// The bound on every tool call of a resolve, in seconds.
pub const RESOLVE_TIMEOUT_S: u32 = 5;

/// The X display the EDID is read from when the hub's environment names none (the kiosk's `:0`).
pub const DEFAULT_X_DISPLAY: &str = ":0";

/// The connector's EDID from `xrandr --props`: the hex lines under its `EDID:` property, `None` when
/// the output is not listed, has no EDID (disconnected, or a panel without one) or the hex is broken.
pub fn xrandr_connector_edid(text: &str, connector: &str) -> Option<Vec<u8>> {
    let mut lines = text.lines();
    // The output's own line starts at column 0 with its name ("HDMI-1 connected ...").
    loop {
        let line = lines.next()?;
        if !line.starts_with(char::is_whitespace)
            && line.split_whitespace().next() == Some(connector)
        {
            break;
        }
    }
    let mut hex = String::new();
    let mut in_edid = false;
    for line in lines {
        if !line.starts_with(char::is_whitespace) {
            break; // the next output
        }
        let t = line.trim();
        if in_edid {
            if !t.is_empty() && t.bytes().all(|b| b.is_ascii_hexdigit()) {
                hex.push_str(t);
                continue;
            }
            break;
        }
        if t == "EDID:" {
            in_edid = true;
        }
    }
    decode_hex(&hex)
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if hex.is_empty() || !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

/// The ALSA card numbers in `/proc/asound/cards` (" 0 [NVidia ]: HDA-Intel - ...").
pub fn asound_card_numbers(text: &str) -> Vec<i32> {
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let n = it.next()?;
            it.next()
                .filter(|next| next.starts_with('['))
                .and_then(|_| n.parse().ok())
        })
        .collect()
}

/// The PCM devices of one card that carry an ELD control (`amixer -c N controls`:
/// `numid=12,iface=PCM,name='ELD',device=7`): the card's HDMI/DP PCMs.
pub fn amixer_eld_devices(controls: &str) -> Vec<i32> {
    controls
        .lines()
        .filter(|l| l.contains("iface=PCM") && l.contains("name='ELD'"))
        .filter_map(|l| {
            l.split(',')
                .find_map(|f| f.trim().strip_prefix("device="))
                .and_then(|d| d.parse().ok())
        })
        .collect()
}

/// How many [`IEC958_SWITCH`] controls one card has (`amixer -c N controls`).
pub fn amixer_iec958_switch_count(controls: &str) -> usize {
    let name = format!("name='{IEC958_SWITCH}'");
    controls
        .lines()
        .filter(|l| l.contains("iface=MIXER") && l.contains(&name))
        .count()
}

/// The bytes of a BYTES control from `amixer cget` (`  : values=0x10,0x00,...`). An empty ELD (no
/// monitor) is `Some(empty)`; no values line or a garbled byte is `None`.
pub fn amixer_bytes(cget: &str) -> Option<Vec<u8>> {
    let line = cget
        .lines()
        .map(str::trim_start)
        .find(|l| l.starts_with(": values="))?;
    let vals = line[": values=".len()..].trim();
    if vals.is_empty() {
        return Some(Vec::new());
    }
    vals.split(',')
        .map(|v| {
            let v = v.trim();
            u8::from_str_radix(v.strip_prefix("0x").unwrap_or(v), 16).ok()
        })
        .collect()
}

/// The leg's resolve: the matcher's result and, on [`MATCH`], the `(card, device)` it plays on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HdmiResolution {
    pub result: i32,
    pub pcm: Option<(i32, i32)>,
}

/// Resolve a connector's EDID (`None` = unreadable / no EDID) against the HDMI PCMs' ELDs.
pub fn resolve_from(edid: Option<&[u8]>, pcms: &[AudioPcm]) -> HdmiResolution {
    let Some(edid) = edid else {
        return HdmiResolution {
            result: NO_EDID,
            pcm: None,
        };
    };
    let (result, idx) = pick_pcm(&edid_monitor_id(edid), pcms);
    HdmiResolution {
        result,
        pcm: idx.map(|i| (pcms[i].card, pcms[i].device)),
    }
}

/// The `amixer` argv that turns on the [`IEC958_SWITCH`] of PCM `device` on `card`, whose HDMI PCMs
/// are `devices` and which has `switches` such switches; `None` when the switch is ambiguous (an
/// extra S/PDIF switch on the card): left untouched, the part-1 rule.
pub fn iec958_on_argv(
    card: i32,
    devices: &[i32],
    device: i32,
    switches: usize,
) -> Option<Vec<String>> {
    let index = iec958_index(devices, device, switches)?;
    Some(vec![
        "amixer".into(),
        "-c".into(),
        card.to_string(),
        "cset".into(),
        format!("iface=MIXER,name={IEC958_SWITCH},index={index}"),
        "on".into(),
    ])
}

/// The `aplay` argv for the resolved hw PCM: raw PCM16 on stdin, the short buffer of
/// [`ALSA_HDMI_BUFFER_FRAMES`]. The hw PCM itself, never `plughw`/`default`/a dmix: nothing
/// converts or mixes, and a busy PCM fails instead of sharing.
pub fn aplay_argv(card: i32, device: i32, rate: u32, channels: u8) -> Vec<String> {
    vec![
        "aplay".into(),
        "-D".into(),
        format!("hw:{card},{device}"),
        "-t".into(),
        "raw".into(),
        "-f".into(),
        APLAY_FORMAT.into(),
        "-r".into(),
        rate.to_string(),
        "-c".into(),
        channels.to_string(),
        format!("--period-size={ALSA_HDMI_PERIOD_FRAMES}"),
        format!("--buffer-size={ALSA_HDMI_BUFFER_FRAMES}"),
        "-".into(),
    ]
}

/// Whether an aplay stderr line reports an xrun (`underrun!!! (at least N ms long)`).
pub fn is_xrun_line(line: &str) -> bool {
    line.contains("underrun!!!")
}

/// One tool call of a resolve, bounded by `timeout`; its stdout on success.
fn run_bounded(argv: &[String]) -> Option<String> {
    let mut cmd = Command::new("timeout");
    cmd.arg(RESOLVE_TIMEOUT_S.to_string()).args(argv);
    if argv.first().map(String::as_str) == Some("xrandr") && std::env::var_os("DISPLAY").is_none() {
        cmd.env("DISPLAY", DEFAULT_X_DISPLAY);
    }
    let out = cmd.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn args(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|a| a.to_string()).collect()
}

/// The connector's EDID read from the X server.
pub fn read_connector_edid(connector: &str) -> Option<Vec<u8>> {
    xrandr_connector_edid(&run_bounded(&args(&["xrandr", "--props"]))?, connector)
}

/// Every HDMI/DP PCM of every card with the monitor its ELD reports (an empty ELD is recorded as no
/// monitor, so the IEC958 switch rank still counts it).
pub fn read_pcm_elds() -> Vec<AudioPcm> {
    let cards = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
    let mut pcms = Vec::new();
    for card in asound_card_numbers(&cards) {
        let c = card.to_string();
        let Some(controls) = run_bounded(&args(&["amixer", "-c", &c, "controls"])) else {
            continue;
        };
        for device in amixer_eld_devices(&controls) {
            let id = format!("iface=PCM,name=ELD,device={device}");
            let bytes = run_bounded(&args(&["amixer", "-c", &c, "cget", &id]))
                .and_then(|t| amixer_bytes(&t))
                .unwrap_or_default();
            pcms.push(AudioPcm {
                card,
                device,
                id: eld_monitor_id(&bytes),
            });
        }
    }
    pcms
}

/// Resolve `connector` on this box, with the PCMs it read (for the IEC958 switch).
pub fn resolve_connector(connector: &str) -> (HdmiResolution, Vec<AudioPcm>) {
    let edid = read_connector_edid(connector);
    let pcms = read_pcm_elds();
    (resolve_from(edid.as_deref(), &pcms), pcms)
}

/// Turn on the IEC958 switch of the resolved PCM. Returns a short note for the leg's log line.
pub fn iec958_on(card: i32, device: i32, pcms: &[AudioPcm]) -> String {
    let devices: Vec<i32> = pcms
        .iter()
        .filter(|p| p.card == card)
        .map(|p| p.device)
        .collect();
    let Some(controls) = run_bounded(&args(&["amixer", "-c", &card.to_string(), "controls"]))
    else {
        return "IEC958 switch not readable".into();
    };
    let switches = amixer_iec958_switch_count(&controls);
    match iec958_on_argv(card, &devices, device, switches) {
        None => format!(
            "IEC958 switch ambiguous ({switches} switches, {} HDMI PCMs) -- untouched",
            devices.len()
        ),
        Some(argv) => match run_bounded(&argv) {
            Some(_) => "IEC958 switch on".into(),
            None => "IEC958 switch write FAILED".into(),
        },
    }
}

/// The leg's state for `/api/state` and the status line (see [`HdmiLegStats`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HdmiLegFacet {
    /// The last resolve's matcher result ([`NOT_RESOLVED_YET`] before the first).
    pub result: i32,
    /// The resolved card and device, -1 while unresolved.
    pub card: i32,
    pub device: i32,
    /// aplay is running on the resolved PCM.
    pub running: bool,
    /// Resolves since the hub started (one per spawn attempt).
    pub resolves: u64,
    /// aplay xruns (its `underrun!!!` lines) since the hub started.
    pub xruns: u64,
}

impl Default for HdmiLegFacet {
    fn default() -> Self {
        HdmiLegFacet {
            result: NOT_RESOLVED_YET,
            card: -1,
            device: -1,
            running: false,
            resolves: 0,
            xruns: 0,
        }
    }
}

impl HdmiLegFacet {
    /// `hw:<card>,<device>` once resolved.
    pub fn hw(&self) -> Option<String> {
        (self.result == MATCH && self.card >= 0 && self.device >= 0)
            .then(|| format!("hw:{},{}", self.card, self.device))
    }

    /// The named reason of the last resolve.
    pub fn reason(&self) -> &'static str {
        if self.result == NOT_RESOLVED_YET {
            "not resolved yet"
        } else {
            reason(self.result)
        }
    }
}

/// Shared, atomic counters of one HDMI leg, read by `/api/state` without a lock.
#[derive(Debug)]
pub struct HdmiLegStats {
    result: AtomicI32,
    card: AtomicI32,
    device: AtomicI32,
    running: AtomicBool,
    resolves: AtomicU64,
    xruns: AtomicU64,
}

impl Default for HdmiLegStats {
    fn default() -> Self {
        HdmiLegStats {
            result: AtomicI32::new(NOT_RESOLVED_YET),
            card: AtomicI32::new(-1),
            device: AtomicI32::new(-1),
            running: AtomicBool::new(false),
            resolves: AtomicU64::new(0),
            xruns: AtomicU64::new(0),
        }
    }
}

impl HdmiLegStats {
    /// Record one resolve: a lost match never keeps the old device.
    pub fn record_resolution(&self, res: &HdmiResolution) {
        let (card, device) = res.pcm.unwrap_or((-1, -1));
        self.card.store(card, Ordering::Relaxed);
        self.device.store(device, Ordering::Relaxed);
        self.result.store(res.result, Ordering::Relaxed);
        self.resolves.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one aplay xrun.
    pub fn record_xrun(&self) {
        self.xruns.fetch_add(1, Ordering::Relaxed);
    }

    /// aplay started (`true`) or exited (`false`).
    pub fn set_running(&self, running: bool) {
        self.running.store(running, Ordering::Relaxed);
    }

    /// A point-in-time snapshot.
    pub fn snapshot(&self) -> HdmiLegFacet {
        HdmiLegFacet {
            result: self.result.load(Ordering::Relaxed),
            card: self.card.load(Ordering::Relaxed),
            device: self.device.load(Ordering::Relaxed),
            running: self.running.load(Ordering::Relaxed),
            resolves: self.resolves.load(Ordering::Relaxed),
            xruns: self.xruns.load(Ordering::Relaxed),
        }
    }
}
