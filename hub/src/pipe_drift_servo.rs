//! The `pw-cat` / aplay egress pipes' single-frame drift servo (issue 1401), kept for the egress
//! only. LANE-2 LEFTOVER of issue 1461: the ingress legs moved to the resampler
//! ([`crate::asrc`], [`crate::vban_jitter::NetworkFill`]); the egress crossing (the cutters' card
//! and the vestibule HDMI through [`crate::pipe_fill::PipeFillControl`]) still drops or repeats a
//! frame here until issue 1461's second lane gives it the same resampler. That lane deletes this
//! module, [`stretch_block`] and `pipe_fill::stretch_interleaved` with it.
//!
//! The logic is the issue-1401 servo verbatim, minus the setpoint walk only the ingress used (the
//! pipe never moves its setpoint): every 1 s the mean fill is compared with the setpoint;
//! [`servo_corrections`] turns the error into the next second's budget of single-frame drops (fill
//! high) or repeats (fill low), spread evenly over that second and never closer than
//! [`SERVO_MIN_SPACING_FRAMES`]. Each frame is spread across its block by [`stretch_block`].

/// The servo's averaging window in output frames: 1 s at the 48 kHz hub rate.
pub const SERVO_WINDOW_FRAMES: usize = 48_000;

/// No correction within this many frames of the setpoint (0.33 ms).
pub const SERVO_DEADBAND_FRAMES: usize = 16;

/// Up to this mean error the servo is GENTLE: one corrected frame per second for every
/// [`SERVO_GENTLE_DIV`] frames beyond the band.
pub const SERVO_KNEE_FRAMES: usize = 128;

/// The gentle slope below [`SERVO_KNEE_FRAMES`]: one corrected frame a second per this many frames.
pub const SERVO_GENTLE_DIV: usize = 16;

/// The steep slope past [`SERVO_KNEE_FRAMES`]: one more corrected frame a second per this many.
pub const SERVO_STEEP_DIV: usize = 2;

/// At most one single-frame correction per this many output frames (1000 ppm).
pub const SERVO_MIN_SPACING_FRAMES: usize = 1_000;

/// The most corrections one second can hold at that spacing.
const SERVO_MAX_PER_WINDOW: usize = SERVO_WINDOW_FRAMES / SERVO_MIN_SPACING_FRAMES;

/// The servo's budget for the next second for a mean fill `error` frames off the setpoint.
pub fn servo_corrections(error: usize) -> usize {
    if error <= SERVO_DEADBAND_FRAMES {
        return 0;
    }
    let gentle = (error.min(SERVO_KNEE_FRAMES) - SERVO_DEADBAND_FRAMES).div_ceil(SERVO_GENTLE_DIV);
    let steep = error
        .saturating_sub(SERVO_KNEE_FRAMES)
        .div_ceil(SERVO_STEEP_DIV);
    (gentle + steep).min(SERVO_MAX_PER_WINDOW)
}

/// What the servo does to one block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServoStep {
    /// No correction on this block.
    Keep,
    /// The fill sits high: an egress write puts one frame fewer into its pipe.
    Drop,
    /// The fill sits low: an egress write puts one frame more into its pipe.
    Repeat,
}

/// The servo's depth numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DriftServoStats {
    /// The mean fill over the last completed 1 s window (0 before the first).
    pub depth_frames: usize,
    /// The lowest fill in that window.
    pub depth_min_frames: usize,
    pub servo_drops: u64,
    pub servo_repeats: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Correction {
    None,
    Drop,
    Repeat,
}

/// The egress drift servo of one pipe.
#[derive(Debug, Clone)]
pub struct DriftServo {
    setpoint: usize,
    win_sum: u64,
    win_min: usize,
    win_pops: u64,
    win_frames: usize,
    correction: Correction,
    budget: usize,
    interval: usize,
    since_correction: usize,
    last_mean: usize,
    last_min: usize,
    servo_drops: u64,
    servo_repeats: u64,
}

