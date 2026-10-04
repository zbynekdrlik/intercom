//! Issue 1401, design 5981457044 (ROZHODNUTÉ 5981448853): a moved setpoint is walked GENTLY.
//!
//! A program feed's target follows its sender's gaps (`adaptive_target`), and every change goes
//! through `NetworkFill::set_target`. That used to hand the drift servo a one-block (256-frame)
//! error, the servo's STEEP zone: a raise was walked at up to 48 corrections a second for about
//! 5 s (47 measured in the growing-gap replay), the flutter review 1 of the VBAN-leg servo
//! rejected. Now the servo holds a setpoint that walks to the new target at most
//! `SERVO_WALK_MAX_PER_WINDOW` (7) frames a second, the gentle zone's own ceiling; the drift
//! correction is measured against that setpoint and keeps its full budget on top.
//!
//! These tests drive the real `NetworkFill` on frame counts, popping one 256-frame block on the
//! hub's exact grid and feeding a clean sender at a fixed rate offset. They count every correction
//! by the output frame it lands on and bound them in EVERY 1 s span (a sliding window), so no
//! alignment between a measured second and the servo's own window can hide a burst.

use intercom_hub::vban_jitter::{
    servo_corrections, vban_cap_blocks, NetworkFill, PopPlan, SERVO_DEADBAND_FRAMES,
    SERVO_WALK_MAX_PER_WINDOW, SERVO_WINDOW_FRAMES, VBAN_PROGRAM_TARGET_BLOCKS,
};

const BLOCK: usize = 256;
const FLOOR: usize = VBAN_PROGRAM_TARGET_BLOCKS * BLOCK;
const RAISED: usize = FLOOR + BLOCK;
const POPS_PER_S: usize = SERVO_WINDOW_FRAMES / BLOCK;

/// One program-feed leg on the real controller: a sender `ppm` off the hub, its frames arriving
/// evenly (no jitter), one pop per hub block.
struct Leg {
    f: NetworkFill,
    fill: usize,
    per_pop: f64,
    owed: f64,
    out_frames: usize,
    /// (output frame, true = a drop) of every correction.
    corrections: Vec<(usize, bool)>,
    ran_dry: u64,
    overruns: u64,
    /// The pre-pop fills and setpoints of the current second, and every finished second's means.
    sec_fill: usize,
    sec_setpoint: usize,
    sec_pops: usize,
    seconds: Vec<(usize, usize)>,
}

impl Leg {
    /// A leg primed at `target` (cap by the derived rule) whose sender runs `ppm` off the hub.
    fn new(target: usize, ppm: f64) -> Self {
        let cap = vban_cap_blocks(target / BLOCK) * BLOCK;
        Leg {
            f: NetworkFill::new(target, cap),
            fill: target,
            per_pop: BLOCK as f64 * (1.0 + ppm * 1e-6),
            owed: 0.0,
            out_frames: 0,
            corrections: Vec::new(),
            ran_dry: 0,
            overruns: 0,
            sec_fill: 0,
            sec_setpoint: 0,
            sec_pops: 0,
            seconds: Vec::new(),
        }
    }

    fn run_s(&mut self, secs: usize) {
        for _ in 0..secs * POPS_PER_S {
            self.pop();
        }
    }

    fn pop(&mut self) {
        let before = self.f.stats();
        self.sec_fill += self.fill;
        self.sec_setpoint += self.f.setpoint();
        self.sec_pops += 1;
        match self.f.plan_pop(self.fill, BLOCK) {
            PopPlan::Silent { ran_dry } => self.ran_dry += u64::from(ran_dry),
            PopPlan::Audio { skip, take } => self.fill -= skip + take,
        }
        let after = self.f.stats();
        if after.servo_drops > before.servo_drops {
            self.corrections.push((self.out_frames, true));
        }
        if after.servo_repeats > before.servo_repeats {
            self.corrections.push((self.out_frames, false));
        }
        self.out_frames += BLOCK;
        if self.sec_pops == POPS_PER_S {
            self.seconds.push((
                self.sec_fill / self.sec_pops,
                self.sec_setpoint / self.sec_pops,
            ));
            (self.sec_fill, self.sec_setpoint, self.sec_pops) = (0, 0, 0);
        }
        self.owed += self.per_pop;
        let arrive = self.owed.floor();
        self.owed -= arrive;
        self.fill += arrive as usize;
        if let Some(keep) = self.f.overrun_keep(self.fill) {
            self.fill = keep;
            self.overruns += 1;
        }
    }

