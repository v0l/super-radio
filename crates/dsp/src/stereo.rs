//! FM stereo decoding: recover L and R from the multiplex.
//!
//! The broadcast multiplex is
//!
//! ```text
//! MPX = (L+R) + (L-R)·cos(2π·38000·t) + K·cos(2π·19000·t)
//! ```
//!
//! so the difference signal is a double-sideband suppressed-carrier subcarrier
//! whose carrier is exactly twice the pilot. Recovering it needs a carrier
//! locked in both frequency *and* phase, which is what the PLL here provides.
//!
//! Ported from the approach in the MIT-licensed `fmradio` crate, with two
//! changes. That implementation filters L-R through a 257-tap FIR but takes
//! L+R straight from the input, so the two arms differ by the filter's group
//! delay and separation suffers; here both arms go through identical filters
//! and stay aligned. It also band-passes the pilot before the phase detector,
//! which adds a phase shift between the recovered carrier and the subcarrier
//! it has to demodulate; the loop filter is already far narrower than any
//! practical prefilter, so this feeds the phase detector directly.

use crate::fir::{lowpass, FirDecimReal};
use std::f64::consts::TAU;

const PILOT_HZ: f64 = 19_000.0;
/// Audio bandwidth of each channel.
const AUDIO_HZ: f64 = 15_000.0;

pub struct StereoDecoder {
    rate: f64,
    /// Tracked pilot phase in radians.
    phase: f64,
    /// Tracked pilot frequency in Hz.
    freq: f64,
    err_lp: f64,
    err_alpha: f64,
    kp: f64,
    ki: f64,
    /// Low-passed in-phase pilot amplitude, the lock indicator.
    lock_lp: f64,
    /// Low-passed input magnitude, to normalise the lock indicator.
    level_lp: f64,
    sum_lp: FirDecimReal,
    diff_lp: FirDecimReal,
    sum: Vec<f32>,
    diff: Vec<f32>,
    mixed: Vec<f32>,
    /// Tracked pilot phase for each sample of the last block. RDS needs this:
    /// its carrier is the third harmonic and its bit clock the sixteenth
    /// sub-harmonic of the same pilot.
    phases: Vec<f64>,
}

impl StereoDecoder {
    pub fn new(rate: f64) -> Self {
        // 15 kHz audio in a 19 kHz guard: a few hundred taps at the multiplex
        // rate, and both arms share the design so their delays match exactly.
        let taps = lowpass(255, AUDIO_HZ / rate, 70.0);
        // ~50 Hz loop filter. Narrow enough that the audio, the difference
        // subcarrier and RDS all average away in the phase detector, leaving
        // only the pilot.
        let fc = 50.0;
        Self {
            rate,
            phase: 0.0,
            freq: PILOT_HZ,
            err_lp: 0.0,
            err_alpha: (TAU * fc / (rate + TAU * fc)) as f64,
            kp: 0.15,
            ki: 0.0005,
            lock_lp: 0.0,
            level_lp: 0.0,
            sum_lp: FirDecimReal::new(taps.clone(), 1),
            diff_lp: FirDecimReal::new(taps, 1),
            sum: Vec::new(),
            diff: Vec::new(),
            mixed: Vec::new(),
            phases: Vec::new(),
        }
    }

    /// 0 when there is no pilot, rising towards 1 as it locks.
    pub fn lock(&self) -> f32 {
        if self.level_lp > 1e-9 {
            // The pilot is nominally 8 to 10% of full deviation, so a locked
            // reading is far below 1; scale so a healthy pilot reads near 1.
            ((self.lock_lp.abs() / self.level_lp) / 0.09).clamp(0.0, 1.0) as f32
        } else {
            0.0
        }
    }

    pub fn is_locked(&self) -> bool {
        self.lock() > 0.3
    }

    /// Recovered pilot frequency, for diagnostics.
    pub fn pilot_freq(&self) -> f64 {
        self.freq
    }

    pub fn reset(&mut self) {
        self.phase = 0.0;
        self.freq = PILOT_HZ;
        self.err_lp = 0.0;
        self.lock_lp = 0.0;
        self.level_lp = 0.0;
        self.sum_lp.reset();
        self.diff_lp.reset();
    }

