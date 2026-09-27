//! SEND mode: read an input device and unicast it to every known receiver.
//!
//! Packets are built and sent straight from the audio callback. That looks
//! aggressive, but it is the lowest-latency option available: with a 2048-frame
//! capture buffer the audio does not *exist* until the callback fires, so
//! handing it to a pacing thread to dribble out would only add delay. The burst
//! is ~17 kB, which clears a LAN link in well under a millisecond.

use crate::config::Config;
use crate::devices::{self, Direction};
use crate::net::{self, NetThreads, PeerTargets};
use crate::protocol::{AUDIO_HEADER, AudioHeader, WIRE_FRAMES, WireFormat, encode_samples};
use crate::stats::Stats;
use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{BufferSize, Device, FromSample, SampleFormat, SizedSample, StreamConfig};
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Channels on the wire. Desktop audio is stereo; anything wider gets folded
/// down to the first pair and mono gets duplicated.
const WIRE_CHANNELS: usize = 2;

pub struct SendRole {
    stream: cpal::Stream,
    _discovery: NetThreads,
    pub device_name: String,
    pub rate: u32,
    pub device_channels: u16,
}

impl SendRole {
    pub fn pause(&self) {
        let _ = self.stream.pause();
    }
}

/// Turns device audio into wire packets.
///
/// Split out from the audio callback so it can be tested without an input
/// device, which matters because the machine that sends is not necessarily the
/// machine this gets built on.
struct Packetizer {
    format: WireFormat,
    rate: u32,
    stream_id: u16,
    seq: u32,
    frame_index: u64,
    /// Stereo audio not yet aligned to a whole packet.
    pending: Vec<f32>,
    /// Scratch for the packet currently being built.
    packet: Vec<u8>,
}

impl Packetizer {
    fn new(format: WireFormat, rate: u32, stream_id: u16) -> Self {
        Self {
            format,
            rate,
            stream_id,
            seq: 0,
            frame_index: 0,
            // Pre-allocated: the audio thread must never allocate.
            pending: Vec::with_capacity(WIRE_FRAMES * WIRE_CHANNELS * 32),
            packet: Vec::with_capacity(AUDIO_HEADER + WIRE_FRAMES * WIRE_CHANNELS * 4),
        }
    }

    /// Samples in one whole packet.
    const fn chunk() -> usize {
        WIRE_FRAMES * WIRE_CHANNELS
    }

    /// Fold interleaved device audio down to stereo and queue it, returning the
    /// peak level seen for the meter.
    fn push_device_audio<T>(&mut self, data: &[T], device_channels: usize, gain: f32) -> f32
    where
        T: Copy,
        f32: FromSample<T>,
    {
        let channels = device_channels.max(1);
        let frames = data.len() / channels;
        let mut peak = 0.0f32;

        for f in 0..frames {
            let base = f * channels;
            let left = f32::from_sample_(data[base]) * gain;
            let right = if channels >= 2 {
                f32::from_sample_(data[base + 1]) * gain
            } else {
                left
            };
            peak = peak.max(left.abs()).max(right.abs());
            self.pending.push(left);
            self.pending.push(right);
        }

        peak
    }

    /// Hand each complete packet to `send`, returning how many were produced.
    fn drain(&mut self, mut send: impl FnMut(&[u8])) -> usize {
        let mut count = 0;
        while self.pending.len() >= Self::chunk() {
            self.packet.clear();
            self.packet.resize(AUDIO_HEADER, 0);
            AudioHeader {
                format: self.format,
                channels: WIRE_CHANNELS as u8,
                sample_rate: self.rate,
                seq: self.seq,
                frame_index: self.frame_index,
                frames: WIRE_FRAMES as u16,
                stream_id: self.stream_id,
            }
            .write(&mut self.packet);
            encode_samples(
                self.format,
                &self.pending[..Self::chunk()],
                &mut self.packet,
            );

            send(&self.packet);

            self.pending.drain(..Self::chunk());
            self.advance();
            count += 1;
        }
        count
    }

    /// Throw away complete packets without sending them, which is what happens
    /// while nobody is listening. The stream clock still advances so a receiver
    /// joining later sees a continuous sequence rather than an apparent gap.
    fn discard(&mut self) {
        while self.pending.len() >= Self::chunk() {
            self.pending.drain(..Self::chunk());
            self.advance();
        }
    }

    fn advance(&mut self) {
        self.seq = self.seq.wrapping_add(1);
        self.frame_index += WIRE_FRAMES as u64;
    }
}

