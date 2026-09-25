//! The Janus leg's codecs (issue 1345, 25.9.2026): Opus 48 kHz mono 20 ms with in-band FEC by
//! default, PCMU still selectable. The encoder takes exactly one 20 ms frame from the paced ring;
//! the decoder turns each received packet back into 48 kHz mono and conceals a short loss (FEC for
//! the frame right before a packet, PLC for earlier ones). libopus is built from the bundled source
//! (`opusic-sys`), so these first run at CI (Tier-0, issue 557).

use intercom_hub::janus_codec::{RxDecoder, TxEncoder};
use intercom_hub::janus_pacing::FRAME_48K;
use intercom_hub::janus_rtp::JanusCodec;

/// A 1 kHz tone at 48 kHz, `frames` 20 ms frames long, amplitude 8000.
fn tone(frames: usize) -> Vec<i16> {
    (0..frames * FRAME_48K)
        .map(|i| {
            let t = i as f64 / 48_000.0;
            (8000.0 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()) as i16
        })
        .collect()
}

fn rms(x: &[i16]) -> f64 {
    (x.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

#[test]
fn opus_roundtrip_keeps_a_tone_at_its_level() {
    let input = tone(25);
    let mut enc = TxEncoder::new(JanusCodec::Opus).expect("opus encoder");
    let mut dec = RxDecoder::new(JanusCodec::Opus).expect("opus decoder");
    let mut out = Vec::new();
    for (seq, frame) in input.chunks(FRAME_48K).enumerate() {
        let pkt = enc.encode(frame).expect("encode one 20 ms frame");
        assert!(
            !pkt.is_empty() && pkt.len() <= 1275,
            "an Opus packet, got {}",
            pkt.len()
        );
        let d = dec.decode(seq as u16, &pkt).expect("decode");
        assert_eq!(
            d.samples.len(),
            FRAME_48K,
            "one packet decodes to one 20 ms frame"
        );
        assert_eq!(d.concealed_frames, 0);
        out.extend(d.samples);
    }
    // Skip the codec's start-up: from frame 5 on, the level is within 3 dB of the input.
    let a = rms(&input[5 * FRAME_48K..]);
    let b = rms(&out[5 * FRAME_48K..]);
    let db = 20.0 * (b / a).log10();
    assert!(db.abs() < 3.0, "tone level changed by {db:.2} dB");
}

#[test]
fn opus_conceals_a_lost_packet_so_the_timeline_stays_whole() {
    let input = tone(10);
    let mut enc = TxEncoder::new(JanusCodec::Opus).unwrap();
    let pkts: Vec<Vec<u8>> = input
        .chunks(FRAME_48K)
        .map(|f| enc.encode(f).unwrap())
        .collect();
    let mut dec = RxDecoder::new(JanusCodec::Opus).unwrap();
    let mut total = 0;
    let mut concealed = 0;
    for (seq, pkt) in pkts.iter().enumerate() {
        if seq == 5 {
            continue; // lost on the wire
        }
        let d = dec.decode(seq as u16, pkt).unwrap();
        total += d.samples.len();
        concealed += d.concealed_frames;
    }
    assert_eq!(concealed, 1, "the one lost frame is concealed (FEC or PLC)");
    assert_eq!(total, 10 * FRAME_48K, "no hole in the decoded timeline");
}

#[test]
fn opus_decoder_drops_duplicates_and_resyncs_after_a_long_gap() {
    let input = tone(3);
    let mut enc = TxEncoder::new(JanusCodec::Opus).unwrap();
    let pkts: Vec<Vec<u8>> = input
        .chunks(FRAME_48K)
        .map(|f| enc.encode(f).unwrap())
        .collect();
    let mut dec = RxDecoder::new(JanusCodec::Opus).unwrap();
    assert_eq!(dec.decode(100, &pkts[0]).unwrap().samples.len(), FRAME_48K);
    let dup = dec.decode(100, &pkts[0]).unwrap();
    assert!(dup.samples.is_empty(), "a duplicate is dropped");
    // A gap far longer than the concealment window: decode as a fresh start, conceal nothing.
    let far = dec.decode(5000, &pkts[1]).unwrap();
    assert_eq!(far.samples.len(), FRAME_48K);
    assert_eq!(far.concealed_frames, 0);
}

/// Review round 1: a new sender (a new SSRC) starts a fresh sequence, so a stream whose numbers are
/// far BEHIND the last accepted packet must not be dropped as late.
#[test]
fn a_new_ssrc_restarts_the_sequence_plan() {
    let input = tone(2);
    let mut enc = TxEncoder::new(JanusCodec::Opus).unwrap();
    let pkts: Vec<Vec<u8>> = input
        .chunks(FRAME_48K)
        .map(|f| enc.encode(f).unwrap())
        .collect();
    let mut dec = RxDecoder::new(JanusCodec::Opus).unwrap();
    dec.observe_ssrc(0xAAAA);
    assert_eq!(
        dec.decode(40_000, &pkts[0]).unwrap().samples.len(),
        FRAME_48K
    );
    dec.observe_ssrc(0xBBBB);
    let fresh = dec.decode(39_990, &pkts[1]).unwrap();
    assert_eq!(
        fresh.samples.len(),
        FRAME_48K,
        "not dropped as a late packet"
    );
    assert_eq!(fresh.concealed_frames, 0);
    // The same SSRC again changes nothing.
    dec.observe_ssrc(0xBBBB);
    assert!(
        dec.decode(39_990, &pkts[1]).unwrap().samples.is_empty(),
        "a duplicate"
    );
}

#[test]
fn encoder_takes_exactly_one_20ms_frame() {
    let mut enc = TxEncoder::new(JanusCodec::Opus).unwrap();
    assert!(
        enc.encode(&[0i16; 100]).is_err(),
        "a short frame is refused"
    );
    let mut pcmu = TxEncoder::new(JanusCodec::Pcmu).unwrap();
    assert!(pcmu.encode(&[0i16; FRAME_48K + 1]).is_err());
}

#[test]
fn pcmu_stays_selectable_160_bytes_per_frame() {
    let input = tone(4);
    let mut enc = TxEncoder::new(JanusCodec::Pcmu).unwrap();
    let mut dec = RxDecoder::new(JanusCodec::Pcmu).unwrap();
    for (seq, frame) in input.chunks(FRAME_48K).enumerate() {
        let pkt = enc.encode(frame).unwrap();
        assert_eq!(pkt.len(), 160, "20 ms of 8 kHz mu-law");
        let d = dec.decode(seq as u16, &pkt).unwrap();
        assert_eq!(d.samples.len(), FRAME_48K, "upsampled back to 48 kHz");
    }
}
