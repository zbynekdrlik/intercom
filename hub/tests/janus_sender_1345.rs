//! The paced Janus sender thread end to end (issue 1345, 25.9.2026). A loopback UDP receiver stands
//! in for Janus; a feeder pushes 256-frame blocks on the 5.33 ms mix beat. The packets must arrive on
//! the sender's own 20 ms grid with contiguous RTP numbering, whatever the block phase.

use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use intercom_hub::janus_codec::TxEncoder;
use intercom_hub::janus_pacing::{
    hub_block_period, PacedRing, RING_CAP_FRAMES, RING_TARGET_FRAMES,
};
use intercom_hub::janus_rtp::{JanusCodec, JanusSharedStats};
use intercom_hub::janus_sender::{
    spawn_paced_sender, PacedSenderConfig, PacedSenderShared, TxTarget,
};

struct Received {
    payload_type: u8,
    marker: bool,
    seq: u16,
    ts: u32,
    payload_len: usize,
    at: Instant,
}

/// Run the real sender thread for `packets` packets and return what arrived.
fn run(codec: JanusCodec, packets: usize) -> (Vec<Received>, Arc<JanusSharedStats>) {
    let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
    rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
    let ring = Arc::new(Mutex::new(PacedRing::new(
        RING_TARGET_FRAMES,
        RING_CAP_FRAMES,
    )));
    let shared = Arc::new(PacedSenderShared::default());
    let stats = Arc::new(JanusSharedStats::new(codec));
    spawn_paced_sender(
        PacedSenderConfig {
            socket: tx,
            codec,
            ssrc: 0x5354_524C,
            encoder: TxEncoder::new(codec).unwrap(),
        },
        ring.clone(),
        shared.clone(),
        stats.clone(),
    )
    .unwrap();
    shared.set_target(Some(TxTarget {
        addr: rx.local_addr().unwrap(),
        session: 1,
    }));
    // The mix block loop: 256 frames per exact block period.
    std::thread::spawn(move || {
        let period = hub_block_period(256, 48_000);
        let start = Instant::now();
        let mut n: u32 = 0;
        loop {
            let due = period * n;
            let now = start.elapsed();
            if due > now {
                std::thread::sleep(due - now);
            }
            ring.lock().unwrap().push(&[1000i16; 256]);
            n += 1;
        }
    });
    let mut buf = [0u8; 2048];
    let mut got = Vec::with_capacity(packets);
    for _ in 0..packets {
        let (len, _) = rx.recv_from(&mut buf).expect("a packet every 20 ms");
        got.push(Received {
            payload_type: buf[1] & 0x7F,
            marker: buf[1] & 0x80 != 0,
            seq: u16::from_be_bytes([buf[2], buf[3]]),
            ts: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
            payload_len: len - 12,
            at: Instant::now(),
        });
    }
    (got, stats)
}

fn mean_interval_ms(got: &[Received]) -> f64 {
    let span = got.last().unwrap().at - got.first().unwrap().at;
    span.as_secs_f64() * 1000.0 / (got.len() - 1) as f64
}

#[test]
fn opus_packets_leave_on_the_20ms_grid_with_contiguous_numbering() {
    let (got, stats) = run(JanusCodec::Opus, 30);
    assert!(got[0].marker, "the first packet of a session marks");
    for (i, p) in got.iter().enumerate() {
        assert_eq!(p.payload_type, 111, "Opus payload type");
        assert!(p.payload_len > 0 && p.payload_len <= 1275);
        if i > 0 {
            assert!(!p.marker, "only the first packet marks");
            assert_eq!(p.seq, got[i - 1].seq.wrapping_add(1));
            assert_eq!(p.ts, got[i - 1].ts.wrapping_add(960));
        }
    }
    let mean = mean_interval_ms(&got);
    assert!((19.0..=21.0).contains(&mean), "mean interval {mean:.3} ms");
    let facet = stats.snapshot();
    assert_eq!(facet.codec, "opus");
    assert!(facet.tx_packets >= 30);
    assert_eq!(facet.tx_overflow_trims, 0);
}

#[test]
fn pcmu_stays_selectable_on_the_same_grid() {
    let (got, stats) = run(JanusCodec::Pcmu, 15);
    for (i, p) in got.iter().enumerate() {
        assert_eq!(p.payload_type, 0, "PCMU payload type");
        assert_eq!(p.payload_len, 160, "20 ms of 8 kHz mu-law");
        if i > 0 {
            assert_eq!(p.ts, got[i - 1].ts.wrapping_add(160));
        }
    }
    let mean = mean_interval_ms(&got);
    assert!((19.0..=21.0).contains(&mean), "mean interval {mean:.3} ms");
    assert_eq!(stats.snapshot().codec, "pcmu");
}