pub fn start(config: &Config, stats: Arc<Stats>) -> Result<SendRole> {
    let selection = config
        .send_device
        .clone()
        .or_else(|| devices::default_entry(Direction::Input).map(|e| e.selection()))
        .context("no input device selected and no system default available")?;

    let device = devices::resolve(Direction::Input, &selection)?;
    let supported = devices::choose_config(
        &device,
        Direction::Input,
        config.sample_rate,
        config.channels(),
    )?;

    let sample_format = supported.sample_format();
    let rate = supported.sample_rate();
    let device_channels = supported.channels();

    let stream_config = StreamConfig {
        channels: device_channels,
        sample_rate: rate,
        buffer_size: BufferSize::Fixed(config.buffer_frames),
    };

    let socket = net::sender_socket()?;
    let targets = net::new_peer_targets();
    let discovery = net::spawn_sender_discovery(stats.clone(), targets.clone(), config.clone())?;
    let packetizer = Packetizer::new(config.format, rate, session_id());

    let stream = match sample_format {
        SampleFormat::F32 => build::<f32>(
            &device,
            &stream_config,
            device_channels as usize,
            packetizer,
            socket,
            targets,
            stats.clone(),
        ),
        SampleFormat::I16 => build::<i16>(
            &device,
            &stream_config,
            device_channels as usize,
            packetizer,
            socket,
            targets,
            stats.clone(),
        ),
        SampleFormat::I32 => build::<i32>(
            &device,
            &stream_config,
            device_channels as usize,
            packetizer,
            socket,
            targets,
            stats.clone(),
        ),
        SampleFormat::U16 => build::<u16>(
            &device,
            &stream_config,
            device_channels as usize,
            packetizer,
            socket,
            targets,
            stats.clone(),
        ),
        other => anyhow::bail!("unsupported input sample format {other:?}"),
    }?;

    stream.play().context("could not start the input stream")?;

    Ok(SendRole {
        device_name: devices::device_name(&device),
        stream,
        _discovery: discovery,
        rate,
        device_channels,
    })
}

fn build<T>(
    device: &Device,
    stream_config: &StreamConfig,
    device_channels: usize,
    mut packetizer: Packetizer,
    socket: UdpSocket,
    targets: PeerTargets,
    stats: Arc<Stats>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    // Local copy of the target list, refreshed without ever blocking the audio
    // thread on the discovery thread.
    let mut cached: Arc<Vec<SocketAddr>> = Arc::new(Vec::new());

    let error_stats = stats.clone();
    let stream = device
        .build_input_stream(
            *stream_config,
            move |data: &[T], _info| {
                if let Ok(guard) = targets.try_lock()
                    && !Arc::ptr_eq(&cached, &guard)
                {
                    cached = guard.clone();
                }

                let gain = stats.get_volume();
                let peak = packetizer.push_device_audio(data, device_channels, gain);
                stats.report_peak(peak);

                if cached.is_empty() {
                    packetizer.discard();
                    return;
                }

                let mut bytes = 0u64;
                let mut sent = 0u64;
                packetizer.drain(|packet| {
                    for addr in cached.iter() {
                        if socket.send_to(packet, addr).is_ok() {
                            bytes += packet.len() as u64;
                            sent += 1;
                        }
                    }
                });

                if sent > 0 {
                    stats.packets.fetch_add(sent, Ordering::Relaxed);
                    stats.bytes.fetch_add(bytes, Ordering::Relaxed);
                    stats.note_audio();
                }
            },
            move |err| {
                error_stats.set_error(format!("input device error: {err}"));
                error_stats.restart_requested.store(true, Ordering::Relaxed);
            },
            None,
        )
        .context("could not open the input device")?;

    Ok(stream)
}

/// A per-session id so a receiver can tell "the sender restarted" from "a packet
/// went missing". Time-derived, which is plenty for distinguishing consecutive
/// runs on a LAN.
fn session_id() -> u16 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_nanos() as u16) | 1)
        .unwrap_or(1)
}

