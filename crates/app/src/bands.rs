//! Service allocations, drawn as a ribbon under the spectrum.
//!
//! A bare frequency axis tells you where you are but not what you are looking
//! at. Naming the allocation turns the span into something readable, and the
//! same table picks a sensible demodulator when you click.

use crate::radio::Demod;
use egui::Color32;

pub struct Band {
    pub lo: f64,
    pub hi: f64,
    pub name: &'static str,
    pub demod: Demod,
    pub color: Color32,
    /// Legal channel spacing, where the band has one.
    ///
    /// `None` means tune anywhere: the amateur and ISM bands have no raster,
    /// and neither do the bands here whose real channel lists are specific
    /// frequencies rather than an even step. Claiming a spacing that does not
    /// exist would snap a channel away from the signal it was aimed at.
    pub raster: Option<Raster>,
}

/// An evenly spaced channel plan.
#[derive(Clone, Copy, Debug)]
pub struct Raster {
    pub step: f64,
    /// A frequency the plan lands on. Most rasters happen to align with zero,
    /// but PMR446 is offset by half a channel and would be wrong without this.
    pub origin: f64,
}

impl Raster {
    pub const fn step(step: f64) -> Self {
        Self { step, origin: 0.0 }
    }

    pub const fn from(origin: f64, step: f64) -> Self {
        Self { step, origin }
    }

    pub fn snap(&self, hz: f64) -> f64 {
        self.origin + ((hz - self.origin) / self.step).round() * self.step
    }
}

const BROADCAST: Color32 = Color32::from_rgb(0x4A, 0x6F, 0x8A);
const AERO: Color32 = Color32::from_rgb(0x8A, 0x6B, 0x4A);
const AMATEUR: Color32 = Color32::from_rgb(0x53, 0x7A, 0x5C);
const UTILITY: Color32 = Color32::from_rgb(0x6B, 0x5A, 0x7A);
const ISM: Color32 = Color32::from_rgb(0x8A, 0x4A, 0x55);

/// Region 1 (Europe) allocations, coarse enough to stay readable.
pub const BANDS: &[Band] = &[
    Band { lo: 26.965e6, hi: 27.405e6, name: "CB", demod: Demod::Am, color: UTILITY, raster: Some(Raster::from(26.965e6, 10_000.0)) },
    Band { lo: 28.0e6, hi: 29.7e6, name: "10 m", demod: Demod::Nfm, color: AMATEUR, raster: None },
    Band { lo: 50.0e6, hi: 52.0e6, name: "6 m", demod: Demod::Nfm, color: AMATEUR, raster: None },
    Band { lo: 76.0e6, hi: 87.5e6, name: "Band II low", demod: Demod::Wfm, color: BROADCAST, raster: Some(Raster::step(100_000.0)) },
    Band { lo: 87.5e6, hi: 108.0e6, name: "FM broadcast", demod: Demod::Wfm, color: BROADCAST, raster: Some(Raster::step(100_000.0)) },
    Band { lo: 108.0e6, hi: 117.975e6, name: "VOR / ILS", demod: Demod::Am, color: AERO, raster: Some(Raster::step(50_000.0)) },
    Band { lo: 117.975e6, hi: 137.0e6, name: "Airband", demod: Demod::Am, color: AERO, raster: Some(Raster::step(25_000.0)) },
    Band { lo: 137.0e6, hi: 138.0e6, name: "Weather sat", demod: Demod::Wfm, color: UTILITY, raster: None },
    Band { lo: 144.0e6, hi: 146.0e6, name: "2 m", demod: Demod::Nfm, color: AMATEUR, raster: Some(Raster::step(12_500.0)) },
    Band { lo: 146.0e6, hi: 156.0e6, name: "Land mobile", demod: Demod::Nfm, color: UTILITY, raster: Some(Raster::step(12_500.0)) },
    Band { lo: 156.0e6, hi: 162.05e6, name: "Marine VHF", demod: Demod::Nfm, color: UTILITY, raster: Some(Raster::step(25_000.0)) },
    Band { lo: 174.0e6, hi: 230.0e6, name: "DAB / Band III", demod: Demod::Nfm, color: BROADCAST, raster: None },
    Band { lo: 240.0e6, hi: 270.0e6, name: "Milair UHF", demod: Demod::Am, color: AERO, raster: Some(Raster::step(25_000.0)) },
    Band { lo: 380.0e6, hi: 400.0e6, name: "TETRA", demod: Demod::Nfm, color: UTILITY, raster: Some(Raster::step(25_000.0)) },
    Band { lo: 430.0e6, hi: 440.0e6, name: "70 cm", demod: Demod::Nfm, color: AMATEUR, raster: Some(Raster::step(12_500.0)) },
    Band { lo: 446.0e6, hi: 446.2e6, name: "PMR446", demod: Demod::Nfm, color: UTILITY, raster: Some(Raster::from(446.00625e6, 12_500.0)) },
    Band { lo: 433.05e6, hi: 434.79e6, name: "ISM 433", demod: Demod::Nfm, color: ISM, raster: None },
    Band { lo: 862.0e6, hi: 876.0e6, name: "ISM 868", demod: Demod::Nfm, color: ISM, raster: None },
    Band { lo: 1090.0e6, hi: 1090.1e6, name: "ADS-B", demod: Demod::Am, color: AERO, raster: None },
];

/// The narrowest band containing `hz`, so ISM 433 wins over the 70 cm band it
/// sits inside.
pub fn at(hz: f64) -> Option<&'static Band> {
    BANDS
        .iter()
        .filter(|b| hz >= b.lo && hz < b.hi)
        .min_by(|a, b| (a.hi - a.lo).partial_cmp(&(b.hi - b.lo)).unwrap())
}

