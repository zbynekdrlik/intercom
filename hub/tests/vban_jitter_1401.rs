//! Issue 1401 (owner, 4.10.2026: "zvuk na strih obs co ide strihacovi tak raz za cas kratke ma
//! vypadky"): the VBAN network legs had no jitter target. The old network policy popped whatever
//! was queued and zero-padded a short block, so its depth was wherever the first packets' phase
//! left it, often under one block, and a packet a little late against the hub's block clock wrote
//! a zero-padded block into the strih program mix (live: single underruns every 2-15 min on the
//! FOH feed over a clean network).
//!
//! Since issue 1461 the leg crosses the sender's clock with the resampler ([`intercom_hub::asrc`])
//! instead of the issue-1401 single-frame servo, and its target is fixed. These tests pin the leg
//! policy through the public API:
//! - the prefill to the target plus the resampler's history, and every prime starting exactly
//!   there (at ratio 1 the first block is the input from the history on, sample for sample);
//! - one whole silent block per underrun, then the re-prime to the SAME target;
//! - a lost hub tick (the part of a late wake beyond the four ticks the block loop runs late)
//!   read past at once, never below half a block under the target;
//! - the counted resync to the target above the cap;
//! - the stale / never-received / muted-cambox / stall rules, a sender changing its channel count;
//! - the mono cambox fan-out, the card-capture kind, a target that never moves.
//!
//! The resampler itself and the hours-long two-clock bench are `asrc_1461.rs`; the audio through
//! this buffer is graded in `asrc_leg_1461.rs`.

use std::time::{Duration, Instant};

use intercom_hub::asrc::{ASRC_HISTORY, ASRC_LOOKAHEAD};
use intercom_hub::vban_io::{BufferKind, DecodedAudio, JitterBuffer, STALE_STREAM_MS};
use intercom_hub::vban_jitter::{VBAN_CAP_BLOCKS, VBAN_TARGET_BLOCKS};

const BLOCK: usize = 256;
const TARGET: usize = VBAN_TARGET_BLOCKS * BLOCK;
const CAP: usize = VBAN_CAP_BLOCKS * BLOCK;
/// What a prime waits for: the target plus the resampler's history.
const PRIME: usize = TARGET + ASRC_HISTORY;
/// What one block needs queued at ratio 1 from a read point on an input frame: the history, the
/// block and the look-ahead.
const NEEDED: usize = ASRC_HISTORY + BLOCK + ASRC_LOOKAHEAD;

fn ramp(start: i16, n: usize) -> Vec<i16> {
    (0..n).map(|i| start.wrapping_add(i as i16)).collect()
}

fn mono(samples: Vec<i16>) -> DecodedAudio {
    let frames = samples.len();
    DecodedAudio {
        stream_name: "fohabl-strih".into(),
        channels: vec![samples],
        frames,
    }
}

fn multi(channels: usize, frames: usize) -> DecodedAudio {
    DecodedAudio {
        stream_name: "fohabl-strih".into(),
        channels: vec![vec![100; frames]; channels],
        frames,
    }
}

fn leg() -> JitterBuffer {
    JitterBuffer::vban_leg(CAP, TARGET)
}

fn is_silent(block: &[Vec<i16>]) -> bool {
    block
        .iter()
        .all(|ch| ch.len() == BLOCK && ch.iter().all(|&s| s == 0))
}

/// A leg primed at exactly the prime level, one block played, then fed `tail` more frames: the
/// ramp 0..PRIME+tail with one block of it read (the history before it kept).
fn primed_leg_with(tail: usize, t0: Instant) -> JitterBuffer {
    let mut jb = leg();
    jb.push_at(&mono(ramp(0, PRIME)), t0);
    assert_eq!(
        jb.pop_block_at(BLOCK, t0),
        vec![ramp(ASRC_HISTORY as i16, BLOCK)]
    );
    jb.push_at(&mono(ramp(PRIME as i16, tail)), t0);
    jb
}

// --- the budget ----------------------------------------------------------------------------------