impl DriftServo {
    /// A servo holding `setpoint_frames`, clamped into `1..=cap_frames`.
    pub fn new(setpoint_frames: usize, cap_frames: usize) -> Self {
        DriftServo {
            setpoint: setpoint_frames.clamp(1, cap_frames.max(1)),
            win_sum: 0,
            win_min: usize::MAX,
            win_pops: 0,
            win_frames: 0,
            correction: Correction::None,
            budget: 0,
            interval: SERVO_MIN_SPACING_FRAMES,
            since_correction: 0,
            last_mean: 0,
            last_min: 0,
            servo_drops: 0,
            servo_repeats: 0,
        }
    }

    /// The setpoint in frames.
    pub fn target(&self) -> usize {
        self.setpoint
    }

    /// Forget the current window and any pending correction (a guard refilled or trimmed the
    /// pipe).
    pub fn restart(&mut self) {
        self.win_sum = 0;
        self.win_min = usize::MAX;
        self.win_pops = 0;
        self.win_frames = 0;
        self.correction = Correction::None;
        self.budget = 0;
    }

    /// Add one block's fill to the 1 s window and decide this block's correction. A drop needs
    /// `fill > frames`, a repeat `frames > 1`.
    pub fn servo_step(&mut self, fill: usize, frames: usize) -> ServoStep {
        self.observe(fill, frames);
        self.since_correction = self.since_correction.saturating_add(frames);
        if self.budget == 0 || self.since_correction < self.interval {
            return ServoStep::Keep;
        }
        let step = match self.correction {
            Correction::Drop if fill > frames => {
                self.servo_drops += 1;
                ServoStep::Drop
            }
            Correction::Repeat if frames > 1 => {
                self.servo_repeats += 1;
                ServoStep::Repeat
            }
            _ => ServoStep::Keep,
        };
        if step != ServoStep::Keep {
            self.budget -= 1;
            self.since_correction = 0;
        }
        step
    }

    fn observe(&mut self, fill: usize, frames: usize) {
        self.win_sum = self.win_sum.saturating_add(fill as u64);
        self.win_min = self.win_min.min(fill);
        self.win_pops += 1;
        self.win_frames = self.win_frames.saturating_add(frames);
        if self.win_frames < SERVO_WINDOW_FRAMES {
            return;
        }
        let mean = usize::try_from(self.win_sum / self.win_pops).unwrap_or(usize::MAX);
        self.last_mean = mean;
        self.last_min = self.win_min;
        let n = servo_corrections(mean.abs_diff(self.setpoint));
        if n == 0 {
            self.correction = Correction::None;
            self.budget = 0;
        } else {
            self.correction = if mean < self.setpoint {
                Correction::Repeat
            } else {
                Correction::Drop
            };
            self.budget = n;
            self.interval = (SERVO_WINDOW_FRAMES / n).max(SERVO_MIN_SPACING_FRAMES);
        }
        self.win_sum = 0;
        self.win_min = usize::MAX;
        self.win_pops = 0;
        self.win_frames = 0;
    }

    /// The live numbers.
    pub fn stats(&self) -> DriftServoStats {
        DriftServoStats {
            depth_frames: self.last_mean,
            depth_min_frames: self.last_min,
            servo_drops: self.servo_drops,
            servo_repeats: self.servo_repeats,
        }
    }
}

/// Resample one block of `input` to `out_len` samples by linear interpolation, keeping both end
/// samples: the servo's single-frame drop or repeat spread across the whole block.
pub fn stretch_block(input: &[i16], out_len: usize) -> Vec<i16> {
    let n_in = input.len();
    if n_in == out_len {
        return input.to_vec();
    }
    if out_len == 0 {
        return Vec::new();
    }
    let Some(&first) = input.first() else {
        return vec![0; out_len];
    };
    if n_in == 1 || out_len == 1 {
        return vec![first; out_len];
    }
    let num = (n_in - 1) as u64;
    let den = (out_len - 1) as u64;
    (0..out_len)
        .map(|i| {
            let pos = i as u64 * num;
            let idx = (pos / den) as usize;
            let rem = pos % den;
            if rem == 0 {
                input[idx]
            } else {
                let a = f64::from(input[idx]);
                let b = f64::from(input[idx + 1]);
                (a + (b - a) * rem as f64 / den as f64).round() as i16
            }
        })
        .collect()
}
