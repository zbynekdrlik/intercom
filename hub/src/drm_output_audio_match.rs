//! issue 1424 — the Rust twin of `vendor/obs-studio/libobs/obs-drm-output-audio-match.h`, the PURE
//! decisions of the fixed HDMI output's audio leg (owner 13.10.2026: the output plays the OBS program
//! mix on the SAME connector's HDMI audio by itself, straight into that connector's ALSA hw PCM, with
//! no PipeWire, no monitoring device and no per-box setting; no match = no audio and one named log
//! line).
//!
//! Every function mirrors its C twin line for line; `tests/drm_output_audio_1424.rs` compiles the C
//! header and requires both to print the same answer for every vector (the strih-pp fixtures in
//! `tests/fixtures/drm_output_audio_1424.tsv` plus the shapes that must fail).
//!
//! The vendor bytes: EDID bytes 8..11 (manufacturer PNP id big-endian, product code little-endian)
//! are the SAME four bytes as HDA ELD bytes 16..19, so the match compares raw bytes and never swaps.

/// The longest monitor name kept (an ELD caps it at 16, an EDID descriptor carries 13).
pub const NAME_MAX: usize = 16;
/// The fixed part of a CEA-861-D ELD in front of the monitor name.
pub const ELD_FIXED_BYTES: usize = 20;

/// Results. `MATCH` is the only one that plays; every other one names why there is no audio.
pub const MATCH: i32 = 0;
pub const NO_EDID: i32 = 1;
pub const NO_MONITOR: i32 = 2;
pub const NO_MATCH: i32 = 3;
pub const AMBIGUOUS: i32 = 4;

/// One monitor as an EDID or an ELD describes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MonitorId {
    pub valid: bool,
    pub vendor: [u8; 4],
    pub name: String,
}

/// One HDMI/DP PCM and the monitor its ELD reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioPcm {
    pub card: i32,
    pub device: i32,
    pub id: MonitorId,
}

/// Placing one mix block against the picture (see `plan_block`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AudioPlan {
    pub silence_ns: u64,
    pub drop_ns: u64,
    pub offset_ns: i64,
    /// +1 = repeat one frame, -1 = drop one frame (the clock-drift walk), 0 = none.
    pub nudge_frames: i32,
}

/// A monitor name: at most `n` bytes, ends at NUL or LF, trailing spaces dropped, non-printable
/// bytes read as '?'.
fn copy_name(src: &[u8], n: usize) -> String {
    let n = n.min(NAME_MAX).min(src.len());
    let mut out: Vec<u8> = Vec::with_capacity(n);
    for &c in &src[..n] {
        if c == 0 || c == 0x0A {
            break;
        }
        out.push(if (0x20..0x7F).contains(&c) { c } else { b'?' });
    }
    while out.last() == Some(&b' ') {
        out.pop();
    }
    String::from_utf8(out).unwrap_or_default()
}

/// The connector's monitor from its EDID base block (header, checksum, vendor bytes, the 0xFC
/// display-name descriptor). An invalid EDID is `valid == false`.
pub fn edid_monitor_id(edid: &[u8]) -> MonitorId {
    const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
    let mut out = MonitorId::default();
    if edid.len() < 128 || edid[..8] != HEADER {
        return out;
    }
    let sum = edid[..128].iter().fold(0u8, |a, &b| a.wrapping_add(b));
    if sum != 0 {
        return out;
    }
    out.vendor.copy_from_slice(&edid[8..12]);
    let mut d = 54;
    while d + 18 <= 126 {
        let p = &edid[d..d + 18];
        if p[0] == 0 && p[1] == 0 && p[2] == 0 && p[3] == 0xFC {
            out.name = copy_name(&p[5..], 13);
            break;
        }
        d += 18;
    }
    out.valid = true;
    out
}

/// The monitor on one HDMI/DP PCM from its raw ELD. Empty, short, partial (not version 2) or with a
/// name past the buffer is `valid == false`.
pub fn eld_monitor_id(eld: &[u8]) -> MonitorId {
    let mut out = MonitorId::default();
    if eld.len() < ELD_FIXED_BYTES || (eld[0] >> 3) != 2 {
        return out;
    }
    let mnl = (eld[4] & 0x1F) as usize;
    if mnl > NAME_MAX || ELD_FIXED_BYTES + mnl > eld.len() {
        return out;
    }
    out.vendor.copy_from_slice(&eld[16..20]);
    out.name = copy_name(&eld[ELD_FIXED_BYTES..], mnl);
    out.valid = true;
    out
}