#[test]
fn the_target_is_three_hub_blocks_inside_the_cap() {
    assert_eq!(VBAN_TARGET_BLOCKS, 3, "the design's ~16 ms target");
    assert_eq!(TARGET, 768);
    assert_eq!(TARGET * 1000 / 48_000, 16, "16 ms at the 48 kHz hub rate");
    // Design 5981457044: the cap is the target + the four ticks the block loop runs late + the
    // current tick + one block of headroom (48 ms), so a covered stall's arrivals never trim it.
    assert_eq!(VBAN_CAP_BLOCKS, 9, "the cap is 9 blocks (48 ms)");
    // A late burst has six blocks of headroom before anything is resynced.
    const { assert!(CAP >= TARGET + 6 * BLOCK) };
    assert_eq!(
        ASRC_HISTORY, 15,
        "the 32-tap resampler keeps 15 frames behind its read point"
    );
    assert_eq!(NEEDED, 287);
}

#[test]
fn a_vban_leg_prefills_to_the_target_before_the_first_pop() {
    let t0 = Instant::now();
    let mut jb = leg();
    jb.push_at(&mono(ramp(1, PRIME - 1)), t0);
    let b = jb.pop_block_at(BLOCK, t0);
    assert!(is_silent(&b), "a whole silent block while priming");
    assert_eq!(jb.buffered_frames(), PRIME - 1, "priming consumes nothing");
    assert_eq!(jb.underruns, 0, "priming is not an underrun");

    jb.push_at(&mono(vec![7]), t0);
    let b = jb.pop_block_at(BLOCK, t0);
    assert_eq!(
        b,
        vec![ramp(1 + ASRC_HISTORY as i16, BLOCK)],
        "exactly at the prime: at ratio 1 the block is the input after the history"
    );
    assert_eq!(jb.buffered_frames(), PRIME - BLOCK);
    assert_eq!(jb.underruns, 0);
    let s = jb.network_stats().expect("a VBAN leg reports its fill");
    assert!(s.primed);
    assert_eq!(s.target_frames, TARGET);
    assert_eq!(s.rate_ppm, 0.0, "no rate fitted yet: the ratio is 1");
    assert_eq!(s.resyncs, 0);
}

#[test]
fn a_cold_start_trims_its_prime_overshoot_back_to_the_target() {
    // The first packets arrive in a burst 300 frames past the prime while silence goes out: the
    // OLDEST 300 are dropped (inaudible, the leg is just starting), so the leg starts exactly at its
    // target.
    let t0 = Instant::now();
    let mut jb = leg();
    jb.push_at(&mono(ramp(0, PRIME + 300)), t0);
    let b = jb.pop_block_at(BLOCK, t0);
    assert_eq!(
        b,
        vec![ramp(300 + ASRC_HISTORY as i16, BLOCK)],
        "starts at the newest PRIME frames"
    );
    assert_eq!(jb.buffered_frames(), PRIME - BLOCK);
}

#[test]
fn an_underrun_is_one_whole_silent_block_and_the_reprime_starts_at_the_target() {
    let t0 = Instant::now();
    // PRIME + 100 samples, a block already read: two more play, 115 are left (under NEEDED).
    let mut jb = primed_leg_with(100, t0);
    for i in 1..3 {
        let b = jb.pop_block_at(BLOCK, t0);
        assert_eq!(b, vec![ramp((i * BLOCK + ASRC_HISTORY) as i16, BLOCK)]);
    }
    let left = PRIME + 100 - 3 * BLOCK;
    assert_eq!(jb.buffered_frames(), left);
    assert!(left < NEEDED);

    // The next packet is late: ONE whole silent block, never a partial block + zeros.
    let b = jb.pop_block_at(BLOCK, t0);
    assert!(is_silent(&b), "an underrun is a whole silent block");
    assert_eq!(
        jb.buffered_frames(),
        left,
        "the partial tail waits for the re-prime"
    );
    assert_eq!(
        jb.underruns, 0,
        "counted only once the stream continues (a mute is not a dropout)"
    );

    // The late packets arrive in one burst: the underrun counts, and the very next pop re-primes
    // EXACTLY at the same target (the janus_pacing precedent): the oldest frames past the prime
    // land in the gap that is already there.
    jb.push_at(&mono(ramp((PRIME + 100) as i16, 700)), t0);
    assert_eq!(jb.underruns, 1);
    let over = left + 700 - PRIME;
    let b = jb.pop_block_at(BLOCK, t0);
    assert_eq!(
        b,
        vec![ramp((3 * BLOCK + over + ASRC_HISTORY) as i16, BLOCK)]
    );
    assert_eq!(jb.buffered_frames(), PRIME - BLOCK);
    assert_eq!(jb.underruns, 1, "still one underrun");
    assert_eq!(jb.network_stats().unwrap().target_frames, TARGET);
}

