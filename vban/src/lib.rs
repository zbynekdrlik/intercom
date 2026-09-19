//! VBAN protocol implementation — the shared codec (issue 1345 M1).
//!
//! VBAN (VB-Audio Network) is a simple UDP-based audio streaming protocol.
//! Default port: 6980.
//!
//! This crate is the ONE source of truth for the VBAN wire format across the repo: the
//! camera-box appliance re-exports it verbatim (`src/vban.rs` is now `pub use intercom_vban::*;`,
//! so `src/intercom.rs` and its tests are behaviourally untouched) and the strih-lx intercom hub
//! (`intercom/hub`) speaks the identical bytes to the 7 camboxes. Moved out of `src/vban.rs`
//! unchanged (the codec + its unit tests), so the cambox contract stays byte-identical.

use anyhow::{anyhow, Result};

/// VBAN magic header bytes
pub const VBAN_MAGIC: &[u8; 4] = b"VBAN";

/// Default VBAN UDP port
pub const VBAN_PORT: u16 = 6980;

/// VBAN header size in bytes
pub const VBAN_HEADER_SIZE: usize = 28;

/// Maximum stream name length (including null terminator)
pub const VBAN_STREAM_NAME_SIZE: usize = 16;

/// VBAN protocol types
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum VbanProtocol {
    Audio = 0x00,
    Serial = 0x20,
    Text = 0x40,
    Service = 0x60,
}

/// VBAN sample rates (index -> Hz)
pub const SAMPLE_RATES: &[u32] = &[
    6000, 12000, 24000, 48000, 96000, 192000, 384000, // 0-6
    8000, 16000, 32000, 64000, 128000, 256000, 512000, // 7-13
    11025, 22050, 44100, 88200, 176400, 352800, // 14-19
];

/// VBAN audio codec formats
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum VbanCodec {
    Pcm8 = 0x00,
    Pcm16 = 0x01,
    Pcm24 = 0x02,
    Pcm32 = 0x03,
    Float32 = 0x04,
    Float64 = 0x05,
}

#[allow(dead_code)]
impl VbanCodec {
    /// Get bytes per sample for this codec
    pub fn bytes_per_sample(&self) -> usize {
        match self {
            VbanCodec::Pcm8 => 1,
            VbanCodec::Pcm16 => 2,
            VbanCodec::Pcm24 => 3,
            VbanCodec::Pcm32 => 4,
            VbanCodec::Float32 => 4,
            VbanCodec::Float64 => 8,
        }
    }
}

/// VBAN packet header
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct VbanHeader {
    /// Sample rate index (0-19)
    pub sample_rate_index: u8,
    /// Number of samples per frame (1-256, stored as n-1)
    pub samples_per_frame: u8,
    /// Number of channels (1-256, stored as n-1)
    pub channels: u8,
    /// Data format/codec
    pub codec: u8,
    /// Stream name (up to 16 bytes, null-terminated)
    pub stream_name: [u8; VBAN_STREAM_NAME_SIZE],
    /// Frame counter
    pub frame_counter: u32,
}

#[allow(dead_code)]
impl VbanHeader {
    /// Create a new VBAN header
    pub fn new(
        stream_name: &str,
        sample_rate: u32,
        channels: u8,
        codec: VbanCodec,
    ) -> Result<Self> {
        let sample_rate_index = sample_rate_to_index(sample_rate)
            .ok_or_else(|| anyhow!("Unsupported sample rate: {}", sample_rate))?;

        let mut name_bytes = [0u8; VBAN_STREAM_NAME_SIZE];
        let name_len = stream_name.len().min(VBAN_STREAM_NAME_SIZE - 1);
        name_bytes[..name_len].copy_from_slice(&stream_name.as_bytes()[..name_len]);

        Ok(Self {
            sample_rate_index,
            samples_per_frame: 0, // Will be set per packet
            channels: channels.saturating_sub(1),
            codec: codec as u8,
            stream_name: name_bytes,
            frame_counter: 0,
        })
    }

