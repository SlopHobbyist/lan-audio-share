//! Fractional resampler used for both sample-rate conversion and clock-drift
//! correction.
//!
//! Two machines both running at "48 kHz" are not running at the same 48 kHz.
//! Their crystals differ by tens of parts per million, so over minutes the
//! receiver either runs out of audio or accumulates an ever-growing backlog.
//! Correcting that is what separates a stream that plays for hours from one that
//! glitches every few minutes, and it is the main idea worth borrowing from
//! SonoBus.
//!
//! The fix is to play back at a continuously nudged rate rather than dropping or
//! duplicating whole samples, which would be audible. The same machinery
//! converts between genuinely different sample rates for free, so a 44.1 kHz
//! device on one end and 48 kHz on the other needs no extra code.

/// Catmull-Rom interpolation between `p1` and `p2` at `t` in 0.0..1.0, using
/// `p0` and `p3` as slope context. Cheap, and noticeably cleaner than linear
/// interpolation at the tiny ratio offsets used for drift correction.
#[inline]
fn catmull_rom(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    let a = 2.0 * p1;
    let b = p2 - p0;
    let c = 2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3;
    let d = -p0 + 3.0 * p1 - 3.0 * p2 + p3;
    0.5 * (a + t * (b + t * (c + t * d)))
}

pub struct DriftResampler {
    channels: usize,
    /// The four most recent input frames, interleaved. Interpolation happens
    /// between the middle two.
    hist: Vec<f32>,
    /// Fractional read position between `hist[1]` and `hist[2]`.
    pos: f64,
}

impl DriftResampler {
    pub fn new(channels: usize) -> Self {
        Self {
            channels,
            hist: vec![0.0; 4 * channels],
            pos: 0.0,
        }
    }

    pub fn reset(&mut self) {
        self.hist.fill(0.0);
        self.pos = 0.0;
    }

    /// Resample interleaved `input` into interleaved `out` at `ratio` input
    /// frames per output frame.
    ///
    /// Returns `(input frames consumed, output frames produced)`. Production
    /// stops early when the input runs dry, which is how the caller detects an
    /// underrun; nothing is lost, since unconsumed input stays with the caller.
    ///
    /// The history starts as silence, so the very first frames of a stream
    /// interpolate up from zero. That is a sub-millisecond fade-in, which is
    /// preferable to a click anyway.
    pub fn process(&mut self, input: &[f32], out: &mut [f32], ratio: f64) -> (usize, usize) {
        let ch = self.channels;
        let in_frames = input.len() / ch;
        let out_frames = out.len() / ch;
        let mut consumed = 0usize;
        let mut produced = 0usize;

        while produced < out_frames {
            // Pull input frames until the read position falls inside the
            // interpolation window again.
            while self.pos >= 1.0 {
                if consumed >= in_frames {
                    return (consumed, produced);
                }
                self.hist.copy_within(ch.., 0);
                let base = consumed * ch;
                self.hist[3 * ch..].copy_from_slice(&input[base..base + ch]);
                consumed += 1;
                self.pos -= 1.0;
            }

            let t = self.pos as f32;
            let o = produced * ch;
            for c in 0..ch {
                out[o + c] = catmull_rom(
                    self.hist[c],
                    self.hist[ch + c],
                    self.hist[2 * ch + c],
                    self.hist[3 * ch + c],
                    t,
                );
            }
            produced += 1;
            self.pos += ratio;
        }

        (consumed, produced)
    }
}

/// Closed-loop controller that holds the receive buffer at a target depth by
/// trimming the playback rate.
///
/// The correction is deliberately tiny and heavily smoothed: it only has to
/// cancel clock drift (tens of ppm) plus slow changes in network delay, and a
/// fast or large correction would be an audible pitch wobble.
pub struct DriftController {
    /// Heavily smoothed buffer depth in frames.
    smoothed_fill: f64,
    initialized: bool,
    /// Last correction actually applied, as a fraction (so 1e-4 is 100 ppm).
    correction: f64,
}