#[test]
fn after_an_underrun_it_reprimes_to_the_target_counting_one_underrun() {
    let t0 = Instant::now();
    let mut jb = primed_leg_with(100, t0);
    for _ in 0..2 {
        jb.pop_block_at(BLOCK, t0);
    }
    // The stream ran dry, then refills at the normal rate of one block per pop.
    let mut silent = 0;
    let mut next = (PRIME + 100) as i16;
    let first_audio = loop {
        let b = jb.pop_block_at(BLOCK, t0);
        if !is_silent(&b) {
            break b;
        }
        silent += 1;
        jb.push_at(&mono(ramp(next, BLOCK)), t0);
        next = next.wrapping_add(BLOCK as i16);
        assert!(silent < 10, "re-priming must end");
    };
    // The underrun block leaves 115 + 256 = 371, one re-priming block 627 (< 783), the next
    // 883 >= 783: audio again after three silent blocks, starting exactly at the prime (the 100
    // oldest frames dropped).
    assert_eq!(silent, 3, "one underrun block + two re-priming blocks");
    assert_eq!(jb.underruns, 1, "the re-prime counts no further underruns");
    let dropped = PRIME + 100 - 3 * BLOCK + 3 * BLOCK - PRIME;
    assert_eq!(
        first_audio,
        vec![ramp((3 * BLOCK + dropped + ASRC_HISTORY) as i16, BLOCK)]
    );
    assert_eq!(jb.buffered_frames(), PRIME - BLOCK);
}

#[test]
fn above_the_cap_one_counted_resync_back_to_the_target() {
    let t0 = Instant::now();
    let mut jb = leg();
    jb.push_at(&mono(ramp(0, CAP)), t0);
    assert_eq!(jb.overruns, 0, "exactly the cap is not an overrun");
    jb.push_at(&mono(vec![CAP as i16]), t0);
    assert_eq!(jb.overruns, 1);
    assert_eq!(jb.network_stats().unwrap().resyncs, 1, "one counted resync");
    assert_eq!(
        jb.buffered_frames(),
        PRIME,
        "back to the target (plus the history), not the cap"
    );
    let b = jb.pop_block_at(BLOCK, t0);
    assert_eq!(
        b,
        vec![ramp((CAP + 1 - PRIME + ASRC_HISTORY) as i16, BLOCK)],
        "the newest audio kept"
    );
    // A primed leg resyncs to its target depth too: queued = target + its read point. After the
    // pop 527 are queued; the seventh block takes it past the cap.
    for _ in 0..7 {
        jb.push_at(&mono(ramp(0, BLOCK)), t0);
    }
    assert_eq!(jb.overruns, 2);
    assert_eq!(jb.buffered_frames(), PRIME);
    assert_eq!(jb.network_stats().unwrap().resyncs, 2);
    assert_eq!(jb.network_stats().unwrap().target_frames, TARGET);
}

#[test]
fn a_never_received_vban_leg_pops_silence_without_an_underrun() {
    let mut jb = leg();
    for _ in 0..5 {
        let b = jb.pop_block(BLOCK);
        assert_eq!(b, vec![vec![0i16; BLOCK]]);
    }
    assert_eq!(jb.underruns, 0);
    assert!(!jb.network_stats().unwrap().primed);
}

#[test]
fn a_stale_vban_leg_counts_no_underrun_and_a_live_one_counts_one() {
    let t0 = Instant::now();
    let mut jb = leg();
    jb.push_at(&mono(ramp(0, PRIME)), t0);
    for _ in 0..3 {
        jb.pop_block_at(BLOCK, t0);
    }
    // The stream stopped: by the time it runs dry it is stale.
    let stale = t0 + Duration::from_millis(STALE_STREAM_MS + 1);
    jb.pop_block_at(BLOCK, stale);
    jb.pop_block_at(BLOCK, stale);
    assert_eq!(jb.underruns, 0, "a stale stream counts no underrun");

    // It comes back, re-primes, plays, then runs dry while live and continues 10 ms later: one
    // underrun.
    let back = stale + Duration::from_secs(1);
    jb.push_at(&mono(ramp(0, PRIME)), back);
    for _ in 0..2 {
        assert!(!is_silent(&jb.pop_block_at(BLOCK, back)));
    }
    assert!(is_silent(&jb.pop_block_at(BLOCK, back)));
    jb.push_at(&mono(ramp(0, BLOCK)), back + Duration::from_millis(10));
    assert_eq!(jb.underruns, 1);
}

