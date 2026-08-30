//! DSP primitives wrapped as graph nodes.
//!
//! Each is a thin adapter: the arithmetic stays in `dsp`, and these add
//! rate negotiation and parameter introspection. Keeping them separate means
//! `dsp` is usable without the graph, and the graph never constrains how the
//! DSP is written.

use common::{Result, C32};
use dsp::{Deemphasis, FirDecim, FmDemod, Mixer};
use pipeline::param::{Param, ParamValue};
use pipeline::node::{NodeCtx, PortSpec, Simple};
use pipeline::port::{Payload, PortKind, StreamSpec};

/// Shift a signal in frequency.
///
/// Used to bring an off-centre signal to baseband. Tuning a receiver so the
/// signal of interest sits at 0 Hz puts it directly on the RTL2832U's DC spur,
/// so the usual arrangement is to tune deliberately off and correct here.
pub struct MixerNode {
    shift_hz: f64,
    mixer: Mixer,
}

impl MixerNode {
    pub fn new(shift_hz: f64) -> Self {
        Self { shift_hz, mixer: Mixer::new(shift_hz, 1.0) }
    }
}

impl Simple for MixerNode {
    fn name(&self) -> &str {
        "mixer"
    }

    fn negotiate(&mut self, i: &PortSpec) -> Result<StreamSpec> {
        if i.spec.kind != PortKind::Iq {
            return Err(common::Error::other("mixer needs an IQ input"));
        }
        self.mixer.set_shift(self.shift_hz, i.spec.rate);
        // The centre frequency moves with the shift, so anything downstream
        // reporting "where did this come from" stays correct.
        let mut out = i.spec;
        out.center = common::Hz(
            (i.spec.center.get() as i64).saturating_sub(self.shift_hz as i64).max(0) as u64,
        );
        Ok(out)
    }

    fn process(&mut self, i: &Payload, o: &mut Payload, _c: &mut NodeCtx<'_>) -> Result<()> {
        self.mixer.process(i.as_iq().unwrap(), o.iq_mut());
        Ok(())
    }

    fn reset(&mut self) {
        self.mixer.reset();
    }

    fn params(&self) -> Vec<Param> {
        vec![Param::float("shift_hz", self.shift_hz, -30e6..=30e6)
            .unit("Hz")
            .label("Frequency shift")]
    }

    fn set_param(&mut self, name: &str, v: ParamValue) -> Result<()> {
        match name {
            "shift_hz" => {
                self.shift_hz = v.as_f64().ok_or_else(|| common::Error::other("expected a number"))?;
                Ok(())
            }
            _ => Err(common::Error::other(format!("mixer: unknown parameter {name:?}"))),
        }
    }
}

/// Lowpass and decimate.
pub struct DecimateNode {
    factor: usize,
    passband: f64,
    atten_db: f64,
    dec: FirDecim,
}

impl DecimateNode {
    pub fn new(factor: usize) -> Self {
        Self {
            factor: factor.max(1),
            passband: 0.9,
            atten_db: 80.0,
            dec: FirDecim::design(factor.max(1), 0.9, 80.0),
        }
    }
}

impl Simple for DecimateNode {
    fn name(&self) -> &str {
        "decimate"
    }

    fn negotiate(&mut self, i: &PortSpec) -> Result<StreamSpec> {
        if i.spec.kind != PortKind::Iq {
            return Err(common::Error::other("decimate needs an IQ input"));
        }
        // Rebuild here, not in the constructor: the tap count depends on the
        // transition width, which is only knowable once the rate is.
        self.dec = FirDecim::design(self.factor, self.passband, self.atten_db);
        Ok(i.spec.with_rate(i.spec.rate / self.factor as f64))
    }

    fn latency(&self) -> u64 {
        // A symmetric FIR delays by half its length, measured at the output
        // rate. Reporting this is what lets a fan-in node align its branches.
        self.dec.latency() as u64
    }

