//! Pulse extraction: turning an envelope into mark/gap timings.
//!
//! This is the shared front end that makes broad protocol support tractable.
//! rtl_433 supports hundreds of devices without hundreds of DSP chains,
//! because almost all of them are on-off keyed or two-level FSK, and both
//! reduce to the same thing: a list of how long the carrier was present and
//! how long it was absent. Everything protocol-specific happens afterwards, on
//! integers, at negligible cost.
//!
//! So the expensive work (magnitude, thresholding, timing) is done once per
//! channel, and adding the two hundredth protocol costs a table entry rather
//! than another filter chain.
//!
//! # Thresholding
//!
//! The threshold sits midway between a tracked noise estimate and a tracked
//! signal estimate, both updated only while the detector believes it is in the
//! corresponding state. A fixed threshold cannot work: ISM receivers see
//! signals ranging from a meter away to the edge of sensitivity, and the AGC
//! moves the floor underneath everything.

/// One mark/gap pair, in microseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pulse {
    /// Carrier present, in microseconds.
    pub mark: u32,
    /// Carrier absent, in microseconds. The final gap of a package is the
    /// timeout that ended it, and carries no information.
    pub gap: u32,
}

/// A complete burst: the pulses between two long silences.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Package {
    pub pulses: Vec<Pulse>,
    /// Estimated SNR of the burst, in dB.
    pub snr_db: f32,
    /// Sample index where the burst started, for correlating with a waterfall.
    pub start_sample: u64,
}

impl Package {
    pub fn len(&self) -> usize {
        self.pulses.len()
    }
    pub fn is_empty(&self) -> bool {
        self.pulses.is_empty()
    }
    /// Total on-air duration in microseconds, excluding the trailing timeout.
    pub fn duration_us(&self) -> u64 {
        self.pulses.iter().map(|p| p.mark as u64 + p.gap as u64).sum::<u64>()
            - self.pulses.last().map(|p| p.gap as u64).unwrap_or(0)
    }

    /// Histogram of mark widths, bucketed to `tol_us`. Reading this is how you
    /// identify an unknown protocol by hand: a PWM signal shows two clear
    /// clusters, a PPM signal shows one.
    pub fn mark_histogram(&self, tol_us: u32) -> Vec<(u32, usize)> {
        histogram(self.pulses.iter().map(|p| p.mark), tol_us)
    }

    pub fn gap_histogram(&self, tol_us: u32) -> Vec<(u32, usize)> {
        // The trailing gap is a timeout, not signal, so leave it out.
        let n = self.pulses.len().saturating_sub(1);
        histogram(self.pulses[..n].iter().map(|p| p.gap), tol_us)
    }
}

fn histogram(vals: impl Iterator<Item = u32>, tol_us: u32) -> Vec<(u32, usize)> {
    let mut buckets: Vec<(u32, usize, u64)> = Vec::new();
    for v in vals {
        match buckets.iter_mut().find(|(c, _, _)| v.abs_diff(*c) <= tol_us) {
            Some((c, n, sum)) => {
                *n += 1;
                *sum += v as u64;
                *c = (*sum / *n as u64) as u32;
            }
            None => buckets.push((v, 1, v as u64)),
        }
    }
    buckets.sort_by_key(|(c, _, _)| *c);
    buckets.into_iter().map(|(c, n, _)| (c, n)).collect()
}

#[derive(Clone, Copy, Debug)]
pub struct PulseConfig {
    /// Gap longer than this ends the package, in microseconds.
    pub reset_us: u32,
    /// Ignore marks shorter than this: impulse noise, not signal.
    pub min_mark_us: u32,
    /// Discard packages with fewer pulses than this.
    pub min_pulses: usize,
    /// Hysteresis as a fraction of the gap between the noise and signal
    /// estimates. Without it a signal sitting near the threshold shreds into
    /// hundreds of spurious pulses.
    pub hysteresis: f32,
    /// Estimator time constant in microseconds.
    pub tau_us: f32,
    /// Minimum ratio between signal and noise estimates before pulses are
    /// emitted at all, guarding against a package made entirely of noise.
    pub min_snr_db: f32,
}