#[test]
fn a_muted_cambox_counts_a_stall_not_an_underrun_and_never_replays_its_stale_tail() {
    // A cambox sends only while unmuted. It runs dry right after the mute, while its last packet
    // is still fresh, and the next packet comes long after: no underrun (the status line would
    // otherwise name the cambox and hide a real dropout), one stall, and the frames left from
    // before the mute are dropped, never played in front of the fresh audio.
    let t0 = Instant::now();
    let mut jb = leg().with_min_channels(2);
    jb.push_at(&mono(ramp(0, PRIME)), t0);
    jb.pop_block_at(BLOCK, t0);
    jb.push_at(&mono(ramp(PRIME as i16, 100)), t0);
    for _ in 0..2 {
        jb.pop_block_at(BLOCK, t0);
    }
    assert!(is_silent(&jb.pop_block_at(BLOCK, t0)), "ran dry");
    let unmute = t0 + Duration::from_secs(2);
    jb.push_at(&mono(ramp(10_000, PRIME)), unmute);
    assert_eq!(jb.underruns, 0, "a mute is not a dropout");
    assert_eq!(jb.network_stats().unwrap().stalls, 1);
    assert_eq!(jb.buffered_frames(), PRIME, "the stale tail is gone");
    let b = jb.pop_block_at(BLOCK, unmute);
    let first = 10_000 + ASRC_HISTORY as i16;
    assert_eq!(b, vec![ramp(first, BLOCK), ramp(first, BLOCK)]);
}

#[test]
fn a_program_feed_outage_counts_one_stall() {
    // The FOH sender stops for 2 s (a restart): the leg runs dry while still fresh, then the stream
    // comes back. Not an underrun (that is a gap inside a running stream), but a stall, so the
    // outage is not lost from the counters.
    let t0 = Instant::now();
    let mut jb = leg();
    jb.push_at(&mono(ramp(0, PRIME)), t0);
    for _ in 0..4 {
        jb.pop_block_at(BLOCK, t0);
    }
    let t1 = t0 + Duration::from_secs(2);
    jb.push_at(&mono(ramp(0, PRIME)), t1);
    let s = jb.network_stats().unwrap();
    assert_eq!((jb.underruns, s.stalls), (0, 1));
    // A gap inside the stale limit is an underrun, not a stall.
    for _ in 0..4 {
        jb.pop_block_at(BLOCK, t1);
    }
    jb.push_at(&mono(ramp(0, PRIME)), t1 + Duration::from_millis(100));
    let s = jb.network_stats().unwrap();
    assert_eq!((jb.underruns, s.stalls), (1, 1));
}

#[test]
fn a_sender_that_changes_its_channel_count_starts_the_leg_over() {
    // fohabl sends 4 channels, then is reconfigured to 2: the two missing queues would hold the
    // shortest-channel fill at zero forever. The leg starts over on the new layout instead.
    let t0 = Instant::now();
    let mut jb = leg();
    jb.push_at(&multi(4, PRIME), t0);
    assert_eq!(jb.pop_block_at(BLOCK, t0).len(), 4);
    jb.push_at(&multi(2, PRIME), t0);
    assert_eq!(jb.buffered_frames(), PRIME, "only the new layout's audio");
    let b = jb.pop_block_at(BLOCK, t0);
    assert_eq!(b.len(), 2);
    assert!(!is_silent(&b), "the leg plays on the new layout");
    // And back up to 4: the same.
    jb.push_at(&multi(4, PRIME), t0);
    let b = jb.pop_block_at(BLOCK, t0);
    assert_eq!(b.len(), 4);
    assert!(!is_silent(&b));
}