    /// Decode one block of multiplex into left and right.
    pub fn process(&mut self, mpx: &[f32], left: &mut Vec<f32>, right: &mut Vec<f32>) {
        let inc = TAU / self.rate;
        self.mixed.clear();
        self.mixed.reserve(mpx.len());
        self.phases.clear();
        self.phases.reserve(mpx.len());

        for &x in mpx {
            let v = x as f64;
            // The carrier for this sample must use this sample's phase. Mixing
            // with the phase after the loop update is one sample late, which
            // at 38 kHz on a 288 kHz multiplex is 47.5 degrees and costs a
            // factor of cos(47.5) in the difference arm.
            let phase = self.phase;
            self.phases.push(phase);
            let (s, c) = phase.sin_cos();

            // Locking to a cosine pilot: the error is -mpx·sin(phase), whose
            // average is (K/2)·sin(pilot - phase), and the in-phase product
            // mpx·cos(phase) averages to the pilot amplitude once locked.
            let err_raw = -v * s;
            self.err_lp += self.err_alpha * (err_raw - self.err_lp);
            self.lock_lp += self.err_alpha * (v * c - self.lock_lp);
            self.level_lp += self.err_alpha * (v.abs() - self.level_lp);

            self.freq += self.ki * self.err_lp;
            // A real pilot is within a few hundred ppm; a wider clamp just
            // lets the loop chase audio during silence.
            self.freq = self.freq.clamp(PILOT_HZ * 0.999, PILOT_HZ * 1.001);
            self.phase += inc * self.freq + self.kp * self.err_lp;
            if self.phase > TAU {
                self.phase -= TAU;
            } else if self.phase < 0.0 {
                self.phase += TAU;
            }

            // Coherent demodulation by the second harmonic. The factor of two
            // undoes the half that falls out of the product.
            self.mixed.push((2.0 * v * (2.0 * phase).cos()) as f32);
        }

        self.sum.clear();
        self.diff.clear();
        self.sum_lp.process(mpx, &mut self.sum);
        self.diff_lp.process(&self.mixed, &mut self.diff);

        left.clear();
        right.clear();
        left.reserve(self.sum.len());
        right.reserve(self.sum.len());
        for (s, d) in self.sum.iter().zip(&self.diff) {
            left.push(s + d);
            right.push(s - d);
        }
    }

    /// Pilot phase for each sample of the last processed block.
    pub fn phases(&self) -> &[f64] {
        &self.phases
    }

