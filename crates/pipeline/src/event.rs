//! Out-of-band results produced by stages.

use common::Hz;

/// Anything a stage wants to report that is not a sample.
///
/// Events travel out of a chain alongside the sample output, so a decoder can
/// surface a packet without needing a channel back to the UI, and without the
/// sample path becoming generic over a sink type.
#[derive(Clone, Debug)]
pub enum Event {
    /// A signal appeared or vanished in this chain's band.
    Squelch { open: bool, at: f64, level_db: f32 },

    /// A detector believes there is a carrier here.
    Detection {
        center: Hz,
        bandwidth: f64,
        snr_db: f32,
        at: f64,
    },

    /// A decoder produced a frame.
    Decoded(Decoded),

    /// Periodic measurement for the UI: level meters, lock indicators.
    Metric { name: &'static str, value: f64 },

    /// Something went wrong but the chain can continue: a CRC failure, a
    /// framing slip. Fatal problems come back as `Err` from `process`.
    Warning { stage: String, message: String },
}

/// A successfully decoded frame from some protocol.
#[derive(Clone, Debug)]
pub struct Decoded {
    /// Protocol identifier: "pocsag", "ais", "adsb", "rds".
    pub protocol: &'static str,
    /// Where it came from, for the log and for correlating across channels.
    pub center: Hz,
    /// Seconds since stream start.
    pub at: f64,
    /// Raw payload bytes, before any protocol-specific interpretation.
    pub payload: Vec<u8>,
    /// Human-readable rendering, if the decoder can produce one.
    pub text: Option<String>,
    /// Whether an integrity check passed. `None` means the protocol has none,
    /// which matters: an unchecked decode should never be presented with the
    /// same confidence as a CRC-verified one.
    pub crc_ok: Option<bool>,
}