/// Hard ceiling on the rate trim. 0.4% is far more than clock drift needs and
/// stays under the threshold where a pitch shift becomes noticeable.
const MAX_CORRECTION: f64 = 0.004;
/// Proportional gain from relative fill error to rate correction.
const GAIN: f64 = 0.05;
/// Smoothing factor applied per audio callback.
const SMOOTHING: f64 = 0.02;

impl DriftController {
    pub fn new() -> Self {
        Self {
            smoothed_fill: 0.0,
            initialized: false,
            correction: 0.0,
        }
    }

    pub fn reset(&mut self) {
        self.initialized = false;
        self.correction = 0.0;
    }

    /// Feed the current buffer depth and desired depth (both in frames), and get
    /// back the ratio to multiply the base resampling ratio by.
    pub fn update(&mut self, fill_frames: f64, target_frames: f64) -> f64 {
        if !self.initialized {
            self.smoothed_fill = fill_frames;
            self.initialized = true;
        } else {
            self.smoothed_fill += (fill_frames - self.smoothed_fill) * SMOOTHING;
        }

        if target_frames <= 0.0 {
            return 1.0;
        }

        // Buffer too full means we are playing too slowly, so read faster.
        let error = (self.smoothed_fill - target_frames) / target_frames;
        self.correction = (GAIN * error).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        1.0 + self.correction
    }

    /// Current correction in parts per million, for display.
    pub fn drift_ppm(&self) -> f32 {
        (self.correction * 1.0e6) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ratio of exactly 1.0 should pass audio through essentially unchanged
    /// (after the interpolator has filled its history).
    #[test]
    fn unity_ratio_is_transparent() {
        let mut rs = DriftResampler::new(1);
        let input: Vec<f32> = (0..256).map(|i| (i as f32 * 0.05).sin()).collect();
        let mut out = vec![0.0f32; 256];
        let (consumed, produced) = rs.process(&input, &mut out, 1.0);
        assert_eq!(produced, 256);
        assert!(consumed <= 256);
        // Past the start-up window the output should track the input closely.
        for i in 8..200 {
            assert!(
                (out[i] - input[i - 2]).abs() < 0.05,
                "sample {i}: {} vs {}",
                out[i],
                input[i - 2]
            );
        }
    }

    /// Stops cleanly when input runs out instead of producing garbage.
    #[test]
    fn reports_short_input() {
        let mut rs = DriftResampler::new(2);
        let input = vec![0.5f32; 20 * 2];
        let mut out = vec![0.0f32; 100 * 2];
        let (consumed, produced) = rs.process(&input, &mut out, 1.0);
        assert_eq!(consumed, 20);
        assert!(produced < 100);
    }

    /// Channels must not bleed into each other.
    #[test]
    fn keeps_channels_separate() {
        let mut rs = DriftResampler::new(2);
        let mut input = Vec::new();
        for _ in 0..64 {
            input.push(1.0f32);
            input.push(-1.0f32);
        }
        let mut out = vec![0.0f32; 64 * 2];
        rs.process(&input, &mut out, 1.0);
        // Once primed, left stays at +1 and right at -1.
        for f in 8..60 {
            assert!((out[f * 2] - 1.0).abs() < 0.01);
            assert!((out[f * 2 + 1] + 1.0).abs() < 0.01);
        }
    }

    /// An over-full buffer must speed playback up, and a thin one slow it down.
    #[test]
    fn controller_pushes_fill_toward_target() {
        let mut c = DriftController::new();
        let mut ratio = 1.0;
        for _ in 0..500 {
            ratio = c.update(4000.0, 2000.0);
        }
        assert!(ratio > 1.0, "over-full buffer should read faster: {ratio}");

        let mut c = DriftController::new();
        let mut ratio = 1.0;
        for _ in 0..500 {
            ratio = c.update(500.0, 2000.0);
        }
        assert!(ratio < 1.0, "thin buffer should read slower: {ratio}");
    }

    /// The correction must stay small enough to be inaudible.
    #[test]
    fn controller_clamps_correction() {
        let mut c = DriftController::new();
        let mut ratio = 1.0;
        for _ in 0..10_000 {
            ratio = c.update(1_000_000.0, 100.0);
        }
        assert!(ratio <= 1.0 + MAX_CORRECTION + 1e-9, "unclamped: {ratio}");
    }
}