    /// Get the actual sample rate in Hz
    #[allow(dead_code)]
    pub fn sample_rate(&self) -> u32 {
        let idx = (self.sample_rate_index & 0x1F) as usize;
        if idx < SAMPLE_RATES.len() {
            SAMPLE_RATES[idx]
        } else {
            48000 // Default fallback
        }
    }

    /// Get the actual number of channels
    #[allow(dead_code)]
    pub fn num_channels(&self) -> u8 {
        self.channels.saturating_add(1)
    }

    /// Get the actual samples per frame
    #[allow(dead_code)]
    pub fn num_samples(&self) -> usize {
        (self.samples_per_frame as usize).saturating_add(1)
    }

    /// Encode header to bytes
    pub fn encode(&self, samples_per_frame: usize) -> [u8; VBAN_HEADER_SIZE] {
        let mut buf = [0u8; VBAN_HEADER_SIZE];

        // Magic "VBAN"
        buf[0..4].copy_from_slice(VBAN_MAGIC);

        // Sample rate index (lower 5 bits) + protocol (upper 3 bits)
        buf[4] = self.sample_rate_index & 0x1F; // Audio protocol = 0x00

        // Samples per frame - 1
        buf[5] = (samples_per_frame.saturating_sub(1) & 0xFF) as u8;

        // Channels - 1
        buf[6] = self.channels;

        // Codec/format
        buf[7] = self.codec;

        // Stream name
        buf[8..24].copy_from_slice(&self.stream_name);

        // Frame counter (little-endian)
        buf[24..28].copy_from_slice(&self.frame_counter.to_le_bytes());

        buf
    }

    /// Decode header from bytes
    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < VBAN_HEADER_SIZE {
            return Err(anyhow!("VBAN packet too short: {} bytes", data.len()));
        }

        // Check magic
        if &data[0..4] != VBAN_MAGIC {
            return Err(anyhow!("Invalid VBAN magic"));
        }

        // Check protocol type (upper 3 bits of byte 4)
        let protocol = data[4] & 0xE0;
        if protocol != VbanProtocol::Audio as u8 {
            return Err(anyhow!("Not a VBAN audio packet"));
        }

        let mut stream_name = [0u8; VBAN_STREAM_NAME_SIZE];
        stream_name.copy_from_slice(&data[8..24]);

        Ok(Self {
            sample_rate_index: data[4] & 0x1F,
            samples_per_frame: data[5],
            channels: data[6],
            codec: data[7],
            stream_name,
            frame_counter: u32::from_le_bytes([data[24], data[25], data[26], data[27]]),
        })
    }

    /// Get stream name as string
    pub fn stream_name_str(&self) -> &str {
        let end = self
            .stream_name
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(VBAN_STREAM_NAME_SIZE);
        std::str::from_utf8(&self.stream_name[..end]).unwrap_or("")
    }
}

/// Convert sample rate to VBAN index
#[allow(dead_code)]
pub fn sample_rate_to_index(rate: u32) -> Option<u8> {
    SAMPLE_RATES
        .iter()
        .position(|&r| r == rate)
        .map(|i| i as u8)
}

