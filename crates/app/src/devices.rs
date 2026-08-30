//! Enumerating and opening radios across drivers.
//!
//! Kept in the app rather than `common`, because `common` defines the trait
//! every driver implements and must not depend on any of them.

use common::{Device, DriverKind, Error, Result, Sps};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: DriverKind,
    /// Position within that driver's own enumeration.
    pub index: usize,
    pub label: String,
}

/// Every attached radio, RTL-SDR first because they are the common case.
pub fn list() -> Vec<Entry> {
    let mut v = Vec::new();
    for d in rtlsdr::enumerate() {
        let name = if d.product.is_empty() { d.name.clone() } else { d.product.clone() };
        let tail = short(&d.serial);
        v.push(Entry {
            kind: DriverKind::RtlSdr,
            index: d.index as usize,
            label: if tail.is_empty() { name } else { format!("{name} {tail}") },
        });
    }
    for (i, serial) in hackrf::enumerate().into_iter().enumerate() {
        v.push(Entry {
            kind: DriverKind::HackRf,
            index: i,
            label: format!("HackRF One {}", short(&serial)),
        });
    }
    v
}

/// Serial tails identify a unit; the leading zeros do not.
fn short(s: &str) -> String {
    let t = s.trim_start_matches('0');
    if t.len() > 8 { t[t.len() - 8..].to_string() } else { t.to_string() }
}

pub fn open(e: &Entry) -> Result<Box<dyn Device>> {
    match e.kind {
        DriverKind::RtlSdr => Ok(Box::new(rtlsdr::RtlSdr::open(e.index as u32)?)),
        DriverKind::HackRf => Ok(Box::new(hackrf::HackRfDevice::open(e.index)?)),
        other => Err(Error::other(format!("{} cannot be opened live", other.as_str()))),
    }
}

/// Sample rates worth offering for a device, within what it supports.
///
/// The two radios barely overlap: an RTL-SDR tops out around 2.4 MS/s while a
/// HackRF starts at 2, so a single hard-coded list is wrong for both.
pub fn spans_for(range: &std::ops::RangeInclusive<Sps>) -> Vec<(String, f64)> {
    const CANDIDATES: [f64; 11] = [
        250_000.0,
        1_024_000.0,
        2_048_000.0,
        2_304_000.0,
        2_400_000.0,
        4_000_000.0,
        8_000_000.0,
        10_000_000.0,
        12_500_000.0,
        16_000_000.0,
        20_000_000.0,
    ];
    let (lo, hi) = (range.start().0 as f64, range.end().0 as f64);
    CANDIDATES
        .iter()
        .filter(|r| **r >= lo && **r <= hi)
        .map(|r| (label(*r), *r))
        .collect()
}

fn label(hz: f64) -> String {
    if hz >= 1e6 {
        let m = hz / 1e6;
        if (m - m.round()).abs() < 1e-9 {
            format!("{m:.0}M")
        } else {
            format!("{m:.3}M")
        }
    } else {
        format!("{:.0}k", hz / 1e3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_respect_what_the_device_can_do() {
        // An RTL-SDR cannot do 8 MS/s and a HackRF cannot do 250 kS/s.
        let rtl = spans_for(&(Sps(225_000)..=Sps(2_400_000)));
        assert!(rtl.iter().all(|(_, r)| *r <= 2_400_000.0));
        assert!(rtl.iter().any(|(_, r)| (*r - 2_400_000.0).abs() < 1.0));

        let hrf = spans_for(&(Sps(2_000_000)..=Sps(20_000_000)));
        assert!(hrf.iter().all(|(_, r)| *r >= 2_000_000.0));
        assert!(hrf.iter().any(|(_, r)| (*r - 20_000_000.0).abs() < 1.0));
        assert!(!hrf.iter().any(|(_, r)| (*r - 250_000.0).abs() < 1.0));
    }

    #[test]
    fn every_device_offers_at_least_one_rate() {
        for r in [Sps(225_000)..=Sps(2_400_000), Sps(2_000_000)..=Sps(20_000_000)] {
            assert!(!spans_for(&r).is_empty(), "no rates offered for {r:?}");
        }
    }

    #[test]
    fn rate_labels_are_readable() {
        assert_eq!(label(2_400_000.0), "2.400M");
        assert_eq!(label(8_000_000.0), "8M");
        assert_eq!(label(250_000.0), "250k");
    }

    #[test]
    fn serials_shorten_to_the_identifying_tail() {
        assert_eq!(short("0000000000000000457863dc3579c1df"), "3579c1df");
        assert_eq!(short("00000001"), "1");
        assert_eq!(short(""), "");
    }

    #[test]
    fn the_same_index_on_two_drivers_is_not_the_same_device() {
        let a = Entry { kind: DriverKind::RtlSdr, index: 0, label: "a".into() };
        let b = Entry { kind: DriverKind::HackRf, index: 0, label: "b".into() };
        assert_ne!(a, b, "device identity must include the driver");
    }
}
