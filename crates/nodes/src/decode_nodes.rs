//! Pulse extraction and protocol decoding as graph nodes.
//!
//! This is where the architecture pays off. `PulseDetectNode` is the boundary:
//! everything above it is per-sample DSP, everything below is integer parsing.
//! `ProtocolDecodeNode` sits below and is cheap enough to run every known
//! protocol against every burst.

use common::Result;
use decode::protocol::{DecodeError, Protocols};
use dsp::{OokDetector, PulseConfig};
use pipeline::event::{Decoded, Event};
use pipeline::node::{NodeCtx, PortSpec, Simple};
use pipeline::param::{Param, ParamValue};
use pipeline::port::{Payload, PortKind, StreamSpec, Tag, TagValue};

/// Envelope to pulse packages.
pub struct PulseDetectNode {
    cfg: PulseConfig,
    det: OokDetector,
}

impl PulseDetectNode {
    pub fn new(cfg: PulseConfig) -> Self {
        Self { cfg, det: OokDetector::new(1.0, cfg) }
    }

    pub fn default_ook() -> Self {
        Self::new(PulseConfig::default())
    }
}

impl Simple for PulseDetectNode {
    fn name(&self) -> &str {
        "pulse_detect"
    }

    fn negotiate(&mut self, i: &PortSpec) -> Result<StreamSpec> {
        if i.spec.kind != PortKind::Real {
            return Err(common::Error::other(
                "pulse_detect needs a real envelope; put an `envelope` node before it",
            ));
        }
        self.det = OokDetector::new(i.spec.rate, self.cfg);
        // Packages are events in time, not a sampled stream, so a "rate" here
        // would be a fiction. Zero says so explicitly rather than inviting
        // something downstream to divide by it.
        let mut out = i.spec.with_kind(PortKind::Pulses);
        out.rate = 0.0;
        Ok(out)
    }

    fn process(&mut self, i: &Payload, o: &mut Payload, c: &mut NodeCtx<'_>) -> Result<()> {
        let pkgs = o.pulses_mut();
        self.det.process(i.as_real().unwrap(), pkgs);
        for p in pkgs.iter() {
            // Tag the burst so anything downstream, or a waterfall, can point
            // at exactly where in the stream it happened.
            c.tag(Tag::new(p.start_sample, "burst", TagValue::Float(p.snr_db as f64)));
        }

        // Report what was thrown away. Without this a mistuned chain produces
        // total silence, which looks identical to a disconnected antenna and
        // gives no hint which parameter is wrong.
        let s = self.det.take_stats();
        if s.rejected_total() > 0 {
            let mut why = Vec::new();
            if s.rejected_too_few_pulses > 0 {
                why.push(format!(
                    "{} with fewer than {} pulses (raise reset_us, or lower min_pulses)",
                    s.rejected_too_few_pulses, self.cfg.min_pulses
                ));
            }
            if s.rejected_low_snr > 0 {
                why.push(format!(
                    "{} below {:.0} dB SNR (lower min_snr_db, or increase gain)",
                    s.rejected_low_snr, self.cfg.min_snr_db
                ));
            }
            c.emit(Event::Warning {
                stage: "pulse_detect".into(),
                message: format!("discarded {}", why.join("; ")),
            });
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.det.reset();
    }

    fn params(&self) -> Vec<Param> {
        vec![
            Param::float("reset_us", self.cfg.reset_us as f64, 500.0..=100_000.0)
                .unit("us")
                .label("Gap that ends a packet")
                .log(),
            Param::float("min_mark_us", self.cfg.min_mark_us as f64, 10.0..=2000.0)
                .unit("us")
                .label("Shortest credible mark"),
            Param::int("min_pulses", self.cfg.min_pulses as i64, 2..=512)
                .label("Minimum pulses per packet"),
            Param::float("min_snr_db", self.cfg.min_snr_db as f64, 3.0..=40.0)
                .unit("dB")
                .label("Minimum SNR"),
            Param::float("hysteresis", self.cfg.hysteresis as f64, 0.0..=0.5)
                .label("Threshold hysteresis"),
        ]
    }

    fn set_param(&mut self, name: &str, v: ParamValue) -> Result<()> {
        let f = v.as_f64().unwrap_or_default();
        match name {
            "reset_us" => self.cfg.reset_us = f.max(1.0) as u32,
            "min_mark_us" => self.cfg.min_mark_us = f.max(0.0) as u32,
            "min_pulses" => self.cfg.min_pulses = f.max(1.0) as usize,
            "min_snr_db" => self.cfg.min_snr_db = f as f32,
            "hysteresis" => self.cfg.hysteresis = f.clamp(0.0, 0.9) as f32,
            _ => {
                return Err(common::Error::other(format!(
                    "pulse_detect: unknown parameter {name:?}"
                )))
            }
        }
        // The detector caches derived values, so rebuild at the current rate.
        let rate = self.det.rate();
        self.det = OokDetector::new(rate, self.cfg);
        Ok(())
    }
}

/// Run protocols against pulse packages and emit decodes as events.
pub struct ProtocolDecodeNode {
    protocols: Protocols,
    /// Report every protocol that claims a package, rather than only the first.
    report_all: bool,
    /// Emit a warning event when a package matched a protocol's timings but
    /// failed its CRC.
    report_crc_failures: bool,
}

impl ProtocolDecodeNode {
    pub fn new(protocols: Protocols) -> Self {
        Self { protocols, report_all: true, report_crc_failures: true }
    }