impl Default for PulseConfig {
    fn default() -> Self {
        Self {
            // rtl_433's default reset is 100 klots at 250 kHz, about 400 us.
            // A little longer is safer: some protocols use long inter-symbol
            // gaps and would otherwise be split mid-packet.
            reset_us: 4_000,
            // Below about 100 us is noise for practically every ISM protocol:
            // the fastest common OOK symbol is around 100 us and anything
            // briefer is a threshold crossing, not a transmission.
            min_mark_us: 100,
            min_pulses: 4,
            hysteresis: 0.2,
            tau_us: 500.0,
            min_snr_db: 6.0,
        }
    }
}

/// On-off-keying pulse detector.
///
/// Consumes a real envelope (magnitude, not magnitude squared) and produces
/// complete packages.
pub struct OokDetector {
    cfg: PulseConfig,
    rate: f64,
    /// Microseconds per sample.
    us_per_sample: f64,
    alpha: f32,
    noise: f32,
    signal: f32,
    /// Currently above threshold.
    high: bool,
    run: u64,
    current: Package,
    /// Mark awaiting its gap. Held back because a mark's gap is only known
    /// once the next valid mark begins.
    pending_mark: u32,
    /// Gap accumulated since `pending_mark`, including any sub-threshold
    /// crossings folded into it.
    gap_accum: u32,
    sample: u64,
    seeded: bool,
}

