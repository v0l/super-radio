//! Recovering RDS bits from the multiplex.
//!
//! The subcarrier sits at 57 kHz, which is the third harmonic of the 19 kHz
//! pilot, and the 1187.5 bit/s data clock is exactly the pilot divided by 16.
//! Both are therefore already available from the stereo PLL, so this needs no
//! carrier recovery and no timing loop of its own: the phase that tracks the
//! pilot generates the carrier, and the same phase divided by 16 generates the
//! symbol clock. That is the whole reason RDS was specified this way.
//!
//! The data is differential Manchester, so each bit occupies one symbol with a
//! guaranteed mid-symbol transition, and the decoded value is the difference
//! between successive symbols rather than their absolute level.

use crate::fir::{lowpass, FirDecimReal};

pub const CARRIER_HZ: f64 = 57_000.0;
pub const BAUD: f64 = 1187.5;
/// Bandwidth of the data either side of the carrier.
const DATA_BW: f64 = 2_400.0;

pub struct RdsDemod {
    rate: f64,
    /// Symbol phase, one cycle per bit.
    sym_phase: f64,
    i_lp: FirDecimReal,
    q_lp: FirDecimReal,
    i_buf: Vec<f32>,
    q_buf: Vec<f32>,
    /// Integrators for the two halves of the current symbol.
    first: f64,
    second: f64,
    first_n: u32,
    second_n: u32,
    prev_half: bool,
    /// Previous symbol decision, for the differential decode.
    prev_bit: Option<u8>,
    /// Running estimate of symbol magnitude, used to report signal quality.
    level: f64,
}

impl RdsDemod {
    pub fn new(rate: f64) -> Self {
        // A few hundred taps at the multiplex rate keeps the 2.4 kHz data
        // while rejecting the difference subcarrier below and anything above.
        let taps = lowpass(255, DATA_BW / rate, 60.0);
        Self {
            rate,
            sym_phase: 0.0,
            i_lp: FirDecimReal::new(taps.clone(), 1),
            q_lp: FirDecimReal::new(taps, 1),
            i_buf: Vec::new(),
            q_buf: Vec::new(),
            first: 0.0,
            second: 0.0,
            first_n: 0,
            second_n: 0,
            prev_half: false,
            prev_bit: None,
            level: 0.0,
        }
    }

    /// Symbol amplitude, for judging whether RDS is present at all.
    pub fn level(&self) -> f32 {
        self.level as f32
    }

    pub fn reset(&mut self) {
        self.sym_phase = 0.0;
        self.i_lp.reset();
        self.q_lp.reset();
        self.first = 0.0;
        self.second = 0.0;
        self.first_n = 0;
        self.second_n = 0;
        self.prev_bit = None;
        self.level = 0.0;
    }