/// Maximum VBAN packet size (header + 256 samples * 8 channels * 4 bytes)
pub const MAX_VBAN_PACKET_SIZE: usize = VBAN_HEADER_SIZE + 256 * 8 * 4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_encode_decode() {
        let header = VbanHeader::new("test", 48000, 2, VbanCodec::Pcm16).unwrap();
        let encoded = header.encode(256);
        let decoded = VbanHeader::decode(&encoded).unwrap();

        assert_eq!(decoded.sample_rate(), 48000);
        assert_eq!(decoded.num_channels(), 2);
        assert_eq!(decoded.stream_name_str(), "test");
    }

    #[test]
    fn test_sample_rate_index() {
        assert_eq!(sample_rate_to_index(48000), Some(3));
        assert_eq!(sample_rate_to_index(44100), Some(16));
        assert_eq!(sample_rate_to_index(12345), None);
    }

    #[test]
    fn test_codec_bytes_per_sample_all_variants() {
        assert_eq!(VbanCodec::Pcm8.bytes_per_sample(), 1);
        assert_eq!(VbanCodec::Pcm16.bytes_per_sample(), 2);
        assert_eq!(VbanCodec::Pcm24.bytes_per_sample(), 3);
        assert_eq!(VbanCodec::Pcm32.bytes_per_sample(), 4);
        assert_eq!(VbanCodec::Float32.bytes_per_sample(), 4);
        assert_eq!(VbanCodec::Float64.bytes_per_sample(), 8);
    }

    #[test]
    fn test_header_new_invalid_sample_rate() {
        let result = VbanHeader::new("test", 12345, 2, VbanCodec::Pcm16);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Unsupported sample rate"));
    }

    #[test]
    fn test_header_decode_too_short() {
        let short_data = [0u8; 20]; // Less than VBAN_HEADER_SIZE (28)
        let result = VbanHeader::decode(&short_data);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too short"));
    }

    #[test]
    fn test_header_decode_invalid_magic() {
        let mut data = [0u8; VBAN_HEADER_SIZE];
        data[0..4].copy_from_slice(b"XXXX"); // Wrong magic
        let result = VbanHeader::decode(&data);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid VBAN magic"));
    }

    #[test]
    fn test_header_decode_non_audio_protocol() {
        let mut data = [0u8; VBAN_HEADER_SIZE];
        data[0..4].copy_from_slice(VBAN_MAGIC);
        data[4] = 0x20; // Serial protocol, not Audio
        let result = VbanHeader::decode(&data);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Not a VBAN audio packet"));
    }

    #[test]
    fn test_stream_name_truncation() {
        // Name longer than 15 chars should be truncated
        let long_name = "this_is_a_very_long_stream_name";
        let header = VbanHeader::new(long_name, 48000, 2, VbanCodec::Pcm16).unwrap();
        let name = header.stream_name_str();
        assert_eq!(name.len(), 15); // Max 15 chars (16 - null terminator)
        assert_eq!(name, "this_is_a_very_");
    }

    #[test]
    fn test_stream_name_exactly_max_length() {
        let exact_name = "exactly15chars!"; // 15 chars
        let header = VbanHeader::new(exact_name, 48000, 2, VbanCodec::Pcm16).unwrap();
        assert_eq!(header.stream_name_str(), exact_name);
    }

    #[test]
    fn test_sample_rate_index_all_rates() {
        // Test all 20 valid sample rates
        let expected_rates = [
            (6000, 0),
            (12000, 1),
            (24000, 2),
            (48000, 3),
            (96000, 4),
            (192000, 5),
            (384000, 6),
            (8000, 7),
            (16000, 8),
            (32000, 9),
            (64000, 10),
            (128000, 11),
            (256000, 12),
            (512000, 13),
            (11025, 14),
            (22050, 15),
            (44100, 16),
            (88200, 17),
            (176400, 18),
            (352800, 19),
        ];
        for (rate, expected_index) in expected_rates {
            assert_eq!(
                sample_rate_to_index(rate),
                Some(expected_index),
                "Failed for rate {}",
                rate
            );
        }
    }

    #[test]
    fn test_header_encode_decode_roundtrip_all_sample_rates() {
        for &rate in SAMPLE_RATES {
            let header = VbanHeader::new("test", rate, 2, VbanCodec::Pcm16).unwrap();
            let encoded = header.encode(128);
            let decoded = VbanHeader::decode(&encoded).unwrap();
            assert_eq!(
                decoded.sample_rate(),
                rate,
                "Round-trip failed for rate {}",
                rate
            );
        }
    }

    #[test]
    fn test_header_channels() {
        // Test channel count encoding (stored as n-1)
        for channels in 1..=8 {
            let header = VbanHeader::new("test", 48000, channels, VbanCodec::Pcm16).unwrap();
            let encoded = header.encode(256);
            let decoded = VbanHeader::decode(&encoded).unwrap();
            assert_eq!(
                decoded.num_channels(),
                channels,
                "Failed for {} channels",
                channels
            );
        }
    }

    #[test]
    fn test_header_samples_per_frame() {
        let header = VbanHeader::new("test", 48000, 2, VbanCodec::Pcm16).unwrap();
        for samples in [1, 64, 128, 256] {
            let encoded = header.encode(samples);
            let decoded = VbanHeader::decode(&encoded).unwrap();
            assert_eq!(
                decoded.num_samples(),
                samples,
                "Failed for {} samples",
                samples
            );
        }
    }

    #[test]
    fn test_header_frame_counter() {
        let mut header = VbanHeader::new("test", 48000, 2, VbanCodec::Pcm16).unwrap();
        header.frame_counter = 0x12345678;
        let encoded = header.encode(256);
        let decoded = VbanHeader::decode(&encoded).unwrap();
        assert_eq!(decoded.frame_counter, 0x12345678);
    }

    #[test]
    fn test_protocol_enum_values() {
        assert_eq!(VbanProtocol::Audio as u8, 0x00);
        assert_eq!(VbanProtocol::Serial as u8, 0x20);
        assert_eq!(VbanProtocol::Text as u8, 0x40);
        assert_eq!(VbanProtocol::Service as u8, 0x60);
    }

    #[test]
    fn test_constants() {
        assert_eq!(VBAN_PORT, 6980);
        assert_eq!(VBAN_HEADER_SIZE, 28);
        assert_eq!(VBAN_STREAM_NAME_SIZE, 16);
        assert_eq!(VBAN_MAGIC, b"VBAN");
    }

    /// issue 1345 M1: a full PACKET round-trip (header + PCM16 payload) — the exact wire path the
    /// strih-lx hub's `vban_io` sender/receiver walk. Build a `camN`-named stereo PCM16 packet the
    /// way the cambox does (header::encode + little-endian i16 samples), then decode it back and
    /// recover the stream name, channel count, sample count and every sample byte-exactly. This is
    /// the byte-compatibility proof the extracted crate must uphold for the 7 camboxes.
    #[test]
    fn test_full_packet_roundtrip_pcm16() {
        let stream = "cam3";
        let header = VbanHeader::new(stream, 48000, 2, VbanCodec::Pcm16).unwrap();
        // Interleaved stereo samples (L,R per frame), 128 frames.
        let frames = 128usize;
        let samples: Vec<i16> = (0..frames * 2)
            .map(|i| (i as i32 * 137 - 9000) as i16)
            .collect();

        // Encode: header for `frames` frames, then the interleaved PCM16 payload (little-endian).
        let mut packet = header.encode(frames).to_vec();
        for &s in &samples {
            packet.extend_from_slice(&s.to_le_bytes());
        }
        assert_eq!(packet.len(), VBAN_HEADER_SIZE + samples.len() * 2);

        // Decode the header back.
        let decoded = VbanHeader::decode(&packet).unwrap();
        assert_eq!(decoded.stream_name_str(), stream);
        assert_eq!(decoded.sample_rate(), 48000);
        assert_eq!(decoded.num_channels(), 2);
        assert_eq!(decoded.num_samples(), frames);
        assert_eq!(decoded.codec, VbanCodec::Pcm16 as u8);

        // Recover every sample from the payload byte-for-byte.
        let payload = &packet[VBAN_HEADER_SIZE..];
        let recovered: Vec<i16> = payload
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes(*c))
            .collect();
        assert_eq!(recovered, samples);
    }
}