/// Which PCM carries the connector's monitor: the vendor bytes decide, the name only breaks a tie,
/// two equal candidates are `AMBIGUOUS`. Returns (result, index on `MATCH`).
pub fn pick_pcm(want: &MonitorId, pcms: &[AudioPcm]) -> (i32, Option<usize>) {
    if !want.valid {
        return (NO_EDID, None);
    }
    let (mut present, mut hits, mut hit, mut named, mut named_hit) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    for (i, p) in pcms.iter().enumerate() {
        if !p.id.valid {
            continue;
        }
        present += 1;
        if p.id.vendor != want.vendor {
            continue;
        }
        hits += 1;
        hit = i;
        if !want.name.is_empty() && p.id.name == want.name {
            named += 1;
            named_hit = i;
        }
    }
    if present == 0 {
        return (NO_MONITOR, None);
    }
    if hits == 0 {
        return (NO_MATCH, None);
    }
    if hits == 1 {
        return (MATCH, Some(hit));
    }
    if named == 1 {
        return (MATCH, Some(named_hit));
    }
    (AMBIGUOUS, None)
}

/// The card's "IEC958 Playback Switch" index of PCM `device`: its rank among the card's
/// ELD-carrying PCM devices (the HDA driver creates one switch per HDMI/DP PCM, in PCM order);
/// `None` when `device` is not one of them, or when the card's switch count is not its HDMI PCM
/// count (an analog codec's own S/PDIF switch: ambiguous, never a guess). Duplicates never count
/// twice.
pub fn iec958_index(devices: &[i32], device: i32, n_switches: usize) -> Option<i32> {
    let mut seen: Vec<i32> = Vec::with_capacity(devices.len());
    let mut rank = 0;
    let mut found = false;
    for &d in devices {
        if seen.contains(&d) {
            continue;
        }
        seen.push(d);
        if d == device {
            found = true;
        } else if d < device {
            rank += 1;
        }
    }
    (found && seen.len() == n_switches).then_some(rank)
}

/// Whether a failed resolve is worth another read inside the bounded window (an ELD the HDA driver
/// publishes only once a mode is driven). A missing EDID never comes back by waiting.
pub fn retryable(result: i32) -> bool {
    result != MATCH && result != NO_EDID
}

/// The named reason a result gives in the leg's log line.
pub fn reason(result: i32) -> &'static str {
    match result {
        MATCH => "matched",
        NO_EDID => "the connector has no readable EDID",
        NO_MONITOR => "no HDMI/DP PCM reports a monitor (no ELD)",
        NO_MATCH => "no PCM's ELD matches the connector's EDID",
        AMBIGUOUS => "two PCMs match the connector's EDID (identical monitors)",
        _ => "unknown",
    }
}

/// Placing one mix block: hold silence when the audio is early (never sooner than `now + floor`),
/// trim at most one block off the front when the buffer grew past the floor, inside the threshold
/// walk a drift back one frame per block outside `band_ns`, and report the kept first sample's
/// offset against its picture (+ = audio behind the picture).
#[allow(clippy::too_many_arguments)]
pub fn plan_block(
    block_ts_ns: u64,
    present_delay_ns: u64,
    now_ns: u64,
    stream_latency_ns: u64,
    floor_ns: u64,
    threshold_ns: u64,
    block_ns: u64,
    band_ns: u64,
) -> AudioPlan {
    let mut plan = AudioPlan::default();
    let want = block_ts_ns.wrapping_add(present_delay_ns) as i64;
    let natural = now_ns.wrapping_add(stream_latency_ns) as i64;
    let safe = now_ns.wrapping_add(floor_ns) as i64;
    let desired = want.max(safe);
    let hold = desired.wrapping_sub(natural);
    let thr = threshold_ns as i64;
    if hold > thr {
        plan.silence_ns = hold as u64;
    } else if hold.wrapping_neg() > thr {
        let blk = block_ns as i64;
        plan.drop_ns = hold.wrapping_neg().min(blk) as u64;
    } else if band_ns != 0 && hold > band_ns as i64 {
        plan.nudge_frames = 1;
    } else if band_ns != 0 && hold.wrapping_neg() > band_ns as i64 {
        plan.nudge_frames = -1;
    }
    plan.offset_ns = natural
        .wrapping_add(plan.silence_ns as i64)
        .wrapping_sub(want)
        .wrapping_sub(plan.drop_ns as i64);
    plan
}