    fn process(&mut self, i: &Payload, o: &mut Payload, _c: &mut NodeCtx<'_>) -> Result<()> {
        self.dec.process(i.as_iq().unwrap(), o.iq_mut());
        Ok(())
    }

    fn params(&self) -> Vec<Param> {
        vec![
            Param::int("factor", self.factor as i64, 1..=1024)
                .label("Decimation")
                .affects_rate(),
            Param::float("passband", self.passband, 0.5..=0.99).label("Passband fraction"),
            Param::float("atten_db", self.atten_db, 30.0..=120.0)
                .unit("dB")
                .label("Stopband attenuation"),
        ]
    }

    fn set_param(&mut self, name: &str, v: ParamValue) -> Result<()> {
        match name {
            "factor" => {
                self.factor = v.as_i64().unwrap_or(1).max(1) as usize;
                Ok(())
            }
            "passband" => {
                self.passband = v.as_f64().unwrap_or(0.9).clamp(0.1, 0.99);
                Ok(())
            }
            "atten_db" => {
                self.atten_db = v.as_f64().unwrap_or(80.0).clamp(20.0, 150.0);
                Ok(())
            }
            _ => Err(common::Error::other(format!("decimate: unknown parameter {name:?}"))),
        }
    }
}

/// Complex magnitude: the envelope an OOK detector needs.
pub struct EnvelopeNode;

impl Simple for EnvelopeNode {
    fn name(&self) -> &str {
        "envelope"
    }

    fn negotiate(&mut self, i: &PortSpec) -> Result<StreamSpec> {
        if i.spec.kind != PortKind::Iq {
            return Err(common::Error::other("envelope needs an IQ input"));
        }
        Ok(i.spec.with_kind(PortKind::Real))
    }

    fn process(&mut self, i: &Payload, o: &mut Payload, _c: &mut NodeCtx<'_>) -> Result<()> {
        o.real_mut().extend(i.as_iq().unwrap().iter().map(|c| c.norm()));
        Ok(())
    }
}

/// Frequency demodulator.
pub struct FmDemodNode {
    deviation_hz: f64,
    demod: FmDemod,
}

impl FmDemodNode {
    pub fn new(deviation_hz: f64) -> Self {
        Self { deviation_hz, demod: FmDemod::new(1.0, deviation_hz) }
    }

    /// Broadcast WFM: 75 kHz peak deviation.
    pub fn wide() -> Self {
        Self::new(75_000.0)
    }

    /// Narrowband voice and most FSK telemetry.
    pub fn narrow() -> Self {
        Self::new(5_000.0)
    }
}

impl Simple for FmDemodNode {
    fn name(&self) -> &str {
        "fm_demod"
    }

    fn negotiate(&mut self, i: &PortSpec) -> Result<StreamSpec> {
        if i.spec.kind != PortKind::Iq {
            return Err(common::Error::other("fm_demod needs an IQ input"));
        }
        self.demod = FmDemod::new(i.spec.rate, self.deviation_hz);
        Ok(i.spec.with_kind(PortKind::Real))
    }

    fn process(&mut self, i: &Payload, o: &mut Payload, _c: &mut NodeCtx<'_>) -> Result<()> {
        self.demod.process(i.as_iq().unwrap(), o.real_mut());
        Ok(())
    }

    fn reset(&mut self) {
        self.demod.reset();
    }

    fn params(&self) -> Vec<Param> {
        vec![Param::float("deviation_hz", self.deviation_hz, 500.0..=200_000.0)
            .unit("Hz")
            .label("Peak deviation")
            .log()]
    }

    fn set_param(&mut self, name: &str, v: ParamValue) -> Result<()> {
        match name {
            "deviation_hz" => {
                self.deviation_hz = v.as_f64().unwrap_or(75_000.0).max(1.0);
                Ok(())
            }
            _ => Err(common::Error::other(format!("fm_demod: unknown parameter {name:?}"))),
        }
    }
}