    /// The most corrections inside any span of one second of output from `from` on.
    fn max_in_any_second(&self, from: usize) -> usize {
        let at: Vec<usize> = self
            .corrections
            .iter()
            .map(|c| c.0)
            .filter(|&o| o >= from)
            .collect();
        let mut best = 0;
        let mut lo = 0;
        for hi in 0..at.len() {
            while at[hi] - at[lo] >= SERVO_WINDOW_FRAMES {
                lo += 1;
            }
            best = best.max(hi - lo + 1);
        }
        best
    }

    fn count_from(&self, from: usize, drop: bool) -> usize {
        self.corrections
            .iter()
            .filter(|c| c.0 >= from && c.1 == drop)
            .count()
    }

    /// The first output second (counted from `from`) whose setpoint is the target.
    fn walk_done_after_s(&self, from: usize) -> Option<usize> {
        let first = from / SERVO_WINDOW_FRAMES;
        self.seconds[first..]
            .iter()
            .position(|&(_, sp)| sp == self.f.target())
    }
}

#[test]
fn a_raised_target_is_walked_in_at_no_more_than_seven_corrections_a_second() {
    assert_eq!(
        SERVO_WALK_MAX_PER_WINDOW, 7,
        "the gentle zone's own ceiling"
    );
    assert_eq!(SERVO_WALK_MAX_PER_WINDOW, servo_corrections(128));
    let mut leg = Leg::new(FLOOR, 0.0);
    leg.run_s(30);
    assert!(
        leg.corrections.is_empty(),
        "a clean feed needs no correction"
    );
    let raise_at = leg.out_frames;
    leg.f.set_target(RAISED);
    assert_eq!(leg.f.target(), RAISED);
    assert_eq!(
        leg.f.setpoint(),
        FLOOR,
        "the servo still holds the old fill"
    );
    leg.run_s(60);
    let max = leg.max_in_any_second(raise_at);
    assert!(
        max <= SERVO_WALK_MAX_PER_WINDOW,
        "a raise is walked in the gentle zone only: {max} corrections in one second"
    );
    assert_eq!(leg.count_from(raise_at, true), 0, "a raise never drops");
    assert_eq!(
        leg.count_from(raise_at, false),
        BLOCK,
        "exactly the one block of setpoint error is repeated in"
    );
    // 256 frames at 7 a second: about 37 s, never the steep zone's 5 s.
    let done = leg.walk_done_after_s(raise_at).expect("the walk finishes");
    assert!((36..=40).contains(&done), "walked in {done} s");
    let (mean, _) = *leg.seconds.last().unwrap();
    assert!(
        mean.abs_diff(RAISED) <= SERVO_DEADBAND_FRAMES,
        "the fill sits at the new target: {mean}"
    );
    assert_eq!((leg.ran_dry, leg.overruns), (0, 0));
}

#[test]
fn a_lowered_target_is_walked_down_just_as_gently() {
    let mut leg = Leg::new(RAISED, 0.0);
    leg.run_s(30);
    let lower_at = leg.out_frames;
    leg.f.set_target(FLOOR);
    leg.run_s(60);
    let max = leg.max_in_any_second(lower_at);
    assert!(max <= SERVO_WALK_MAX_PER_WINDOW, "{max} in one second");
    assert_eq!(
        leg.count_from(lower_at, false),
        0,
        "a lowering never repeats"
    );
    assert_eq!(leg.count_from(lower_at, true), BLOCK);
    let done = leg.walk_done_after_s(lower_at).expect("the walk finishes");
    assert!((36..=40).contains(&done), "walked down in {done} s");
    assert_eq!((leg.ran_dry, leg.overruns), (0, 0));
}

