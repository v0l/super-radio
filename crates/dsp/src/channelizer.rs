//! Polyphase filter bank channelizer.
//!
//! This is the reason super-radio can decode dozens of signals at once. A naive
//! design would run one mixer, one lowpass and one decimator per channel, so
//! cost grows linearly with the number of channels. The polyphase bank splits
//! one prototype lowpass into `M` branches and follows it with a single
//! `M`-point FFT, producing all `M` channels for roughly the cost of one
//! filter plus an FFT. Going from 8 channels to 512 costs about `log M`, not
//! `M`.
//!
//! # Layout
//!
//! With `M` channels the bank is run 2x oversampled: it advances `M/2` input
//! samples per output frame, so each channel comes out at `2 * fs / M` while
//! occupying only `fs / M` of spectrum. That headroom matters. A critically
//! sampled bank aliases anything near a channel edge, which for blind
//! detection is fatal, since signals do not politely centre themselves on the
//! bin grid. Paying 2x in output samples buys the ability to receive a signal
//! that straddles a boundary.
//!
//! # Channel ordering
//!
//! Output index `m` is the FFT bin, so channels come out in FFT order: bins
//! `0..M/2` are baseband offsets `0..+fs/2`, and bins `M/2..M` are the negative
//! half. Use [`Channelizer::channel_offset_hz`] rather than assuming.
//!
//! # Derivation of the phase correction
//!
//! With the history buffer indexed newest-first, branch `k` computes
//! `b[k] = sum_t x[n0 - k - tM] h[k + tM]`. Taking the inverse DFT gives
//! `y[m] = sum_n x[n0-n] h[n] e^{j2*pi*n*m/M}`, while the wanted channel
//! output is `z[m] = e^{-j2*pi*m*n0/M} * sum_n x[n0-n] h[n] e^{j2*pi*n*m/M}`.
//! So `y[m] = z[m] * e^{+j2*pi*m*n0/M}` and the bank must undo that spinner.
//! Because `n0` advances by `M/2` per frame the correction collapses to
//! `(-1)^m` on odd frames, which is a sign flip rather than a complex
//! multiply.

use common::C32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

use crate::fir::pfb_prototype;

/// Output of one channelizer frame: `channels` complex samples, one per
/// channel, all from the same instant in time.
pub struct Frame<'a> {
    pub samples: &'a [C32],
}

pub struct Channelizer {
    channels: usize,
    taps_per_branch: usize,
    /// Prototype filter, length `channels * taps_per_branch`.
    proto: Vec<f32>,
    /// Doubled history buffer. The most recent `len` samples in time order
    /// (oldest first) live at `hist[pos .. pos + len]`.
    hist: Vec<C32>,
    pos: usize,
    /// Input samples accumulated toward the next frame.
    fill: usize,
    fft: Arc<dyn Fft<f32>>,
    scratch: Vec<C32>,
    /// Frame parity, driving the (-1)^m phase correction.
    odd_frame: bool,
    /// Reusable output staging so `process` allocates nothing per frame.
    frame: Vec<C32>,
}

