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
}

const BROADCAST: Color32 = Color32::from_rgb(0x4A, 0x6F, 0x8A);
const AERO: Color32 = Color32::from_rgb(0x8A, 0x6B, 0x4A);
const AMATEUR: Color32 = Color32::from_rgb(0x53, 0x7A, 0x5C);
const UTILITY: Color32 = Color32::from_rgb(0x6B, 0x5A, 0x7A);
const ISM: Color32 = Color32::from_rgb(0x8A, 0x4A, 0x55);

/// Region 1 (Europe) allocations, coarse enough to stay readable.
pub const BANDS: &[Band] = &[
    Band { lo: 26.965e6, hi: 27.405e6, name: "CB", demod: Demod::Am, color: UTILITY },
    Band { lo: 28.0e6, hi: 29.7e6, name: "10 m", demod: Demod::Nfm, color: AMATEUR },
    Band { lo: 50.0e6, hi: 52.0e6, name: "6 m", demod: Demod::Nfm, color: AMATEUR },
    Band { lo: 76.0e6, hi: 87.5e6, name: "Band II low", demod: Demod::Wfm, color: BROADCAST },
    Band { lo: 87.5e6, hi: 108.0e6, name: "FM broadcast", demod: Demod::Wfm, color: BROADCAST },
    Band { lo: 108.0e6, hi: 117.975e6, name: "VOR / ILS", demod: Demod::Am, color: AERO },
    Band { lo: 117.975e6, hi: 137.0e6, name: "Airband", demod: Demod::Am, color: AERO },
    Band { lo: 137.0e6, hi: 138.0e6, name: "Weather sat", demod: Demod::Wfm, color: UTILITY },
    Band { lo: 144.0e6, hi: 146.0e6, name: "2 m", demod: Demod::Nfm, color: AMATEUR },
    Band { lo: 146.0e6, hi: 156.0e6, name: "Land mobile", demod: Demod::Nfm, color: UTILITY },
    Band { lo: 156.0e6, hi: 162.05e6, name: "Marine VHF", demod: Demod::Nfm, color: UTILITY },
    Band { lo: 174.0e6, hi: 230.0e6, name: "DAB / Band III", demod: Demod::Nfm, color: BROADCAST },
    Band { lo: 240.0e6, hi: 270.0e6, name: "Milair UHF", demod: Demod::Am, color: AERO },
    Band { lo: 380.0e6, hi: 400.0e6, name: "TETRA", demod: Demod::Nfm, color: UTILITY },
    Band { lo: 430.0e6, hi: 440.0e6, name: "70 cm", demod: Demod::Nfm, color: AMATEUR },
    Band { lo: 446.0e6, hi: 446.2e6, name: "PMR446", demod: Demod::Nfm, color: UTILITY },
    Band { lo: 433.05e6, hi: 434.79e6, name: "ISM 433", demod: Demod::Nfm, color: ISM },
    Band { lo: 862.0e6, hi: 876.0e6, name: "ISM 868", demod: Demod::Nfm, color: ISM },
    Band { lo: 1090.0e6, hi: 1090.1e6, name: "ADS-B", demod: Demod::Am, color: AERO },
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
