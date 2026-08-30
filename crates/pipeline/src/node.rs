//! The `Node` trait: one block in the signal flow graph.

use crate::event::Event;
use crate::param::{Param, ParamValue};
use crate::port::{Payload, StreamSpec, Tag};
use common::Result;

/// What a node is told about one of its input ports during negotiation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PortSpec {
    pub spec: StreamSpec,
    /// Cumulative group delay from the graph source to this port, expressed in
    /// samples at *this port's* rate.
    ///
    /// Fan-in is the whole reason this exists. Two paths into a merge node
    /// almost never have equal delay, because each filter along the way adds
    /// its own. Without this a combiner silently adds misaligned signals, which
    /// looks like a mysterious loss of SNR rather than an obvious bug.
    pub latency: u64,
}

/// Per-call context.
pub struct NodeCtx<'a> {
    /// Index of the first input sample in this call, from stream start.
    pub sample_index: u64,
    /// Specs of each input port.
    pub inputs: &'a [PortSpec],
    /// Tags landing within this call's input window, sorted by index.
    pub in_tags: &'a [Tag],
    events: &'a mut Vec<Event>,
    out_tags: &'a mut Vec<Tag>,
}

impl<'a> NodeCtx<'a> {
    pub fn new(
        sample_index: u64,
        inputs: &'a [PortSpec],
        in_tags: &'a [Tag],
        events: &'a mut Vec<Event>,
        out_tags: &'a mut Vec<Tag>,
    ) -> Self {
        Self { sample_index, inputs, in_tags, events, out_tags }
    }

    /// Report something that is not a sample: a detection, a decoded frame.
    pub fn emit(&mut self, e: Event) {
        self.events.push(e);
    }

    /// Attach metadata to an absolute output sample index. Tags propagate
    /// downstream automatically, rate-scaled at each node.
    pub fn tag(&mut self, t: Tag) {
        self.out_tags.push(t);
    }

    pub fn timestamp(&self) -> f64 {
        let rate = self.inputs.first().map(|p| p.spec.rate).unwrap_or(1.0);
        self.sample_index as f64 / rate
    }
}

/// A block in the graph.
///
/// Unlike GNU Radio there is no `forecast`/`consume`/`produce` protocol. A node
/// is handed whatever arrived and writes whatever it can; if it needs to
/// accumulate (a framer waiting for a full packet) it buffers internally and
/// emits nothing that call. This removes the single largest class of bugs in
/// GNU Radio block authoring at the cost of each node owning a little state.
pub trait Node: Send {
    fn name(&self) -> &str;

    fn num_inputs(&self) -> usize {
        1
    }

    fn num_outputs(&self) -> usize {
        1
    }

    /// Validate inputs and declare one spec per output port.
    ///
    /// Also where rate-dependent state is built: a filter designs its taps
    /// here, since only now does it know its input rate. Must be idempotent,
    /// as it is re-run whenever an upstream rate changes.
    fn negotiate(&mut self, inputs: &[PortSpec]) -> Result<Vec<StreamSpec>>;

    /// Group delay this node adds on `port`, in output samples. A symmetric
    /// FIR reports half its tap count.
    fn latency(&self, _port: usize) -> u64 {
        0
    }

    /// Transform inputs into outputs. Output buffers arrive cleared and of the
    /// negotiated variant.
    fn process(
        &mut self,
        inputs: &[&Payload],
        outputs: &mut [Payload],
        ctx: &mut NodeCtx<'_>,
    ) -> Result<()>;

    /// Drop all history: called on retune, stream restart, or channel reuse.
    fn reset(&mut self) {}

    fn params(&self) -> Vec<Param> {
        Vec::new()
    }

    fn set_param(&mut self, name: &str, _value: ParamValue) -> Result<()> {
        Err(common::Error::other(format!("{}: unknown parameter {name:?}", self.name())))
    }
}

/// Convenience for the overwhelmingly common single-in single-out node.
///
/// Implement this and get a `Node` impl for free, without hand-writing slice
/// indexing in every filter.
pub trait Simple: Send {
    fn name(&self) -> &str;
    fn negotiate(&mut self, input: &PortSpec) -> Result<StreamSpec>;
    fn latency(&self) -> u64 {
        0
    }
    fn process(
        &mut self,
        input: &Payload,
        output: &mut Payload,
        ctx: &mut NodeCtx<'_>,
    ) -> Result<()>;
    fn reset(&mut self) {}
    fn params(&self) -> Vec<Param> {
        Vec::new()
    }
    fn set_param(&mut self, name: &str, _value: ParamValue) -> Result<()> {
        Err(common::Error::other(format!("{}: unknown parameter {name:?}", self.name())))
    }
}

impl<T: Simple> Node for T {
    fn name(&self) -> &str {
        Simple::name(self)
    }
    fn negotiate(&mut self, inputs: &[PortSpec]) -> Result<Vec<StreamSpec>> {
        let input = inputs
            .first()
            .ok_or_else(|| common::Error::other(format!("{}: needs one input", Simple::name(self))))?;
        Ok(vec![Simple::negotiate(self, input)?])
    }
    fn latency(&self, _port: usize) -> u64 {
        Simple::latency(self)
    }
    fn process(
        &mut self,
        inputs: &[&Payload],
        outputs: &mut [Payload],
        ctx: &mut NodeCtx<'_>,
    ) -> Result<()> {
        Simple::process(self, inputs[0], &mut outputs[0], ctx)
    }
    fn reset(&mut self) {
        Simple::reset(self)
    }
    fn params(&self) -> Vec<Param> {
        Simple::params(self)
    }
    fn set_param(&mut self, name: &str, value: ParamValue) -> Result<()> {
        Simple::set_param(self, name, value)
    }
}
