//! Splitting one gain figure across the HackRF's three stages.
//!
//! The hardware exposes an RF amp, an LNA and a baseband VGA separately.
//! `rs-hackrf` sets each one; deciding how much to ask of each is ours.

/// LNA gain is 0-40 dB in 8 dB steps.
pub fn quantise_lna(db: f32) -> u32 {
    ((db.clamp(0.0, 40.0) / 8.0).round() as u32) * 8
}

/// VGA gain is 0-62 dB in 2 dB steps.
pub fn quantise_vga(db: f32) -> u32 {
    ((db.clamp(0.0, 62.0) / 2.0).round() as u32) * 2
}

/// Front-end amp contribution when switched in.
pub const AMP_DB: f32 = 14.0;
/// Total gain available across all three stages.
pub const MAX_DB: f32 = AMP_DB + 40.0 + 62.0;

/// Distribute a requested total across (amp, lna, vga).
///
/// Split roughly evenly rather than filling the LNA first. The two stages do
/// different jobs: the LNA sets the noise figure, the VGA drives the ADC. All
/// 40 dB in the LNA with the VGA at zero measures 30 dB below what the same
/// total delivers when shared, because the converter is left starved.
///
/// The front-end amp stays off until the other two are exhausted, since it
/// costs noise figure and overloads easily on a crowded band.
pub fn distribute(total_db: f32) -> (bool, u32, u32) {
    let t = total_db.clamp(0.0, MAX_DB);
    let amp = t > 40.0 + 62.0;
    let left = if amp { t - AMP_DB } else { t };

    let mut lna = quantise_lna((left * 0.55).min(40.0));
    let mut vga = quantise_vga((left - lna as f32).clamp(0.0, 62.0));
    // Past the VGA's ceiling the remainder has nowhere else to go.
    if left - lna as f32 > 62.0 {
        lna = quantise_lna((left - 62.0).min(40.0));
        vga = 62;
    }
    (amp, lna, vga)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn achieved(db: f32) -> u32 {
        let (a, l, v) = distribute(db);
        (if a { AMP_DB as u32 } else { 0 }) + l + v
    }

    #[test]
    fn gains_land_on_the_hardware_steps() {
        for db in 0..=116 {
            let (_, l, v) = distribute(db as f32);
            assert_eq!(l % 8, 0, "LNA {l} is not a multiple of 8");
            assert_eq!(v % 2, 0, "VGA {v} is not a multiple of 2");
            assert!(l <= 40 && v <= 62);
        }
    }

    #[test]
    fn gain_is_shared_rather_than_filling_one_stage() {
        // Both stages must contribute at a normal operating point, or the ADC
        // is starved even though the requested total looks right.
        for db in [30.0f32, 40.0, 60.0, 80.0] {
            let (_, lna, vga) = distribute(db);
            assert!(lna > 0, "LNA idle at {db} dB");
            assert!(vga > 0, "VGA idle at {db} dB, the converter would be starved");
        }
    }

    #[test]
    fn quiet_stages_are_used_before_the_amp() {
        let (amp, lna, _) = distribute(30.0);
        assert!(!amp, "amp switched in for only 30 dB");
        assert!(lna > 0, "LNA should take gain before the VGA");
        assert!(distribute(110.0).0, "amp should engage once the rest is exhausted");
    }

    #[test]
    fn more_requested_never_means_less_delivered() {
        let mut prev = 0;
        for i in 0..=116 {
            let t = achieved(i as f32);
            // Steps are coarse, so allow a step of slack but never a real drop.
            assert!(t + 8 >= prev, "gain fell from {prev} to {t} at {i} dB");
            prev = t;
        }
    }

    #[test]
    fn the_request_is_tracked_within_one_step() {
        for db in [0.0f32, 10.0, 24.0, 40.0, 60.0, 90.0, 102.0] {
            let got = achieved(db) as f32;
            assert!((got - db).abs() <= 8.0, "asked {db}, got {got}");
        }
    }

    #[test]
    fn out_of_range_requests_clamp() {
        assert_eq!(distribute(-50.0), (false, 0, 0));
        let (amp, lna, vga) = distribute(1e6);
        assert!(amp && lna == 40 && vga == 62);
    }
}
