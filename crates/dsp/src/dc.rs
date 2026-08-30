//! Removing the spur at the centre of a direct-conversion receiver.
//!
//! A zero-IF front end mixes the tuned frequency down to 0 Hz, so anything
//! that leaks from the local oscillator into the mixer input arrives at
//! exactly the same frequency as itself and lands at DC. Add the ADC's own
//! offset and the result is a permanent spike at the centre of the span that
//! moves with the tuning, because it *is* the tuning. It looks like a very
//! strong carrier and is not a signal at all.
//!
//! The HackRF is zero-IF, so it shows this plainly. The RTL2832U with an R820T
//! runs a low IF and shifts down digitally, which moves the spur but does not
//! remove it.
//!
//! The cure is a very narrow highpass at DC. Narrow matters: this notches out
//! real signals at the centre frequency too, so it must be far narrower than
//! anything being received. Tuning deliberately off-centre remains the better
//! answer where it is possible, and this is what makes the remainder tolerable.

use common::C32;

/// Default notch width. Wide enough to follow offset drift with temperature,
/// far narrower than the narrowest channel the app demodulates.
pub const DEFAULT_CUTOFF_HZ: f64 = 1_000.0;

/// Tracks the mean of a complex stream and subtracts it.
///
/// A first-order highpass rather than a subtracted block average: the offset
/// drifts, and a per-block mean would step at every block boundary and put a
/// click into the audio.
#[derive(Clone, Debug)]
pub struct DcBlock {
    mean: C32,
    alpha: f32,
}

impl DcBlock {
    pub fn new(rate: f64) -> Self {
        Self::with_cutoff(rate, DEFAULT_CUTOFF_HZ)
    }

    pub fn with_cutoff(rate: f64, cutoff_hz: f64) -> Self {
        // Single-pole coefficient for the requested corner. Clamped below so a
        // very high sample rate cannot make it denormal, and above so a silly
        // cutoff cannot turn this into a differentiator.
        let a = (std::f64::consts::TAU * cutoff_hz / rate.max(1.0)).clamp(1e-9, 0.5);
        Self { mean: C32::new(0.0, 0.0), alpha: a as f32 }
    }

    /// Current estimate of the offset, which is also a useful health readout:
    /// a large value means the front end is not well balanced.
    pub fn offset(&self) -> C32 {
        self.mean
    }

    pub fn reset(&mut self) {
        self.mean = C32::new(0.0, 0.0);
    }

    /// Remove the offset in place.
    pub fn process(&mut self, buf: &mut [C32]) {
        let a = self.alpha;
        let mut m = self.mean;
        for s in buf.iter_mut() {
            m += (*s - m) * a;
            *s -= m;
        }
        self.mean = m;
    }

    /// Prime the estimate from a block without altering it.
    ///
    /// Without this the first block is emitted with the full offset still in
    /// it, which the spectrum shows as a spike that fades over a second.
    pub fn prime(&mut self, buf: &[C32]) {
        if buf.is_empty() {
            return;
        }
        let mut sum = C32::new(0.0, 0.0);
        for s in buf {
            sum += *s;
        }
        self.mean = sum / buf.len() as f32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    const RATE: f64 = 2_400_000.0;

    fn rms(v: &[C32]) -> f64 {
        (v.iter().map(|c| c.norm_sqr() as f64).sum::<f64>() / v.len() as f64).sqrt()
    }

    #[test]
    fn a_constant_offset_is_removed() {
        let mut buf = vec![C32::new(0.3, -0.2); 200_000];
        let mut d = DcBlock::new(RATE);
        d.prime(&buf);
        d.process(&mut buf);
        assert!(rms(&buf[1000..]) < 1e-4, "offset survived: {}", rms(&buf[1000..]));
    }

    #[test]
    fn priming_removes_the_spur_from_the_very_first_block() {
        // Without priming the estimate starts at zero and the first samples
        // carry the whole offset, which shows as a spike that fades away.
        let make = || vec![C32::new(0.3, -0.2); 4096];
        let cold = {
            let mut b = make();
            DcBlock::new(RATE).process(&mut b);
            rms(&b)
        };
        let primed = {
            let mut b = make();
            let mut d = DcBlock::new(RATE);
            d.prime(&b);
            d.process(&mut b);
            rms(&b)
        };
        assert!(primed < cold / 100.0, "priming barely helped: {primed} vs {cold}");
    }

    #[test]
    fn a_signal_away_from_dc_is_left_alone() {
        // The notch must be narrow enough that a channel a few kHz off centre
        // passes untouched, or removing the spur costs more than it saves.
        let n = 200_000;
        let mut buf: Vec<C32> = (0..n)
            .map(|i| {
                let t = i as f64 / RATE;
                let p = TAU * 50_000.0 * t;
                C32::new(p.cos() as f32, p.sin() as f32) + C32::new(0.3, -0.2)
            })
            .collect();
        let mut d = DcBlock::new(RATE);
        d.prime(&buf);
        d.process(&mut buf);
        let level = rms(&buf[1000..]);
        assert!((level - 1.0).abs() < 0.01, "signal level changed to {level}");
    }

    #[test]
    fn the_notch_is_narrower_than_the_narrowest_channel() {
        // 1 kHz against 12.5 kHz narrowband voice, and against the 2.4 kHz
        // either side of the RDS subcarrier.
        assert!(DEFAULT_CUTOFF_HZ < 12_500.0 / 4.0);
    }

    #[test]
    fn the_offset_estimate_reports_what_was_removed() {
        let mut buf = vec![C32::new(0.25, 0.1); 100_000];
        let mut d = DcBlock::new(RATE);
        d.prime(&buf);
        d.process(&mut buf);
        let o = d.offset();
        assert!((o.re - 0.25).abs() < 1e-3 && (o.im - 0.1).abs() < 1e-3, "reported {o}");
    }

    #[test]
    fn block_boundaries_do_not_click() {
        // A per-block mean steps at every boundary; a tracking filter must not.
        let n = 60_000;
        let src: Vec<C32> = (0..n)
            .map(|i| {
                let t = i as f64 / RATE;
                let p = TAU * 30_000.0 * t;
                C32::new(0.5 * p.cos() as f32, 0.5 * p.sin() as f32) + C32::new(0.3, -0.2)
            })
            .collect();
        let whole = {
            let mut b = src.clone();
            let mut d = DcBlock::new(RATE);
            d.prime(&b);
            d.process(&mut b);
            b
        };
        let split = {
            let mut d = DcBlock::new(RATE);
            d.prime(&src[..4096]);
            let mut out = Vec::new();
            for c in src.chunks(4096) {
                let mut b = c.to_vec();
                d.process(&mut b);
                out.extend(b);
            }
            out
        };
        for (i, (a, b)) in whole.iter().zip(&split).enumerate().skip(8192) {
            assert!((a - b).norm() < 1e-5, "sample {i} differs across blocking");
        }
    }

    #[test]
    fn a_drifting_offset_is_followed() {
        // Thermal drift moves the offset, so a fixed correction measured once
        // at startup would come apart.
        let n = 400_000;
        let mut buf: Vec<C32> = (0..n)
            .map(|i| {
                let d = 0.3 + 0.2 * (i as f32 / n as f32);
                C32::new(d, -0.2)
            })
            .collect();
        let mut d = DcBlock::new(RATE);
        d.prime(&buf);
        d.process(&mut buf);
        assert!(rms(&buf[10_000..]) < 1e-3, "drift not tracked: {}", rms(&buf[10_000..]));
    }
}
