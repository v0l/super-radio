//! Decode the real capture through a graph assembled at runtime from names
//! and settings, with no compile-time knowledge of the chain.
//!
//! `crates/decode/tests/fineoffset_capture.rs` already proves the DSP and the
//! protocol are right by calling them directly. This proves the *graph* adds
//! nothing and loses nothing: same recording, same answer, but routed through
//! the registry the way a user-configured chain would be.

use common::Hz;
use nodes::{build_chain, ook_chain, registry, NodeSpec};
use pipeline::event::Event;
use pipeline::{ParamValue, StreamSpec};
use sources::FileSource;

const FIXTURE: &str = "fineoffset_wh1080_433.92M_250k.cu8";

fn fixture() -> Option<common::IqBuf> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(FIXTURE);
    if !p.exists() {
        return None;
    }
    Some(FileSource::open(&p).ok()?.read_all().ok()?)
}

macro_rules! need_fixture {
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

/// The chain that decodes this capture. The recording is already at baseband
/// and at 250 kS/s, so no shift and no decimation are needed.
fn chain_specs() -> Vec<NodeSpec> {
    vec![
        NodeSpec::new("envelope"),
        NodeSpec::new("pulse_detect").f("reset_us", 10_000.0).i("min_pulses", 20),
        NodeSpec::new("protocol_decode"),
    ]
}

fn decodes_from(graph_events: &[Event]) -> Vec<String> {
    graph_events
        .iter()
        .filter_map(|e| match e {
            Event::Decoded(d) => d.text.clone(),
            _ => None,
        })
        .collect()
}

#[test]
fn a_runtime_assembled_graph_decodes_the_real_capture() {
    let buf = need_fixture!(fixture());
    let spec = StreamSpec::iq(buf.rate.as_f64(), buf.center);
    let mut g = build_chain(spec, &chain_specs(), &registry()).expect("build chain");

    let events = g.feed_iq(&buf.samples).expect("run graph").to_vec();
    let decodes = decodes_from(&events);

    assert_eq!(decodes.len(), 1, "expected one decode, got {decodes:?}");
    let text = &decodes[0];
    // Same ground truth as the direct test: rtl_433 25.02 on this recording.
    assert!(text.contains("Fineoffset-WHx080"), "{text}");
    assert!(text.contains("station_id=196"), "{text}");
    assert!(text.contains("temperature_c=16.2"), "{text}");
    assert!(text.contains("humidity_pct=89"), "{text}");
    assert!(text.contains("rain_total_mm=84.3"), "{text}");
    assert!(text.contains("[CRC ok]"), "{text}");
}

#[test]
fn the_graph_negotiates_rates_and_kinds_correctly() {
    let buf = need_fixture!(fixture());
    let spec = StreamSpec::iq(buf.rate.as_f64(), buf.center);
    let g = build_chain(spec, &chain_specs(), &registry()).unwrap();

    let names: Vec<&str> = g.order().map(|(_, n)| n).collect();
    assert_eq!(names, vec!["envelope", "pulse_detect", "protocol_decode"]);
    assert_eq!(g.output_spec().kind, pipeline::PortKind::Bytes);
}

#[test]
fn a_misordered_chain_fails_at_build_with_an_actionable_message() {
    // Pulse detection before the envelope: the classic mistake.
    let specs = vec![
        NodeSpec::new("pulse_detect"),
        NodeSpec::new("envelope"),
    ];
    let spec = StreamSpec::iq(250_000.0, Hz::mhz(433));
    let err = build_chain(spec, &specs, &registry()).unwrap_err().to_string();
    assert!(err.contains("pulse_detect"), "{err}");
    assert!(err.contains("envelope"), "error should say how to fix it: {err}");
}

#[test]
fn an_unknown_node_type_lists_what_is_available() {
    let specs = vec![NodeSpec::new("magic_decoder")];
    let spec = StreamSpec::iq(250_000.0, Hz::mhz(433));
    let err = build_chain(spec, &specs, &registry()).unwrap_err().to_string();
    assert!(err.contains("magic_decoder"), "{err}");
    assert!(err.contains("pulse_detect"), "should list known types: {err}");
}

#[test]
fn retuning_a_parameter_at_runtime_changes_behaviour() {
    // The point of the whole exercise: an ambiguous signal is handled by
    // reconfiguring the chain, not recompiling. A reset gap far below Fine
    // Offset's ~1 ms inter-symbol gaps must fragment the packet and stop it
    // decoding; restoring it must bring the decode back.
    let buf = need_fixture!(fixture());
    let spec = StreamSpec::iq(buf.rate.as_f64(), buf.center);

    let bad = vec![
        NodeSpec::new("envelope"),
        NodeSpec::new("pulse_detect").f("reset_us", 600.0).i("min_pulses", 20),
        NodeSpec::new("protocol_decode"),
    ];
    let mut g = build_chain(spec, &bad, &registry()).unwrap();
    let events = g.feed_iq(&buf.samples).unwrap().to_vec();
    assert!(
        decodes_from(&events).is_empty(),
        "too short a reset gap should have fragmented the packet"
    );

    // Now fix it in place, without rebuilding the graph.
    let id = pipeline::NodeId(1);
    g.node_mut(id)
        .unwrap()
        .set_param("reset_us", ParamValue::Float(10_000.0))
        .expect("set reset_us");
    g.negotiate().expect("renegotiate");
    g.reset();

    let events = g.feed_iq(&buf.samples).unwrap().to_vec();
    assert_eq!(
        decodes_from(&events).len(),
        1,
        "restoring the reset gap should decode again"
    );
}

#[test]
fn an_unrecognised_burst_is_reported_rather_than_silently_dropped() {
    // A chain whose timings cannot match anything should still say what it
    // saw. Silence is the worst possible output for an unknown signal.
    let buf = need_fixture!(fixture());
    let spec = StreamSpec::iq(buf.rate.as_f64(), buf.center);
    let specs = vec![
        NodeSpec::new("envelope"),
        // Decimating the envelope by 20 scales every pulse width by 20 and
        // makes the frame unmatchable.
        NodeSpec::new("real_decimate").i("factor", 20),
        NodeSpec::new("pulse_detect").f("reset_us", 10_000.0).i("min_pulses", 20),
        NodeSpec::new("protocol_decode"),
    ];
    let mut g = build_chain(spec, &specs, &registry()).unwrap();
    let events = g.feed_iq(&buf.samples).unwrap().to_vec();

    assert!(decodes_from(&events).is_empty(), "should not have decoded");
    let warnings: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e, Event::Warning { .. }))
        .collect();
    assert!(!warnings.is_empty(), "an unknown burst must be reported: {events:?}");
}

