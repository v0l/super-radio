//! Shared machinery for thresholding a buffered burst that has two levels.
//!
//! Used by the FSK detector, where the two levels are frequencies, and by the
//! ASK detector, where they are amplitudes. Neither can use a threshold fixed
//! in advance: the FSK tones sit wherever the crystal error puts them, and the
//! ASK levels sit wherever the path loss and the AGC put them. Both can only
//! be thresholded against levels measured from the burst itself, which is why
//! both buffer the burst first and why the code is worth sharing.

use crate::pulse::Pulse;

/// The two levels of a burst, as percentiles rather than the true extremes.
///
/// A single outlier decides a min or a max, and both inputs produce them: a
/// discriminator throws huge spikes whenever the signal passes near zero, and
/// an envelope has noise peaks. Either would drag a level to a place no real
/// sample sits and put the threshold beyond the data.
///
/// `NaN` entries are samples with no evidence about either level and are
/// skipped. Returns `None` when too little is left to say anything.
pub(crate) fn levels(scratch: &mut Vec<f32>, samples: &[f32]) -> Option<(f32, f32)> {
    scratch.clear();
    scratch.extend(samples.iter().copied().filter(|v| !v.is_nan()));
    if scratch.len() < 16 {
        return None;
    }
    scratch.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = |q: f32| ((scratch.len() - 1) as f32 * q).round() as usize;
    Some((scratch[idx(0.02)], scratch[idx(0.98)]))
}

/// Threshold a burst into alternating runs of `(is_high, samples)`.
///
/// Runs shorter than `min_run` are merged into the run they interrupted rather
/// than dropped: dropping one would join the runs either side into a single
/// run of the wrong length and corrupt every timing after it, which is worse
/// than the glitch. `NaN` samples hold the current level instead of deciding
/// one, so a dropout cannot fabricate a transition.
pub(crate) fn runs(
    samples: &[f32],
    mid: f32,
    hyst: f32,
    min_run: usize,
    glitches: &mut u64,
) -> Vec<(bool, usize)> {
    let mut raw: Vec<(bool, usize)> = Vec::new();
    let mut state = false;
    for (i, &v) in samples.iter().enumerate() {
        let level = if v.is_nan() {
            state
        } else if state {
            v > mid - hyst
        } else {
            v > mid + hyst
        };
        if i == 0 || level != state {
            state = level;
            raw.push((level, 1));
        } else {
            raw.last_mut().unwrap().1 += 1;
        }
    }

    let min_run = min_run.max(1);
    let mut merged: Vec<(bool, usize)> = Vec::with_capacity(raw.len());
    for (level, n) in raw {
        match merged.last_mut() {
            Some(last) if n < min_run && last.0 != level => {
                last.1 += n;
                *glitches += 1;
            }
            Some(last) if last.0 == level => last.1 += n,
            _ => merged.push((level, n)),
        }
    }
    merged
}

/// Pair alternating runs into mark/gap pulses, high level first.
///
/// A burst that opens on the low level loses that first run: a gap with no
/// mark before it has no pulse to belong to, and it is the tail of the silence
/// anyway.
pub(crate) fn pair_runs(runs: &[(bool, usize)], us_per_sample: f64, reset_us: u32) -> Vec<Pulse> {
    let us = |n: usize| (n as f64 * us_per_sample).round() as u32;
    let mut pulses = Vec::with_capacity(runs.len() / 2 + 1);
    let mut i = usize::from(runs.first().is_some_and(|(level, _)| !*level));
    while i < runs.len() {
        // The last mark's gap is the silence that ended the burst, reported as
        // the reset timeout so it reads the same as an OOK package.
        let gap = runs.get(i + 1).map(|(_, n)| us(*n)).unwrap_or(reset_us);
        pulses.push(Pulse { mark: us(runs[i].1), gap });
        i += 2;
    }
    pulses
}
