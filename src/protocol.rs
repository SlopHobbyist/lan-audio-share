//! Wire format for LAN audio sharing.
//!
//! All integers are little-endian. Audio travels as raw PCM in sub-MTU packets:
//! on a LAN the bandwidth is free (48k/stereo/f32 is ~3 Mbit/s), and skipping a
//! codec removes its encoder/decoder lookahead entirely. That is why this is
//! both simpler and lower-latency than a compressed transport.
//!
//! Discovery is peer-to-peer, with no server anywhere: receivers announce
//! themselves on a multicast group (and, as a fallback for networks that drop
//! multicast, the subnet broadcast address), and senders unicast audio to
//! whoever they have heard from recently. Unicast audio survives Wi-Fi far
//! better than multicast audio would.

use serde::{Deserialize, Serialize};

pub const MAGIC: [u8; 4] = *b"LAS1";
pub const VERSION: u8 = 1;

/// Multicast group and port used only for the tiny discovery beacons.
pub const MCAST_GROUP: [u8; 4] = [239, 77, 12, 7];
pub const DISCOVERY_PORT: u16 = 47771;

/// Default port a receiver listens for audio on. The beacon carries the actual
/// port, so a second instance falling back to an ephemeral port still works.
pub const DEFAULT_AUDIO_PORT: u16 = 47772;

/// Frames of audio per network packet. 160 stereo f32 frames is 1280 bytes of
/// payload plus a 28-byte header, comfortably inside the usual 1472-byte UDP
/// payload budget so nothing gets IP-fragmented.
pub const WIRE_FRAMES: usize = 160;

/// Size of the audio packet header, in bytes.
pub const AUDIO_HEADER: usize = 28;

/// Largest audio packet we will ever build or accept.
pub const MAX_PACKET: usize = AUDIO_HEADER + WIRE_FRAMES * 2 * 4;

/// A packet must fit in one datagram on a standard 1500-byte MTU, or every
/// packet gets IP-fragmented and effective loss rises sharply. Raising
/// `WIRE_FRAMES` past that point should fail the build, not a test.
const _: () = assert!(MAX_PACKET <= 1472);

pub const PT_AUDIO: u8 = 1;
pub const PT_HELLO: u8 = 2;
pub const PT_SENDER: u8 = 3;
pub const PT_BYE: u8 = 4;

/// How often a receiver re-announces itself, and how long a sender remembers a
/// peer that has gone quiet.
pub const BEACON_INTERVAL_MS: u64 = 500;
pub const PEER_TIMEOUT_MS: u64 = 3_000;

/// Sample format used on the wire.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum WireFormat {
    /// 32-bit float: exactly what the audio stack hands us, so no conversion loss.
    F32,
    /// 16-bit int: half the bandwidth, still CD quality. Useful on flaky Wi-Fi.
    I16,
}

impl WireFormat {
    pub fn code(self) -> u8 {
        match self {
            WireFormat::F32 => 0,
            WireFormat::I16 => 1,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(WireFormat::F32),
            1 => Some(WireFormat::I16),
            _ => None,
        }
    }

    pub fn bytes_per_sample(self) -> usize {
        match self {
            WireFormat::F32 => 4,
            WireFormat::I16 => 2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            WireFormat::F32 => "32-bit float",
            WireFormat::I16 => "16-bit int",
        }
    }
}

/// Header of an audio packet.
#[derive(Clone, Copy, Debug)]
pub struct AudioHeader {
    pub format: WireFormat,
    pub channels: u8,
    pub sample_rate: u32,
    /// Increments by one per packet; used to spot loss and reordering.
    pub seq: u32,
    /// Absolute index of this packet's first frame within the stream. Lets the
    /// receiver work out the exact size of a gap without assuming packet sizes.
    pub frame_index: u64,
    pub frames: u16,
    /// Random per sender session, so a receiver can tell a restart from a gap.
    pub stream_id: u16,
}