#[test]
fn a_fill_already_past_the_setpoint_carries_the_walk_without_a_correction_against_it() {
    // A lowering walk, and 5 s in, 200 frames of the oldest audio go (a lost hub tick's give-up):
    // the fill is then past the setpoint toward the target. The setpoint follows it; no correction
    // ever pulls the fill back up against the walk.
    let mut leg = Leg::new(RAISED, 0.0);
    leg.run_s(30);
    let lower_at = leg.out_frames;
    leg.f.set_target(FLOOR);
    leg.run_s(5);
    leg.fill -= 200;
    leg.run_s(60);
    assert_eq!(
        leg.count_from(lower_at, false),
        0,
        "no repeat fights the walk"
    );
    let drops = leg.count_from(lower_at, true);
    assert!(
        drops <= BLOCK - 200 + SERVO_DEADBAND_FRAMES,
        "only what the give-up did not already take: {drops}"
    );
    assert!(leg.max_in_any_second(lower_at) <= SERVO_WALK_MAX_PER_WINDOW);
    assert_eq!(leg.f.setpoint(), FLOOR);
}

#[test]
fn drift_at_200_ppm_keeps_its_full_budget_while_a_walk_is_in_progress() {
    // The step-2 bound at +-200 ppm: the servo's view of the depth within 147 frames of where it
    // holds it. A walk must not starve the drift correction, nor the drift the walk.
    for ppm in [200.0, -200.0] {
        let mut leg = Leg::new(FLOOR, ppm);
        leg.run_s(60);
        let raise_at = leg.out_frames;
        leg.f.set_target(RAISED);
        leg.run_s(120);
        let first = raise_at / SERVO_WINDOW_FRAMES;
        for (s, &(mean, setpoint)) in leg.seconds[first..].iter().enumerate() {
            assert!(
                mean.abs_diff(setpoint) <= 147,
                "{ppm} ppm, {s} s after the raise: depth {mean} vs setpoint {setpoint}"
            );
        }
        // The drift's share of every second is spent first (its full budget), so under a heavy
        // drift the walk may run a little slower: at -200 ppm about 17 corrections a second are
        // planned and the 1000-frame spacing on 256-frame blocks fits about 16 (45 s measured).
        // At +200 ppm the sender itself carries the fill toward the raised target (11 s).
        let done = leg.walk_done_after_s(raise_at).expect("the walk finishes");
        assert!(done <= 50, "{ppm} ppm: the walk keeps going ({done} s)");
        let (mean, _) = *leg.seconds.last().unwrap();
        assert!(mean.abs_diff(RAISED) <= 147, "{ppm} ppm: {mean}");
        let max = leg.max_in_any_second(raise_at);
        assert!(
            max <= servo_corrections(147) + SERVO_WALK_MAX_PER_WINDOW,
            "{ppm} ppm: {max} in one second"
        );
        assert_eq!((leg.ran_dry, leg.overruns), (0, 0), "{ppm} ppm");
    }
}

#[test]
fn set_target_moves_the_target_and_cap_and_a_prime_or_a_trim_ends_the_walk() {
    let cap = vban_cap_blocks(VBAN_PROGRAM_TARGET_BLOCKS) * BLOCK;
    // Not primed yet: the leg simply primes to the new target.
    let mut f = NetworkFill::new(FLOOR, cap);
    f.set_target(RAISED);
    assert_eq!((f.target(), f.setpoint()), (RAISED, RAISED));
    // Primed: the setpoint stays, the target moves and the cap with it, by the derived rule.
    let mut f = NetworkFill::new(FLOOR, cap);
    assert!(matches!(f.plan_pop(FLOOR, BLOCK), PopPlan::Audio { .. }));
    f.set_target(RAISED);
    assert_eq!((f.target(), f.setpoint()), (RAISED, FLOOR));
    let raised_cap = vban_cap_blocks(RAISED / BLOCK) * BLOCK;
    assert_eq!(f.overrun_keep(raised_cap), None, "the cap moved up");
    // The trim leaves the fill exactly at the target: the walk is over.
    assert_eq!(f.overrun_keep(raised_cap + 1), Some(RAISED));
    assert_eq!(f.setpoint(), RAISED);
    // An underrun's re-prime starts at the target too.
    f.set_target(FLOOR);
    assert_eq!(f.setpoint(), RAISED);
    assert_eq!(
        f.plan_pop(BLOCK - 1, BLOCK),
        PopPlan::Silent { ran_dry: true }
    );
    assert!(matches!(f.plan_pop(FLOOR, BLOCK), PopPlan::Audio { .. }));
    assert_eq!(f.setpoint(), FLOOR);
}
