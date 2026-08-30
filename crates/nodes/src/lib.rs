//! Graph nodes and the registry that builds them by name.
//!
//! The registry is what lets a chain be described as data. That matters for
//! ambiguous signals: when a burst does not decode, the answer is usually to
//! change the chain (a different decimation, a longer reset gap, FM
//! discrimination instead of an envelope), and that should be a
//! reconfiguration rather than a recompile.

pub mod bank;
pub mod decode_nodes;
pub mod dsp_nodes;
pub mod wfm;

pub use bank::{ChannelBank, ChannelEvent, Gating};
pub use decode_nodes::{ProtocolDecodeNode, PulseDetectNode};
pub use dsp_nodes::{
    DecimateNode, DeemphasisNode, EnvelopeNode, FmDemodNode, MixerNode, RealDecimateNode,
};

use common::Result;
use dsp::PulseConfig;
use pipeline::node::Node;
use pipeline::registry::{Registry, Settings, SettingsExt, StageDesc};
use pipeline::{Graph, StreamSpec};

/// Every node type compiled into this build.
pub fn registry() -> Registry {
    let mut r = Registry::new();

    r.register(
        StageDesc {
            name: "mixer",
            summary: "Shift the signal in frequency, to bring an off-centre \
                      carrier to baseband and away from the DC spur",
            category: "filter",
        },
        |s: &Settings| Ok(Box::new(MixerNode::new(s.f64_or("shift_hz", 0.0))) as Box<dyn Node>),
    );

    r.register(
        StageDesc {
            name: "decimate",
            summary: "Lowpass and reduce the sample rate of an IQ stream",
            category: "filter",
        },
        |s: &Settings| {
            Ok(Box::new(DecimateNode::new(s.i64_or("factor", 1).max(1) as usize))
                as Box<dyn Node>)
        },
    );

    r.register(
        StageDesc {
            name: "real_decimate",
            summary: "Reduce the sample rate of a real stream, for audio",
            category: "filter",
        },
        |s: &Settings| {
            Ok(Box::new(RealDecimateNode::new(s.i64_or("factor", 1).max(1) as usize))
                as Box<dyn Node>)
        },
    );

    r.register(
        StageDesc {
            name: "envelope",
            summary: "Complex magnitude; the input an OOK pulse detector needs",
            category: "demod",
        },
        |_s: &Settings| Ok(Box::new(EnvelopeNode) as Box<dyn Node>),
    );

    r.register(
        StageDesc {
            name: "fm_demod",
            summary: "Quadrature frequency discriminator, for FM and FSK",
            category: "demod",
        },
        |s: &Settings| {
            Ok(Box::new(FmDemodNode::new(s.f64_or("deviation_hz", 75_000.0)))
                as Box<dyn Node>)
        },
    );

    r.register(
        StageDesc {
            name: "deemphasis",
            summary: "Undo broadcast FM pre-emphasis (50 us in Europe, 75 in the Americas)",
            category: "filter",
        },
        |s: &Settings| {
            Ok(Box::new(DeemphasisNode::new(s.f64_or("tau_us", 50.0))) as Box<dyn Node>)
        },
    );

    r.register(
        StageDesc {
            name: "pulse_detect",
            summary: "Envelope to mark/gap timings; the boundary between DSP \
                      and protocol parsing",
            category: "decode",
        },
        |s: &Settings| {
            let d = PulseConfig::default();
            let cfg = PulseConfig {
                reset_us: s.f64_or("reset_us", d.reset_us as f64) as u32,
                min_mark_us: s.f64_or("min_mark_us", d.min_mark_us as f64) as u32,
                min_pulses: s.i64_or("min_pulses", d.min_pulses as i64).max(1) as usize,
                min_snr_db: s.f64_or("min_snr_db", d.min_snr_db as f64) as f32,
                hysteresis: s.f64_or("hysteresis", d.hysteresis as f64) as f32,
                noise_threshold_ratio: s.f64_or("noise_threshold_ratio", d.noise_threshold_ratio as f64) as f32,
                tau_us: s.f64_or("tau_us", d.tau_us as f64) as f32,
            };
            Ok(Box::new(PulseDetectNode::new(cfg)) as Box<dyn Node>)
        },
    );

    r.register(
        StageDesc {
            name: "protocol_decode",
            summary: "Try every known protocol against each burst",
            category: "decode",
        },
        |_s: &Settings| Ok(Box::new(ProtocolDecodeNode::all()) as Box<dyn Node>),
    );

    r
}

/// One node in a chain description.
#[derive(Clone, Debug, Default)]
pub struct NodeSpec {
    pub kind: String,
    pub settings: Settings,
}

impl NodeSpec {
    pub fn new(kind: &str) -> Self {
        Self { kind: kind.into(), settings: Settings::new() }
    }

    pub fn set(mut self, k: &str, v: pipeline::ParamValue) -> Self {
        self.settings.insert(k.into(), v);
        self
    }

    pub fn f(self, k: &str, v: f64) -> Self {
        self.set(k, pipeline::ParamValue::Float(v))
    }

    pub fn i(self, k: &str, v: i64) -> Self {
        self.set(k, pipeline::ParamValue::Int(v))
    }

    pub fn b(self, k: &str, v: bool) -> Self {
        self.set(k, pipeline::ParamValue::Bool(v))
    }
}

/// Build a linear graph from a chain description.
///
/// Errors name the offending node and its index, because a chain assembled
/// from a config file is exactly the situation where "type mismatch" without a
/// position is useless.
pub fn build_chain(input: StreamSpec, specs: &[NodeSpec], reg: &Registry) -> Result<Graph> {
    let mut nodes: Vec<Box<dyn Node>> = Vec::with_capacity(specs.len());
    for (i, s) in specs.iter().enumerate() {
        if !reg.contains(&s.kind) {
            let known: Vec<&str> = reg.list().map(|d| d.name).collect();
            return Err(common::Error::other(format!(
                "chain node {i}: no node type named {:?}. Known types: {}",
                s.kind,
                known.join(", ")
            )));
        }
        nodes.push(
            reg.build(&s.kind, &s.settings)
                .map_err(|e| common::Error::other(format!("chain node {i} ({}): {e}", s.kind)))?,
        );
    }
    pipeline::chain(input, nodes)
}

/// A ready-made OOK chain for ISM decoding: shift, decimate, envelope, detect,
/// decode.
pub fn ook_chain(shift_hz: f64, decimate: usize, reset_us: u32) -> Vec<NodeSpec> {
    vec![
        NodeSpec::new("mixer").f("shift_hz", shift_hz),
        NodeSpec::new("decimate").i("factor", decimate as i64),
        NodeSpec::new("envelope"),
        NodeSpec::new("pulse_detect").f("reset_us", reset_us as f64).i("min_pulses", 20),
        NodeSpec::new("protocol_decode"),
    ]
}