impl AudioHeader {
    pub fn write(&self, buf: &mut [u8]) {
        buf[0..4].copy_from_slice(&MAGIC);
        buf[4] = VERSION;
        buf[5] = PT_AUDIO;
        buf[6] = self.format.code();
        buf[7] = self.channels;
        buf[8..12].copy_from_slice(&self.sample_rate.to_le_bytes());
        buf[12..16].copy_from_slice(&self.seq.to_le_bytes());
        buf[16..24].copy_from_slice(&self.frame_index.to_le_bytes());
        buf[24..26].copy_from_slice(&self.frames.to_le_bytes());
        buf[26..28].copy_from_slice(&self.stream_id.to_le_bytes());
    }

    /// Parse a header, returning it alongside the payload bytes.
    pub fn parse(buf: &[u8]) -> Option<(AudioHeader, &[u8])> {
        if buf.len() < AUDIO_HEADER || buf[0..4] != MAGIC || buf[4] != VERSION || buf[5] != PT_AUDIO
        {
            return None;
        }
        let format = WireFormat::from_code(buf[6])?;
        let channels = buf[7];
        if channels == 0 || channels > 2 {
            return None;
        }
        let header = AudioHeader {
            format,
            channels,
            sample_rate: u32::from_le_bytes(buf[8..12].try_into().ok()?),
            seq: u32::from_le_bytes(buf[12..16].try_into().ok()?),
            frame_index: u64::from_le_bytes(buf[16..24].try_into().ok()?),
            frames: u16::from_le_bytes(buf[24..26].try_into().ok()?),
            stream_id: u16::from_le_bytes(buf[26..28].try_into().ok()?),
        };
        if header.sample_rate < 8_000 || header.sample_rate > 384_000 {
            return None;
        }
        let want = header.frames as usize * channels as usize * format.bytes_per_sample();
        let payload = buf.get(AUDIO_HEADER..AUDIO_HEADER + want)?;
        Some((header, payload))
    }
}

/// A control beacon: either a receiver saying it is listening, or a sender
/// saying it is here (the latter exists purely so the receiver UI can name who
/// it is playing).
#[derive(Clone, Debug)]
pub struct Beacon {
    pub kind: u8,
    pub audio_port: u16,
    pub sample_rate: u32,
    pub channels: u8,
    pub name: String,
}

impl Beacon {
    pub fn encode(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        let name = &name[..name.len().min(64)];
        let mut buf = Vec::with_capacity(14 + name.len());
        buf.extend_from_slice(&MAGIC);
        buf.push(VERSION);
        buf.push(self.kind);
        buf.extend_from_slice(&self.audio_port.to_le_bytes());
        buf.extend_from_slice(&self.sample_rate.to_le_bytes());
        buf.push(self.channels);
        buf.push(name.len() as u8);
        buf.extend_from_slice(name);
        buf
    }

    pub fn parse(buf: &[u8]) -> Option<Beacon> {
        if buf.len() < 14 || buf[0..4] != MAGIC || buf[4] != VERSION {
            return None;
        }
        let kind = buf[5];
        if kind != PT_HELLO && kind != PT_SENDER && kind != PT_BYE {
            return None;
        }
        let name_len = buf[13] as usize;
        let name = buf.get(14..14 + name_len)?;
        Some(Beacon {
            kind,
            audio_port: u16::from_le_bytes(buf[6..8].try_into().ok()?),
            sample_rate: u32::from_le_bytes(buf[8..12].try_into().ok()?),
            channels: buf[12],
            name: String::from_utf8_lossy(name).into_owned(),
        })
    }
}

