//! Sample sources: files, synthetic signals, and (via the driver crates) live
//! hardware.

pub mod file;

pub use file::{parse_filename, FileMeta, FileSource};
