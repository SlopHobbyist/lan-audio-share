//! RECEIVE mode: take packets off the network into a jitter buffer, then play
//! that buffer out at a continuously corrected rate.
//!
//! The buffer is what absorbs network jitter and the sender's bursty callback
//! timing; the rate correction is what stops the buffer from slowly draining or
//! overflowing as the two machines' clocks diverge. Both are needed — a buffer
//! alone eventually underruns, and rate correction alone has nothing to measure.

use crate::config::Config;
use crate::devices::{self, Direction};
use crate::net::{self, NetThreads};
use crate::protocol::{AudioHeader, MAX_PACKET, decode_samples};
use crate::resample::{DriftController, DriftResampler};
use crate::stats::Stats;
use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{BufferSize, Device, FromSample, Sample, SampleFormat, SizedSample, StreamConfig};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Channels held in the jitter buffer. The network thread normalises whatever
/// arrives to this, so the audio callback only ever maps stereo to the device.
const BUF_CHANNELS: usize = 2;

/// Jitter buffer capacity, in seconds of audio. Generous: it costs a megabyte
/// or so and removes any chance of the network thread stalling on a full ring.
const BUFFER_SECONDS: usize = 2;

/// A gap larger than this is treated as the stream having broken rather than a
/// run of lost packets worth concealing.
const MAX_CONCEAL_MS: u64 = 400;

/// Ceiling on how far the buffer target may auto-grow after underruns.
const MAX_TARGET_MS: f64 = 400.0;

pub struct RecvRole {
    stream: cpal::Stream,
    _beacon: NetThreads,
    _rx: NetThreads,
    pub device_name: String,
    pub rate: u32,
    pub device_channels: u16,
    pub audio_port: u16,
}

impl RecvRole {
    pub fn pause(&self) {
        let _ = self.stream.pause();
    }
}

/// Everything the output callback needs, with the device-specific channel
/// mapping left out so the timing-critical logic can be tested on its own.
struct Playback {
    consumer: HeapCons<f32>,
    stats: Arc<Stats>,
    resampler: DriftResampler,
    controller: DriftController,
    /// Input handed to the resampler; keeps whatever it did not consume.
    stage: Vec<f32>,
    /// Rendered stereo output for the current callback.
    scratch: Vec<f32>,
    /// False while refilling the cushion, during which we output silence.
    playing: bool,
    device_rate: u32,
    floor_target_ms: f64,
    target_ms: f64,
}

impl Playback {
    fn new(
        consumer: HeapCons<f32>,
        stats: Arc<Stats>,
        floor_target_ms: f64,
        device_rate: u32,
    ) -> Self {
        Self {
            consumer,
            stats,
            resampler: DriftResampler::new(BUF_CHANNELS),
            controller: DriftController::new(),
            stage: Vec::with_capacity(8192),
            scratch: Vec::with_capacity(8192),
            playing: false,
            device_rate,
            floor_target_ms,
            target_ms: floor_target_ms,
        }
    }

    /// Buffered audio ahead of the playhead, in source frames.
    fn fill_frames(&self) -> f64 {
        ((self.consumer.occupied_len() + self.stage.len()) / BUF_CHANNELS) as f64
    }