/// Encode `samples` (interleaved f32, -1.0..=1.0) into `dst` in the wire format.
pub fn encode_samples(format: WireFormat, samples: &[f32], dst: &mut Vec<u8>) {
    match format {
        WireFormat::F32 => {
            for s in samples {
                dst.extend_from_slice(&s.to_le_bytes());
            }
        }
        WireFormat::I16 => {
            for s in samples {
                let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                dst.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
}

/// Decode wire payload bytes into interleaved f32, appended to `dst`.
pub fn decode_samples(format: WireFormat, payload: &[u8], dst: &mut Vec<f32>) {
    match format {
        WireFormat::F32 => {
            for chunk in payload.chunks_exact(4) {
                dst.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            }
        }
        WireFormat::I16 => {
            for chunk in payload.chunks_exact(2) {
                let v = i16::from_le_bytes([chunk[0], chunk[1]]);
                dst.push(v as f32 / i16::MAX as f32);
            }
        }
    }
}

/// This machine's name, for display in the other end's UI.
pub fn local_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_header_round_trips() {
        let header = AudioHeader {
            format: WireFormat::F32,
            channels: 2,
            sample_rate: 48_000,
            seq: 123_456,
            frame_index: 9_876_543_210,
            frames: WIRE_FRAMES as u16,
            stream_id: 0xBEEF,
        };
        let mut buf = vec![0u8; AUDIO_HEADER];
        header.write(&mut buf);
        buf.resize(AUDIO_HEADER + WIRE_FRAMES * 2 * 4, 0);

        let (parsed, payload) = AudioHeader::parse(&buf).expect("header should parse");
        assert_eq!(parsed.sample_rate, 48_000);
        assert_eq!(parsed.seq, 123_456);
        assert_eq!(parsed.frame_index, 9_876_543_210);
        assert_eq!(parsed.frames, WIRE_FRAMES as u16);
        assert_eq!(parsed.stream_id, 0xBEEF);
        assert_eq!(parsed.channels, 2);
        assert_eq!(payload.len(), WIRE_FRAMES * 2 * 4);
    }

    /// Float audio must survive the wire bit-exactly.
    #[test]
    fn f32_samples_are_lossless() {
        let samples: Vec<f32> = vec![0.0, 1.0, -1.0, 0.5, -0.333_333, 1e-9];
        let mut bytes = Vec::new();
        encode_samples(WireFormat::F32, &samples, &mut bytes);
        let mut back = Vec::new();
        decode_samples(WireFormat::F32, &bytes, &mut back);
        assert_eq!(samples, back);
    }

    #[test]
    fn i16_samples_round_trip_within_quantisation() {
        let samples: Vec<f32> = vec![0.0, 1.0, -1.0, 0.5, -0.25];
        let mut bytes = Vec::new();
        encode_samples(WireFormat::I16, &samples, &mut bytes);
        assert_eq!(bytes.len(), samples.len() * 2);
        let mut back = Vec::new();
        decode_samples(WireFormat::I16, &bytes, &mut back);
        for (a, b) in samples.iter().zip(&back) {
            assert!((a - b).abs() < 1.0 / 32_000.0, "{a} vs {b}");
        }
    }

    /// Garbage on the port must be rejected, not played.
    #[test]
    fn rejects_malformed_packets() {
        assert!(AudioHeader::parse(&[]).is_none());
        assert!(AudioHeader::parse(&[0u8; 40]).is_none(), "bad magic");

        // Right magic, but the payload is shorter than the header claims.
        let header = AudioHeader {
            format: WireFormat::F32,
            channels: 2,
            sample_rate: 48_000,
            seq: 0,
            frame_index: 0,
            frames: WIRE_FRAMES as u16,
            stream_id: 1,
        };
        let mut buf = vec![0u8; AUDIO_HEADER + 16];
        header.write(&mut buf);
        assert!(AudioHeader::parse(&buf).is_none(), "truncated payload");
    }

    #[test]
    fn beacon_round_trips() {
        let beacon = Beacon {
            kind: PT_HELLO,
            audio_port: 47_772,
            sample_rate: 48_000,
            channels: 2,
            name: "studio-pc".to_string(),
        };
        let parsed = Beacon::parse(&beacon.encode()).expect("beacon should parse");
        assert_eq!(parsed.kind, PT_HELLO);
        assert_eq!(parsed.audio_port, 47_772);
        assert_eq!(parsed.name, "studio-pc");
    }

    /// An over-long hostname must be truncated rather than corrupting the frame.
    #[test]
    fn beacon_survives_a_long_name() {
        let beacon = Beacon {
            kind: PT_HELLO,
            audio_port: 1234,
            sample_rate: 48_000,
            channels: 2,
            name: "x".repeat(500),
        };
        let parsed = Beacon::parse(&beacon.encode()).expect("beacon should parse");
        assert_eq!(parsed.audio_port, 1234);
        assert!(parsed.name.len() <= 64);
    }
}