    /// Mono output, for when there is no pilot or stereo is not wanted.
    pub fn process_mono(&mut self, mpx: &[f32], out: &mut Vec<f32>) {
        out.clear();
        self.sum_lp.process(mpx, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 288_000.0;

    fn tone(n: usize, hz: f64, amp: f64, phase: f64) -> Vec<f64> {
        (0..n).map(|i| amp * (TAU * hz * i as f64 / RATE + phase).sin()).collect()
    }

    /// Build a broadcast multiplex from separate left and right channels.
    fn multiplex(l: &[f64], r: &[f64], pilot: f64) -> Vec<f32> {
        l.iter()
            .zip(r)
            .enumerate()
            .map(|(i, (a, b))| {
                let t = i as f64 / RATE;
                let sum = a + b;
                let diff = a - b;
                (sum + diff * (TAU * 2.0 * PILOT_HZ * t).cos()
                    + pilot * (TAU * PILOT_HZ * t).cos()) as f32
            })
            .collect()
    }

    fn rms(v: &[f32]) -> f64 {
        (v.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>() / v.len().max(1) as f64).sqrt()
    }

    /// Skip the filter's settling time and the PLL's acquisition.
    fn settled(v: &[f32]) -> &[f32] {
        &v[v.len() / 2..]
    }

    #[test]
    fn the_pll_locks_to_a_real_pilot() {
        let n = 200_000;
        let mpx = multiplex(&tone(n, 1000.0, 0.3, 0.0), &tone(n, 1000.0, 0.3, 0.0), 0.1);
        let mut d = StereoDecoder::new(RATE);
        let (mut l, mut r) = (Vec::new(), Vec::new());
        d.process(&mpx, &mut l, &mut r);
        assert!(d.is_locked(), "did not lock, indicator {:.3}", d.lock());
        assert!(
            (d.pilot_freq() - PILOT_HZ).abs() < 20.0,
            "locked to {:.1} Hz",
            d.pilot_freq()
        );
    }

    #[test]
    fn there_is_no_lock_without_a_pilot() {
        let n = 200_000;
        // Same programme, pilot amplitude zero.
        let mpx = multiplex(&tone(n, 1000.0, 0.3, 0.0), &tone(n, 800.0, 0.3, 0.0), 0.0);
        let mut d = StereoDecoder::new(RATE);
        let (mut l, mut r) = (Vec::new(), Vec::new());
        d.process(&mpx, &mut l, &mut r);
        assert!(!d.is_locked(), "claimed lock on a mono signal: {:.3}", d.lock());
    }

    #[test]
    fn a_signal_only_on_the_left_stays_on_the_left() {
        let n = 300_000;
        let l_in = tone(n, 1000.0, 0.4, 0.0);
        let r_in = vec![0.0; n];
        let mpx = multiplex(&l_in, &r_in, 0.1);

        let mut d = StereoDecoder::new(RATE);
        let (mut l, mut r) = (Vec::new(), Vec::new());
        d.process(&mpx, &mut l, &mut r);
        let sep = 20.0 * (rms(settled(&l)) / rms(settled(&r)).max(1e-12)).log10();
        // A one-sample carrier offset drops this to about 14 dB, so the
        // threshold is set well above that to catch it.
        assert!(sep > 40.0, "only {sep:.1} dB of separation");
    }

    #[test]
    fn a_signal_only_on_the_right_stays_on_the_right() {
        let n = 300_000;
        let mpx = multiplex(&vec![0.0; n], &tone(n, 1000.0, 0.4, 0.0), 0.1);
        let mut d = StereoDecoder::new(RATE);
        let (mut l, mut r) = (Vec::new(), Vec::new());
        d.process(&mpx, &mut l, &mut r);
        let sep = 20.0 * (rms(settled(&r)) / rms(settled(&l)).max(1e-12)).log10();
        assert!(sep > 40.0, "only {sep:.1} dB of separation");
    }

    #[test]
    fn the_arms_stay_aligned_so_a_mono_signal_cancels() {
        // Identical L and R means L-R is zero, which only comes out right if
        // the sum and difference arms have the same group delay.
        let n = 300_000;
        let t = tone(n, 1000.0, 0.4, 0.0);
        let mpx = multiplex(&t, &t, 0.1);
        let mut d = StereoDecoder::new(RATE);
        let (mut l, mut r) = (Vec::new(), Vec::new());
        d.process(&mpx, &mut l, &mut r);
        let (a, b) = (rms(settled(&l)), rms(settled(&r)));
        let imbalance = 20.0 * (a / b.max(1e-12)).log10();
        assert!(imbalance.abs() < 1.0, "channels differ by {imbalance:.2} dB on mono");
    }

    #[test]
    fn block_boundaries_do_not_disturb_the_output() {
        let n = 300_000;
        let mpx = multiplex(&tone(n, 1000.0, 0.4, 0.0), &vec![0.0; n], 0.1);
        let one = {
            let mut d = StereoDecoder::new(RATE);
            let (mut l, mut r) = (Vec::new(), Vec::new());
            d.process(&mpx, &mut l, &mut r);
            l
        };
        let split = {
            let mut d = StereoDecoder::new(RATE);
            let mut acc = Vec::new();
            let (mut l, mut r) = (Vec::new(), Vec::new());
            for chunk in mpx.chunks(4096) {
                d.process(chunk, &mut l, &mut r);
                acc.extend_from_slice(&l);
            }
            acc
        };
        assert_eq!(one.len(), split.len());
        for (i, (a, b)) in one.iter().zip(&split).enumerate().skip(1000) {
            assert!((a - b).abs() < 1e-3, "sample {i} differs: {a} vs {b}");
        }
    }

    #[test]
    fn mono_output_rejects_the_pilot_and_subcarrier() {
        let n = 200_000;
        let mpx = multiplex(&tone(n, 1000.0, 0.4, 0.0), &vec![0.0; n], 0.1);
        let mut d = StereoDecoder::new(RATE);
        let mut out = Vec::new();
        d.process_mono(&mpx, &mut out);
        let g = |f: f64| {
            let x = settled(&out);
            let k = TAU * f / RATE;
            let c = 2.0 * k.cos();
            let (mut a, mut b) = (0.0f64, 0.0f64);
            for &v in x {
                let s = v as f64 + c * a - b;
                b = a;
                a = s;
            }
            (a * a + b * b - c * a * b).sqrt() / x.len() as f64
        };
        let audio = g(1000.0);
        assert!(g(PILOT_HZ) < audio * 0.01, "pilot leaked into mono audio");
        assert!(g(38_000.0) < audio * 0.01, "subcarrier leaked into mono audio");
    }
}

#[cfg(test)]
mod diag {
    use super::*;
    use std::f64::consts::TAU;
    const RATE: f64 = 288_000.0;

    #[test]
    #[ignore]
    fn measure_phase_error() {
        let n = 400_000usize;
        let mpx: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f64 / RATE;
                let l = 0.4 * (TAU * 1000.0 * t).sin();
                (l + l * (TAU * 38_000.0 * t).cos() + 0.1 * (TAU * 19_000.0 * t).cos()) as f32
            })
            .collect();
        let mut d = StereoDecoder::new(RATE);
        let (mut l, mut r) = (Vec::new(), Vec::new());
        // Feed in blocks and report the tracked phase against the true pilot.
        let mut consumed = 0usize;
        for chunk in mpx.chunks(40_000) {
            d.process(chunk, &mut l, &mut r);
            consumed += chunk.len();
            let true_phase = (TAU * 19_000.0 * (consumed as f64) / RATE) % TAU;
            let mut e = d.phase - true_phase;
            while e > std::f64::consts::PI { e -= TAU; }
            while e < -std::f64::consts::PI { e += TAU; }
            let g = |x: &[f32], f: f64| {
                let k = TAU * f / RATE; let c = 2.0 * k.cos();
                let (mut a, mut b) = (0.0f64, 0.0f64);
                for &v in x { let t = v as f64 + c * a - b; b = a; a = t; }
                (a * a + b * b - c * a * b).sqrt() / x.len() as f64
            };
            let rms = |x: &[f32]| (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt();
            println!(
                "n={consumed:>7} err {:+7.2}deg  L {:.4} R {:.4} sep {:5.1}dB | R@1k {:.5} R@2k {:.5} R@dc {:.5}",
                e.to_degrees(), rms(&l), rms(&r),
                20.0*(rms(&l)/rms(&r).max(1e-12)).log10(),
                g(&r, 1000.0), g(&r, 2000.0), g(&r, 1.0)
            );
        }
    }
}