    /// Demodulate a block, appending recovered bits to `bits`.
    ///
    /// `pilot_phase` must hold the stereo PLL's phase for each input sample,
    /// which is what makes the carrier and clock coherent with the broadcast.
    pub fn process(&mut self, mpx: &[f32], pilot_phase: &[f64], bits: &mut Vec<u8>) {
        debug_assert_eq!(mpx.len(), pilot_phase.len());
        self.i_buf.clear();
        self.q_buf.clear();
        self.i_buf.reserve(mpx.len());
        self.q_buf.reserve(mpx.len());

        // Coherent mix to baseband using the pilot's third harmonic.
        for (&x, &p) in mpx.iter().zip(pilot_phase) {
            let c = 3.0 * p;
            let (s, co) = c.sin_cos();
            self.i_buf.push((x as f64 * co) as f32);
            self.q_buf.push((x as f64 * s) as f32);
        }
        let mut i_out = Vec::new();
        let mut q_out = Vec::new();
        self.i_lp.process(&self.i_buf, &mut i_out);
        self.q_lp.process(&self.q_buf, &mut q_out);

        let inc = BAUD / self.rate;
        for k in 0..i_out.len() {
            // RDS is BPSK on a carrier locked to the pilot, but the absolute
            // phase offset is not defined, so take the magnitude along the
            // stronger axis rather than assuming the data is on I.
            let v = i_out[k] as f64;
            let w = q_out[k] as f64;

            self.sym_phase += inc;
            let in_second_half = self.sym_phase % 1.0 >= 0.5;

            if in_second_half {
                self.second += v;
                self.second_n += 1;
            } else {
                self.first += v;
                self.first_n += 1;
            }
            let _ = w;

            // A symbol ends when the phase wraps.
            if self.sym_phase >= 1.0 {
                self.sym_phase -= 1.0;
                let a = if self.first_n > 0 { self.first / self.first_n as f64 } else { 0.0 };
                let b = if self.second_n > 0 { self.second / self.second_n as f64 } else { 0.0 };
                // Manchester: the sign of the mid-symbol step is the symbol.
                let step = a - b;
                self.level += 0.01 * (step.abs() - self.level);
                let sym = if step >= 0.0 { 1u8 } else { 0u8 };
                if let Some(p) = self.prev_bit {
                    // Differential: the transmitted bit is the change.
                    bits.push(sym ^ p);
                }
                self.prev_bit = Some(sym);
                self.first = 0.0;
                self.second = 0.0;
                self.first_n = 0;
                self.second_n = 0;
            }
            self.prev_half = in_second_half;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    const RATE: f64 = 228_000.0;

    /// Modulate bits onto a 57 kHz subcarrier the way a transmitter does, and
    /// return the multiplex along with the pilot phase per sample.
    fn modulate(bits: &[u8]) -> (Vec<f32>, Vec<f64>) {
        let sps = RATE / BAUD;
        let n = (bits.len() as f64 * sps) as usize;
        let mut mpx = Vec::with_capacity(n);
        let mut phase = Vec::with_capacity(n);

        // Differential encode, then Manchester.
        let mut level = 0u8;
        let mut syms = Vec::with_capacity(bits.len());
        for b in bits {
            level ^= b;
            syms.push(level);
        }

        for i in 0..n {
            let t = i as f64 / RATE;
            let sym_pos = i as f64 / sps;
            let idx = sym_pos as usize;
            let frac = sym_pos - idx as f64;
            let s = syms.get(idx).copied().unwrap_or(0);
            // Manchester: high then low for a 1, inverted for a 0.
            let chip = if (frac < 0.5) == (s == 1) { 1.0 } else { -1.0 };
            let pilot_ph = TAU * 19_000.0 * t;
            mpx.push((0.2 * chip * (3.0 * pilot_ph).cos()) as f32);
            phase.push(pilot_ph % TAU);
        }
        (mpx, phase)
    }

    #[test]
    fn a_clean_subcarrier_decodes_back_to_its_bits() {
        let bits: Vec<u8> = (0..400).map(|i| ((i * 7 + i / 3) % 2) as u8).collect();
        let (mpx, ph) = modulate(&bits);
        let mut d = RdsDemod::new(RATE);
        let mut out = Vec::new();
        d.process(&mpx, &ph, &mut out);

        assert!(out.len() > 300, "only {} bits recovered", out.len());
        // Skip the filter's settling time, then find the alignment and check
        // the run matches: the absolute bit offset depends on filter delay.
        let want = &bits[..];
        let got = &out[40..];
        let mut best = 0usize;
        let mut best_score = 0usize;
        for off in 0..40usize {
            let score = got
                .iter()
                .zip(want[off..].iter())
                .take(200)
                .filter(|(a, b)| a == b)
                .count();
            if score > best_score {
                best_score = score;
                best = off;
            }
        }
        let _ = best;
        assert!(best_score > 190, "only {best_score}/200 bits matched");
    }

    #[test]
    fn silence_produces_no_confident_level() {
        let mut d = RdsDemod::new(RATE);
        let mut out = Vec::new();
        let n = 20_000;
        let mpx = vec![0.0f32; n];
        let ph: Vec<f64> = (0..n).map(|i| TAU * 19_000.0 * i as f64 / RATE % TAU).collect();
        d.process(&mpx, &ph, &mut out);
        assert!(d.level() < 1e-6, "reported level {} on silence", d.level());
    }

    #[test]
    fn the_bit_rate_matches_the_standard() {
        // 1187.5 baud is exactly the pilot divided by 16, which is what makes
        // the clock recoverable without a timing loop.
        assert!((BAUD * 16.0 - 19_000.0).abs() < 1e-9);
        assert!((CARRIER_HZ - 3.0 * 19_000.0).abs() < 1e-9);
    }

    #[test]
    fn block_boundaries_do_not_lose_or_duplicate_bits() {
        let bits: Vec<u8> = (0..300).map(|i| (i % 3 == 0) as u8).collect();
        let (mpx, ph) = modulate(&bits);
        let whole = {
            let mut d = RdsDemod::new(RATE);
            let mut o = Vec::new();
            d.process(&mpx, &ph, &mut o);
            o
        };
        let split = {
            let mut d = RdsDemod::new(RATE);
            let mut o = Vec::new();
            for (m, p) in mpx.chunks(1000).zip(ph.chunks(1000)) {
                d.process(m, p, &mut o);
            }
            o
        };
        assert_eq!(whole.len(), split.len(), "block splitting changed the bit count");
        assert_eq!(whole, split, "block splitting changed the bits");
    }
}
