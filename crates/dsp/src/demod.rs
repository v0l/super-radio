//! Demodulators.

use common::C32;

/// Quadrature FM discriminator.
///
/// Instantaneous frequency is the derivative of phase, which for a sampled
/// signal is `arg(x[n] * conj(x[n-1]))`. Computing it this way rather than
/// differencing two `atan2` results is not an optimisation detail: phase
/// unwrapping is where naive FM demodulators go wrong, and the product form
/// gives the wrapped difference directly, with no unwrapping needed.
#[derive(Clone, Debug)]
pub struct FmDemod {
    prev: C32,
    /// Scales radians per sample to a normalised output.
    gain: f32,
}

impl FmDemod {
    /// `deviation_hz` is the peak deviation that should map to an output of
    /// 1.0: 75 kHz for broadcast WFM, 2.5 to 5 kHz for narrowband voice.
    pub fn new(rate: f64, deviation_hz: f64) -> Self {
        Self {
            prev: C32::new(0.0, 0.0),
            gain: (rate / (std::f64::consts::TAU * deviation_hz)) as f32,
        }
    }

    pub fn reset(&mut self) {
        self.prev = C32::new(0.0, 0.0);
    }

    pub fn process(&mut self, input: &[C32], out: &mut Vec<f32>) {
        out.reserve(input.len());
        for &x in input {
            let d = x * self.prev.conj();
            // A zero product means no signal, and `arg` of zero is arbitrary.
            // Emitting 0 rather than a random angle keeps squelched channels
            // silent instead of producing full-scale noise.
            let v = if d.norm_sqr() > 0.0 { d.arg() } else { 0.0 };
            out.push(v * self.gain);
            self.prev = x;
        }
    }
}

/// AM envelope detector with a DC blocker.
#[derive(Clone, Debug)]
pub struct AmDemod {
    dc: f32,
    alpha: f32,
}

impl AmDemod {
    /// `cutoff_hz` sets how fast the carrier estimate adapts. A few hertz is
    /// right: fast enough to follow fading, slow enough not to eat the audio.
    pub fn new(rate: f64, cutoff_hz: f64) -> Self {
        let alpha = (1.0 - (-std::f64::consts::TAU * cutoff_hz / rate).exp()) as f32;
        Self { dc: 0.0, alpha }
    }

    pub fn reset(&mut self) {
        self.dc = 0.0;
    }

    pub fn process(&mut self, input: &[C32], out: &mut Vec<f32>) {
        out.reserve(input.len());
        for &x in input {
            let env = x.norm();
            self.dc += self.alpha * (env - self.dc);
            out.push(env - self.dc);
        }
    }
}

/// Single-pole de-emphasis filter.
///
/// FM broadcast pre-emphasises treble at the transmitter to improve SNR, so a
/// receiver must undo it or everything sounds harsh and thin. The time
/// constant is regional: 50 us across Europe, 75 us in the Americas. Getting
/// this wrong is audible but not obviously "broken", which is why it is worth
/// making explicit rather than hard-coding.
#[derive(Clone, Debug)]
pub struct Deemphasis {
    alpha: f32,
    state: f32,
}

impl Deemphasis {
    pub fn new(rate: f64, tau_us: f64) -> Self {
        let tau = tau_us * 1e-6;
        let alpha = (1.0 - (-1.0 / (rate * tau)).exp()) as f32;
        Self { alpha, state: 0.0 }
    }

    /// 50 us, the standard outside the Americas.
    pub fn eu(rate: f64) -> Self {
        Self::new(rate, 50.0)
    }

    /// 75 us, the standard in the Americas.
    pub fn us(rate: f64) -> Self {
        Self::new(rate, 75.0)
    }

    pub fn reset(&mut self) {
        self.state = 0.0;
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        self.process_strided(buf, 0, 1);
    }

