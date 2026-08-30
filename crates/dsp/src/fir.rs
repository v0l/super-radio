//! FIR filter design and application.

use crate::window::{kaiser, kaiser_beta_for_atten};
use common::C32;

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// Windowed-sinc lowpass. `cutoff` is in cycles per sample (0..0.5), measured
/// to the -6 dB point. `taps` should be odd for a true linear-phase type-I
/// filter; an even count is bumped up by one.
pub fn lowpass(taps: usize, cutoff: f64, atten_db: f64) -> Vec<f32> {
    let n = if taps % 2 == 0 { taps + 1 } else { taps };
    let beta = kaiser_beta_for_atten(atten_db);
    let w = kaiser(n, beta);
    let mid = (n - 1) as f64 / 2.0;
    let mut h: Vec<f32> = (0..n)
        .map(|i| (2.0 * cutoff * sinc(2.0 * cutoff * (i as f64 - mid))) as f32 * w[i])
        .collect();
    // Normalise to unity DC gain so cascading filters does not change level.
    let dc: f32 = h.iter().sum();
    if dc.abs() > 1e-20 {
        for v in &mut h {
            *v /= dc;
        }
    }
    h
}

/// Number of taps needed for a given transition width, per Kaiser's estimate.
/// `transition` is in cycles per sample.
pub fn estimate_taps(transition: f64, atten_db: f64) -> usize {
    let n = ((atten_db - 8.0) / (2.285 * 2.0 * std::f64::consts::PI * transition)).ceil();
    (n.max(3.0) as usize) | 1
}

/// Prototype lowpass for an `channels`-path polyphase filter bank.
///
/// Length is forced to `channels * taps_per_branch` exactly, because the
/// polyphase decomposition requires every branch to hold the same tap count.
/// Cutoff sits at half a channel width so adjacent channels cross at -6 dB.
pub fn pfb_prototype(channels: usize, taps_per_branch: usize, atten_db: f64) -> Vec<f32> {
    let n = channels * taps_per_branch;
    let beta = kaiser_beta_for_atten(atten_db);
    let w = kaiser(n, beta);
    let cutoff = 0.5 / channels as f64;
    let mid = (n - 1) as f64 / 2.0;
    let mut h: Vec<f32> = (0..n)
        .map(|i| (2.0 * cutoff * sinc(2.0 * cutoff * (i as f64 - mid))) as f32 * w[i])
        .collect();
    // Scale so a full-scale tone lands at unity in its channel: each branch
    // sees 1/channels of the energy, and the DFT sums the branches back up.
    let dc: f32 = h.iter().sum();
    let g = channels as f32 / dc;
    for v in &mut h {
        *v *= g;
    }
    h
}

/// Direct-form FIR over complex samples with real taps, keeping state across
/// calls so block boundaries are seamless.
#[derive(Clone)]
pub struct Fir {
    taps: Vec<f32>,
    hist: Vec<C32>,
}

impl Fir {
    pub fn new(taps: Vec<f32>) -> Self {
        let n = taps.len();
        Self { taps, hist: vec![C32::new(0.0, 0.0); n] }
    }

    pub fn len(&self) -> usize {
        self.taps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.taps.is_empty()
    }

    pub fn reset(&mut self) {
        self.hist.fill(C32::new(0.0, 0.0));
    }

    /// Filter `input`, appending `input.len()` samples to `out`.
    pub fn process(&mut self, input: &[C32], out: &mut Vec<C32>) {
        out.reserve(input.len());
        let n = self.taps.len();
        for &x in input {
            self.hist.copy_within(0..n - 1, 1);
            self.hist[0] = x;
            let mut acc = C32::new(0.0, 0.0);
            for (h, s) in self.taps.iter().zip(self.hist.iter()) {
                acc += s * *h;
            }
            out.push(acc);
        }
    }
}

/// Decimating FIR. Only computes the outputs it keeps, so cost scales with the
/// output rate rather than the input rate.
#[derive(Clone)]
pub struct FirDecim {
    taps: Vec<f32>,
    hist: Vec<C32>,
    /// Write cursor into the second half of the doubled history buffer.
    pos: usize,
    factor: usize,
    phase: usize,
}

