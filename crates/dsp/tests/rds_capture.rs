//! RDS decoded from a recorded broadcast.
//!
//! `rds_endtoend.rs` proves the chain against a signal this code generated,
//! which cannot catch a wrong assumption shared by the encoder and the
//! decoder. This one is a real station: Spirit Radio on 92.4 MHz, identifier
//! 2208, name "SPIRIT".
//!
//! The identifier matters more than the name. It is constant for a station, so
//! one that changes between runs means the synchroniser is accepting noise
//! that happens to produce a valid syndrome, which is exactly the fault this
//! capture was recorded to catch.

use common::C32;
use dsp::rds::{BlockSync, GroupDecoder, RdsDemod};
use dsp::{FirDecim, FmDemod, StereoDecoder};

const FIXTURE: &str = "../../testdata/rds_92.4M_1024k.cu8";
const RATE: f64 = 1_024_000.0;
const EXPECT_PI: u16 = 0x2208;

struct Decoded {
    pi: Option<u16>,
    name: Option<String>,
    radiotext: Option<String>,
    groups: u64,
    bits: u64,
    stereo_locked: bool,
}

fn decode() -> Option<Decoded> {
    let raw = std::fs::read(FIXTURE).ok()?;
    let samples: Vec<C32> = raw
        .chunks_exact(2)
        .map(|p| C32::new((p[0] as f32 - 127.5) / 127.5, (p[1] as f32 - 127.5) / 127.5))
        .collect();

    // Occupied bandwidth is 264 kHz: Carson with RDS at 57 kHz as the highest
    // modulating frequency, not audio at 15 kHz.
    let dec = ((RATE / 330_000.0).round() as usize).max(1);
    let if_rate = RATE / dec as f64;
    let mut iff = FirDecim::design_hz(RATE, dec, 132_000.0, 70.0);
    let mut fm = FmDemod::new(if_rate, 75_000.0);
    let mut st = StereoDecoder::new(if_rate);
    let mut rds = RdsDemod::new(if_rate);
    let mut sync = BlockSync::new();
    let mut groups = GroupDecoder::new();

    let (mut iq, mut disc) = (Vec::new(), Vec::new());
    let (mut l, mut r, mut bits) = (Vec::new(), Vec::new(), Vec::new());
    let mut total = 0u64;

    for chunk in samples.chunks(262_144) {
        iq.clear();
        iff.process(chunk, &mut iq);
        disc.clear();
        fm.process(&iq, &mut disc);
        st.process(&disc, &mut l, &mut r);
        bits.clear();
        rds.process(&disc, st.phases(), &mut bits);
        total += bits.len() as u64;
        for b in &bits {
            if let Some(g) = sync.push(*b) {
                groups.push(&g);
            }
        }
    }

    let s = groups.station();
    Some(Decoded {
        pi: s.pi,
        name: s.name.clone(),
        radiotext: s.radiotext.clone(),
        groups: sync.groups,
        bits: total,
        stereo_locked: st.is_locked(),
    })
}

macro_rules! need {
    ($e:expr) => {
        match $e {
            Some(v) => v,
            None => {
                eprintln!("skipping: {FIXTURE} absent, run testdata/fetch.sh");
                return;
            }
        }
    };
}

#[test]
fn the_station_identifier_and_name_are_recovered() {
    let d = need!(decode());
    assert_eq!(d.pi, Some(EXPECT_PI), "wrong identifier from {} groups", d.groups);
    assert_eq!(d.name.as_deref(), Some("SPIRIT"));
}

#[test]
fn radiotext_is_recovered() {
    let d = need!(decode());
    let rt = d.radiotext.unwrap_or_default();
    assert!(rt.contains("Spirit"), "radiotext was {rt:?}");
}

#[test]
fn enough_groups_survive_to_be_useful() {
    // A group is 104 bits. Without forward error correction some blocks are
    // always lost, but a yield this far below what the air time allows would
    // mean the demodulator, not the reception, is the limit.
    let d = need!(decode());
    let possible = d.bits / 104;
    let yield_pct = 100.0 * d.groups as f64 / possible.max(1) as f64;
    assert!(
        yield_pct > 60.0,
        "only {yield_pct:.1}% of possible groups decoded ({} of {possible})",
        d.groups
    );
}

#[test]
fn the_pilot_is_found_on_a_real_broadcast() {
    let d = need!(decode());
    assert!(d.stereo_locked, "no stereo lock on a live station");
}