    /// Render exactly `out_frames` frames of stereo audio, zero-filled wherever
    /// there is nothing to play.
    fn render(&mut self, out_frames: usize) -> &[f32] {
        self.scratch.clear();
        self.scratch.resize(out_frames * BUF_CHANNELS, 0.0);

        if self.stats.resync.swap(false, Ordering::Relaxed) {
            self.consumer.clear();
            self.stage.clear();
            self.resampler.reset();
            self.controller.reset();
            self.playing = false;
            self.target_ms = self.floor_target_ms;
        }

        let source_rate = self.stats.source_rate.load(Ordering::Relaxed);
        if source_rate == 0 {
            self.stats.set_buffer_ms(0.0);
            return &self.scratch;
        }

        let target_frames = self.target_ms * source_rate as f64 / 1000.0;
        let mut fill_frames = self.fill_frames();

        // Wait for the buffer to reach its target before starting, so playback
        // begins with enough slack to ride out jitter.
        if !self.playing {
            self.stats
                .set_buffer_ms((fill_frames * 1000.0 / source_rate as f64) as f32);
            if fill_frames < target_frames {
                return &self.scratch;
            }
            self.playing = true;
            self.controller.reset();
        }

        // A backlog this far past target means something stalled — typically the
        // device callback not having started yet while packets piled up. Drop
        // straight back to target rather than waiting minutes for the rate
        // correction to walk it down.
        if fill_frames > target_frames * 3.0 {
            let excess = (fill_frames - target_frames) as usize * BUF_CHANNELS;
            self.consumer.skip(excess);
            // Re-measure before the controller sees it. Feeding the controller
            // the pre-trim depth would peg the correction at its limit, and the
            // heavy smoothing would hold it there long after the trim.
            fill_frames = self.fill_frames();
            self.controller.reset();
        }

        self.stats
            .set_buffer_ms((fill_frames * 1000.0 / source_rate as f64) as f32);

        let ratio = (source_rate as f64 / self.device_rate as f64)
            * self.controller.update(fill_frames, target_frames);
        self.stats
            .drift_ppm
            .store(self.controller.drift_ppm() as i32, Ordering::Relaxed);

        // Top the staging buffer up to what the resampler could need.
        let need_samples = ((out_frames as f64 * ratio).ceil() as usize + 4) * BUF_CHANNELS;
        if self.stage.len() < need_samples {
            let start = self.stage.len();
            self.stage.resize(need_samples, 0.0);
            let got = self.consumer.pop_slice(&mut self.stage[start..]);
            self.stage.truncate(start + got);
        }

        let (consumed, produced) = self
            .resampler
            .process(&self.stage, &mut self.scratch, ratio);
        self.stage.drain(..consumed * BUF_CHANNELS);

        if produced < out_frames {
            // Ran dry. The rest of this buffer plays as silence, then the
            // cushion is rebuilt — and the target rises, since the configured
            // one has been shown to be too thin for this link.
            self.stats.underruns.fetch_add(1, Ordering::Relaxed);
            self.playing = false;
            self.target_ms = (self.target_ms * 1.25).min(MAX_TARGET_MS);
        }

        &self.scratch
    }
}