#[test]
fn a_missed_hub_tick_is_read_past_at_once_never_more_than_half_a_block_below_the_target() {
    let t0 = Instant::now();
    let mut jb = primed_leg_with(BLOCK, t0);
    // Steady: the target queued ahead of the read point before every pop.
    assert_eq!(jb.buffered_frames(), PRIME);
    // The block loop misses a tick: no pop, the sender keeps sending one more block.
    jb.push_at(&mono(ramp((PRIME + BLOCK) as i16, BLOCK)), t0);
    assert_eq!(jb.buffered_frames(), PRIME + BLOCK);
    jb.skip_missed(1, BLOCK);
    assert_eq!(
        jb.buffered_frames(),
        PRIME,
        "the missed block is read past, exactly as its pop would have"
    );
    // The next pop plays on right after the lost block's audio.
    let b = jb.pop_block_at(BLOCK, t0);
    assert_eq!(b, vec![ramp((2 * BLOCK + ASRC_HISTORY) as i16, BLOCK)]);
    // A missed block is given up whole even when the jitter has the fill a little under the target
    // at that moment: 60 frames short of it plus the missed block leaves it 60 short again.
    jb.push_at(&mono(ramp(0, BLOCK - 60)), t0);
    jb.push_at(&mono(ramp(0, BLOCK)), t0);
    assert_eq!(jb.buffered_frames(), PRIME - 60 + BLOCK);
    jb.skip_missed(1, BLOCK);
    assert_eq!(jb.buffered_frames(), PRIME - 60);
    // But never more than half a block under the target: 3 missed ticks with only 100 frames above
    // it (a stall long enough to have overrun) give up 100 + BLOCK / 2.
    jb.push_at(&mono(ramp(0, 160)), t0);
    assert_eq!(jb.buffered_frames(), PRIME + 100);
    jb.skip_missed(3, BLOCK);
    assert_eq!(jb.buffered_frames(), PRIME - BLOCK / 2);
    // While priming nothing is given up (the prime trims anyway).
    let mut cold = leg();
    cold.push_at(&mono(ramp(0, BLOCK)), t0);
    cold.skip_missed(2, BLOCK);
    assert_eq!(cold.buffered_frames(), BLOCK);
}

#[test]
fn a_mono_cambox_vban_leg_fans_into_ch2() {
    let t0 = Instant::now();
    let mut jb = leg().with_min_channels(2);
    jb.push_at(&mono(ramp(5, PRIME)), t0);
    let b = jb.pop_block_at(BLOCK, t0);
    let first = 5 + ASRC_HISTORY as i16;
    assert_eq!(b, vec![ramp(first, BLOCK), ramp(first, BLOCK)]);
}

#[test]
fn the_card_capture_is_the_same_resampler_leg_and_the_others_report_no_fill() {
    assert_eq!(leg().kind(), BufferKind::VbanLeg);
    let card = JitterBuffer::card_capture(32 * BLOCK, 2048, 48_000);
    assert_eq!(card.kind(), BufferKind::CardCapture);
    assert_eq!(card.network_stats().unwrap().target_frames, 2048);
    let plain = JitterBuffer::new(CAP);
    assert_eq!(plain.kind(), BufferKind::Plain);
    assert!(plain.network_stats().is_none());
    let capture = JitterBuffer::local_capture(CAP, TARGET);
    assert_eq!(capture.kind(), BufferKind::LocalCapture);
    assert!(capture.network_stats().is_none());
    // A missed tick leaves them untouched.
    let t0 = Instant::now();
    let mut plain = JitterBuffer::new(CAP);
    plain.push_at(&mono(ramp(0, TARGET + BLOCK)), t0);
    plain.skip_missed(1, BLOCK);
    assert_eq!(plain.buffered_frames(), TARGET + BLOCK);
}

#[test]
fn the_target_never_moves_whatever_the_sender_does() {
    // Issue 1461: no runtime latency change. A bursty sender with long gaps, an underrun and a
    // resync leave the target where the start put it.
    let t0 = Instant::now();
    let mut jb = leg();
    let mut t = t0;
    for i in 0..2_000 {
        // a 30 ms gap every 100 packets, otherwise one block per pop
        if i % 100 == 99 {
            t += Duration::from_millis(30);
        }
        jb.push_at(&mono(ramp(0, BLOCK)), t);
        jb.pop_block_at(BLOCK, t);
        t += Duration::from_micros(5_333);
        assert_eq!(jb.network_stats().unwrap().target_frames, TARGET);
    }
    for _ in 0..20 {
        jb.push_at(&mono(ramp(0, BLOCK)), t);
    }
    assert!(jb.network_stats().unwrap().resyncs >= 1);
    assert_eq!(jb.network_stats().unwrap().target_frames, TARGET);
}
