//! issue 1462 (22.10.2026): the hub decodes 24-bit PCM VBAN.
//!
//! mbc.lan sends the SNV stream program as VBAN `stream` in 24-bit PCM (header byte 7 = 0x02,
//! 239 frames x 2 ch at 48 kHz, read live with tcpdump on stream-snv). The decoder knew only PCM16
//! and Float32, so every such packet was dropped and the program stayed silent. Pinned here:
//!
//! * a 24-bit stereo packet decodes to its frames, each sample the top 16 bits of the 24;
//! * negative samples keep their sign;
//! * a trailing partial frame is trimmed, never read past the payload.

use intercom_hub::vban_io::decode_packet;
use intercom_vban::{VbanCodec, VbanHeader};

/// One VBAN packet: our header with `codec`, `frames` frames of `channels`, and `payload` bytes.
fn packet(codec: VbanCodec, channels: u8, frames: usize, payload: &[u8]) -> Vec<u8> {
    let header = VbanHeader::new("stream", 48_000, channels, codec).unwrap();
    let mut out = header.encode(frames).to_vec();
    out.extend_from_slice(payload);
    out
}

/// A 24-bit little-endian sample.
fn s24(v: i32) -> [u8; 3] {
    let b = v.to_le_bytes();
    [b[0], b[1], b[2]]
}

#[test]
fn a_24_bit_stereo_packet_decodes_to_the_top_16_bits_of_each_sample() {
    // Frame 0: L = 0x123456, R = 0x7FFFFF; frame 1: L = 0x000100, R = 0x00FF00.
    let mut payload = Vec::new();
    for v in [0x12_3456, 0x7F_FFFF, 0x00_0100, 0x00_FF00] {
        payload.extend_from_slice(&s24(v));
    }
    let (audio, rate) = decode_packet(&packet(VbanCodec::Pcm24, 2, 2, &payload)).unwrap();
    assert_eq!(rate, 48_000);
    assert_eq!(audio.stream_name, "stream");
    assert_eq!(audio.frames, 2);
    assert_eq!(audio.channels.len(), 2);
    assert_eq!(audio.channels[0], vec![0x1234, 0x0001]);
    assert_eq!(audio.channels[1], vec![0x7FFF, 0x00FF]);
}

#[test]
fn negative_24_bit_samples_keep_their_sign() {
    let mut payload = Vec::new();
    for v in [-1, -0x80_0000, -0x1_0000] {
        payload.extend_from_slice(&s24(v));
    }
    let (audio, _) = decode_packet(&packet(VbanCodec::Pcm24, 1, 3, &payload)).unwrap();
    assert_eq!(audio.channels[0], vec![-1, i16::MIN, -0x100]);
}

#[test]
fn the_live_mbc_packet_shape_decodes_every_frame() {
    // 239 frames x 2 ch x 3 bytes, as mbc sends it.
    let frames = 239usize;
    let mut payload = Vec::new();
    for i in 0..frames * 2 {
        payload.extend_from_slice(&s24((i as i32) << 8));
    }
    let (audio, _) = decode_packet(&packet(VbanCodec::Pcm24, 2, 239, &payload)).unwrap();
    assert_eq!(audio.frames, frames);
    assert_eq!(audio.channels[0][0], 0);
    assert_eq!(audio.channels[1][0], 1);
    assert_eq!(audio.channels[1][frames - 1], (frames * 2 - 1) as i16);
}

#[test]
fn a_trailing_partial_24_bit_frame_is_trimmed() {
    // Two whole stereo frames and two stray bytes.
    let mut payload = Vec::new();
    for v in [1 << 8, 2 << 8, 3 << 8, 4 << 8] {
        payload.extend_from_slice(&s24(v));
    }
    payload.extend_from_slice(&[0xAA, 0xBB]);
    let (audio, _) = decode_packet(&packet(VbanCodec::Pcm24, 2, 2, &payload)).unwrap();
    assert_eq!(audio.frames, 2);
    assert_eq!(audio.channels[0], vec![1, 3]);
    assert_eq!(audio.channels[1], vec![2, 4]);
}