/// FM de-emphasis, undoing the transmitter's treble boost.
pub struct DeemphasisNode {
    tau_us: f64,
    filt: Deemphasis,
}

impl DeemphasisNode {
    pub fn new(tau_us: f64) -> Self {
        Self { tau_us, filt: Deemphasis::new(1.0, tau_us) }
    }
}

impl Simple for DeemphasisNode {
    fn name(&self) -> &str {
        "deemphasis"
    }

    fn negotiate(&mut self, i: &PortSpec) -> Result<StreamSpec> {
        if i.spec.kind != PortKind::Real {
            return Err(common::Error::other("deemphasis needs a real input"));
        }
        self.filt = Deemphasis::new(i.spec.rate, self.tau_us);
        Ok(i.spec)
    }

    fn process(&mut self, i: &Payload, o: &mut Payload, _c: &mut NodeCtx<'_>) -> Result<()> {
        let out = o.real_mut();
        out.extend_from_slice(i.as_real().unwrap());
        self.filt.process(out);
        Ok(())
    }

    fn reset(&mut self) {
        self.filt.reset();
    }

    fn params(&self) -> Vec<Param> {
        vec![Param::float("tau_us", self.tau_us, 25.0..=100.0)
            .unit("us")
            .label("Time constant (50 EU, 75 Americas)")]
    }

    fn set_param(&mut self, name: &str, v: ParamValue) -> Result<()> {
        match name {
            "tau_us" => {
                self.tau_us = v.as_f64().unwrap_or(50.0).clamp(1.0, 1000.0);
                Ok(())
            }
            _ => Err(common::Error::other(format!("deemphasis: unknown parameter {name:?}"))),
        }
    }
}

/// Decimate a real-valued stream, for audio after a demodulator.
pub struct RealDecimateNode {
    factor: usize,
    dec: FirDecim,
    scratch: Vec<C32>,
    out: Vec<C32>,
}

impl RealDecimateNode {
    pub fn new(factor: usize) -> Self {
        Self {
            factor: factor.max(1),
            dec: FirDecim::design(factor.max(1), 0.9, 80.0),
            scratch: Vec::new(),
            out: Vec::new(),
        }
    }
}

impl Simple for RealDecimateNode {
    fn name(&self) -> &str {
        "real_decimate"
    }

    fn negotiate(&mut self, i: &PortSpec) -> Result<StreamSpec> {
        if i.spec.kind != PortKind::Real {
            return Err(common::Error::other("real_decimate needs a real input"));
        }
        self.dec = FirDecim::design(self.factor, 0.9, 80.0);
        Ok(i.spec.with_rate(i.spec.rate / self.factor as f64))
    }

    fn process(&mut self, i: &Payload, o: &mut Payload, _c: &mut NodeCtx<'_>) -> Result<()> {
        // Reuses the complex decimator with a zero imaginary part. Wasteful by
        // half, but it keeps one well-tested filter implementation instead of
        // two that can drift apart.
        self.scratch.clear();
        self.scratch.extend(i.as_real().unwrap().iter().map(|v| C32::new(*v, 0.0)));
        self.out.clear();
        self.dec.process(&self.scratch, &mut self.out);
        o.real_mut().extend(self.out.iter().map(|c| c.re));
        Ok(())
    }

    fn params(&self) -> Vec<Param> {
        vec![Param::int("factor", self.factor as i64, 1..=1024)
            .label("Decimation")
            .affects_rate()]
    }

    fn set_param(&mut self, name: &str, v: ParamValue) -> Result<()> {
        match name {
            "factor" => {
                self.factor = v.as_i64().unwrap_or(1).max(1) as usize;
                Ok(())
            }
            _ => Err(common::Error::other(format!("real_decimate: unknown parameter {name:?}"))),
        }
    }
}