    pub fn all() -> Self {
        Self::new(Protocols::all())
    }

    pub fn protocols(&self) -> &Protocols {
        &self.protocols
    }
}

impl Simple for ProtocolDecodeNode {
    fn name(&self) -> &str {
        "protocol_decode"
    }

    fn negotiate(&mut self, i: &PortSpec) -> Result<StreamSpec> {
        if i.spec.kind != PortKind::Pulses {
            return Err(common::Error::other(
                "protocol_decode needs pulses; put a `pulse_detect` node before it",
            ));
        }
        Ok(i.spec.with_kind(PortKind::Bytes))
    }

    fn process(&mut self, i: &Payload, o: &mut Payload, c: &mut NodeCtx<'_>) -> Result<()> {
        let out = o.bytes_mut();
        for pkg in i.as_pulses().unwrap() {
            let mut matched = false;
            for (name, res) in self.protocols.diagnose(pkg) {
                match res {
                    Ok(report) => {
                        matched = true;
                        out.extend_from_slice(&report.raw);
                        c.emit(Event::Decoded(Decoded {
                            protocol: report.model,
                            center: c.inputs[0].spec.center,
                            at: pkg.start_sample as f64,
                            payload: report.raw.clone(),
                            text: Some(report.to_string()),
                            crc_ok: report.crc_valid,
                        }));
                        if !self.report_all {
                            break;
                        }
                    }
                    Err(DecodeError::CrcFailed) if self.report_crc_failures => {
                        // Distinguishing "wrong protocol" from "right protocol,
                        // bad reception" is the difference between a silent
                        // tool and one that tells you to move the antenna.
                        c.emit(Event::Warning {
                            stage: name.to_string(),
                            message: format!(
                                "timings matched but CRC failed ({} pulses, {:.1} dB SNR)",
                                pkg.pulses.len(),
                                pkg.snr_db
                            ),
                        });
                    }
                    Err(_) => {}
                }
            }
            if !matched {
                c.emit(Event::Warning {
                    stage: "protocol_decode".into(),
                    message: format!(
                        "unrecognised burst: {} pulses, {:.1} ms, {:.1} dB; marks {:?}",
                        pkg.pulses.len(),
                        pkg.duration_us() as f64 / 1000.0,
                        pkg.snr_db,
                        pkg.mark_histogram(100)
                    ),
                });
            }
        }
        Ok(())
    }

    fn params(&self) -> Vec<Param> {
        vec![
            Param::bool("report_all", self.report_all).label("Report every matching protocol"),
            Param::bool("report_crc_failures", self.report_crc_failures)
                .label("Warn on CRC failures"),
        ]
    }

    fn set_param(&mut self, name: &str, v: ParamValue) -> Result<()> {
        match name {
            "report_all" => self.report_all = v.as_bool().unwrap_or(true),
            "report_crc_failures" => self.report_crc_failures = v.as_bool().unwrap_or(true),
            _ => {
                return Err(common::Error::other(format!(
                    "protocol_decode: unknown parameter {name:?}"
                )))
            }
        }
        Ok(())
    }
}
