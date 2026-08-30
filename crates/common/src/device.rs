//! The hardware abstraction every driver implements.
//!
//! Deliberately narrow. Drivers expose capabilities as data (`DeviceInfo`) so
//! the UI can build controls generically instead of special-casing each radio.

use crate::error::Result;
use crate::iq::{IqBuf, SampleFormat};
use crate::units::{Hz, Sps};
use std::ops::RangeInclusive;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DriverKind {
    RtlSdr,
    HackRf,
    File,
    Synthetic,
}

impl DriverKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RtlSdr => "rtlsdr",
            Self::HackRf => "hackrf",
            Self::File => "file",
            Self::Synthetic => "synthetic",
        }
    }
}

/// A tunable span. Tuners like the E4000 have gaps, so a device reports a list.
#[derive(Clone, Debug)]
pub struct TunerRange {
    pub range: RangeInclusive<Hz>,
    pub label: &'static str,
}

/// How gain is being controlled for one stage.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum GainMode {
    /// Hardware or driver AGC picks the gain.
    Auto,
    /// Fixed gain in dB. Drivers snap to the nearest supported step.
    Manual(f32),
}

/// Everything the UI needs to render controls for a device without knowing
/// which driver it is.
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub kind: DriverKind,
    /// Stable identifier usable to reopen this exact unit: serial where the
    /// hardware has one, otherwise a bus path.
    pub id: String,
    pub label: String,
    pub tuner: String,
    pub ranges: Vec<TunerRange>,
    /// Discrete rates if the device only does a fixed set, else empty and
    /// `rate_range` applies.
    pub rates: Vec<Sps>,
    pub rate_range: RangeInclusive<Sps>,
    /// Named gain stages with their dB bounds, in signal-path order.
    pub gain_stages: Vec<(String, RangeInclusive<f32>)>,
    pub native_format: SampleFormat,
    /// Usable fraction of the sample rate before the analogue filter rolls off.
    /// HackRF's usable span is well under its nominal rate; the channelizer
    /// uses this to avoid detecting garbage at the band edges.
    pub usable_bandwidth_ratio: f32,
}

impl DeviceInfo {
    pub fn covers(&self, f: Hz) -> bool {
        self.ranges.iter().any(|r| r.range.contains(&f))
    }
}

/// An opened radio. Control operations only; sampling happens on `RxStream` so
/// the streaming thread never contends with the UI thread for the device lock.
pub trait Device: Send {
    fn info(&self) -> &DeviceInfo;

    fn set_center(&mut self, f: Hz) -> Result<()>;
    fn center(&self) -> Hz;

    fn set_rate(&mut self, r: Sps) -> Result<()>;
    fn rate(&self) -> Sps;

    /// Set one gain stage by the name given in `DeviceInfo::gain_stages`.
    fn set_gain(&mut self, stage: &str, mode: GainMode) -> Result<()>;

    /// Correction in parts per million applied to the reference oscillator.
    fn set_ppm(&mut self, _ppm: f64) -> Result<()> {
        Ok(())
    }

    /// Begin streaming. Consumes control of sampling until the stream drops.
    fn start_rx(&mut self) -> Result<Box<dyn RxStream>>;
}

/// A running sample stream.
pub trait RxStream: Send {
    /// Block until the next buffer is available.
    ///
    /// Returns `Err(Error::Disconnected)` once the device is gone. Buffers
    /// carry a sequence number so a gap indicates dropped samples rather than
    /// the caller having to time the calls.
    fn read(&mut self) -> Result<IqBuf>;

    /// Total samples dropped since the stream started.
    fn dropped(&self) -> u64;

    /// Request the stream stop. `read` will return `Disconnected` afterwards.
    fn stop(&mut self);
}