#[test]
fn the_registry_describes_every_node_for_a_ui() {
    let r = registry();
    let names: Vec<&str> = r.list().map(|d| d.name).collect();
    for want in ["mixer", "decimate", "envelope", "fm_demod", "pulse_detect", "protocol_decode"] {
        assert!(names.contains(&want), "registry is missing {want}: {names:?}");
    }
    // Categories let a UI group the palette without hard-coding node names.
    assert!(r.by_category("decode").count() >= 2);
    assert!(r.by_category("filter").count() >= 3);
    for d in r.list() {
        assert!(!d.summary.is_empty(), "{} has no summary", d.name);
    }
}

#[test]
fn every_node_exposes_its_parameters() {
    let r = registry();
    let spec = StreamSpec::iq(250_000.0, Hz::mhz(433));
    for desc in r.list() {
        let g = build_chain(spec, &[NodeSpec::new(desc.name)], &r);
        // Some nodes reject an IQ input; only check the ones that accept it.
        let Ok(g) = g else { continue };
        let n = g.node(pipeline::NodeId(0)).unwrap();
        for p in n.params() {
            assert!(!p.name.is_empty(), "{} has an unnamed parameter", desc.name);
            assert!(
                !p.display_label().is_empty(),
                "{} parameter {} has no label",
                desc.name,
                p.name
            );
        }
    }
}


#[test]
fn a_mistuned_detector_says_what_it_discarded_and_which_knob_to_turn() {
    // The failure mode that matters most in practice. A reset gap below Fine
    // Offset's ~1 ms inter-symbol spacing fragments one packet into dozens of
    // short ones, all filtered out by min_pulses. Reporting zero events would
    // be indistinguishable from a dead antenna.
    let buf = need_fixture!(fixture());
    let spec = StreamSpec::iq(buf.rate.as_f64(), buf.center);
    let specs = vec![
        NodeSpec::new("envelope"),
        NodeSpec::new("pulse_detect").f("reset_us", 600.0).i("min_pulses", 20),
        NodeSpec::new("protocol_decode"),
    ];
    let mut g = build_chain(spec, &specs, &registry()).unwrap();
    let events = g.feed_iq(&buf.samples).unwrap().to_vec();

    assert!(decodes_from(&events).is_empty());
    let msg = events
        .iter()
        .find_map(|e| match e {
            Event::Warning { stage, message } if stage == "pulse_detect" => Some(message.clone()),
            _ => None,
        })
        .expect("a mistuned detector must not fail silently");
    assert!(msg.contains("discarded"), "{msg}");
    assert!(msg.contains("min_pulses") || msg.contains("reset_us"), "must name a knob: {msg}");
}