pub fn start(config: &Config, stats: Arc<Stats>) -> Result<RecvRole> {
    let selection = config
        .recv_device
        .clone()
        .or_else(|| devices::default_entry(Direction::Output).map(|e| e.selection()))
        .context("no output device selected and no system default available")?;

    let device = devices::resolve(Direction::Output, &selection)?;
    let supported = devices::choose_config(
        &device,
        Direction::Output,
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

    let socket = net::audio_socket(config.audio_port)?;
    let audio_port = socket
        .local_addr()
        .map(|a| a.port())
        .unwrap_or(config.audio_port);

    let capacity = rate as usize * BUF_CHANNELS * BUFFER_SECONDS;
    let (producer, consumer) = HeapRb::<f32>::new(capacity).split();

    let beacon_socket = socket
        .try_clone()
        .context("could not share the audio socket")?;
    let rx = spawn_receive_thread(socket, producer, stats.clone())?;
    let beacon = net::spawn_receiver_beacon(
        stats.clone(),
        beacon_socket,
        config.sample_rate,
        config.channels() as u8,
    )?;

    let playback = Playback::new(consumer, stats.clone(), config.jitter_ms as f64, rate);

    let stream = match sample_format {
        SampleFormat::F32 => build::<f32>(
            &device,
            &stream_config,
            device_channels as usize,
            playback,
            stats.clone(),
        ),
        SampleFormat::I16 => build::<i16>(
            &device,
            &stream_config,
            device_channels as usize,
            playback,
            stats.clone(),
        ),
        SampleFormat::I32 => build::<i32>(
            &device,
            &stream_config,
            device_channels as usize,
            playback,
            stats.clone(),
        ),
        SampleFormat::U16 => build::<u16>(
            &device,
            &stream_config,
            device_channels as usize,
            playback,
            stats.clone(),
        ),
        other => anyhow::bail!("unsupported output sample format {other:?}"),
    }?;

    stream.play().context("could not start the output stream")?;

    Ok(RecvRole {
        device_name: devices::device_name(&device),
        stream,
        _beacon: beacon,
        _rx: rx,
        rate,
        device_channels,
        audio_port,
    })
}

/// Pull packets off the socket, conceal gaps, and feed the jitter buffer.
fn spawn_receive_thread(
    socket: std::net::UdpSocket,
    mut producer: HeapProd<f32>,
    stats: Arc<Stats>,
) -> Result<NetThreads> {
    socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .context("could not configure the audio socket")?;

    net::spawn_stoppable("audio-rx", move |stop| {
        let mut buf = [0u8; MAX_PACKET];
        let mut decoded: Vec<f32> = Vec::with_capacity(2048);
        let mut silence: Vec<f32> = Vec::new();

        let mut stream_id: Option<u16> = None;
        let mut expected_seq: u32 = 0;
        let mut next_frame: u64 = 0;
        let mut have_position = false;

        while !stop.load(Ordering::Relaxed) {
            let Ok((len, from)) = socket.recv_from(&mut buf) else {
                continue;
            };
            let Some((header, payload)) = AudioHeader::parse(&buf[..len]) else {
                continue;
            };

            // A new stream id, or a rate change, means the far end restarted.
            // Anything already buffered is stale, so ask the callback to flush.
            let rate_changed = stats.source_rate.load(Ordering::Relaxed) != header.sample_rate;
            if stream_id != Some(header.stream_id) || rate_changed {
                stream_id = Some(header.stream_id);
                have_position = false;
                stats
                    .source_rate
                    .store(header.sample_rate, Ordering::Relaxed);
                stats
                    .source_channels
                    .store(header.channels as u32, Ordering::Relaxed);
                stats.resync.store(true, Ordering::Relaxed);
                if let Ok(mut guard) = stats.remote_name.lock()
                    && guard.is_empty()
                {
                    *guard = from.ip().to_string();
                }
            }

            if have_position {
                // Reordering is rare on a LAN and a sample ring cannot accept
                // out-of-order inserts, so a late packet is dropped and counted.
                // If this counter ever moves, the network is the thing to check.
                if header.seq.wrapping_sub(expected_seq) > u32::MAX / 2 {
                    stats.late_packets.fetch_add(1, Ordering::Relaxed);
                    continue;
                }

                if header.frame_index > next_frame {
                    let gap = header.frame_index - next_frame;
                    let max_gap = header.sample_rate as u64 * MAX_CONCEAL_MS / 1000;
                    if gap > max_gap {
                        // Too big to paper over. Flag a resync so the callback
                        // flushes and re-cushions, and skip concealment: the
                        // position is re-anchored from this packet below.
                        stats.resync.store(true, Ordering::Relaxed);
                    } else {
                        let samples = gap as usize * BUF_CHANNELS;
                        if silence.len() < samples {
                            silence.resize(samples, 0.0);
                        }
                        producer.push_slice(&silence[..samples]);
                        stats.lost_frames.fetch_add(gap, Ordering::Relaxed);
                    }
                }
            }

            have_position = true;
            expected_seq = header.seq.wrapping_add(1);
            next_frame = header.frame_index + header.frames as u64;

            decoded.clear();
            decode_samples(header.format, payload, &mut decoded);

            if header.channels as usize == BUF_CHANNELS {
                producer.push_slice(&decoded);
            } else {
                // Mono upmix: the ring is always stereo.
                for sample in &decoded {
                    let pair = [*sample, *sample];
                    producer.push_slice(&pair);
                }
            }

            stats.packets.fetch_add(1, Ordering::Relaxed);
            stats.bytes.fetch_add(len as u64, Ordering::Relaxed);
            stats.note_audio();
        }
    })
}

fn build<T>(
    device: &Device,
    stream_config: &StreamConfig,
    device_channels: usize,
    mut playback: Playback,
    stats: Arc<Stats>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32> + Sample,
{
    let error_stats = stats.clone();
    let stream = device
        .build_output_stream(
            *stream_config,
            move |out: &mut [T], _info| {
                let out_frames = out.len() / device_channels.max(1);
                let gain = stats.get_volume();
                let stereo = playback.render(out_frames);

                let mut peak = 0.0f32;
                for f in 0..out_frames {
                    let left = stereo[f * BUF_CHANNELS] * gain;
                    let right = stereo[f * BUF_CHANNELS + 1] * gain;
                    peak = peak.max(left.abs()).max(right.abs());

                    let base = f * device_channels;
                    if device_channels == 1 {
                        out[base] = T::from_sample_((left + right) * 0.5);
                        continue;
                    }
                    for c in 0..device_channels {
                        let value = match c {
                            0 => left,
                            1 => right,
                            // Leave surround/centre channels alone rather than
                            // duplicating stereo into them.
                            _ => 0.0,
                        };
                        out[base + c] = T::from_sample_(value);
                    }
                }
                stats.report_peak(peak);
            },
            move |err| {
                error_stats.set_error(format!("output device error: {err}"));
                error_stats.restart_requested.store(true, Ordering::Relaxed);
            },
            None,
        )
        .context("could not open the output device")?;

    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// Build a playback path wired to a producer we can feed by hand.
    fn harness(target_ms: f64) -> (HeapProd<f32>, Playback, Arc<Stats>) {
        let stats = Arc::new(Stats::new());
        stats.source_rate.store(RATE, Ordering::Relaxed);
        let (producer, consumer) = HeapRb::<f32>::new(RATE as usize * BUF_CHANNELS * 2).split();
        let playback = Playback::new(consumer, stats.clone(), target_ms, RATE);
        (producer, playback, stats)
    }

    fn feed(producer: &mut HeapProd<f32>, frames: usize, value: f32) {
        let block = vec![value; frames * BUF_CHANNELS];
        producer.push_slice(&block);
    }

    /// With nothing arriving, output must be silent rather than noise.
    #[test]
    fn silent_until_a_stream_appears() {
        let stats = Arc::new(Stats::new());
        let (_producer, consumer) = HeapRb::<f32>::new(1024).split();
        let mut playback = Playback::new(consumer, stats, 60.0, RATE);
        assert!(playback.render(512).iter().all(|s| *s == 0.0));
    }

    /// Playback must not start until the cushion is built, or the first thing
    /// the user hears is an underrun.
    #[test]
    fn waits_for_the_cushion_then_plays() {
        let (mut producer, mut playback, stats) = harness(60.0);
        // 60 ms at 48 kHz is 2880 frames; half of that is not enough.
        feed(&mut producer, 1440, 0.5);
        assert!(
            playback.render(512).iter().all(|s| *s == 0.0),
            "started playing before the buffer reached its target"
        );
        assert_eq!(stats.underruns.load(Ordering::Relaxed), 0);

        feed(&mut producer, 3000, 0.5);
        let out = playback.render(512).to_vec();
        assert!(
            out.iter().any(|s| s.abs() > 0.1),
            "buffer was full enough but nothing played"
        );
    }

    /// The whole point of the drift correction: a receiver whose clock runs
    /// slightly faster than the sender's must not drain the buffer dry.
    ///
    /// This feeds audio 150 ppm slower than it is consumed, which without
    /// correction empties any fixed cushion and glitches forever after.
    #[test]
    fn survives_a_mismatched_clock() {
        let (mut producer, mut playback, stats) = harness(60.0);
        let block = 512usize;

        // Build the initial cushion.
        feed(&mut producer, 4000, 0.25);
        playback.render(block);

        let mut owed = 0.0f64;
        for _ in 0..4000 {
            // Sender delivers 150 ppm fewer frames than we are about to play.
            owed += block as f64 * (1.0 - 150e-6);
            let whole = owed.floor() as usize;
            owed -= whole as f64;
            feed(&mut producer, whole, 0.25);
            playback.render(block);
        }

        assert_eq!(
            stats.underruns.load(Ordering::Relaxed),
            0,
            "drift correction failed to hold the buffer; ended at {} ms",
            stats.get_buffer_ms()
        );
        // And it should have settled near the target rather than drifting off.
        let settled = stats.get_buffer_ms();
        assert!(
            (20.0..=200.0).contains(&settled),
            "buffer settled at an unreasonable {settled} ms"
        );
        // The correction should be pulling playback slower to match the feed.
        let ppm = stats.drift_ppm.load(Ordering::Relaxed);
        assert!(ppm < 0, "expected a negative rate trim, got {ppm} ppm");
    }

    /// A stall long enough to empty the buffer must be reported, and must make
    /// the receiver ask for a deeper cushion next time.
    #[test]
    fn underrun_grows_the_target() {
        let (mut producer, mut playback, stats) = harness(40.0);
        feed(&mut producer, 4000, 0.5);
        playback.render(512);
        let target_before = playback.target_ms;

        // Drain far past what was buffered.
        for _ in 0..40 {
            playback.render(512);
        }

        assert!(stats.underruns.load(Ordering::Relaxed) > 0);
        assert!(
            playback.target_ms > target_before,
            "target did not grow after an underrun"
        );
    }

    /// A resync (the sender restarted) must throw away the stale audio instead
    /// of playing it out late.
    #[test]
    fn resync_discards_stale_audio() {
        let (mut producer, mut playback, stats) = harness(60.0);
        feed(&mut producer, 6000, 0.9);
        playback.render(512);

        stats.resync.store(true, Ordering::Relaxed);
        let out = playback.render(512).to_vec();
        assert!(
            out.iter().all(|s| *s == 0.0),
            "stale audio survived a resync"
        );
        assert_eq!(stats.get_buffer_ms(), 0.0);
    }

    /// Different sample rates on the two ends must be handled by the same
    /// resampler, not rejected.
    #[test]
    fn converts_between_sample_rates() {
        let stats = Arc::new(Stats::new());
        // Sender at 44.1 kHz, this device at 48 kHz.
        stats.source_rate.store(44_100, Ordering::Relaxed);
        let (mut producer, consumer) = HeapRb::<f32>::new(48_000 * BUF_CHANNELS * 2).split();
        let mut playback = Playback::new(consumer, stats.clone(), 60.0, 48_000);

        feed(&mut producer, 10_000, 0.4);
        playback.render(512);
        let out = playback.render(512).to_vec();

        assert!(
            out.iter().any(|s| (*s - 0.4).abs() < 0.05),
            "resampled output did not carry the input level"
        );
        assert_eq!(stats.underruns.load(Ordering::Relaxed), 0);
    }

    /// End-to-end over a real socket: encode packets the way the sender does,
    /// push them through the actual receive thread, and confirm the samples
    /// come out of the playback path intact.
    #[test]
    fn audio_flows_over_a_real_socket() {
        use crate::protocol::{AUDIO_HEADER, WIRE_FRAMES, WireFormat, encode_samples};

        let stats = Arc::new(Stats::new());
        let socket = net::audio_socket(0).expect("bind receive socket");
        let port = socket.local_addr().unwrap().port();
        let (producer, consumer) = HeapRb::<f32>::new(RATE as usize * BUF_CHANNELS * 2).split();
        let _rx = spawn_receive_thread(socket, producer, stats.clone()).expect("start rx thread");

        let tx = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind send socket");
        let dest = format!("127.0.0.1:{port}");
        let mut seq: u32 = 0;

        let send_batch = |count: u32, seq: &mut u32, level: f32| {
            for _ in 0..count {
                let mut packet = vec![0u8; AUDIO_HEADER];
                AudioHeader {
                    format: WireFormat::F32,
                    channels: 2,
                    sample_rate: RATE,
                    seq: *seq,
                    frame_index: *seq as u64 * WIRE_FRAMES as u64,
                    frames: WIRE_FRAMES as u16,
                    stream_id: 7,
                }
                .write(&mut packet);
                let samples = vec![level; WIRE_FRAMES * BUF_CHANNELS];
                encode_samples(WireFormat::F32, &samples, &mut packet);
                tx.send_to(&packet, &dest).expect("send packet");
                *seq += 1;
            }
        };

        let wait_for = |want: u64| {
            for _ in 0..200 {
                if stats.packets.load(Ordering::Relaxed) >= want {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        };

        // First packet of a stream always triggers a resync, so let the playback
        // path absorb that before measuring, exactly as happens in real use.
        send_batch(1, &mut seq, 0.5);
        assert!(wait_for(1), "no packets reached the receive thread");
        let mut playback = Playback::new(consumer, stats.clone(), 60.0, RATE);
        playback.render(512);

        // 60 packets of 160 frames is 200 ms, comfortably past the 60 ms target.
        send_batch(60, &mut seq, 0.5);
        assert!(wait_for(61), "packets stopped arriving");

        playback.render(512);
        let out = playback.render(512).to_vec();
        assert!(
            out.iter().any(|s| (*s - 0.5).abs() < 0.01),
            "audio did not survive the round trip"
        );
        assert_eq!(
            stats.source_rate.load(Ordering::Relaxed),
            RATE,
            "receiver did not learn the stream sample rate"
        );
        assert_eq!(stats.lost_frames.load(Ordering::Relaxed), 0, "false loss");
        assert_eq!(
            stats.late_packets.load(Ordering::Relaxed),
            0,
            "false lateness"
        );
    }

    /// A dropped packet must be concealed with exactly the right number of
    /// frames, so everything after it stays in sync rather than shifting early.
    #[test]
    fn conceals_a_dropped_packet_with_the_right_length() {
        use crate::protocol::{AUDIO_HEADER, WIRE_FRAMES, WireFormat, encode_samples};

        let stats = Arc::new(Stats::new());
        let socket = net::audio_socket(0).expect("bind receive socket");
        let port = socket.local_addr().unwrap().port();
        let (producer, _consumer) = HeapRb::<f32>::new(RATE as usize * BUF_CHANNELS * 2).split();
        let _rx = spawn_receive_thread(socket, producer, stats.clone()).expect("start rx thread");

        let tx = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind send socket");
        let dest = format!("127.0.0.1:{port}");

        // Send sequence 0 and 2: packet 1 never arrives.
        for seq in [0u32, 2u32] {
            let mut packet = vec![0u8; AUDIO_HEADER];
            AudioHeader {
                format: WireFormat::F32,
                channels: 2,
                sample_rate: RATE,
                seq,
                frame_index: seq as u64 * WIRE_FRAMES as u64,
                frames: WIRE_FRAMES as u16,
                stream_id: 11,
            }
            .write(&mut packet);
            let samples = vec![0.3f32; WIRE_FRAMES * BUF_CHANNELS];
            encode_samples(WireFormat::F32, &samples, &mut packet);
            tx.send_to(&packet, &dest).expect("send packet");
        }

        for _ in 0..200 {
            if stats.lost_frames.load(Ordering::Relaxed) > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            stats.lost_frames.load(Ordering::Relaxed),
            WIRE_FRAMES as u64,
            "concealment length did not match the gap"
        );
    }

    /// Opens the machine's real output device through the full RECEIVE path and
    /// streams live packets at it, the way the sender actually paces them.
    ///
    /// This is the check that a fixed 2048-frame buffer is genuinely accepted by
    /// the platform audio backend, and that the output callback runs and renders
    /// the audio rather than silence.
    #[test]
    fn receiver_opens_a_real_device_and_plays() {
        use crate::config::{Config, Mode};
        use crate::protocol::{AUDIO_HEADER, WIRE_FRAMES, WireFormat, encode_samples};

        let stats = Arc::new(Stats::new());
        let config = Config {
            mode: Mode::Receive,
            audio_port: 0,
            ..Config::default()
        };

        let role = match start(&config, stats.clone()) {
            Ok(role) => role,
            Err(err) => {
                eprintln!("skipping: no usable output device here ({err:#})");
                return;
            }
        };
        println!(
            "opened {:?} at {} Hz, {} ch, buffer {} frames",
            role.device_name, role.rate, role.device_channels, config.buffer_frames
        );

        let tx = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind send socket");
        let dest = format!("127.0.0.1:{}", role.audio_port);
        let mut seq: u32 = 0;

        // Feed at exactly real time, derived from a clock rather than from
        // sleeps: Windows sleep overshoot would otherwise skew the feed rate by
        // far more than clock drift ever does and make this test meaningless.
        // Sleep jitter still makes delivery bursty, which is realistic.
        let began = std::time::Instant::now();
        let run_for = Duration::from_millis(2500);
        let mut sent_frames: u64 = 0;
        while began.elapsed() < run_for {
            let due = (began.elapsed().as_secs_f64() * 48_000.0) as u64;
            while sent_frames < due {
                let mut packet = vec![0u8; AUDIO_HEADER];
                AudioHeader {
                    format: WireFormat::F32,
                    channels: 2,
                    sample_rate: 48_000,
                    seq,
                    frame_index: seq as u64 * WIRE_FRAMES as u64,
                    frames: WIRE_FRAMES as u16,
                    stream_id: 42,
                }
                .write(&mut packet);
                // A steady tone, so a rendered buffer is unmistakably not silence.
                let samples: Vec<f32> = (0..WIRE_FRAMES * BUF_CHANNELS)
                    .map(|i| {
                        let frame = seq as usize * WIRE_FRAMES + i / BUF_CHANNELS;
                        (frame as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.3
                    })
                    .collect();
                encode_samples(WireFormat::F32, &samples, &mut packet);
                tx.send_to(&packet, &dest).expect("send packet");
                seq += 1;
                sent_frames += WIRE_FRAMES as u64;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        let sent = seq as u64;
        let seen = stats.packets.load(Ordering::Relaxed);
        assert!(
            seen * 100 >= sent * 95,
            "receive thread only saw {seen} of {sent} packets"
        );
        assert_eq!(
            stats.source_rate.load(Ordering::Relaxed),
            48_000,
            "stream rate was never picked up"
        );

        // The output callback must actually have run and rendered the tone.
        println!(
            "buffer {:.0} ms, drift {} ppm, underruns {}, peak {:.3}",
            stats.get_buffer_ms(),
            stats.drift_ppm.load(Ordering::Relaxed),
            stats.underruns.load(Ordering::Relaxed),
            stats.get_peak()
        );
        assert!(
            stats.get_peak() > 0.01,
            "output callback never rendered audio (peak {})",
            stats.get_peak()
        );
        assert!(stats.get_buffer_ms() > 0.0, "jitter buffer never filled");
        // ~1.5 s of steady streaming should not glitch at all.
        assert!(
            stats.underruns.load(Ordering::Relaxed) <= 1,
            "{} underruns during steady streaming",
            stats.underruns.load(Ordering::Relaxed)
        );
    }
}
