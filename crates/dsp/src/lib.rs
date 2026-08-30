//! Signal processing primitives for super-radio.
//!
//! All hot paths take slices and reuse buffers; nothing here allocates per
//! sample. Parallelism is the caller's business, so these types are `Send` but
//! not internally threaded.

pub mod channelizer;
pub mod demod;
pub mod detect;
pub mod fir;
pub mod mixer;
pub mod pulse;
pub mod window;

pub use channelizer::Channelizer;
pub use demod::{AmDemod, Deemphasis, FmDemod};
pub use detect::{Burst, Detector, DetectorConfig, NoiseFloor};
pub use fir::{Fir, FirDecim};
pub use mixer::Mixer;
pub use pulse::{OokDetector, Package, Pulse, PulseConfig, PulseStats};