impl Channelizer {
    /// `channels` must be even (the 2x oversampling advances by `channels/2`)
    /// and is best a power of two so the FFT is cheap.
    ///
    /// `taps_per_branch` controls channel-to-channel isolation. 12 gives around
    /// 90 dB of stopband with a Kaiser prototype, which is enough to stop a
    /// strong pager transmitter from painting false detections across the band.
    pub fn new(channels: usize, taps_per_branch: usize, atten_db: f64) -> Self {
        assert!(channels >= 2 && channels % 2 == 0, "channels must be even and >= 2");
        assert!(taps_per_branch >= 2, "need at least 2 taps per branch");

        let len = channels * taps_per_branch;
        let proto = pfb_prototype(channels, taps_per_branch, atten_db);
        let fft = FftPlanner::new().plan_fft_inverse(channels);
        let scratch = vec![C32::new(0.0, 0.0); fft.get_inplace_scratch_len()];

        Self {
            channels,
            taps_per_branch,
            proto,
            hist: vec![C32::new(0.0, 0.0); len * 2],
            pos: 0,
            fill: 0,
            fft,
            scratch,
            odd_frame: false,
            frame: vec![C32::new(0.0, 0.0); channels],
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Input samples consumed per output frame.
    pub fn advance(&self) -> usize {
        self.channels / 2
    }

    /// Output sample rate of every channel, given the input rate.
    pub fn channel_rate(&self, input_rate: f64) -> f64 {
        input_rate * 2.0 / self.channels as f64
    }

    /// Spectral width actually occupied by one channel.
    pub fn channel_bandwidth(&self, input_rate: f64) -> f64 {
        input_rate / self.channels as f64
    }

    /// Baseband offset of channel `m` from the tuned centre, in hertz.
    /// Bins above `M/2` map to negative offsets.
    pub fn channel_offset_hz(&self, m: usize, input_rate: f64) -> f64 {
        let half = self.channels / 2;
        let signed = if m < half { m as i64 } else { m as i64 - self.channels as i64 };
        signed as f64 * input_rate / self.channels as f64
    }

    /// Inverse of [`Self::channel_offset_hz`]: the channel whose centre is
    /// nearest a given baseband offset.
    pub fn channel_for_offset(&self, offset_hz: f64, input_rate: f64) -> usize {
        let spacing = input_rate / self.channels as f64;
        let m = (offset_hz / spacing).round() as i64;
        m.rem_euclid(self.channels as i64) as usize
    }

    /// Total group delay through the prototype filter, in input samples.
    pub fn latency_samples(&self) -> usize {
        self.channels * self.taps_per_branch / 2
    }

    pub fn reset(&mut self) {
        self.hist.fill(C32::new(0.0, 0.0));
        self.pos = 0;
        self.fill = 0;
        self.odd_frame = false;
    }

    /// Push input samples, invoking `on_frame` once per completed frame.
    ///
    /// The closure form avoids materialising an interleaved output buffer that
    /// the caller would immediately have to de-interleave; at 100 MS/s that
    /// copy alone would cost more than the filtering.
    pub fn process<F: FnMut(Frame<'_>)>(&mut self, input: &[C32], mut on_frame: F) {
        let len = self.channels * self.taps_per_branch;
        let advance = self.channels / 2;

        for &x in input {
            if self.pos + len == self.hist.len() {
                self.hist.copy_within(self.pos..self.pos + len, 0);
                self.pos = 0;
            }
            self.hist[self.pos + len] = x;
            self.pos += 1;
            self.fill += 1;

            if self.fill == advance {
                self.fill = 0;
                self.compute_frame();
                on_frame(Frame { samples: &self.frame });
            }
        }
    }

    fn compute_frame(&mut self) {
        let m = self.channels;
        let t = self.taps_per_branch;
        let len = m * t;
        // Newest sample sits at `pos + len - 1`, and index k walks backwards.
        let base = self.pos + len - 1;

        for k in 0..m {
            let mut acc = C32::new(0.0, 0.0);
            let mut idx = base - k;
            let mut tap = k;
            for _ in 0..t {
                acc += self.hist[idx] * self.proto[tap];
                // Guard against underflow on the last branch of the last tap.
                idx = idx.wrapping_sub(m);
                tap += m;
            }
            self.frame[k] = acc;
        }

        self.fft.process_with_scratch(&mut self.frame, &mut self.scratch);

        if self.odd_frame {
            // Undo the e^{+j*pi*m} spinner accumulated by advancing M/2 samples.
            for c in self.frame.iter_mut().skip(1).step_by(2) {
                *c = -*c;
            }
        }
        self.odd_frame = !self.odd_frame;

        let norm = 1.0 / m as f32;
        for c in &mut self.frame {
            *c *= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Phase must be reduced modulo one cycle in f64 *before* narrowing to
    /// f32. Accumulating `TAU * f * i` naively overruns f32's mantissa by
    /// i ~ 100k and injects about -60 dB of phase noise, which sits well above
    /// the bank's real leakage floor and would mask genuine regressions.
    fn tone(n: usize, cycles_per_sample: f64, amp: f32) -> Vec<C32> {
        (0..n)
            .map(|i| {
                let frac = (cycles_per_sample * i as f64).rem_euclid(1.0);
                let p = (frac * std::f64::consts::TAU) as f32;
                C32::new(amp * p.cos(), amp * p.sin())
            })
            .collect()
    }

    /// Run a signal through and return mean power per channel, skipping the
    /// filter's settling transient.
    fn channel_powers(ch: &mut Channelizer, sig: &[C32], skip: usize) -> Vec<f32> {
        let m = ch.channels();
        let mut acc = vec![0.0f64; m];
        let mut frames = 0usize;
        ch.process(sig, |f| {
            frames += 1;
            if frames > skip {
                for (a, s) in acc.iter_mut().zip(f.samples) {
                    *a += s.norm_sqr() as f64;
                }
            }
        });
        let n = (frames.saturating_sub(skip)).max(1) as f64;
        acc.into_iter().map(|v| (v / n) as f32).collect()
    }

    #[test]
    fn tone_lands_in_the_expected_channel() {
        let m = 16;
        let mut ch = Channelizer::new(m, 12, 90.0);
        // Channel 3 centre is 3/16 cycles per sample.
        let sig = tone(1 << 16, 3.0 / m as f64, 1.0);
        let p = channel_powers(&mut ch, &sig, 64);

        let best = (0..m).max_by(|&a, &b| p[a].total_cmp(&p[b])).unwrap();
        assert_eq!(best, 3, "energy landed in channel {best}, powers {p:?}");

        // Unity amplitude in, unity amplitude out of its own channel.
        assert!((p[3].sqrt() - 1.0).abs() < 0.02, "gain was {}", p[3].sqrt());
    }

    #[test]
    fn negative_frequencies_map_to_upper_bins() {
        let m = 16;
        let mut ch = Channelizer::new(m, 12, 90.0);
        let sig = tone(1 << 16, -5.0 / m as f64, 1.0);
        let p = channel_powers(&mut ch, &sig, 64);
        let best = (0..m).max_by(|&a, &b| p[a].total_cmp(&p[b])).unwrap();
        assert_eq!(best, m - 5);
        assert!(ch.channel_offset_hz(m - 5, 16.0) < 0.0);
    }

    #[test]
    fn adjacent_channel_rejection_is_deep() {
        let m = 32;
        let mut ch = Channelizer::new(m, 12, 100.0);
        let sig = tone(1 << 17, 8.0 / m as f64, 1.0);
        let p = channel_powers(&mut ch, &sig, 128);

        let leak_db = 10.0 * (p[10] / p[8]).log10();
        assert!(leak_db < -80.0, "channel 8 leaked {leak_db} dB into channel 10");
    }

    #[test]
    fn edge_tone_is_recoverable_thanks_to_oversampling() {
        // A tone exactly on the boundary between channels 4 and 5. In a
        // critically sampled bank this aliases; here both neighbours should
        // still see roughly half the power and the sum should be preserved.
        let m = 16;
        let mut ch = Channelizer::new(m, 12, 90.0);
        let sig = tone(1 << 16, 4.5 / m as f64, 1.0);
        let p = channel_powers(&mut ch, &sig, 64);
        assert!(p[4] > 0.1 && p[5] > 0.1, "edge tone vanished: {:?}", &p[3..7]);
    }

    #[test]
    fn offset_and_channel_lookups_are_inverses() {
        let ch = Channelizer::new(64, 8, 80.0);
        let rate = 20e6;
        for m in 0..64 {
            let off = ch.channel_offset_hz(m, rate);
            assert_eq!(ch.channel_for_offset(off, rate), m);
        }
    }

    #[test]
    fn frame_count_matches_advance_rate() {
        let mut ch = Channelizer::new(8, 6, 80.0);
        let mut frames = 0;
        ch.process(&tone(4000, 0.1, 1.0), |_| frames += 1);
        assert_eq!(frames, 4000 / 4);
    }

    #[test]
    fn split_input_gives_identical_output() {
        let sig = tone(8192, 3.0 / 16.0, 1.0);

        let mut a = Channelizer::new(16, 8, 80.0);
        let mut whole = Vec::new();
        a.process(&sig, |f| whole.extend_from_slice(f.samples));

        let mut b = Channelizer::new(16, 8, 80.0);
        let mut split = Vec::new();
        for c in sig.chunks(101) {
            b.process(c, |f| split.extend_from_slice(f.samples));
        }
        assert_eq!(whole.len(), split.len());
        for (x, y) in whole.iter().zip(&split) {
            assert!((x - y).norm() < 1e-6);
        }
    }
}