/// The output's present delay, smoothed 1/8 per sample; a sample outside (0, 1 s) is ignored.
pub fn present_ewma(prev_ns: u64, sample_ns: u64) -> u64 {
    if sample_ns == 0 || sample_ns >= 1_000_000_000 {
        return prev_ns;
    }
    if prev_ns == 0 {
        return sample_ns;
    }
    prev_ns - prev_ns / 8 + sample_ns / 8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eld(vendor: [u8; 4], name: &str) -> Vec<u8> {
        let mnl = name.len();
        let dw = (16 + mnl + 3).div_ceil(4);
        let mut b = vec![0u8; 4 + dw * 4];
        b[0] = 2 << 3;
        b[2] = dw as u8;
        b[4] = (3 << 5) | mnl as u8;
        b[5] = 1 << 4;
        b[16..20].copy_from_slice(&vendor);
        b[20..20 + mnl].copy_from_slice(name.as_bytes());
        b
    }

    #[test]
    fn eld_reads_vendor_bytes_and_name() {
        let id = eld_monitor_id(&eld([0x09, 0xd1, 0x24, 0x80], "BenQ LCD"));
        assert!(id.valid);
        assert_eq!(id.vendor, [0x09, 0xd1, 0x24, 0x80]);
        assert_eq!(id.name, "BenQ LCD");
        assert!(!eld_monitor_id(&[]).valid, "no monitor = an empty ELD");
    }

    #[test]
    fn name_breaks_a_vendor_tie_but_never_matches_alone() {
        let want = MonitorId {
            valid: true,
            vendor: [1, 2, 3, 4],
            name: "A".into(),
        };
        let a = AudioPcm {
            card: 0,
            device: 3,
            id: eld_monitor_id(&eld([1, 2, 3, 4], "A")),
        };
        let b = AudioPcm {
            card: 0,
            device: 7,
            id: eld_monitor_id(&eld([1, 2, 3, 4], "B")),
        };
        assert_eq!(pick_pcm(&want, &[a.clone(), b]), (MATCH, Some(0)));
        let other = AudioPcm {
            card: 0,
            device: 8,
            id: eld_monitor_id(&eld([9, 9, 9, 9], "A")),
        };
        assert_eq!(pick_pcm(&want, &[other]), (NO_MATCH, None));
        assert_eq!(pick_pcm(&want, &[a.clone(), a]), (AMBIGUOUS, None));
    }

    #[test]
    fn iec958_index_is_the_pcm_rank_on_its_card() {
        // strih-snv card0: HDMI PCMs 3/7/8/9 <-> switch indexes 0/1/2/3
        assert_eq!(iec958_index(&[3, 7, 8, 9], 3, 4), Some(0));
        assert_eq!(iec958_index(&[9, 8, 7, 3], 8, 4), Some(2));
        assert_eq!(iec958_index(&[3, 7, 7], 7, 2), Some(1));
        assert_eq!(iec958_index(&[3, 7], 0, 2), None);
        assert_eq!(
            iec958_index(&[3, 7, 8], 3, 4),
            None,
            "an extra S/PDIF switch: ambiguous"
        );
    }

    #[test]
    fn plan_trims_at_most_one_block() {
        let ms = 1_000_000u64;
        let p = plan_block(
            1000 * ms,
            0,
            1000 * ms,
            500 * ms,
            60 * ms,
            42 * ms,
            21 * ms,
            5 * ms,
        );
        assert_eq!(p.drop_ns, 21 * ms);
        assert_eq!(p.silence_ns, 0);
    }
}