impl OokDetector {
    pub fn new(rate: f64, cfg: PulseConfig) -> Self {
        let us_per_sample = 1e6 / rate;
        let tau_samples = (cfg.tau_us / us_per_sample as f32).max(1.0);
        Self {
            cfg,
            rate,
            us_per_sample,
            alpha: 1.0 / tau_samples,
            noise: 0.0,
            signal: 0.0,
            high: false,
            run: 0,
            current: Package::default(),
            pending_mark: 0,
            gap_accum: 0,
            sample: 0,
            seeded: false,
        }
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    pub fn noise_level(&self) -> f32 {
        self.noise
    }

    pub fn signal_level(&self) -> f32 {
        self.signal
    }

    pub fn snr_db(&self) -> f32 {
        20.0 * (self.signal.max(1e-20) / self.noise.max(1e-20)).log10()
    }

    pub fn reset(&mut self) {
        self.high = false;
        self.run = 0;
        self.current = Package::default();
        self.pending_mark = 0;
        self.gap_accum = 0;
        self.seeded = false;
    }

    fn us(&self, samples: u64) -> u32 {
        (samples as f64 * self.us_per_sample).round() as u32
    }

    /// Feed an envelope block, appending any completed packages to `out`.
    pub fn process(&mut self, env: &[f32], out: &mut Vec<Package>) {
        let reset_samples = (self.cfg.reset_us as f64 / self.us_per_sample) as u64;
        let min_ratio = 10f32.powf(self.cfg.min_snr_db / 20.0);

        for &v in env {
            if !self.seeded {
                self.noise = v;
                self.signal = v * 4.0;
                self.seeded = true;
            }

            let mid = 0.5 * (self.noise + self.signal);
            let hyst = self.cfg.hysteresis * (self.signal - self.noise).max(0.0);
            let thresh = if self.high { mid - hyst } else { mid + hyst };
            let now_high = v > thresh;

            // Track each level only while in that state, so a long
            // transmission cannot pull the noise estimate up after itself.
            if now_high {
                self.signal += self.alpha * (v - self.signal);
                // Without this clamp the detector destroys itself. A single
                // noise spike crosses the threshold, the signal estimate then
                // adapts *down* towards that noise, which lowers the
                // threshold, which admits more noise. The estimator converges
                // on the noise floor and emits hundreds of spurious
                // micro-pulses. Holding the signal estimate at least
                // `min_snr_db` above the noise breaks the feedback loop.
                let floor = self.noise * min_ratio;
                if self.signal < floor {
                    self.signal = floor;
                }
            } else {
                self.noise += self.alpha * (v - self.noise);
            }

            if now_high != self.high {
                let dur = self.us(self.run);
                if self.high {
                    // A high run ended: a candidate mark.
                    if dur >= self.cfg.min_mark_us {
                        // Emit the *previous* pulse, whose gap is now complete.
                        if self.pending_mark > 0 {
                            self.current.pulses.push(Pulse {
                                mark: self.pending_mark,
                                gap: self.gap_accum,
                            });
                        } else if self.current.pulses.is_empty() {
                            self.current.start_sample =
                                self.sample.saturating_sub(self.run);
                        }
                        self.pending_mark = dur;
                        self.gap_accum = 0;
                    } else {
                        // Too short to be a symbol. It is a threshold
                        // crossing inside a gap, so it must be folded *into*
                        // that gap. Simply discarding it would split one gap
                        // into two shorter ones and corrupt every timing that
                        // follows, which is far worse than the noise itself.
                        self.gap_accum += dur;
                    }
                } else {
                    self.gap_accum += dur;
                }
                self.high = now_high;
                self.run = 0;
            }

            self.run += 1;
            self.sample += 1;

            // A long silence terminates the package.
            if !self.high && self.run > reset_samples {
                if self.pending_mark >= self.cfg.min_mark_us {
                    self.current
                        .pulses
                        .push(Pulse { mark: self.pending_mark, gap: self.cfg.reset_us });
                }
                self.pending_mark = 0;
                self.gap_accum = 0;
                if self.current.pulses.len() >= self.cfg.min_pulses {
                    let snr = self.snr_db();
                    if snr >= self.cfg.min_snr_db {
                        let mut p = std::mem::take(&mut self.current);
                        p.snr_db = snr;
                        out.push(p);
                    }
                }
                self.current.pulses.clear();
                self.run = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 250_000.0;

    /// Build an envelope from a mark/gap list in microseconds.
    fn envelope(pulses: &[(u32, u32)], amp: f32, noise_amp: f32) -> Vec<f32> {
        let sp = |us: u32| (us as f64 * RATE / 1e6).round() as usize;
        let mut seed = 42u64;
        let mut rng = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 33) as f32 / (1u64 << 31) as f32) * noise_amp
        };
        let mut v = Vec::new();
        v.extend((0..sp(20_000)).map(|_| rng()));
        for (m, g) in pulses {
            v.extend((0..sp(*m)).map(|_| amp + rng()));
            v.extend((0..sp(*g)).map(|_| rng()));
        }
        v.extend((0..sp(20_000)).map(|_| rng()));
        v
    }

    fn approx(a: u32, b: u32, tol: u32) -> bool {
        a.abs_diff(b) <= tol
    }

    #[test]
    fn recovers_pwm_timings() {
        // A typical PWM remote: 500 us short, 1000 us long, 500 us gaps.
        let want = [
            (500u32, 500u32),
            (1000, 500),
            (500, 500),
            (1000, 500),
            (1000, 500),
            (500, 500),
        ];
        let env = envelope(&want, 1.0, 0.02);
        let mut d = OokDetector::new(RATE, PulseConfig::default());
        let mut out = Vec::new();
        d.process(&env, &mut out);

        assert_eq!(out.len(), 1, "expected one package, got {}", out.len());
        let p = &out[0];
        assert_eq!(p.pulses.len(), want.len(), "pulses: {:?}", p.pulses);
        for (got, exp) in p.pulses.iter().zip(&want) {
            assert!(approx(got.mark, exp.0, 30), "mark {} vs {}", got.mark, exp.0);
        }
        // Every gap but the last, which is the terminating timeout.
        for (got, exp) in p.pulses[..want.len() - 1].iter().zip(&want) {
            assert!(approx(got.gap, exp.1, 30), "gap {} vs {}", got.gap, exp.1);
        }
    }

    #[test]
    fn histogram_separates_short_from_long() {
        let want = [(500u32, 500u32), (1000, 500), (500, 500), (1000, 500), (500, 500)];
        let env = envelope(&want, 1.0, 0.02);
        let mut d = OokDetector::new(RATE, PulseConfig::default());
        let mut out = Vec::new();
        d.process(&env, &mut out);

        let h = out[0].mark_histogram(100);
        assert_eq!(h.len(), 2, "expected two clusters, got {h:?}");
        assert!(approx(h[0].0, 500, 40) && h[0].1 == 3, "{h:?}");
        assert!(approx(h[1].0, 1000, 40) && h[1].1 == 2, "{h:?}");
    }

    #[test]
    fn separates_two_bursts_by_the_reset_gap() {
        let sp = |us: u32| (us as f64 * RATE / 1e6).round() as usize;
        let burst = [(500u32, 500u32), (1000, 500), (500, 500), (1000, 500)];
        let mut env = envelope(&burst, 1.0, 0.02);
        env.extend((0..sp(10_000)).map(|_| 0.01));
        env.extend(envelope(&burst, 1.0, 0.02));

        let mut d = OokDetector::new(RATE, PulseConfig::default());
        let mut out = Vec::new();
        d.process(&env, &mut out);
        assert_eq!(out.len(), 2, "expected two packages, got {}", out.len());
    }

    #[test]
    fn rejects_pure_noise() {
        let env: Vec<f32> = {
            let mut seed = 7u64;
            (0..250_000)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    (seed >> 33) as f32 / (1u64 << 31) as f32 * 0.05
                })
                .collect()
        };
        let mut d = OokDetector::new(RATE, PulseConfig::default());
        let mut out = Vec::new();
        d.process(&env, &mut out);
        assert!(out.is_empty(), "noise produced {} packages: {:?}", out.len(), out);
    }

    #[test]
    fn hysteresis_prevents_shredding_a_weak_pulse() {
        // A long mark with amplitude wobbling around the threshold. Without
        // hysteresis this fragments into a great many pulses.
        let sp = |us: u32| (us as f64 * RATE / 1e6).round() as usize;
        let mut env: Vec<f32> = vec![0.01; sp(20_000)];
        for i in 0..sp(2_000) {
            env.push(if i % 3 == 0 { 0.45 } else { 0.55 });
        }
        env.extend(vec![0.01; sp(20_000)]);

        let mut d = OokDetector::new(RATE, PulseConfig::default());
        let mut out = Vec::new();
        d.process(&env, &mut out);
        let n = out.first().map(|p| p.pulses.len()).unwrap_or(0);
        assert!(n <= 2, "wobbling pulse shredded into {n} pulses");
    }

    #[test]
    fn block_boundaries_do_not_split_pulses() {
        let want = [(500u32, 500u32), (1000, 500), (500, 500), (1000, 500), (500, 500)];
        let env = envelope(&want, 1.0, 0.02);

        let mut whole = OokDetector::new(RATE, PulseConfig::default());
        let mut a = Vec::new();
        whole.process(&env, &mut a);

        let mut split = OokDetector::new(RATE, PulseConfig::default());
        let mut b = Vec::new();
        for c in env.chunks(997) {
            split.process(c, &mut b);
        }
        assert_eq!(a, b, "block splitting changed the pulse train");
    }

    #[test]
    fn duration_excludes_the_trailing_timeout() {
        let want = [(500u32, 500u32), (1000, 500), (500, 500), (1000, 500)];
        let env = envelope(&want, 1.0, 0.02);
        let mut d = OokDetector::new(RATE, PulseConfig::default());
        let mut out = Vec::new();
        d.process(&env, &mut out);
        let dur = out[0].duration_us();
        let expect: u64 = want.iter().map(|(m, g)| *m as u64 + *g as u64).sum::<u64>() - 500;
        assert!(dur.abs_diff(expect) < 200, "duration {dur} vs expected {expect}");
    }
}