    /// Filter one channel of an interleaved buffer in place.
    pub fn process_strided(&mut self, buf: &mut [f32], offset: usize, stride: usize) {
        let stride = stride.max(1);
        let mut i = offset;
        while i < buf.len() {
            self.state += self.alpha * (buf[i] - self.state);
            buf[i] = self.state;
            i += stride;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An FM carrier modulated by a single sine tone.
    fn fm_tone(n: usize, rate: f64, tone_hz: f64, deviation_hz: f64) -> Vec<C32> {
        let mut phase = 0.0f64;
        (0..n)
            .map(|i| {
                let t = i as f64 / rate;
                let inst = deviation_hz * (std::f64::consts::TAU * tone_hz * t).sin();
                phase += std::f64::consts::TAU * inst / rate;
                phase = phase.rem_euclid(std::f64::consts::TAU);
                C32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn fm_recovers_the_modulating_tone() {
        let rate = 300_000.0;
        let sig = fm_tone(30_000, rate, 1_000.0, 75_000.0);
        let mut d = FmDemod::new(rate, 75_000.0);
        let mut out = Vec::new();
        d.process(&sig, &mut out);

        // Skip the first sample, which has no predecessor.
        let body = &out[10..];
        let peak = body.iter().fold(0.0f32, |a, b| a.max(b.abs()));
        assert!((peak - 1.0).abs() < 0.05, "peak deviation mapped to {peak}, expected 1.0");

        // Correlate against the known tone to confirm it is the right one.
        let mut corr = 0.0f64;
        let mut norm = 0.0f64;
        for (i, v) in body.iter().enumerate() {
            let t = (i + 10) as f64 / rate;
            let r = (std::f64::consts::TAU * 1000.0 * t).sin();
            corr += *v as f64 * r;
            norm += r * r;
        }
        let scale = corr / norm;
        assert!(scale > 0.95, "correlation with the modulating tone was only {scale}");
    }

    #[test]
    fn fm_output_is_silent_on_zero_input() {
        let mut d = FmDemod::new(300_000.0, 75_000.0);
        let mut out = Vec::new();
        d.process(&vec![C32::new(0.0, 0.0); 100], &mut out);
        assert!(out.iter().all(|v| *v == 0.0), "silence produced noise");
    }

    #[test]
    fn deemphasis_attenuates_treble_more_than_bass() {
        let rate = 48_000.0;
        let level = |hz: f64| {
            let mut d = Deemphasis::eu(rate);
            let mut buf: Vec<f32> = (0..20_000)
                .map(|i| ((std::f64::consts::TAU * hz * i as f64 / rate).sin()) as f32)
                .collect();
            d.process(&mut buf);
            let tail = &buf[10_000..];
            (tail.iter().map(|v| v * v).sum::<f32>() / tail.len() as f32).sqrt()
        };
        let low = level(300.0);
        let high = level(8_000.0);
        let db = 20.0 * (high / low).log10();
        // A 50 us pole sits at ~3.2 kHz, so 8 kHz should be well down on 300 Hz.
        assert!(db < -6.0, "treble only {db} dB below bass");
    }

    #[test]
    fn am_removes_the_carrier_dc() {
        let rate = 48_000.0;
        let sig: Vec<C32> = (0..48_000)
            .map(|i| {
                let m = 1.0 + 0.5 * (std::f64::consts::TAU * 1000.0 * i as f64 / rate).sin() as f32;
                C32::new(m, 0.0)
            })
            .collect();
        let mut d = AmDemod::new(rate, 5.0);
        let mut out = Vec::new();
        d.process(&sig, &mut out);
        let tail = &out[24_000..];
        let mean = tail.iter().sum::<f32>() / tail.len() as f32;
        assert!(mean.abs() < 0.02, "carrier DC left behind: {mean}");
        let peak = tail.iter().fold(0.0f32, |a, b| a.max(b.abs()));
        assert!((peak - 0.5).abs() < 0.05, "modulation depth came out as {peak}");
    }
}