/// Bandwidth of a stream, for display in the UI.
pub fn bitrate_kbps(rate: u32, format: WireFormat) -> u32 {
    (rate as u64 * WIRE_CHANNELS as u64 * format.bytes_per_sample() as u64 * 8 / 1000) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(packetizer: &mut Packetizer) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        packetizer.drain(|packet| out.push(packet.to_vec()));
        out
    }

    /// A 2048-frame device buffer should come out as whole packets plus a
    /// remainder that waits for the next callback.
    #[test]
    fn splits_a_device_buffer_into_whole_packets() {
        let mut p = Packetizer::new(WireFormat::F32, 48_000, 9);
        let frames = 2048;
        let data = vec![0.25f32; frames * 2];
        p.push_device_audio(&data, 2, 1.0);

        let packets = collect(&mut p);
        assert_eq!(packets.len(), frames / WIRE_FRAMES);
        for packet in &packets {
            assert_eq!(packet.len(), AUDIO_HEADER + WIRE_FRAMES * 2 * 4);
        }
        // The leftover frames stay queued rather than being padded or dropped.
        assert_eq!(p.pending.len(), (frames % WIRE_FRAMES) * 2);
    }

    /// Sequence numbers and frame indices must be gapless, since the receiver
    /// uses them to size concealment.
    #[test]
    fn numbers_packets_continuously() {
        let mut p = Packetizer::new(WireFormat::F32, 48_000, 9);
        let mut seen = Vec::new();
        for _ in 0..5 {
            let data = vec![0.1f32; 2048 * 2];
            p.push_device_audio(&data, 2, 1.0);
            for packet in collect(&mut p) {
                let (header, _) = AudioHeader::parse(&packet).expect("packet should parse");
                seen.push((header.seq, header.frame_index));
            }
        }
        assert!(seen.len() > 50);
        for (i, (seq, frame_index)) in seen.iter().enumerate() {
            assert_eq!(*seq, i as u32, "sequence numbers must not skip");
            assert_eq!(
                *frame_index,
                i as u64 * WIRE_FRAMES as u64,
                "frame index must advance by exactly one packet"
            );
        }
    }

    /// While nobody is listening the clock must keep advancing, so a receiver
    /// that joins later is not told it missed a huge run of audio.
    #[test]
    fn discarding_keeps_the_clock_advancing() {
        let mut sending = Packetizer::new(WireFormat::F32, 48_000, 9);
        let mut idle = Packetizer::new(WireFormat::F32, 48_000, 9);

        for _ in 0..4 {
            let data = vec![0.1f32; 2048 * 2];
            sending.push_device_audio(&data, 2, 1.0);
            idle.push_device_audio(&data, 2, 1.0);
            collect(&mut sending);
            idle.discard();
        }

        assert_eq!(sending.seq, idle.seq);
        assert_eq!(sending.frame_index, idle.frame_index);
    }

    /// A mono input device must come out as stereo, not half-speed audio.
    #[test]
    fn duplicates_mono_input() {
        let mut p = Packetizer::new(WireFormat::F32, 48_000, 9);
        let data: Vec<f32> = (0..WIRE_FRAMES).map(|i| i as f32 / 1000.0).collect();
        p.push_device_audio(&data, 1, 1.0);

        let packets = collect(&mut p);
        assert_eq!(packets.len(), 1);
        let (_, payload) = AudioHeader::parse(&packets[0]).unwrap();
        let mut samples = Vec::new();
        crate::protocol::decode_samples(WireFormat::F32, payload, &mut samples);
        assert_eq!(samples.len(), WIRE_FRAMES * 2);
        for f in 0..WIRE_FRAMES {
            assert_eq!(samples[f * 2], samples[f * 2 + 1], "channels should match");
            assert_eq!(samples[f * 2], f as f32 / 1000.0);
        }
    }

    /// A device offering more than two channels should contribute its first
    /// pair, not an interleaving mistake.
    #[test]
    fn takes_the_first_pair_of_a_wide_device() {
        let mut p = Packetizer::new(WireFormat::F32, 48_000, 9);
        // Four channels: 0.1 / 0.2 are the pair we want, the rest is noise.
        let mut data = Vec::new();
        for _ in 0..WIRE_FRAMES {
            data.extend_from_slice(&[0.1f32, 0.2, 0.9, -0.9]);
        }
        p.push_device_audio(&data, 4, 1.0);

        let packets = collect(&mut p);
        let (_, payload) = AudioHeader::parse(&packets[0]).unwrap();
        let mut samples = Vec::new();
        crate::protocol::decode_samples(WireFormat::F32, payload, &mut samples);
        for f in 0..WIRE_FRAMES {
            assert!((samples[f * 2] - 0.1).abs() < 1e-6);
            assert!((samples[f * 2 + 1] - 0.2).abs() < 1e-6);
        }
    }

    /// The 16-bit wire format should halve packet size, and still fit an MTU.
    #[test]
    fn i16_format_halves_packet_size() {
        let mut p = Packetizer::new(WireFormat::I16, 48_000, 9);
        let data = vec![0.5f32; WIRE_FRAMES * 2];
        p.push_device_audio(&data, 2, 1.0);
        let packets = collect(&mut p);
        assert_eq!(packets[0].len(), AUDIO_HEADER + WIRE_FRAMES * 2 * 2);
        assert!(packets[0].len() <= 1472);
    }

    /// Gain is applied on the way out, and the meter reports what was sent.
    #[test]
    fn applies_gain_and_reports_peak() {
        let mut p = Packetizer::new(WireFormat::F32, 48_000, 9);
        let data = vec![0.4f32; WIRE_FRAMES * 2];
        let peak = p.push_device_audio(&data, 2, 0.5);
        assert!((peak - 0.2).abs() < 1e-6, "peak was {peak}");

        let packets = collect(&mut p);
        let (_, payload) = AudioHeader::parse(&packets[0]).unwrap();
        let mut samples = Vec::new();
        crate::protocol::decode_samples(WireFormat::F32, payload, &mut samples);
        assert!(samples.iter().all(|s| (*s - 0.2).abs() < 1e-6));
    }

    /// Nothing should be emitted until a whole packet is available.
    #[test]
    fn emits_nothing_for_a_partial_packet() {
        let mut p = Packetizer::new(WireFormat::F32, 48_000, 9);
        let data = vec![0.5f32; (WIRE_FRAMES - 1) * 2];
        p.push_device_audio(&data, 2, 1.0);
        assert!(collect(&mut p).is_empty());
        assert_eq!(p.seq, 0);
    }
}
