//! Device protocols.
//!
//! Each module is one family of devices. They share the slicers and the pulse
//! detector entirely, so the marginal cost of a protocol is its frame layout
//! and integrity check, not another DSP chain.

mod fineoffset;

pub use fineoffset::FineOffsetWh1080;
