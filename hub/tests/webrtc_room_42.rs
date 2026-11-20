//! The phones' room (intercom#42): every phone hears the phones bus and every OTHER phone, never
//! itself (N-1 by construction), the hub hears the sum of the phones, and a slot's lifecycle keeps
//! one phone's audio out of the next one's. Graded with a Goertzel detector on distinct tones.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use intercom_hub::janus_pacing::{PacedRing, FRAME_48K, RING_CAP_FRAMES, RING_TARGET_FRAMES};
use intercom_hub::vban_io::JitterBuffer;
use intercom_hub::webrtc_leg::WebrtcShared;
use intercom_hub::webrtc_room::{
    mix_minus, sum_mics, PacketSink, PhonesRoom, RoomIo, RoomSlots, TxPacket, PHONE_MIC_STALE,
};

const RATE: f64 = 48_000.0;

/// The amplitude of `freq` in `x` in dBFS, Hann-windowed so the other tones' leakage stays far
/// below what a "not heard" check needs.
fn goertzel_db(x: &[i16], freq: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * freq / RATE;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    let len = x.len() as f64;
    for (i, &v) in x.iter().enumerate() {
        let hann = 1.0 - (2.0 * std::f64::consts::PI * i as f64 / len).cos();
        let s0 = f64::from(v) * hann + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    // A full-scale-equivalent sine of amplitude A gives power (A * N / 2)^2.
    let n = x.len() as f64;
    let amp = 2.0 * power.max(0.0).sqrt() / n;
    20.0 * (amp / 32768.0).max(1e-12).log10()
}

fn tone(freq: f64, amp: f64, start: usize, n: usize) -> Vec<i16> {
    (start..start + n)
        .map(|i| (amp * (2.0 * std::f64::consts::PI * freq * i as f64 / RATE).sin()).round() as i16)
        .collect()
}

// Tones that are not whole cycles per 20 ms frame (the strih-intercom-hub-mix test note).
const BUS_HZ: f64 = 997.0;
const PHONE_HZ: [f64; 3] = [433.0, 1559.0, 2711.0];

#[test]
fn every_phone_hears_the_bus_and_the_others_never_itself() {
    let n = 4800;
    let bus = tone(BUS_HZ, 4000.0, 0, n);
    let mics: Vec<Vec<i16>> = PHONE_HZ.iter().map(|&f| tone(f, 4000.0, 0, n)).collect();
    let active = vec![true; 3];
    let mut outs = vec![vec![0i16; n]; 3];
    mix_minus(&bus, &mics, &active, &mut outs);
    for (i, out) in outs.iter().enumerate() {
        assert!(goertzel_db(out, BUS_HZ) > -25.0, "phone {i} hears the bus");
        for (j, &f) in PHONE_HZ.iter().enumerate() {
            let db = goertzel_db(out, f);
            if i == j {
                assert!(
                    db < -90.0,
                    "phone {i} must not hear itself: {db:.1} dB at {f} Hz"
                );
            } else {
                assert!(db > -25.0, "phone {i} hears phone {j}: {db:.1} dB");
            }
        }
    }
}

#[test]
fn an_inactive_slot_is_neither_mixed_nor_written() {
    let n = 960;
    let bus = vec![100i16; n];
    let mics = vec![vec![1000i16; n], vec![2000i16; n], vec![4000i16; n]];
    let active = vec![true, false, true];
    let mut outs = vec![vec![7i16; n]; 3];
    mix_minus(&bus, &mics, &active, &mut outs);
    assert!(
        outs[0].iter().all(|&s| s == 100 + 4000),
        "slot 1 is not mixed in"
    );
    assert!(
        outs[1].iter().all(|&s| s == 7),
        "an inactive slot's output is untouched"
    );
    assert!(outs[2].iter().all(|&s| s == 100 + 1000));
    let mut sum = vec![0i16; n];
    sum_mics(&mics, &active, &mut sum);
    assert!(sum.iter().all(|&s| s == 5000));
}

#[test]
fn the_mix_saturates_instead_of_wrapping() {
    let n = 4;
    let bus = vec![30000i16; n];
    let mics = vec![vec![30000i16; n], vec![30000i16; n], vec![-30000i16; n]];
    let mut outs = vec![vec![0i16; n]; 3];
    mix_minus(&bus, &mics, &[true, true, true], &mut outs);
    assert_eq!(outs[2], vec![i16::MAX; n]);
    let mics = vec![vec![-30000i16; n], vec![-30000i16; n]];
    let mut sum = vec![0i16; n];
    sum_mics(&mics, &[true, true], &mut sum);
    assert_eq!(sum, vec![i16::MIN; n]);
}

#[derive(Clone, Default)]
struct Collect(Arc<Mutex<Vec<TxPacket>>>, bool);

impl PacketSink for Collect {
    fn offer(&mut self, packet: TxPacket) -> bool {
        if self.1 {
            return false;
        }
        self.0.lock().unwrap().push(packet);
        true
    }
}

struct Rig {
    room: PhonesRoom<Collect>,
    bus: Arc<Mutex<PacedRing>>,
    slots: Arc<RoomSlots>,
    jitter: Arc<Mutex<Vec<JitterBuffer>>>,
    sent: Arc<Mutex<Vec<TxPacket>>>,
    stats: Arc<WebrtcShared>,
}

fn rig(max: usize, full_queue: bool) -> Rig {
    let bus = Arc::new(Mutex::new(PacedRing::new(
        RING_TARGET_FRAMES,
        RING_CAP_FRAMES,
    )));
    let slots = Arc::new(RoomSlots::new(max));
    let jitter = Arc::new(Mutex::new(vec![
        JitterBuffer::new(4096),
        JitterBuffer::janus_capture(8192, 2048, 48_000).with_min_channels(2),
    ]));
    let sent = Arc::new(Mutex::new(Vec::new()));
    let stats = Arc::new(WebrtcShared::new("127.0.0.1:6992".into(), vec![], max));
    let room = PhonesRoom::new(RoomIo {
        bus: bus.clone(),
        slots: slots.clone(),
        jitter: jitter.clone(),
        phones_id: 1,
        sink: Collect(sent.clone(), full_queue),
        stats: stats.clone(),
    })
    .unwrap();
    Rig {
        room,
        bus,
        slots,
        jitter,
        sent,
        stats,
    }
}

/// Decode every packet sent to `slot` (generation `generation`) in order.
fn decode_slot(sent: &[TxPacket], slot: usize, generation: u64) -> Vec<i16> {
    let mut dec = opus::Decoder::new(48_000, opus::Channels::Mono).unwrap();
    let mut out = Vec::new();
    let mut buf = vec![0i16; 5760];
    for p in sent
        .iter()
        .filter(|p| p.slot == slot && p.generation == generation)
    {
        let n = dec.decode(p.payload(), &mut buf, false).unwrap();
        out.extend_from_slice(&buf[..n]);
    }
    out
}

#[test]
fn the_room_tick_sends_each_phone_its_own_n_minus_1_as_opus() {
    let mut r = rig(3, false);
    let generations = [11u64, 12, 13];
    for (slot, g) in generations.iter().enumerate() {
        r.slots.open(slot, *g);
    }
    let t0 = Instant::now();
    let ticks = 150; // 3 s
    for k in 0..ticks {
        let at = t0 + Duration::from_millis(20 * k as u64);
        r.bus
            .lock()
            .unwrap()
            .push_at(&tone(BUS_HZ, 6000.0, k * FRAME_48K, FRAME_48K), at);
        for (slot, f) in PHONE_HZ.iter().enumerate() {
            assert!(r.slots.push_mic(
                slot,
                generations[slot],
                &tone(*f, 6000.0, k * FRAME_48K, FRAME_48K),
                at
            ));
        }
        r.room.tick(at);
    }
    let sent = r.sent.lock().unwrap().clone();
    assert_eq!(
        sent.len(),
        3 * ticks,
        "one packet per phone per tick (no DTX)"
    );
    for (slot, g) in generations.iter().enumerate() {
        // Skip the priming second; grade the steady last 1.5 s.
        let audio = decode_slot(&sent, slot, *g);
        let tail = &audio[audio.len() - 72_000..];
        assert!(
            goertzel_db(tail, BUS_HZ) > -30.0,
            "slot {slot} hears the bus"
        );
        for (j, f) in PHONE_HZ.iter().enumerate() {
            let db = goertzel_db(tail, *f);
            if j == slot {
                assert!(db < -50.0, "slot {slot} must not hear itself: {db:.1} dB");
            } else {
                assert!(db > -30.0, "slot {slot} hears slot {j}: {db:.1} dB");
            }
        }
    }
    let facet = r.stats.snapshot();
    assert_eq!(facet.room_ticks, ticks as u64);
    assert_eq!(facet.room_packets, 3 * ticks as u64);
    assert_eq!(facet.room_dropped, 0);
    // The hub hears every phone (Σ mics into the phones participant's input).
    let mut jb = r.jitter.lock().unwrap();
    assert!(jb[1].rx_packets >= ticks as u64 - 1);
    let mut heard = Vec::new();
    // The buffer resynced to its 2048-frame target while nothing popped: read 7 blocks of it.
    for _ in 0..7 {
        heard.extend_from_slice(&jb[1].pop_block(256)[0]);
    }
    for f in PHONE_HZ {
        assert!(goertzel_db(&heard, f) > -30.0, "the hub hears {f} Hz");
    }
    assert!(
        goertzel_db(&heard, BUS_HZ) < -60.0,
        "the bus never loops back into the hub"
    );
}

#[test]
fn a_room_without_phones_sends_nothing_and_feeds_nothing() {
    let mut r = rig(2, false);
    let t0 = Instant::now();
    for k in 0..20 {
        r.room.tick(t0 + Duration::from_millis(20 * k));
    }
    assert!(r.sent.lock().unwrap().is_empty());
    assert_eq!(r.jitter.lock().unwrap()[1].rx_packets, 0);
    assert_eq!(r.stats.snapshot().room_ticks, 20);
}

#[test]
fn a_full_queue_drops_and_counts_never_blocks() {
    let mut r = rig(1, true);
    r.slots.open(0, 5);
    let t0 = Instant::now();
    for k in 0..10 {
        r.room.tick(t0 + Duration::from_millis(20 * k));
    }
    let f = r.stats.snapshot();
    assert_eq!(f.room_dropped, 10);
    assert_eq!(f.room_packets, 0);
}

#[test]
fn a_slot_belongs_to_one_generation_at_a_time() {
    let slots = RoomSlots::new(2);
    let now = Instant::now();
    assert_eq!(slots.generation(0), 0);
    assert!(
        !slots.push_mic(0, 0, &[1; 960], now),
        "a free slot takes nothing"
    );
    slots.open(0, 7);
    assert_eq!(slots.generation(0), 7);
    assert!(slots.push_mic(0, 7, &[1; 960], now));
    assert!(
        !slots.push_mic(0, 6, &[1; 960], now),
        "a late packet of the slot's old phone is refused"
    );
    slots.close(0, 6);
    assert_eq!(slots.generation(0), 7, "a stale close changes nothing");
    slots.close(0, 7);
    assert_eq!(slots.generation(0), 0);
    assert!(!slots.push_mic(9, 7, &[1; 960], now), "out of range");
    assert_eq!(slots.mic_rate_ppm(0), 0.0);
}

#[test]
fn a_new_phone_in_a_slot_never_hears_the_old_phones_tail() {
    let mut r = rig(2, false);
    r.slots.open(0, 1);
    r.slots.open(1, 2);
    let t0 = Instant::now();
    // Phone 2 talks, then leaves; its slot is reopened for phone 3, which stays silent.
    for k in 0..60 {
        let at = t0 + Duration::from_millis(20 * k as u64);
        r.slots.push_mic(
            1,
            2,
            &tone(PHONE_HZ[1], 6000.0, k * FRAME_48K, FRAME_48K),
            at,
        );
        r.room.tick(at);
    }
    r.slots.close(1, 2);
    r.slots.open(1, 3);
    r.sent.lock().unwrap().clear();
    for k in 60..120 {
        let at = t0 + Duration::from_millis(20 * k as u64);
        r.room.tick(at);
    }
    let sent = r.sent.lock().unwrap().clone();
    let heard = decode_slot(&sent, 0, 1);
    assert!(
        goertzel_db(&heard, PHONE_HZ[1]) < -60.0,
        "phone 1 hears nothing of the phone that left"
    );
    assert!(sent.iter().all(|p| p.slot != 1 || p.generation == 3));
}

#[test]
fn a_mic_back_after_a_stall_starts_over() {
    let slots = RoomSlots::new(1);
    slots.open(0, 1);
    let t0 = Instant::now();
    for k in 0..10 {
        slots.push_mic(0, 1, &[500; 960], t0 + Duration::from_millis(20 * k));
    }
    // After a gap longer than PHONE_MIC_STALE the old tail is dropped and the ring re-primes:
    // its rate fit never sees the gap as a clock step.
    let back = t0 + Duration::from_millis(200) + PHONE_MIC_STALE + Duration::from_millis(1);
    assert!(slots.push_mic(0, 1, &[500; 960], back));
    assert!(slots.mic_rate_ppm(0).abs() < 1000.0);
}

/// A queue that takes every other packet.
struct EveryOther(Arc<Mutex<Vec<TxPacket>>>, bool);

impl PacketSink for EveryOther {
    fn offer(&mut self, packet: TxPacket) -> bool {
        self.1 = !self.1;
        if self.1 {
            self.0.lock().unwrap().push(packet);
        }
        self.1
    }
}

// intercom#42 review: a packet the full queue dropped must still move the phone's RTP clock, so
// every packet carries the room tick it was made in.
#[test]
fn every_packet_carries_its_room_tick_dropped_ones_included() {
    let bus = Arc::new(Mutex::new(PacedRing::new(
        RING_TARGET_FRAMES,
        RING_CAP_FRAMES,
    )));
    let slots = Arc::new(RoomSlots::new(1));
    let jitter = Arc::new(Mutex::new(vec![
        JitterBuffer::new(4096),
        JitterBuffer::janus_capture(8192, 2048, 48_000).with_min_channels(2),
    ]));
    let sent = Arc::new(Mutex::new(Vec::new()));
    let stats = Arc::new(WebrtcShared::new("127.0.0.1:6992".into(), vec![], 1));
    let mut room = PhonesRoom::new(RoomIo {
        bus,
        slots: slots.clone(),
        jitter,
        phones_id: 1,
        sink: EveryOther(sent.clone(), false),
        stats: stats.clone(),
    })
    .unwrap();
    let t0 = Instant::now();
    // Two ticks before the phone exists still count on the room clock.
    room.tick(t0);
    room.tick(t0 + Duration::from_millis(20));
    slots.open(0, 9);
    for k in 2..12u64 {
        room.tick(t0 + Duration::from_millis(20 * k));
    }
    let ticks: Vec<u64> = sent.lock().unwrap().iter().map(|p| p.tick).collect();
    assert_eq!(
        ticks,
        vec![3, 5, 7, 9, 11],
        "every other packet dropped, the ticks go on"
    );
    assert_eq!(stats.snapshot().room_dropped, 5);
}
