//! issue 1438: an `alsa` participant's card leg resolved from the matrix TOML and checked at load
//! (kept out of `matrix.rs`, which holds the routing; the leg's pure half is `alsa_card`): the card
//! ID, the period, the card's channel counts, the two channel maps, one participant per card +
//! device, and a playback pipe that fits a Linux pipe.

use std::collections::HashSet;

use anyhow::{anyhow, bail, Result};

use crate::alsa_card::{
    card_id_ok, identity_map, parse_channel_map, AlsaCardSpec, ALSA_CARD_DEFAULT_PERIOD_FRAMES,
    ALSA_CARD_MAX_CHANNELS, ALSA_CARD_PERIODS, ALSA_CARD_PERIOD_RANGE, FORMAT_PREFERENCE,
};
use crate::matrix::{Participant, ADAPTER_ALSA};
use crate::pipe_fill::pipe_high_frames_for;

/// A Linux pipe's default capacity: the aplay stdin pipe holds this much before a write blocks.
pub const LINUX_PIPE_BYTES: usize = 65_536;

/// The bytes the playback pipe holds at its trim mark (the aplay pipe guard: the target and two
/// reads, plus aplay's own ALSA buffer and the period it holds), at the widest format the leg may
/// pick. Above [`LINUX_PIPE_BYTES`] a write blocks the leg's thread before the trim can act.
pub fn playback_pipe_trim_bytes(period_frames: usize, hw_channels: usize) -> usize {
    let widest = FORMAT_PREFERENCE
        .iter()
        .map(|f| f.bytes_per_sample())
        .max()
        .unwrap_or(4);
    let frames = pipe_high_frames_for(period_frames) + (ALSA_CARD_PERIODS + 1) * period_frames;
    frames * hw_channels * widest
}

/// Check one participant's `alsa_*` keys at load: an `alsa` participant resolves to its spec (and
/// takes its card + device, refused when another participant holds it); every other adapter must
/// carry none of the keys.
pub fn check_alsa(
    id: usize,
    p: &Participant,
    alsa_pcms: &mut HashSet<(String, u32)>,
) -> Result<Option<AlsaCardSpec>> {
    if p.adapter != ADAPTER_ALSA {
        if has_alsa_keys(p) {
            bail!(
                "participant '{}': the alsa_* keys are only for adapter 'alsa' (got '{}')",
                p.name,
                p.adapter
            );
        }
        return Ok(None);
    }
    let spec = alsa_card_spec(id, p)?;
    if !alsa_pcms.insert((spec.card.clone(), spec.device)) {
        bail!(
            "participant '{}': card {} device {} is already opened by another participant",
            p.name,
            spec.card,
            spec.device
        );
    }
    Ok(Some(spec))
}

/// Whether a participant carries any `alsa_*` key.
pub fn has_alsa_keys(p: &Participant) -> bool {
    p.alsa_card.is_some()
        || p.alsa_device.is_some()
        || p.alsa_period_frames.is_some()
        || p.alsa_out_channels.is_some()
        || p.alsa_in_channels.is_some()
        || p.alsa_channel_map.is_some()
        || p.alsa_in_channel_map.is_some()
}

/// Resolve and check an `alsa` participant's card leg (issue 1438): the card ID, the period, the
/// card's channel counts and the two channel maps. `id` is the participant's index.
pub fn alsa_card_spec(id: usize, p: &Participant) -> Result<AlsaCardSpec> {
    let card = p.alsa_card.clone().unwrap_or_default();
    if !card_id_ok(&card) {
        bail!(
            "participant '{}': adapter 'alsa' needs alsa_card = the ALSA card ID (e.g. M4; letters, digits, _; at most 15; never a bare index), got '{}'",
            p.name,
            card
        );
    }
    if p.in_channels == 0 && p.out_channels == 0 {
        bail!(
            "participant '{}': adapter 'alsa' needs in_channels or out_channels > 0",
            p.name
        );
    }
    let device = p.alsa_device.unwrap_or(0);
    if device > 31 {
        bail!(
            "participant '{}': alsa_device {} out of range 0..=31",
            p.name,
            device
        );
    }
    let period_frames = p
        .alsa_period_frames
        .unwrap_or(ALSA_CARD_DEFAULT_PERIOD_FRAMES);
    if !ALSA_CARD_PERIOD_RANGE.contains(&period_frames) {
        bail!(
            "participant '{}': alsa_period_frames {} out of range {:?}",
            p.name,
            period_frames,
            ALSA_CARD_PERIOD_RANGE
        );
    }
    let hw_channels = |key: &str, set: Option<usize>, used: usize| -> Result<usize> {
        let hw = set.unwrap_or(used);
        if used > 0 && !(used..=ALSA_CARD_MAX_CHANNELS).contains(&hw) {
            bail!(
                "participant '{}': {} {} must hold the participant's {} channels (and at most {})",
                p.name,
                key,
                hw,
                used,
                ALSA_CARD_MAX_CHANNELS
            );
        }
        if used == 0 && set.is_some() {
            bail!(
                "participant '{}': {} is set but the participant uses no channel on that side",
                p.name,
                key
            );
        }
        Ok(hw)
    };
    let hw_out_channels = hw_channels("alsa_out_channels", p.alsa_out_channels, p.out_channels)?;
    let hw_in_channels = hw_channels("alsa_in_channels", p.alsa_in_channels, p.in_channels)?;
    let map = |key: &str,
               set: Option<&str>,
               used: usize,
               hw: usize,
               unique: bool|
     -> Result<Vec<usize>> {
        match set {
            None => Ok(identity_map(used)),
            Some(_) if used == 0 => bail!(
                "participant '{}': {} is set but the participant uses no channel on that side",
                p.name,
                key
            ),
            Some(m) => parse_channel_map(m, used, hw, unique)
                .map_err(|e| anyhow!("participant '{}': {} '{}' {}", p.name, key, m, e)),
        }
    };
    let out_map = map(
        "alsa_channel_map",
        p.alsa_channel_map.as_deref(),
        p.out_channels,
        hw_out_channels,
        true,
    )?;
    let in_map = map(
        "alsa_in_channel_map",
        p.alsa_in_channel_map.as_deref(),
        p.in_channels,
        hw_in_channels,
        false,
    )?;
    if p.out_channels > 0
        && playback_pipe_trim_bytes(period_frames, hw_out_channels) > LINUX_PIPE_BYTES
    {
        bail!(
            "participant '{}': alsa_period_frames {} x alsa_out_channels {} does not fit the aplay pipe ({} bytes at its trim mark, a Linux pipe holds {}); use a shorter period",
            p.name,
            period_frames,
            hw_out_channels,
            playback_pipe_trim_bytes(period_frames, hw_out_channels),
            LINUX_PIPE_BYTES
        );
    }
    Ok(AlsaCardSpec {
        participant: id,
        card,
        device,
        period_frames,
        out_channels: p.out_channels,
        hw_out_channels,
        out_map,
        in_channels: p.in_channels,
        hw_in_channels,
        in_map,
    })
}
