//! The VBAN ingress sample-rate policy (issue 1345, 24.9.2026 production fix).
//!
//! A VBAN header carries the stream's sample rate. The FOH desk sends `fohabl-strih` at 96 kHz
//! (103 frames x 2 ch per packet); the hub mixes at 48 kHz. Pushing those samples straight into the
//! 48 kHz jitter ring overran it on about 60 % of packets and played the survivors at the wrong
//! rate — the corrupted strih program audio and cans of the 24.9 production.
//!
//! Each VBAN input stream therefore owns one [`VbanRateConverter`] between decode and the ring push:
//!
//! * the hub rate (48 kHz) passes through byte-identical, with no filter and no copy;
//! * 2x and 4x the hub rate (96 / 192 kHz) are decimated 2:1 / 4:1 by a Kaiser windowed-sinc FIR
//!   ([`crate::fir::FirDecimator`], the same machinery as the PCMU leg), flat to about 20 kHz and at
//!   least 70 dB down from 24.5 kHz, with the filter history and phase carried across packets;
//! * any other rate (44.1 / 88.2 kHz, ...) is REJECTED — counted in `rate_rejects` and reported to
//!   the caller once per transition, never mis-played;
//! * a rate change (or a channel-count change) restarts the filter from silence.
//!
//! [`VbanRateStats`] is the lock-free slot the receive task publishes into and the block loop reads
//! for `/api/state`. Pure, std-only.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::fir::{kaiser_lowpass_taps, FirDecimator};

/// The integer decimation ratios the hub accepts: passthrough, 2:1 and 4:1.
pub const SUPPORTED_DECIMATIONS: [usize; 3] = [1, 2, 4];

/// FIR taps per unit of decimation: 2:1 = 127 taps, 4:1 = 255 taps, so the transition band stays
/// the same width in Hz (about 3.3 kHz) at every input rate. The group delay is `(taps - 1) / 2`
/// input samples — about 0.66 ms at either rate.
const TAPS_PER_DECIMATION: usize = 64;

/// The -6 dB cut-off as a fraction of the OUTPUT rate: 22 kHz at a 48 kHz hub. The pass band is flat
/// (< 0.1 dB) to 20 kHz and everything that would fold back (>= 24.5 kHz) is >= 70 dB down.
const CUTOFF_OF_OUTPUT_RATE: f64 = 22.0 / 48.0;

/// The Kaiser shape parameter for a ~70 dB stop band (`0.1102 * (70 - 8.7)`).
const KAISER_BETA: f64 = 6.755;

/// The decimation ratio that takes `in_rate` to the hub's `out_rate`: `Some(1 | 2 | 4)` for an
/// exact supported multiple, `None` for anything else (including a zero rate).
pub fn decimation_factor(in_rate: u32, out_rate: u32) -> Option<usize> {
    if out_rate == 0 {
        return None;
    }
    SUPPORTED_DECIMATIONS
        .into_iter()
        .find(|&f| u64::from(out_rate) * f as u64 == u64::from(in_rate))
}

/// A fresh (silent-history) anti-alias decimator for a `factor`:1 ratio (`factor` >= 2).
pub fn decimator(factor: usize) -> FirDecimator {
    let factor = factor.max(1);
    let taps = kaiser_lowpass_taps(
        TAPS_PER_DECIMATION * factor - 1,
        CUTOFF_OF_OUTPUT_RATE / factor as f64,
        KAISER_BETA,
    );
    FirDecimator::new(taps, factor)
}

/// The result of passing one packet through a [`VbanRateConverter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateStep {
    /// Planar channels at the hub rate, or `None` when the packet's rate is rejected.
    pub channels: Option<Vec<Vec<i16>>>,
    /// The stream's rate differs from its previous packet's (or this is its first packet) — the
    /// caller logs it once: a warn when `channels` is `None`, an info otherwise.
    pub rate_changed: bool,
}

/// One VBAN input stream's rate policy + per-channel decimator state.
#[derive(Debug, Clone)]
pub struct VbanRateConverter {
    out_rate: u32,
    in_rate: Option<u32>,
    /// One decimator per channel; empty for a passthrough or rejected rate.
    decimators: Vec<FirDecimator>,
    rate_rejects: u64,
}

impl VbanRateConverter {
    /// A converter to the hub's `out_rate` that has seen no packet yet.
    pub fn new(out_rate: u32) -> Self {
        VbanRateConverter {
            out_rate,
            in_rate: None,
            decimators: Vec::new(),
            rate_rejects: 0,
        }
    }

    /// The rate of the most recent packet (even a rejected one), `None` before the first packet.
    pub fn sample_rate(&self) -> Option<u32> {
        self.in_rate
    }

    /// Packets dropped because their rate is not a supported multiple of the hub rate.
    pub fn rate_rejects(&self) -> u64 {
        self.rate_rejects
    }

    /// Convert one packet's planar `channels`, sampled at `in_rate`, to the hub rate.
    pub fn process(&mut self, in_rate: u32, channels: Vec<Vec<i16>>) -> RateStep {
        let rate_changed = self.in_rate != Some(in_rate);
        if rate_changed {
            self.in_rate = Some(in_rate);
            self.decimators.clear();
        }
        let factor = match decimation_factor(in_rate, self.out_rate) {
            Some(f) => f,
            None => {
                self.rate_rejects += 1;
                return RateStep {
                    channels: None,
                    rate_changed,
                };
            }
        };
        if factor == 1 {
            return RateStep {
                channels: Some(channels),
                rate_changed,
            };
        }
        if self.decimators.len() != channels.len() {
            // The first packet at this rate, or the stream changed its channel count: every channel
            // restarts from silence so all of them stay in lock-step.
            self.decimators = (0..channels.len()).map(|_| decimator(factor)).collect();
        }
        let out = channels
            .iter()
            .zip(self.decimators.iter_mut())
            .map(|(ch, d)| d.process(ch))
            .collect();
        RateStep {
            channels: Some(out),
            rate_changed,
        }
    }
}

/// The lock-free per-stream slot the VBAN receive task publishes a converter's state into and the
/// block loop reads for `/api/state`. A rate of 0 means "no packet yet".
#[derive(Debug, Default)]
pub struct VbanRateStats {
    sample_rate: AtomicU32,
    rate_rejects: AtomicU64,
}

impl VbanRateStats {
    /// Copy `conv`'s current rate + reject count into the slot.
    pub fn publish(&self, conv: &VbanRateConverter) {
        self.sample_rate
            .store(conv.sample_rate().unwrap_or(0), Ordering::Relaxed);
        self.rate_rejects
            .store(conv.rate_rejects(), Ordering::Relaxed);
    }

    /// The last published rate, `None` before the stream's first packet.
    pub fn sample_rate(&self) -> Option<u32> {
        match self.sample_rate.load(Ordering::Relaxed) {
            0 => None,
            r => Some(r),
        }
    }

    /// The last published reject count.
    pub fn rate_rejects(&self) -> u64 {
        self.rate_rejects.load(Ordering::Relaxed)
    }
}