impl FirDecim {
    pub fn new(taps: Vec<f32>, factor: usize) -> Self {
        assert!(factor >= 1, "decimation factor must be >= 1");
        let n = taps.len();
        Self { taps, hist: vec![C32::new(0.0, 0.0); n * 2], pos: 0, factor, phase: 0 }
    }

    /// Design and build a decimator in one step. Transition band is placed so
    /// the passband keeps `passband_ratio` of the output Nyquist.
    pub fn design(factor: usize, passband_ratio: f64, atten_db: f64) -> Self {
        let out_nyq = 0.5 / factor as f64;
        let cutoff = out_nyq * passband_ratio;
        let transition = out_nyq - cutoff;
        let taps = estimate_taps(transition.max(1e-4), atten_db);
        Self::new(lowpass(taps, cutoff, atten_db), factor)
    }

    pub fn factor(&self) -> usize {
        self.factor
    }

    pub fn process(&mut self, input: &[C32], out: &mut Vec<C32>) {
        let n = self.taps.len();
        out.reserve(input.len() / self.factor + 1);
        for &x in input {
            if self.pos == n {
                self.hist.copy_within(n..2 * n, 0);
                self.pos = 0;
            }
            self.hist[n + self.pos] = x;
            self.pos += 1;

            self.phase += 1;
            if self.phase == self.factor {
                self.phase = 0;
                // Newest sample is at n + pos - 1, walking backwards in time.
                let base = n + self.pos;
                let mut acc = C32::new(0.0, 0.0);
                for (k, &h) in self.taps.iter().enumerate() {
                    acc += self.hist[base - 1 - k] * h;
                }
                out.push(acc);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn tone(n: usize, freq: f64) -> Vec<C32> {
        (0..n)
            .map(|i| {
                let p = TAU * freq as f32 * i as f32;
                C32::new(p.cos(), p.sin())
            })
            .collect()
    }

    fn rms(v: &[C32]) -> f32 {
        (v.iter().map(|c| c.norm_sqr()).sum::<f32>() / v.len() as f32).sqrt()
    }

    #[test]
    fn lowpass_has_unity_dc_gain() {
        let h = lowpass(63, 0.1, 60.0);
        assert!((h.iter().sum::<f32>() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn lowpass_passes_dc_and_stops_nyquist() {
        let h = lowpass(127, 0.05, 80.0);
        let mut f = Fir::new(h);
        let mut out = Vec::new();
        f.process(&tone(4096, 0.0), &mut out);
        assert!((rms(&out[512..]) - 1.0).abs() < 0.01, "passband: {}", rms(&out[512..]));

        let mut f = Fir::new(lowpass(127, 0.05, 80.0));
        let mut out = Vec::new();
        f.process(&tone(4096, 0.25), &mut out);
        let stop = 20.0 * rms(&out[512..]).log10();
        assert!(stop < -70.0, "stopband only {stop} dB");
    }

    #[test]
    fn decimator_preserves_a_slow_tone() {
        // 0.01 cycles/sample in, decimate by 8 -> 0.08 cycles/sample out.
        let mut d = FirDecim::design(8, 0.8, 80.0);
        let mut out = Vec::new();
        d.process(&tone(8192, 0.01), &mut out);
        assert_eq!(out.len(), 1024);
        assert!((rms(&out[256..]) - 1.0).abs() < 0.05, "got {}", rms(&out[256..]));
    }

    #[test]
    fn decimator_rejects_out_of_band() {
        // 0.3 cycles/sample would alias badly if not filtered first.
        let mut d = FirDecim::design(8, 0.8, 80.0);
        let mut out = Vec::new();
        d.process(&tone(8192, 0.3), &mut out);
        let lvl = 20.0 * rms(&out[256..]).log10();
        assert!(lvl < -60.0, "alias leaked at {lvl} dB");
    }

    #[test]
    fn block_boundaries_are_seamless() {
        let taps = lowpass(63, 0.1, 60.0);
        let sig = tone(1000, 0.02);

        let mut a = Fir::new(taps.clone());
        let mut one = Vec::new();
        a.process(&sig, &mut one);

        let mut b = Fir::new(taps);
        let mut split = Vec::new();
        for chunk in sig.chunks(37) {
            b.process(chunk, &mut split);
        }
        assert_eq!(one.len(), split.len());
        for (x, y) in one.iter().zip(split.iter()) {
            assert!((x - y).norm() < 1e-6);
        }
    }
}
