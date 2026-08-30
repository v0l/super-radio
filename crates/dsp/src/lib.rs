//! Signal processing primitives for super-radio.
//!
//! All hot paths take slices and reuse buffers; nothing here allocates per
//! sample. Parallelism is the caller's business, so these types are `Send` but
//! not internally threaded.

pub mod blend;
pub mod channelizer;
pub mod dc;
pub mod demod;
pub mod detect;
pub mod fir;
pub mod mixer;
pub mod pulse;
pub mod rds;
pub mod spectrum;
pub mod stereo;
pub mod window;

pub use blend::{HighBlend, NoiseMeter, VariableLowpass};
pub use channelizer::Channelizer;
pub use dc::DcBlock;
pub use demod::{AmDemod, Deemphasis, FmDemod};
pub use detect::{Burst, Detector, DetectorConfig, NoiseFloor};
pub use fir::{Fir, FirDecim, FirDecimReal};
pub use mixer::Mixer;
pub use spectrum::Spectrum;
pub use stereo::StereoDecoder;
pub use pulse::{OokDetector, Package, Pulse, PulseConfig, PulseStats};