pub fn demod_at(hz: f64) -> Demod {
    at(hz).map(|b| b.demod).unwrap_or(Demod::Nfm)
}

pub fn name_at(hz: f64) -> &'static str {
    at(hz).map(|b| b.name).unwrap_or("unallocated")
}

/// Nearest legal channel frequency, or `hz` unchanged where the band has no
/// raster or is unallocated.
pub fn snap(hz: f64) -> f64 {
    match at(hz).and_then(|b| b.raster) {
        Some(r) => r.snap(hz),
        None => hz,
    }
}

/// The raster covering `hz`, for telling the operator what snapping will do.
pub fn raster_at(hz: f64) -> Option<Raster> {
    at(hz).and_then(|b| b.raster)
}

/// Bands overlapping a span, for drawing the ribbon.
pub fn in_span(lo: f64, hi: f64) -> impl Iterator<Item = &'static Band> {
    BANDS.iter().filter(move |b| b.hi > lo && b.lo < hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_ordered_and_sane() {
        for b in BANDS {
            assert!(b.hi > b.lo, "{} has hi <= lo", b.name);
        }
    }

    #[test]
    fn known_frequencies_land_in_the_right_band() {
        assert_eq!(name_at(95.8e6), "FM broadcast");
        assert_eq!(name_at(124.0e6), "Airband");
        assert_eq!(name_at(145.5e6), "2 m");
        assert_eq!(name_at(156.8e6), "Marine VHF");
    }

    #[test]
    fn the_narrowest_band_wins_when_they_overlap() {
        // 433.92 is inside both the 70 cm amateur band and ISM 433; the ISM
        // allocation is the more useful label and the narrower entry.
        assert_eq!(name_at(433.92e6), "ISM 433");
    }

    #[test]
    fn modes_match_the_service() {
        assert_eq!(demod_at(95.8e6), Demod::Wfm);
        assert_eq!(demod_at(124.0e6), Demod::Am, "airband is AM, not FM");
        assert_eq!(demod_at(446.1e6), Demod::Nfm);
    }

    #[test]
    fn unallocated_spectrum_falls_back_to_narrow_fm() {
        assert_eq!(name_at(70.0e6), "unallocated");
        assert_eq!(demod_at(70.0e6), Demod::Nfm);
    }

    #[test]
    fn a_span_selects_only_overlapping_bands() {
        let v: Vec<_> = in_span(95.0e6, 96.0e6).map(|b| b.name).collect();
        assert_eq!(v, ["FM broadcast"]);
        let wide: Vec<_> = in_span(100.0e6, 140.0e6).map(|b| b.name).collect();
        assert!(wide.contains(&"Airband") && wide.contains(&"VOR / ILS"));
    }

    #[test]
    fn band_edges_are_half_open() {
        // 108.0 is the top of FM broadcast and the bottom of VOR; it must
        // belong to exactly one of them.
        assert_eq!(name_at(108.0e6), "VOR / ILS");
        assert_eq!(name_at(107.999e6), "FM broadcast");
    }
}

#[cfg(test)]
mod raster_tests {
    use super::*;

    #[test]
    fn fm_broadcast_snaps_to_a_hundred_kilohertz() {
        assert_eq!(snap(92_401_300.0), 92_400_000.0);
        assert_eq!(snap(92_460_000.0), 92_500_000.0);
        assert_eq!(snap(95_800_000.0), 95_800_000.0);
    }

    #[test]
    fn airband_snaps_to_twenty_five_kilohertz() {
        assert_eq!(snap(118_001_000.0), 118_000_000.0);
        assert_eq!(snap(118_030_000.0), 118_025_000.0);
    }

    #[test]
    fn pmr446_is_offset_by_half_a_channel() {
        // The plan runs 446.00625, 446.01875 and so on. Assuming a raster
        // aligned to zero would land every channel 6.25 kHz off, which is half
        // a channel and squarely on the adjacent one's edge.
        assert_eq!(snap(446_006_000.0), 446_006_250.0);
        assert_eq!(snap(446_020_000.0), 446_018_750.0);
    }

    #[test]
    fn bands_without_a_plan_do_not_move() {
        // ISM and the amateur bands are tune-anywhere, and snapping a channel
        // off the signal it was aimed at is worse than not snapping at all.
        for hz in [433_920_000.0, 144_312_500.0, 868_300_000.0] {
            assert_eq!(snap(hz), hz, "{hz} was moved by a band with no plan");
        }
    }

    #[test]
    fn unallocated_spectrum_does_not_move() {
        assert_eq!(snap(70_123_456.0), 70_123_456.0);
    }

    #[test]
    fn every_raster_lands_inside_its_own_band() {
        // A plan whose origin sits outside the band would snap the first
        // channel out of it entirely.
        for b in BANDS {
            let Some(r) = b.raster else { continue };
            let mid = (b.lo + b.hi) / 2.0;
            let s = r.snap(mid);
            assert!(
                s >= b.lo && s < b.hi,
                "{}: midpoint snapped to {s}, outside {}..{}",
                b.name,
                b.lo,
                b.hi
            );
            assert!(r.step > 0.0, "{}: non-positive step", b.name);
            // A step wider than the band means one channel, which is not a plan.
            assert!(r.step < b.hi - b.lo, "{}: step is wider than the band", b.name);
        }
    }

    #[test]
    fn snapping_never_moves_further_than_half_a_channel() {
        for b in BANDS {
            let Some(r) = b.raster else { continue };
            let mut hz = b.lo;
            while hz < b.hi {
                assert!(
                    (r.snap(hz) - hz).abs() <= r.step / 2.0 + 1e-6,
                    "{}: {hz} moved more than half a channel",
                    b.name
                );
                hz += r.step / 3.0;
            }
        }
    }
}
